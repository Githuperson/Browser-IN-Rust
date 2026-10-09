//! User settings.
//!
//! One flat, versioned JSON document. Every section carries `#[serde(default)]` so a
//! settings file written by a newer build (which may have more keys) still loads in an
//! older one, and a missing key never fails startup.

use crate::{paths::ProfilePaths, store, Persistent, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Bumped when a breaking change to the on-disk shape is made.
pub const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
  pub version: u32,
  pub general: General,
  pub privacy: Privacy,
  pub performance: Performance,
  pub appearance: Appearance,
  pub network: Network,
  pub extensions: Extensions,
  pub downloads: Downloads,
  pub advanced: Advanced,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      version: SETTINGS_VERSION,
      general: General::default(),
      privacy: Privacy::default(),
      performance: Performance::default(),
      appearance: Appearance::default(),
      network: Network::default(),
      extensions: Extensions::default(),
      downloads: Downloads::default(),
      advanced: Advanced::default(),
    }
  }
}

impl Settings {
  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let path = paths.settings_file();
    match store::read_to_string(&path)? {
      Some(doc) => {
        let mut settings: Settings = serde_json::from_str(&doc)?;
        settings.migrate();
        Ok(settings)
      }
      None => Ok(Self::default()),
    }
  }

  pub fn save(&self, paths: &ProfilePaths) -> Result<()> {
    let doc = serde_json::to_string_pretty(self)?;
    store::write_atomic(&paths.settings_file(), doc.as_bytes())
  }

  /// Apply a single dotted-path change coming from the settings UI
  /// (e.g. `performance.max_live_webviews`).
  ///
  /// Unknown or malformed paths are ignored rather than fatal: a settings page should
  /// never be able to brick startup.
  pub fn set_dotted(&mut self, path: &str, value: serde_json::Value) -> bool {
    let mut json = match serde_json::to_value(self.clone()) {
      Ok(v) => v,
      Err(_) => return false,
    };
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || !set_in(&mut json, &parts, &value) {
      return false;
    }
    match serde_json::from_value(json) {
      Ok(next) => {
        *self = next;
        true
      }
      Err(_) => false,
    }
  }

  fn migrate(&mut self) {
    if self.version == SETTINGS_VERSION {
      return;
    }
    // Placeholder for future migrations; unknown/newer versions are clamped so that
    // downgrading the binary does not leave a half-migrated document behind.
    self.version = SETTINGS_VERSION;
  }
}

/// Walk a `serde_json::Value` tree along `parts` and assign `value` at the leaf.
///
/// Written recursively rather than with a `&mut` cursor because a mutable cursor
/// reborrow (`cur = cur.get_mut(..)`) does not pass the borrow checker.
fn set_in(node: &mut serde_json::Value, parts: &[&str], value: &serde_json::Value) -> bool {
  match parts.split_first() {
    None => {
      *node = value.clone();
      true
    }
    Some((head, rest)) => match node.get_mut(*head) {
      Some(child) => set_in(child, rest, value),
      None => false,
    },
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
  pub startup: StartupBehavior,
  pub home_url: String,
  pub new_tab_url: String,
  /// Restore the previous session after a crash rather than showing the new-tab page.
  pub restore_after_crash: bool,
  pub confirm_before_closing_multiple_tabs: bool,
  /// Close the window when the last tab closes.
  pub close_window_with_last_tab: bool,
  pub default_search: String,
}

impl Default for General {
  fn default() -> Self {
    Self {
      startup: StartupBehavior::RestoreSession,
      home_url: "bir://newtab".into(),
      new_tab_url: "bir://newtab".into(),
      restore_after_crash: true,
      confirm_before_closing_multiple_tabs: true,
      close_window_with_last_tab: false,
      default_search: "duckduckgo".into(),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupBehavior {
  OpenHome,
  OpenNewTab,
  RestoreSession,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Privacy {
  /// Network + element blocking driven by `bir-net` filter lists.
  pub block_ads: bool,
  pub block_trackers: bool,
  /// `display:none` rules for ad containers (the part users actually notice).
  pub block_cosmetic: bool,
  /// Upgrade `http://` navigations to `https://`, falling back with a warning.
  pub https_only: bool,
  pub do_not_track: bool,
  /// Strip cross-site tracking parameters (utm_*, fbclid, gclid, mc_eid, ...).
  pub strip_tracking_params: bool,
  pub clear_data_on_exit: ClearOnExit,
  /// Randomise the small set of JS-visible surfaces a webview lets us change.
  pub resist_fingerprinting: bool,
}

impl Default for Privacy {
  fn default() -> Self {
    Self {
      block_ads: true,
      block_trackers: true,
      block_cosmetic: true,
      https_only: true,
      do_not_track: true,
      strip_tracking_params: true,
      clear_data_on_exit: ClearOnExit::Nothing,
      resist_fingerprinting: false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearOnExit {
  Nothing,
  History,
  CookiesAndStorage,
  Everything,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Performance {
  /// Hard cap on simultaneously *live* webviews. Tabs beyond this are discarded
  /// (their webview is dropped and memory returned to the OS).
  pub max_live_webviews: usize,
  /// Idle time before a background tab's timers are throttled and it is hidden.
  pub sleep_after_secs: u64,
  /// Idle time before a sleeping tab is discarded entirely.
  pub discard_after_secs: u64,
  /// Discard (not just sleep) when system memory is under pressure.
  pub discard_under_pressure: bool,
  /// Used-memory percentage at which we consider the system under pressure.
  pub memory_pressure_percent: u8,
  /// How often we sample memory, in seconds.
  pub sample_interval_secs: u64,
  pub gpu: GpuMode,
  /// Let the webview suspend work for hidden webviews.
  pub background_throttling: bool,
  /// Restore session tabs without creating their webviews until they are selected.
  pub lazy_session_restore: bool,
  /// Never discard tabs whose URL host matches one of these.
  pub never_discard: Vec<String>,
}

impl Default for Performance {
  fn default() -> Self {
    Self {
      max_live_webviews: 6,
      sleep_after_secs: 30 * 60,
      discard_after_secs: 120 * 60,
      discard_under_pressure: true,
      memory_pressure_percent: 85,
      sample_interval_secs: 15,
      gpu: GpuMode::Auto,
      background_throttling: true,
      lazy_session_restore: true,
      never_discard: Vec::new(),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
  /// Let the platform webview decide (GPU compositing on everywhere by default).
  Auto,
  /// Ask for hardware rasterisation / GPU compositing explicitly.
  Hardware,
  /// Force software rasterisation — useful on broken drivers and remote desktops.
  Software,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
  pub theme: Theme,
  pub tab_layout: TabLayout,
  pub show_bookmarks_bar: bool,
  /// Reduce padding and disable non-essential animation in the chrome.
  pub compact: bool,
  /// Prefer the site's `prefers-color-scheme: dark` override.
  pub force_dark_web_contents: bool,
  pub default_zoom: f64,
}

impl Default for Appearance {
  fn default() -> Self {
    Self {
      theme: Theme::System,
      tab_layout: TabLayout::Horizontal,
      show_bookmarks_bar: false,
      compact: false,
      force_dark_web_contents: false,
      default_zoom: 1.0,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
  System,
  Light,
  Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabLayout {
  Horizontal,
  Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
  pub proxy: Option<ProxyConfig>,
  /// Extra UA string; when `None` the platform default is used.
  pub user_agent: Option<String>,
  pub allow_autoplay: bool,
  /// Enable WebGL / WebGPU-capable rendering paths where the platform exposes them.
  pub enable_webgl: bool,
}

impl Default for Network {
  fn default() -> Self {
    Self {
      proxy: None,
      user_agent: None,
      allow_autoplay: false,
      enable_webgl: true,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProxyConfig {
  Http { host: String, port: u16, bypass: Vec<String> },
  Socks5 { host: String, port: u16, bypass: Vec<String> },
  System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Extensions {
  pub enabled: bool,
  /// On Windows, hand unpacked extensions to WebView2's native (Chromium) loader so
  /// they run in-process with full WebExtension API support.
  pub native_webview2_extensions: bool,
  /// Allow loading unpacked directories / CRX files from disk (developer mode).
  pub developer_mode: bool,
}

impl Default for Extensions {
  fn default() -> Self {
    Self {
      enabled: true,
      native_webview2_extensions: true,
      developer_mode: false,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Downloads {
  pub dir: Option<PathBuf>,
  pub ask_where_to_save: bool,
  pub max_concurrent: usize,
}

impl Default for Downloads {
  fn default() -> Self {
    Self {
      dir: None,
      ask_where_to_save: false,
      max_concurrent: 4,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
  /// Enable the platform web inspector (F12 / Ctrl+Shift+I).
  pub devtools: bool,
  pub javascript: bool,
  /// Show a "site is blocked" page for filter hits instead of silently failing.
  pub show_blocked_page: bool,
  pub filter_lists: Vec<FilterList>,
}

impl Default for Advanced {
  fn default() -> Self {
    Self {
      devtools: true,
      javascript: true,
      show_blocked_page: false,
      filter_lists: vec![
        FilterList {
          name: "BIR baseline ads".into(),
          url: "".into(),
          enabled: true,
          builtin: true,
        },
        FilterList {
          name: "BIR baseline trackers".into(),
          url: "".into(),
          enabled: true,
          builtin: true,
        },
      ],
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterList {
  pub name: String,
  /// Remote URL; empty means the list ships inside the binary.
  pub url: String,
  pub enabled: bool,
  pub builtin: bool,
}

impl Persistent for Settings {
  const FILE: &'static str = "settings.json";

  fn mark_dirty(&mut self) {
    // Settings are written through [`Settings::save`] immediately — they change rarely
    // and losing them is annoying. The flag exists only to satisfy the trait.
  }
  fn is_dirty(&self) -> bool {
    true
  }
  fn clear_dirty(&mut self) {}
  fn to_json(&self) -> Result<String> {
    Ok(serde_json::to_string_pretty(self)?)
  }
}
