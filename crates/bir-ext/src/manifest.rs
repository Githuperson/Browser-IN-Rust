//! `manifest.json` parsing (Manifest V3, with V2 accepted where it is harmless).
//!
//! Parsing is intentionally permissive: an unrecognised key must never stop an
//! extension from loading, because in practice real-world manifests contain keys no
//! specification ever described. Unknown keys are ignored; only genuinely broken
//! manifests are rejected.

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

use crate::Error;

/// When a content script runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAt {
  /// Before any page script, before the DOM exists.
  DocumentStart,
  /// As soon as the DOM is complete, before subresources finish.
  DocumentEnd,
  /// After the page has finished loading (the default).
  DocumentIdle,
}

impl Default for RunAt {
  fn default() -> Self {
    RunAt::DocumentIdle
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentScriptDecl {
  pub matches: Vec<String>,
  pub exclude_matches: Vec<String>,
  pub js: Vec<String>,
  pub css: Vec<String>,
  pub run_at: RunAt,
  pub all_frames: bool,
  pub match_about_blank: bool,
}

impl Default for ContentScriptDecl {
  fn default() -> Self {
    Self {
      matches: Vec::new(),
      exclude_matches: Vec::new(),
      js: Vec::new(),
      css: Vec::new(),
      run_at: RunAt::DocumentIdle,
      all_frames: false,
      match_about_blank: false,
    }
  }
}

/// MV3 background (`{"service_worker": "sw.js"}`) or MV2 (`{"scripts": [...],
/// "persistent": false}`). Both are hosted the same way: in the background page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Background {
  pub service_worker: Option<String>,
  pub scripts: Vec<String>,
  pub persistent: bool,
  #[serde(rename = "type")]
  pub worker_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Action {
  pub default_popup: Option<String>,
  pub default_title: Option<String>,
  pub default_icon: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OptionsUi {
  pub page: Option<String>,
  pub open_in_tab: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Command {
  pub suggested_key: Option<serde_json::Value>,
  pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
  pub manifest_version: u32,
  pub name: String,
  pub version: String,
  pub description: String,
  pub permissions: Vec<String>,
  pub host_permissions: Vec<String>,
  pub optional_permissions: Vec<String>,
  pub background: Option<Background>,
  pub content_scripts: Vec<ContentScriptDecl>,
  /// MV3: `action`. MV2: `browser_action` / `page_action`.
  pub action: Option<Action>,
  pub browser_action: Option<Action>,
  pub page_action: Option<Action>,
  pub options_ui: Option<OptionsUi>,
  pub options_page: Option<String>,
  pub icons: HashMap<String, String>,
  pub default_locale: Option<String>,
  pub commands: HashMap<String, Command>,
  pub author: Option<String>,
  pub homepage_url: Option<String>,
  pub web_accessible_resources: Vec<serde_json::Value>,
  pub content_security_policy: Option<serde_json::Value>,
  /// Keys present in the file that we do not model — kept so the extensions page can
  /// say "this extension declares keys BIR ignores" instead of pretending they work.
  #[serde(flatten)]
  pub unknown: HashMap<String, serde_json::Value>,
}

impl Default for Manifest {
  fn default() -> Self {
    Self {
      manifest_version: 3,
      name: String::new(),
      version: "0.0.0".into(),
      description: String::new(),
      permissions: Vec::new(),
      host_permissions: Vec::new(),
      optional_permissions: Vec::new(),
      background: None,
      content_scripts: Vec::new(),
      action: None,
      browser_action: None,
      page_action: None,
      options_ui: None,
      options_page: None,
      icons: HashMap::new(),
      default_locale: None,
      commands: HashMap::new(),
      author: None,
      homepage_url: None,
      web_accessible_resources: Vec::new(),
      content_security_policy: None,
      unknown: HashMap::new(),
    }
  }
}

impl Manifest {
  /// Read and parse a manifest from an unpacked extension directory.
  pub fn load_dir(dir: &Path) -> Result<Self, Error> {
    let path = dir.join("manifest.json");
    let text = std::fs::read_to_string(&path)
      .map_err(|e| Error::Extension(format!("cannot read {}: {e}", path.display())))?;
    Self::from_str(&text)
  }

  pub fn from_str(text: &str) -> Result<Self, Error> {
    let mut manifest: Manifest = serde_json::from_str(text)
      .map_err(|e| Error::Extension(format!("invalid manifest.json: {e}")))?;
    manifest.normalise();
    Ok(manifest)
  }

  fn normalise(&mut self) {
    if self.name.is_empty() {
      self.name = "Untitled extension".into();
    }
    if self.version.is_empty() {
      self.version = "0.0.0".into();
    }
    // MV2 toolbar buttons are the same thing as MV3 actions in practice.
    if self.action.is_none() {
      self.action = self.browser_action.clone().or_else(|| self.page_action.clone());
    }
  }

  /// MV3 or later.
  pub fn is_mv3(&self) -> bool {
    self.manifest_version >= 3
  }

  /// Files that make up the background context (service worker or MV2 scripts).
  pub fn background_scripts(&self) -> Vec<String> {
    match &self.background {
      Some(bg) => {
        let mut out = Vec::new();
        if let Some(sw) = &bg.service_worker {
          out.push(sw.clone());
        }
        for script in &bg.scripts {
          if !out.contains(script) {
            out.push(script.clone());
          }
        }
        out
      }
      None => Vec::new(),
    }
  }

  /// The toolbar action, whichever key it was declared under.
  pub fn toolbar_action(&self) -> Option<&Action> {
    self.action.as_ref()
  }

  /// Largest icon path in the manifest, if any.
  pub fn icon_path(&self) -> Option<&String> {
    let mut best: Option<(u32, &String)> = None;
    for (size, path) in &self.icons {
      let parsed = size.parse::<u32>().ok();
      if let Some(size) = parsed {
        if best.map(|(b, _)| size > b).unwrap_or(true) {
          best = Some((size, path));
        }
      }
    }
    best.map(|(_, path)| path)
  }

  /// Options page, from either the MV3 `options_ui` or the MV2 `options_page` key.
  pub fn options_page(&self) -> Option<&String> {
    self
      .options_ui
      .as_ref()
      .and_then(|ui| ui.page.as_ref())
      .or(self.options_page.as_ref())
  }
}

/// A parsed `_locales/<lang>/messages.json`, flattened for the JS bridge.
pub type Messages = HashMap<String, String>;

/// Load the message catalogue for `locale`, falling back to the manifest default and
/// then to English. Missing catalogues are not an error: plenty of extensions only
/// ship `en`.
pub fn load_messages(dir: &Path, preferred_locale: &str, fallback: Option<&str>) -> Messages {
  for locale in [preferred_locale, fallback.unwrap_or("en"), "en"] {
    let path = dir.join("_locales").join(locale).join("messages.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
      if let Ok(raw) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&text) {
        let mut out = Messages::new();
        for (key, value) in raw {
          if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
            out.insert(key, message.to_string());
          }
        }
        if !out.is_empty() {
          return out;
        }
      }
    }
  }
  Messages::new()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_a_minimal_manifest() {
    let manifest = Manifest::from_str(
      r#"{
        "manifest_version": 3,
        "name": "Test",
        "version": "1.2.3",
        "action": {"default_popup": "popup.html"},
        "content_scripts": [{
          "matches": ["*://*.example.com/*"],
          "js": ["cs.js"],
          "run_at": "document_start"
        }],
        "some_future_key": 42
      }"#,
    )
    .unwrap();
    assert_eq!(manifest.name, "Test");
    assert!(manifest.is_mv3());
    assert_eq!(manifest.content_scripts[0].run_at, RunAt::DocumentStart);
    assert_eq!(manifest.background_scripts().len(), 0);
    assert!(manifest.toolbar_action().is_some());
    // Unknown keys are preserved, not fatal.
    assert!(manifest.unknown.contains_key("some_future_key"));
  }

  #[test]
  fn mv2_browser_action_is_honoured() {
    let manifest = Manifest::from_str(
      r#"{"manifest_version": 2, "name": "Old", "version": "1.0",
          "browser_action": {"default_title": "hi"},
          "background": {"scripts": ["bg.js"], "persistent": false}}"#,
    )
    .unwrap();
    assert_eq!(manifest.background_scripts(), vec!["bg.js".to_string()]);
    assert_eq!(
      manifest.toolbar_action().and_then(|a| a.default_title.clone()),
      Some("hi".to_string())
    );
  }
}
