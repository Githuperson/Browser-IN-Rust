//! Installed-extension registry.
//!
//! The registry owns the on-disk layout:
//!
//! ```text
//! <profile>/extensions/<id>/      unpacked extension (extracted from a CRX/zip, or
//!                                 referenced in place for developer installs)
//! <profile>/extension-state/<id>.json   extension's `storage.local` / `storage.sync`
//! <profile>/extensions.json       the registry index (id → directory, kind, enabled)
//! ```
//!
//! The index is what makes `enabled` survive restarts; the directories are what make
//! reinstalling an extension keep its id (and therefore its storage and content-script
//! registrations).

use std::{
  collections::HashMap,
  fs,
  path::{Component, Path, PathBuf},
};

use bir_core::{ipc::ExtensionView, ProfilePaths};
use sha2::Digest;
use serde::{Deserialize, Serialize};

use crate::{
  crx,
  bridge::ScriptInjection,
  manifest::{load_messages, Manifest, Messages, RunAt},
  matches::{self, MatchPattern},
  permissions::PermissionSet,
  Error, Result,
};

/// Largest single content/background script we will inline into a webview.
const MAX_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
  /// A directory on disk, loaded as-is (developer mode).
  Unpacked,
  /// Extracted from a `.crx` or `.zip`.
  Packaged,
  /// Handed to the platform webview's own extension loader (WebView2 on Windows).
  Native,
}

impl ExtensionKind {
  pub fn as_str(&self) -> &'static str {
    match self {
      ExtensionKind::Unpacked => "unpacked",
      ExtensionKind::Packaged => "packaged",
      ExtensionKind::Native => "native",
    }
  }
}

pub struct InstalledExtension {
  pub id: String,
  pub manifest: Manifest,
  pub dir: PathBuf,
  pub enabled: bool,
  pub kind: ExtensionKind,
  /// Not owned by the profile: the directory lives wherever the developer put it.
  pub external: bool,
  /// Set when the extension is installed but could not be fully loaded.
  pub error: Option<String>,
  pub messages: Messages,
  pub permissions: PermissionSet,
}

impl InstalledExtension {
  fn permission_set(&self) -> PermissionSet {
    PermissionSet::new(
      self.manifest.permissions.clone(),
      self.manifest.host_permissions.clone(),
    )
  }

  /// Read a file from inside the extension directory.
  ///
  /// `..` and absolute paths are rejected: extension resources are addressed by
  /// `bir://<id>/<path>` and must never escape the extension directory.
  pub fn read_resource(&self, path: &str) -> Option<Vec<u8>> {
    let resolved = safe_join(&self.dir, path)?;
    fs::read(&resolved).ok()
  }

  pub fn resource_path(&self, path: &str) -> Option<PathBuf> {
    safe_join(&self.dir, path)
  }

  /// Inlined content-script batch for `url`, or `None` when nothing applies.
  pub fn content_scripts_for(&self, url: &str) -> Option<ScriptInjection> {
    if !self.enabled || self.error.is_some() {
      return None;
    }
    let mut js = Vec::new();
    let mut css = Vec::new();
    let mut run_at = RunAt::DocumentIdle;
    let mut all_frames = false;
    let mut any = false;

    for script in &self.manifest.content_scripts {
      let includes: Vec<MatchPattern> =
        script.matches.iter().filter_map(|p| MatchPattern::parse(p)).collect();
      let excludes: Vec<MatchPattern> = script
        .exclude_matches
        .iter()
        .filter_map(|p| MatchPattern::parse(p))
        .collect();
      if !matches::matches_any(&includes, &excludes, url) {
        continue;
      }
      any = true;
      for file in &script.js {
        if let Some(text) = self.read_text(file) {
          js.push(text);
        }
      }
      for file in &script.css {
        if let Some(text) = self.read_text(file) {
          css.push(text);
        }
      }
      // Earliest run_at wins, so a document_start script is not delayed by a sibling
      // script declared document_idle.
      if script.run_at == RunAt::DocumentStart {
        run_at = RunAt::DocumentStart;
      } else if script.run_at == RunAt::DocumentEnd && run_at != RunAt::DocumentStart {
        run_at = RunAt::DocumentEnd;
      }
      all_frames = all_frames || script.all_frames;
    }

    if !any || (js.is_empty() && css.is_empty()) {
      return None;
    }

    Some(ScriptInjection {
      extension_id: self.id.clone(),
      run_at,
      all_frames,
      js,
      css,
    })
  }

  /// Background script bodies, in manifest order.
  pub fn background_scripts(&self) -> Vec<String> {
    if !self.enabled || self.error.is_some() {
      return Vec::new();
    }
    self
      .manifest
      .background_scripts()
      .iter()
      .filter_map(|file| self.read_text(file))
      .collect()
  }

  fn read_text(&self, relative: &str) -> Option<String> {
    let path = safe_join(&self.dir, relative)?;
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SCRIPT_BYTES {
      return None;
    }
    match fs::read_to_string(&path) {
      Ok(text) => Some(text),
      Err(_) => fs::read(&path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    }
  }

  pub fn to_view(&self) -> ExtensionView {
    ExtensionView {
      id: self.id.clone(),
      name: self.manifest.name.clone(),
      version: self.manifest.version.clone(),
      description: self.manifest.description.clone(),
      enabled: self.enabled,
      permissions: self.manifest.permissions.clone(),
      kind: self.kind.as_str().to_string(),
      has_options: self.manifest.options_page().is_some(),
      has_popup: popup_path(self).is_some(),
      popup_path: popup_path(self).unwrap_or_default(),
      options_path: self.manifest.options_page().cloned().unwrap_or_default(),
      error: self.error.clone().unwrap_or_default(),
    }
  }
}

/// The toolbar popup path, if the extension declares one and the file exists.
fn popup_path(extension: &InstalledExtension) -> Option<String> {
  let path = extension
    .manifest
    .toolbar_action()
    .and_then(|action| action.default_popup.clone())?;
  if extension.read_resource(&path).is_some() {
    Some(path)
  } else {
    None
  }
}

/// Join `path` onto `root`, refusing anything that escapes it.
fn safe_join(root: &Path, path: &str) -> Option<PathBuf> {
  let trimmed = path.trim_start_matches('/');
  let candidate = Path::new(trimmed);
  for component in candidate.components() {
    match component {
      Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
      _ => {}
    }
  }
  let joined = root.join(candidate);
  if joined.starts_with(root) {
    Some(joined)
  } else {
    None
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Entry {
  id: String,
  dir: PathBuf,
  kind: ExtensionKind,
  enabled: bool,
  external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RegistryDoc {
  entries: Vec<Entry>,
}

pub struct ExtensionRegistry {
  paths: ProfilePaths,
  extensions: Vec<InstalledExtension>,
}

impl ExtensionRegistry {
  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let mut registry = Self {
      paths: paths.clone(),
      extensions: Vec::new(),
    };
    registry.reload()?;
    Ok(registry)
  }

  /// (Re)read the index and the extensions directory from disk.
  pub fn reload(&mut self) -> Result<()> {
    let index_path = self.paths.data_dir().join("extensions.json");
    let mut doc: RegistryDoc = match bir_core::store::read_to_string(&index_path) {
      Ok(Some(text)) => serde_json::from_str(&text).unwrap_or_default(),
      _ => RegistryDoc::default(),
    };

    // Pick up anything dropped into the extensions directory by hand.
    if let Ok(entries) = fs::read_dir(self.paths.extensions_dir()) {
      for dir in entries.flatten().filter(|e| e.path().is_dir()) {
        let path = dir.path();
        if !path.join("manifest.json").exists() {
          continue;
        }
        if doc.entries.iter().any(|e| e.dir == path) {
          continue;
        }
        let id = path
          .file_name()
          .map(|n| n.to_string_lossy().into_owned())
          .unwrap_or_else(|| crx::id_from_path(&path));
        doc.entries.push(Entry {
          id,
          dir: path,
          kind: ExtensionKind::Packaged,
          enabled: true,
          external: false,
        });
      }
    }

    let locale = current_locale();
    let mut extensions = Vec::with_capacity(doc.entries.len());
    for entry in doc.entries {
      match Manifest::load_dir(&entry.dir) {
        Ok(manifest) => {
          let messages = load_messages(&entry.dir, &locale, manifest.default_locale.as_deref());
          let mut installed = InstalledExtension {
            id: entry.id,
            manifest,
            dir: entry.dir,
            enabled: entry.enabled,
            kind: entry.kind,
            external: entry.external,
            error: None,
            messages,
            permissions: PermissionSet::new(Vec::new(), Vec::new()),
          };
          installed.permissions = installed.permission_set();
          extensions.push(installed);
        }
        Err(err) => {
          // Keep the entry visible so the user can remove it, but say what is wrong.
          extensions.push(InstalledExtension {
            id: entry.id,
            manifest: Manifest::default(),
            dir: entry.dir,
            enabled: false,
            kind: entry.kind,
            external: entry.external,
            error: Some(err.to_string()),
            messages: Messages::new(),
            permissions: PermissionSet::new(Vec::new(), Vec::new()),
          });
        }
      }
    }

    self.extensions = extensions;
    Ok(())
  }

  fn persist(&self) -> Result<()> {
    let doc = RegistryDoc {
      entries: self
        .extensions
        .iter()
        .map(|e| Entry {
          id: e.id.clone(),
          dir: e.dir.clone(),
          kind: e.kind,
          enabled: e.enabled,
          external: e.external,
        })
        .collect(),
    };
    let text = serde_json::to_string_pretty(&doc)?;
    bir_core::store::write_atomic(&self.paths.data_dir().join("extensions.json"), text.as_bytes())?;
    Ok(())
  }

  pub fn all(&self) -> &[InstalledExtension] {
    &self.extensions
  }

  pub fn get(&self, id: &str) -> Option<&InstalledExtension> {
    self.extensions.iter().find(|e| e.id == id)
  }

  pub fn get_mut(&mut self, id: &str) -> Option<&mut InstalledExtension> {
    self.extensions.iter_mut().find(|e| e.id == id)
  }

  /// Enabled extensions that loaded cleanly.
  pub fn enabled(&self) -> impl Iterator<Item = &InstalledExtension> {
    self
      .extensions
      .iter()
      .filter(|e| e.enabled && e.error.is_none())
  }

  // ---- installation -------------------------------------------------------

  /// Install from a directory (developer mode), a `.crx`, or a `.zip`.
  ///
  /// Directories are referenced in place so that editing a file and hitting Reload does
  /// what a developer expects. Archives are extracted into the profile.
  pub fn install(&mut self, source: &Path) -> Result<String> {
    if source.is_dir() {
      let manifest = Manifest::load_dir(source)?;
      let id = crx::id_from_path(source);
      self.upsert(InstalledExtension {
        id: id.clone(),
        manifest,
        dir: source.to_path_buf(),
        enabled: true,
        kind: ExtensionKind::Unpacked,
        external: true,
        error: None,
        messages: load_messages(source, &current_locale(), None),
        permissions: PermissionSet::new(Vec::new(), Vec::new()),
      });
      self.persist()?;
      return Ok(id);
    }

    let bytes = fs::read(source).map_err(|e| {
      Error::Extension(format!("cannot read {}: {e}", source.display()))
    })?;

    let (id, archive) = if let Ok(info) = crx::parse_crx(&bytes) {
      let id = info.extension_id.clone().unwrap_or_else(|| {
        let digest = Sha256::digest(&bytes);
        crx::extension_id_from_key(&digest[..])
      });
      (id, info.zip_body(&bytes).to_vec())
    } else if crx::is_zip(&bytes) {
      (crx::id_from_path(source), bytes)
    } else {
      return Err(Error::Extension(
        "not a CRX or zip file, and not a directory".into(),
      ));
    };

    let target = self.paths.extensions_dir().join(&id);
    crx::unpack_package(&archive, &target)?;

    let manifest = Manifest::load_dir(&target)?;
    let messages = load_messages(&target, &current_locale(), manifest.default_locale.as_deref());
    self.upsert(InstalledExtension {
      id: id.clone(),
      manifest,
      dir: target,
      enabled: true,
      kind: ExtensionKind::Packaged,
      external: false,
      error: None,
      messages,
      permissions: PermissionSet::new(Vec::new(), Vec::new()),
    });
    self.persist()?;
    Ok(id)
  }

  fn upsert(&mut self, extension: InstalledExtension) {
    if let Some(existing) = self.extensions.iter_mut().find(|e| e.id == extension.id) {
      *existing = extension;
    } else {
      self.extensions.push(extension);
    }
  }

  /// Uninstall. Directories owned by the profile are deleted; externally referenced
  /// directories are left alone (we did not create them).
  pub fn remove(&mut self, id: &str) -> Result<()> {
    let Some(position) = self.extensions.iter().position(|e| e.id == id) else {
      return Err(Error::Extension("extension not installed".into()));
    };
    let extension = self.extensions.remove(position);
    if !extension.external {
      let _ = fs::remove_dir_all(&extension.dir);
    }
    let state = self.storage_path(id);
    let _ = fs::remove_file(state);
    self.persist()?;
    Ok(())
  }

  pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
    let Some(extension) = self.extensions.iter_mut().find(|e| e.id == id) else {
      return Err(Error::Extension("extension not installed".into()));
    };
    extension.enabled = enabled;
    self.persist()
  }

  /// Re-read a single extension's manifest from disk (used by the Reload button).
  pub fn reload_extension(&mut self, id: &str) -> Result<()> {
    let Some(dir) = self.extensions.iter().find(|e| e.id == id).map(|e| e.dir.clone()) else {
      return Err(Error::Extension("extension not installed".into()));
    };
    match Manifest::load_dir(&dir) {
      Ok(manifest) => {
        let Some(extension) = self.get_mut(id) else {
          return Err(Error::Extension("extension not installed".into()));
        };
        let messages =
          load_messages(&dir, &current_locale(), manifest.default_locale.as_deref());
        extension.manifest = manifest;
        extension.messages = messages;
        extension.error = None;
        extension.permissions = extension.permission_set();
        Ok(())
      }
      Err(err) => {
        if let Some(extension) = self.get_mut(id) {
          extension.error = Some(err.to_string());
        }
        Err(err)
      }
    }
  }

  // ---- lookup -------------------------------------------------------------

  /// Content-script injections that apply to `url`, across every enabled extension.
  /// Does an extension hold an API permission (`tabs`, `storage`, ...)?
  ///
  /// `activeTab`, `scripting` and host permissions resolve on demand: an extension with
  /// `activeTab` gets `tabs`-shaped access limited to the tab it was invoked on, which
  /// this check cannot distinguish, so `activeTab` grants `tabs` too.
  pub fn has_permission(&self, ext: &str, permission: &str) -> bool {
    let Some(extension) = self.get(ext) else {
      return false;
    };
    if !extension.enabled {
      return false;
    }
    extension.permissions.has(permission)
      || extension.permissions.has("activeTab") && permission == "tabs"
  }

  pub fn content_scripts_for(&self, url: &str) -> Vec<ScriptInjection> {
    self
      .enabled()
      .filter_map(|e| e.content_scripts_for(url))
      .collect()
  }

  /// Resolve `bir://<id>/<path>` to a file on disk.
  pub fn resolve_resource(&self, id: &str, path: &str) -> Option<PathBuf> {
    let extension = self.get(id)?;
    let resolved = extension.resource_path(path)?;
    if resolved.is_file() {
      Some(resolved)
    } else {
      None
    }
  }

  /// Where an extension's `storage.local`/`storage.sync` document lives.
  pub fn storage_path(&self, id: &str) -> PathBuf {
    self.paths.extension_state_dir().join(format!("{id}.json"))
  }

  pub fn manifest_json(&self, id: &str) -> serde_json::Value {
    self
      .get(id)
      .map(|e| {
        serde_json::to_value(&e.manifest).unwrap_or_else(|_| serde_json::Value::Null)
      })
      .unwrap_or(serde_json::Value::Null)
  }

  pub fn messages_json(&self, id: &str) -> serde_json::Value {
    self
      .get(id)
      .map(|e| serde_json::to_value(&e.messages).unwrap_or(serde_json::Value::Null))
      .unwrap_or(serde_json::Value::Null)
  }

  /// Serialisable snapshots for the chrome UI.
  pub fn views(&self) -> Vec<ExtensionView> {
    self.extensions.iter().map(|e| e.to_view()).collect()
  }

  // ---- platform integration ----------------------------------------------

  /// Mirror enabled extensions into the directory WebView2 loads extensions from.
  ///
  /// WebView2 loads every immediate subdirectory of its extensions path as an unpacked
  /// Chromium extension, so this is a straight copy of each enabled extension directory.
  /// No-op on every other platform.
  pub fn sync_native_dir(&self) -> Result<usize> {
    if !cfg!(target_os = "windows") {
      return Ok(0);
    }
    let target_root = self.paths.native_extensions_dir();
    // Rebuild from scratch so removals and disabling are reflected.
    let _ = fs::remove_dir_all(&target_root);
    fs::create_dir_all(&target_root)?;

    let mut count = 0;
    for extension in self.enabled() {
      let target = target_root.join(&extension.id);
      copy_dir(&extension.dir, &target)?;
      count += 1;
    }
    Ok(count)
  }

  /// Directory WebView2 should be pointed at, if native loading is in use.
  pub fn native_extensions_dir(&self) -> PathBuf {
    self.paths.native_extensions_dir()
  }

  pub fn paths(&self) -> &ProfilePaths {
    &self.paths
  }
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
  fs::create_dir_all(to)?;
  for entry in fs::read_dir(from)?.flatten() {
    let target = to.join(entry.file_name());
    let meta = entry.metadata()?;
    if meta.is_dir() {
      copy_dir(&entry.path(), &target)?;
    } else {
      let _ = fs::copy(entry.path(), target);
    }
  }
  Ok(())
}

/// Best-effort UI locale, used to pick an extension's message catalogue.
fn current_locale() -> String {
  std::env::var("LANG")
    .or_else(|_| std::env::var("LC_ALL"))
    .unwrap_or_else(|_| "en".into())
    .split('.')
    .next()
    .unwrap_or("en")
    .replace('_', "-")
    .to_ascii_lowercase()
}

/// Extension storage document: a plain JSON object persisted per extension.
pub fn load_storage(path: &Path) -> HashMap<String, serde_json::Value> {
  bir_core::store::read_to_string(path)
    .ok()
    .flatten()
    .and_then(|text| serde_json::from_str(&text).ok())
    .unwrap_or_default()
}

pub fn save_storage(path: &Path, data: &HashMap<String, serde_json::Value>) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  let text = serde_json::to_string(data)?;
  bir_core::store::write_atomic(path, text.as_bytes())?;
  Ok(())
}
