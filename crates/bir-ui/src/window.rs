//! A browser window: one native window, one chrome webview, N tab webviews.

use bir_core::ipc::{TabId, WindowId};
use tao::window::Window;

use crate::tab::Tab;

/// Field order matters: webviews are dropped before the window that hosts them, so a
/// child webview never outlives its parent HWND/NSView/GtkWindow.
pub struct BrowserWindow {
  pub id: WindowId,
  /// The HTML chrome (tab strip, omnibox, menus).
  pub chrome: wry::WebView,
  pub tabs: Vec<Tab>,
  pub active: usize,
  pub private: bool,
  pub vertical_tabs: bool,
  /// Linux only: the GTK container every webview in this window lives inside.
  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  pub container: gtk::Fixed,
  /// Last field: dropped after every webview.
  pub window: Window,
}

impl BrowserWindow {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    id: WindowId,
    window: Window,
    chrome: wry::WebView,
    private: bool,
    vertical_tabs: bool,
    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    container: gtk::Fixed,
  ) -> Self {
    Self {
      id,
      window,
      chrome,
      tabs: Vec::new(),
      active: 0,
      private,
      vertical_tabs,
      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      container,
    }
  }

  pub fn active_tab(&self) -> Option<&Tab> {
    self.tabs.get(self.active)
  }

  pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
    self.tabs.get_mut(self.active)
  }

  pub fn tab(&self, id: TabId) -> Option<&Tab> {
    self.tabs.iter().find(|t| t.id == id)
  }

  pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
    self.tabs.iter_mut().find(|t| t.id == id)
  }

  pub fn index_of(&self, id: TabId) -> Option<usize> {
    self.tabs.iter().position(|t| t.id == id)
  }

  pub fn push(&mut self, tab: Tab) {
    self.tabs.push(tab);
  }

  /// Remove a tab, returning it. The caller is responsible for dropping its webview
  /// (dropping the returned `Tab` does that).
  pub fn remove(&mut self, id: TabId) -> Option<Tab> {
    let index = self.index_of(id)?;
    let tab = self.tabs.remove(index);
    if self.active >= self.tabs.len() && !self.tabs.is_empty() {
      self.active = self.tabs.len() - 1;
    }
    Some(tab)
  }

  /// Activate a tab by id, returning its index when it exists.
  pub fn activate(&mut self, id: TabId) -> Option<usize> {
    let index = self.index_of(id)?;
    self.active = index;
    self.tabs[index].last_active = bir_core::time::now_secs();
    Some(index)
  }

  /// Tab snapshots for the chrome.
  pub fn tab_views(&self) -> Vec<bir_core::ipc::TabView> {
    self.tabs.iter().map(|t| t.view()).collect()
  }

  /// Number of tabs with a live webview.
  pub fn live_webviews(&self) -> usize {
    self.tabs.iter().filter(|t| t.has_webview()).count()
  }

  pub fn is_empty(&self) -> bool {
    self.tabs.is_empty()
  }

  /// Send an event to the chrome UI.
  pub fn send(&self, js: &str) {
    if let Err(err) = self.chrome.evaluate_script(js) {
      eprintln!("[bir] chrome evaluate_script failed: {err}");
    }
  }

  /// Evaluate `js` in the chrome **and** in every internal (`bir://`) tab.
  ///
  /// Internal pages are ordinary webviews, so the state they render (history,
  /// downloads, settings) has to reach them the same way it reaches the chrome.
  pub fn send_to_all(&self, js: &str) {
    self.send(js);
    for tab in &self.tabs {
      if tab.effective_url().starts_with("bir://") {
        tab.eval(js);
      }
    }
  }

  /// Logical size of the window's content area.
  pub fn logical_size(&self) -> (f64, f64) {
    let physical = self.window.inner_size();
    let scale = self.window.scale_factor();
    crate::attach::logical_size(physical.width, physical.height, scale)
  }
}
