//! Where the browser keeps its stuff.
//!
//! Layout under the OS "data" directory (`~/.local/share` on Linux,
//! `~/Library/Application Support` on macOS, `%APPDATA%` on Windows):
//!
//! ```text
//! bir/
//!   profiles/
//!     default/
//!       settings.json
//!       bookmarks.json
//!       session.json
//!       site-settings.json
//!       search-engines.json
//!       history.ndjson
//!       extensions/
//!         <extension-id>/
//!         state/<extension-id>/storage.json
//!     <profile-name>/
//!   webview-native-extensions/   # unpacked dirs handed to WebView2 on Windows
//! ```
//!
//! Caches live under the OS cache directory and can be deleted at any time without
//! losing user data — that split matters for disk-space pressure handling.

use crate::Result;
use std::path::PathBuf;

/// Name used for the top-level application directory.
pub const APP_DIR: &str = "bir";

/// Every path the browser needs, resolved once at startup.
#[derive(Debug, Clone)]
pub struct ProfilePaths {
  /// Per-profile data directory (settings, history, bookmarks, ...).
  data: PathBuf,
  /// Per-profile cache directory (webview cache lives here too).
  cache: PathBuf,
  /// Where downloads land unless the user overrides it.
  downloads: PathBuf,
  /// Name of the profile, e.g. `default`.
  name: String,
}

impl ProfilePaths {
  /// Resolve paths for `profile`, honouring `BIR_HOME` and `BIR_DOWNLOADS`.
  pub fn for_profile(profile: &str) -> Result<Self> {
    let base = match std::env::var_os("BIR_HOME") {
      Some(dir) => PathBuf::from(dir),
      None => dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR),
    };

    let cache_base = dirs::cache_dir()
      .unwrap_or_else(|| base.join("cache"))
      .join(APP_DIR);

    let downloads = match std::env::var_os("BIR_DOWNLOADS") {
      Some(dir) => PathBuf::from(dir),
      None => dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Downloads")),
    };

    Ok(Self {
      data: base.join("profiles").join(profile),
      cache: cache_base.join(profile),
      downloads,
      name: profile.to_string(),
    })
  }

  /// Fresh profile directory with no persistence (used by private windows).
  pub fn ephemeral() -> Self {
    let tmp = std::env::temp_dir().join(format!("bir-ephemeral-{}", std::process::id()));
    Self {
      data: tmp.join("data"),
      cache: tmp.join("cache"),
      downloads: tmp.join("downloads"),
      name: "private".to_string(),
    }
  }

  pub fn name(&self) -> &str {
    &self.name
  }

  pub fn data_dir(&self) -> &std::path::Path {
    &self.data
  }

  pub fn cache_dir(&self) -> &std::path::Path {
    &self.cache
  }

  /// Where the webview keeps its HTTP cache / cookie store / IndexedDB.
  pub fn webview_data_dir(&self) -> PathBuf {
    self.data.join("webview")
  }

  pub fn downloads_dir(&self) -> &std::path::Path {
    &self.downloads
  }

  pub fn set_downloads_dir(&mut self, dir: PathBuf) {
    self.downloads = dir;
  }

  /// Installed extensions, one directory per extension id.
  pub fn extensions_dir(&self) -> PathBuf {
    self.data.join("extensions")
  }

  /// Per-extension persisted `storage.local` / `storage.sync` documents.
  pub fn extension_state_dir(&self) -> PathBuf {
    self.data.join("extension-state")
  }

  /// Staging area for extensions loaded from a CRX/zip.
  pub fn extensions_staging_dir(&self) -> PathBuf {
    self.cache.join("extensions")
  }

  /// Unpacked extension directories handed to WebView2's native extension loader.
  ///
  /// WebView2 loads every immediate subdirectory of this path as an extension, so only
  /// enabled, unpacked extensions are mirrored here.
  pub fn native_extensions_dir(&self) -> PathBuf {
    let dir = match std::env::var_os("BIR_HOME") {
      Some(dir) => PathBuf::from(dir),
      None => dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP_DIR),
    };
    dir.join("webview-native-extensions")
  }

  pub fn settings_file(&self) -> PathBuf {
    self.data.join("settings.json")
  }

  /// Create every directory we intend to write into.
  pub fn ensure(&self) -> Result<()> {
    for dir in [
      self.data.as_path(),
      self.cache.as_path(),
      self.downloads.as_path(),
      self.webview_data_dir().as_path(),
      self.extensions_dir().as_path(),
      self.extension_state_dir().as_path(),
      self.extensions_staging_dir().as_path(),
    ] {
      std::fs::create_dir_all(dir)?;
    }
    Ok(())
  }

  /// Bytes used by the profile on disk. Used by the "clear browsing data" UI to show
  /// what will be reclaimed without walking the tree repeatedly.
  pub fn disk_usage(&self) -> u64 {
    dir_size(&self.data) + dir_size(&self.cache)
  }
}

/// Iterative directory size (no recursion: deep trees would risk stack overflow).
fn dir_size(root: &std::path::Path) -> u64 {
  let mut stack = vec![root.to_path_buf()];
  let mut total = 0u64;
  while let Some(dir) = stack.pop() {
    let Ok(entries) = std::fs::read_dir(&dir) else {
      continue;
    };
    for entry in entries.flatten() {
      let Ok(meta) = entry.metadata() else { continue };
      if meta.is_dir() {
        stack.push(entry.path());
      } else {
        total += meta.len();
      }
    }
  }
  total
}

/// Human-readable byte size (KiB / MiB / GiB).
pub fn human_bytes(bytes: u64) -> String {
  const KIB: f64 = 1024.0;
  let b = bytes as f64;
  if b < KIB {
    format!("{bytes} B")
  } else if b < KIB * KIB {
    format!("{:.1} KiB", b / KIB)
  } else if b < KIB * KIB * KIB {
    format!("{:.1} MiB", b / (KIB * KIB))
  } else {
    format!("{:.2} GiB", b / (KIB * KIB * KIB))
  }
}
