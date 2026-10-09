//! The wire protocol between the HTML chrome and the Rust shell.
//!
//! Both directions are newline-free JSON documents:
//!
//! * chrome → Rust: `window.ipc.postMessage(JSON.stringify(cmd))`
//! * Rust → chrome: `webview.evaluate_script("bir.onEvent(" + json + ")")`
//!
//! Keeping every message in one tagged enum means a typo is a compile error instead of
//! a silently ignored string, and the whole protocol stays greppable.

use crate::{
  downloads::DownloadItem, history::HistoryEntry, search::SearchEngine, settings::Theme,
  site_settings::Permission, Result,
};
use serde::{Deserialize, Serialize};

/// Identifier of a tab. Monotonic per session; not persisted across restarts.
pub type TabId = u64;

/// Identifier of a top-level window.
pub type WindowId = u64;

/// Opaque token tying a permission prompt shown in the chrome to the pending request.
pub type RequestToken = u64;

/// Commands the chrome (or a `bir://` page) sends to the Rust shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum UiCommand {
  /// The chrome has finished loading and wants the initial state dump.
  Ready,

  // ---- navigation ------------------------------------------------------
  Navigate { tab: TabId, url: String },
  Back { tab: TabId },
  Forward { tab: TabId },
  Reload { tab: TabId },
  Stop { tab: TabId },

  // ---- tabs ------------------------------------------------------------
  NewTab {
    url: Option<String>,
    foreground: bool,
    /// Insert directly after this tab instead of at the end of the strip.
    after: Option<TabId>,
  },
  CloseTab { tab: TabId },
  /// Close every other tab in the window.
  CloseOtherTabs { tab: TabId },
  ActivateTab { tab: TabId },
  MoveTab { tab: TabId, to: usize },
  DuplicateTab { tab: TabId },
  PinTab { tab: TabId, pinned: bool },
  MuteTab { tab: TabId, muted: bool },
  /// Force a background tab to release its webview right now.
  DiscardTab { tab: TabId },

  // ---- page interactions -----------------------------------------------
  ZoomIn { tab: TabId },
  ZoomOut { tab: TabId },
  ZoomReset { tab: TabId },
  Find { tab: TabId, text: String, forward: bool },
  StopFind { tab: TabId },
  Print { tab: TabId },
  OpenDevtools { tab: TabId },
  /// Ask the page to enter picture-in-picture / fullscreen where available.
  RequestFullscreen { tab: TabId },

  // ---- omnibox ---------------------------------------------------------
  OmniboxInput {
    tab: TabId,
    text: String,
    request_id: u32,
  },
  CancelOmnibox { request_id: u32 },

  // ---- panels & chrome --------------------------------------------------
  OpenPanel { panel: Panel },
  ClosePanel,
  SetTheme { theme: Theme },
  SetSetting { path: String, value: serde_json::Value },

  // ---- user data --------------------------------------------------------
  AddBookmark {
    url: String,
    title: String,
    parent: Option<String>,
  },
  RemoveBookmark { id: String },
  HistorySearch { query: String, limit: usize },
  ClearHistory,
  ClearBrowsingData { history: bool, cookies: bool, cache: bool },
  DownloadAction { id: String, action: DownloadAction },

  // ---- site permissions -------------------------------------------------
  AnswerPermission {
    token: RequestToken,
    allow: bool,
    remember: bool,
  },
  SetSitePermission {
    origin: String,
    permission: Permission,
    allow: bool,
  },

  // ---- extensions ------------------------------------------------------
  SetExtensionEnabled { id: String, enabled: bool },
  /// Install from a local `.crx`, `.zip` or unpacked directory.
  InstallExtension { path: String },
  /// Install from bytes the user picked in `bir://extensions`, base64-encoded.
  ///
  /// A file input gives a page a `File`, not a path, so this is the only way a page can
  /// install an extension without a native file dialog.
  InstallExtensionData { name: String, data: String },
  RemoveExtension { id: String },
  ReloadExtension { id: String },
  OpenExtensionOptions { id: String },

  // ---- windows ----------------------------------------------------------
  NewWindow { private: bool },
  WindowAction { action: WindowAction },
  Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
  Minimize,
  ToggleMaximize,
  ToggleFullscreen,
  Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadAction {
  Open,
  Reveal,
  Cancel,
  Retry,
  Remove,
  ClearFinished,
}

/// Which full-page overlay the chrome is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Panel {
  None,
  NewTab,
  History,
  Bookmarks,
  Downloads,
  Settings,
  Extensions,
  About,
}

/// Everything Rust pushes into the chrome.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum UiEvent {
  /// Sent once when the chrome signals [`UiCommand::Ready`].
  Bootstrap {
    theme: Theme,
    settings: serde_json::Value,
    tabs: Vec<TabView>,
    active: Option<TabId>,
    extensions: Vec<ExtensionView>,
  },

  Tabs { tabs: Vec<TabView>, active: Option<TabId> },
  Progress { tab: TabId, loading: bool, progress: f64 },
  Title { tab: TabId, title: String },
  Url { tab: TabId, url: String },
  Favicon { tab: TabId, data_url: String },
  Zoom { tab: TabId, scale: f64 },
  FindResult { tab: TabId, matches: usize, current: usize },

  Suggestions {
    request_id: u32,
    items: Vec<Suggestion>,
  },

  Settings { settings: serde_json::Value },
  Theme { theme: Theme },
  /// Registered search engines, for the settings page.
  SearchEngines { engines: Vec<SearchEngine> },

  History { entries: Vec<HistoryEntry> },
  Bookmarks { nodes: Vec<crate::bookmarks::BookmarkNode> },
  Downloads { items: Vec<DownloadItem> },
  Extensions { items: Vec<ExtensionView> },

  /// Ask the chrome to prompt the user; the answer comes back as
  /// [`UiCommand::AnswerPermission`] with the same token.
  PermissionRequest {
    token: RequestToken,
    origin: String,
    permission: Permission,
  },

  /// Live resource usage for the "performance" section of the settings page.
  Stats {
    /// Resident set size of the whole browser process, in MiB.
    rss_mib: u32,
    /// Resident bytes we can attribute to webviews (best effort).
    webview_mib: u32,
    /// Percentage of system memory in use, 0–100.
    system_used_percent: u8,
    /// Percentage of one core, averaged over all processes of the app.
    cpu_percent: f32,
    tabs_live: usize,
    tabs_sleeping: usize,
    tabs_discarded: usize,
  },

  Toast {
    text: String,
    kind: ToastKind,
  },

  /// Navigate the chrome's own panel area to an internal page.
  OpenPanel { panel: Panel },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastKind {
  Info,
  Success,
  Warning,
  Error,
}

/// Serialisable snapshot of a tab for the chrome UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabView {
  pub id: TabId,
  pub title: String,
  pub url: String,
  /// Address-bar form of `url` (no scheme, no `www.`).
  pub display_url: String,
  /// `data:` URL of the favicon, empty when unknown.
  pub favicon: String,
  pub loading: bool,
  pub can_go_back: bool,
  pub can_go_forward: bool,
  pub pinned: bool,
  pub muted: bool,
  /// Hidden from view and throttled, but its webview still exists.
  pub sleeping: bool,
  /// Its webview has been dropped; it reloads when next activated.
  pub discarded: bool,
  pub zoom: f64,
  pub is_secure: bool,
  /// Audio is currently playing (reported by the page through the bridge).
  pub audible: bool,
}

impl Default for TabView {
  fn default() -> Self {
    Self {
      id: 0,
      title: "New tab".into(),
      url: "bir://newtab".into(),
      display_url: "".into(),
      favicon: String::new(),
      loading: false,
      can_go_back: false,
      can_go_forward: false,
      pinned: false,
      muted: false,
      sleeping: false,
      discarded: false,
      zoom: 1.0,
      is_secure: true,
      audible: false,
    }
  }
}

/// One row in the omnibox dropdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
  pub kind: SuggestionKind,
  pub title: String,
  pub url: String,
  /// Secondary line, e.g. the host for a history hit or "Search" for a query.
  pub subtitle: String,
  /// Favicon `data:` URL when we have one.
  pub favicon: String,
  /// 0..1 relevance score; the UI sorts by it.
  pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
  Url,
  Search,
  History,
  Bookmark,
  Tab,
  Internal,
}

/// Extension metadata shown in `bir://extensions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionView {
  pub id: String,
  pub name: String,
  pub version: String,
  pub description: String,
  pub enabled: bool,
  pub permissions: Vec<String>,
  /// `unpacked` (directory on disk), `packaged` (CRX/zip) or `native` (loaded by the
  /// platform webview itself, Windows/WebView2 only).
  pub kind: String,
  pub has_options: bool,
  pub has_popup: bool,
  /// Relative path of the toolbar popup inside the extension, ready to be turned into
  /// `bir://<id>/<path>` by the chrome.
  pub popup_path: String,
  /// Relative path of the options page, same idea.
  pub options_path: String,
  /// Human-readable reason the extension could not be fully loaded, if any.
  pub error: String,
}

/// Decode a command coming from the chrome. Malformed input is reported, never fatal:
/// the chrome is our own code, but a half-written message must not kill the browser.
pub fn decode_command(json: &str) -> Result<UiCommand> {
  Ok(serde_json::from_str(json)?)
}

/// Encode an event as the exact JS expression the chrome evaluates.
pub fn encode_event(event: &UiEvent) -> Result<String> {
  let json = serde_json::to_string(event)?;
  Ok(format!("bir.onEvent({json})"))
}
