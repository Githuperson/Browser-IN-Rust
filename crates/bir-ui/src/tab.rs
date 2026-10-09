//! A tab: metadata plus (optionally) a live webview.

use bir_core::{ipc::TabId, url::UrlInfo};
use bir_perf::lifecycle::TabLifecycle;

/// Everything the shell knows about one tab.
///
/// The webview is `Option` on purpose: a tab that has never been shown, or one that has
/// been discarded to reclaim memory, has no webview at all. That single field is most
/// of the browser's memory strategy.
pub struct Tab {
  pub id: TabId,
  pub webview: Option<wry::WebView>,
  pub title: String,
  pub url: String,
  /// `data:` URL for the favicon, empty when unknown.
  pub favicon: String,
  pub loading: bool,
  pub can_go_back: bool,
  pub can_go_forward: bool,
  pub pinned: bool,
  pub muted: bool,
  pub zoom: f64,
  pub audible: bool,
  pub lifecycle: TabLifecycle,
  /// Seconds since epoch of the last time this tab was selected.
  pub last_active: u64,
  /// URL to restore when waking from [`TabLifecycle::Discarded`].
  pub restore_url: String,
  pub private: bool,
}

impl Tab {
  pub fn new(id: TabId, url: &str, private: bool) -> Self {
    Self {
      id,
      webview: None,
      title: "New tab".into(),
      url: url.to_string(),
      favicon: String::new(),
      loading: false,
      can_go_back: false,
      can_go_forward: false,
      pinned: false,
      muted: false,
      zoom: 1.0,
      audible: false,
      lifecycle: TabLifecycle::Discarded,
      last_active: bir_core::time::now_secs(),
      restore_url: url.to_string(),
      private,
    }
  }

  pub fn has_webview(&self) -> bool {
    self.webview.is_some()
  }

  /// The URL this tab should show: either where it is, or where it will go back to
  /// when it wakes up.
  pub fn effective_url(&self) -> &str {
    if self.lifecycle == TabLifecycle::Discarded {
      &self.restore_url
    } else {
      &self.url
    }
  }

  /// Snapshot for the chrome UI.
  pub fn view(&self) -> bir_core::ipc::TabView {
    let info = UrlInfo::parse(self.effective_url());
    bir_core::ipc::TabView {
      id: self.id,
      title: if self.title.is_empty() {
        self.effective_url().to_string()
      } else {
        self.title.clone()
      },
      url: self.effective_url().to_string(),
      display_url: info
        .as_ref()
        .map(|i| i.display.clone())
        .unwrap_or_else(|| self.effective_url().to_string()),
      favicon: self.favicon.clone(),
      loading: self.loading,
      can_go_back: self.can_go_back,
      can_go_forward: self.can_go_forward,
      pinned: self.pinned,
      muted: self.muted,
      sleeping: self.lifecycle == TabLifecycle::Sleeping,
      discarded: self.lifecycle == TabLifecycle::Discarded,
      zoom: self.zoom,
      is_secure: info.map(|i| i.is_secure).unwrap_or(true),
      audible: self.audible,
    }
  }

  pub fn host(&self) -> String {
    UrlInfo::parse(self.effective_url())
      .map(|i| i.host)
      .unwrap_or_default()
  }

  /// Run a script in this tab's webview, if it has one.
  ///
  /// Failures are logged and ignored: a page in a weird state (mid-navigation, CSP, a
  /// crashed renderer) must not take the browser down with it.
  pub fn eval(&self, js: &str) {
    if let Some(webview) = &self.webview {
      if let Err(err) = webview.evaluate_script(js) {
        eprintln!("[bir] evaluate_script failed on tab {}: {err}", self.id);
      }
    }
  }

  /// Drop the webview, releasing its memory back to the OS.
  pub fn discard(&mut self) {
    self.webview = None;
    self.lifecycle = TabLifecycle::Discarded;
    self.restore_url = self.url.clone();
    self.loading = false;
    self.can_go_back = false;
    self.can_go_forward = false;
    self.audible = false;
  }
}
