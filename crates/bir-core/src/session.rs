//! Session persistence and crash detection.
//!
//! The session document is written on a timer and on clean exit. To tell "the app was
//! closed" apart from "the app died", we drop a `running` marker file next to it: it
//! exists while we are up, and is deleted on a graceful shutdown. If it is still there
//! at startup, the previous run crashed and we offer to restore.

use crate::{store, Error, Persistent, ProfilePaths, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabSnapshot {
  pub url: String,
  pub title: String,
  pub pinned: bool,
  pub muted: bool,
  pub zoom: f64,
  /// Whether this tab was the selected one in its window.
  pub active: bool,
}

impl Default for TabSnapshot {
  fn default() -> Self {
    Self {
      url: "bir://newtab".into(),
      title: "New tab".into(),
      pinned: false,
      muted: false,
      zoom: 1.0,
      active: false,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSnapshot {
  pub tabs: Vec<TabSnapshot>,
  pub active: usize,
  pub width: u32,
  pub height: u32,
  pub x: i32,
  pub y: i32,
  pub maximized: bool,
  pub private: bool,
}

impl Default for WindowSnapshot {
  fn default() -> Self {
    Self {
      tabs: vec![TabSnapshot {
        active: true,
        ..Default::default()
      }],
      active: 0,
      width: 1280,
      height: 860,
      x: 60,
      y: 60,
      maximized: false,
      private: false,
    }
  }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStore {
  pub windows: Vec<WindowSnapshot>,
}

impl SessionStore {
  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let path = paths.data_dir().join(Self::FILE);
    match store::read_to_string(&path)? {
      Some(doc) => Ok(serde_json::from_str(&doc).unwrap_or_default()),
      None => Ok(Self::default()),
    }
  }

  pub fn save_to(&self, paths: &ProfilePaths) -> Result<()> {
    let doc = serde_json::to_string(self)?;
    store::write_atomic(&paths.data_dir().join(Self::FILE), doc.as_bytes())
  }

  /// True when the previous run left its marker behind (i.e. it did not exit cleanly).
  pub fn previous_run_crashed(paths: &ProfilePaths) -> bool {
    paths.data_dir().join("running").exists()
  }

  pub fn mark_running(paths: &ProfilePaths) -> Result<()> {
    std::fs::write(paths.data_dir().join("running"), crate::time::now_secs().to_string())
      .map_err(Error::from)
  }

  pub fn clear_running(paths: &ProfilePaths) {
    let _ = std::fs::remove_file(paths.data_dir().join("running"));
  }

  /// Drop windows/tabs that carry no useful state, so a session never grows a tail of
  /// empty "New tab" entries.
  pub fn normalise(&mut self) {
    for window in &mut self.windows {
      window
        .tabs
        .retain(|t| !t.url.is_empty() && t.url != "bir://newtab");
      if window.tabs.is_empty() {
        window.tabs.push(TabSnapshot {
          active: true,
          ..Default::default()
        });
      }
      if !window.tabs.iter().any(|t| t.active) {
        let active = window.active.min(window.tabs.len() - 1);
        window.tabs[active].active = true;
      }
      window.active = window
        .tabs
        .iter()
        .position(|t| t.active)
        .unwrap_or(0)
        .min(window.tabs.len() - 1);
    }
    self.windows.retain(|w| !w.tabs.is_empty());
  }
}

impl Persistent for SessionStore {
  const FILE: &'static str = "session.json";

  fn mark_dirty(&mut self) {}
  fn is_dirty(&self) -> bool {
    true
  }
  fn clear_dirty(&mut self) {}
  fn to_json(&self) -> Result<String> {
    serde_json::to_string(self).map_err(Error::from)
  }
}
