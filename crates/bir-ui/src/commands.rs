//! Command dispatch: everything the chrome (or an internal page) can ask for.

use bir_core::{
  ipc::{Suggestion, SuggestionKind, TabId, UiCommand, UiEvent, WindowId},
  settings::TabLayout,
};
use bir_ext::bridge::BridgeEnvelope;
use serde::Deserialize;

use crate::{
  app::{AppEvent, BrowserApp},
  pages,
};

/// Signals sent by the page-bridge script (see `pages::page_bridge_js`).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PageSignal {
  Audible { audible: bool },
  LinkHover { url: String },
  Favicon { href: String },
}

/// Classify one IPC message and turn it into an [`AppEvent`].
///
/// Three producers share this channel — the chrome, `bir://` pages, and extension
/// content scripts — so the envelope tag decides where it goes. Anything unrecognised
/// is dropped rather than crashing the browser.
pub fn route_message(raw: &str, window: WindowId, tab: Option<TabId>) -> AppEvent {
  if let Ok(command) = serde_json::from_str::<UiCommand>(raw) {
    return AppEvent::UiCommand { window, tab, command };
  }

  if let Some(envelope) = BridgeEnvelope::decode(raw) {
    return match envelope {
      BridgeEnvelope::Ext(request) => AppEvent::ExtensionRequest { window, tab, request },
      BridgeEnvelope::MenuClicked { ext, menu_item_id, info } => AppEvent::MenuClicked {
        window,
        tab: tab.unwrap_or(0),
        ext,
        menu_item_id,
        info,
      },
    };
  }

  if let Ok(signal) = serde_json::from_str::<PageSignal>(raw) {
    return AppEvent::PageSignal {
      window,
      tab: tab.unwrap_or(0),
      signal,
    };
  }

  AppEvent::Ignored
}

/// Handle a command from the UI.
pub fn handle(
  app: &mut BrowserApp,
  window: WindowId,
  tab: Option<TabId>,
  command: UiCommand,
) {
  // `tab: None` means the message came from the chrome; internal pages always know
  // their own id (Rust injects `window.bir.tabId` into every content webview).
  let from_chrome = tab.is_none();
  let active = app
    .windows
    .iter()
    .find(|w| w.id == window)
    .and_then(|w| w.active_tab())
    .map(|t| t.id);
  // A page that does not know its own id sends 0; fall back to the window's active tab
  // rather than acting on a tab that does not exist.
  let tab = match tab {
    Some(id) if app
      .windows
      .iter()
      .find(|w| w.id == window)
      .and_then(|w| w.tab(id))
      .is_some() =>
    {
      Some(id)
    }
    _ => active,
  };

  match command {
    // ---- lifecycle --------------------------------------------------------
    UiCommand::Ready => {
      if from_chrome {
        let settings_json = serde_json::to_value(&app.settings).unwrap_or_default();
        let tabs = app
          .windows
          .iter()
          .find(|w| w.id == window)
          .map(|w| w.tab_views())
          .unwrap_or_default();
        app.push_event(
          window,
          UiEvent::Bootstrap {
            theme: app.settings.appearance.theme,
            settings: settings_json,
            tabs,
            active,
            extensions: app.extensions.views(),
          },
        );
        app.push_event(
          window,
          UiEvent::SearchEngines {
            engines: app.engines.engines.clone(),
          },
        );
      } else {
        // An internal page: give it the state it renders, nothing more.
        let settings_json = serde_json::to_value(&app.settings).unwrap_or_default();
        app.push_event(window, UiEvent::Settings { settings: settings_json });
        app.push_event(window, UiEvent::Theme { theme: app.settings.appearance.theme });
        app.push_event(
          window,
          UiEvent::SearchEngines {
            engines: app.engines.engines.clone(),
          },
        );
        app.push_event(
          window,
          UiEvent::History {
            entries: app.history.search("", 500),
          },
        );
        app.push_event(
          window,
          UiEvent::Bookmarks {
            nodes: app.bookmarks.tree(),
          },
        );
        app.push_event(
          window,
          UiEvent::Downloads {
            items: app.downloads.items().to_vec(),
          },
        );
        app.push_event(
          window,
          UiEvent::Extensions {
            items: app.extensions.views(),
          },
        );
      }
    }

    // ---- navigation -------------------------------------------------------
    UiCommand::Navigate { tab, url } => navigate(app, window, tab, &url),
    UiCommand::Back { tab } => with_webview(app, window, tab, |wv| {
      let _ = wv.go_back();
    }),
    UiCommand::Forward { tab } => with_webview(app, window, tab, |wv| {
      let _ = wv.go_forward();
    }),
    UiCommand::Reload { tab } => with_webview(app, window, tab, |wv| {
      let _ = wv.reload();
    }),
    UiCommand::Stop { tab } => with_webview(app, window, tab, |wv| {
      // No dedicated stop API in wry; re-loading the current URL is the closest
      // portable equivalent and is what a user pressing stop expects to achieve.
      if let Ok(url) = wv.url() {
        let _ = wv.load_url(&url);
      }
    }),

    // ---- tabs -------------------------------------------------------------
    UiCommand::NewTab { url, foreground, after } => {
      let url = url.unwrap_or_else(|| app.settings.general.new_tab_url.clone());
      match app.open_tab(window, &url, foreground) {
        Ok(id) => {
          if let (Some(after), false) = (after, foreground) {
            move_tab(app, window, id, after);
          }
        }
        Err(err) => eprintln!("[bir] could not open a tab: {err}"),
      }
    }
    UiCommand::CloseTab { tab } => close_tab(app, window, tab),
    UiCommand::CloseOtherTabs { tab } => {
      let others: Vec<TabId> = app
        .windows
        .iter()
        .find(|w| w.id == window)
        .map(|w| w.tabs.iter().filter(|t| t.id != tab && !t.pinned).map(|t| t.id).collect())
        .unwrap_or_default();
      for id in others {
        close_tab(app, window, id);
      }
    }
    UiCommand::ActivateTab { tab } => app.activate_tab(window, tab),
    UiCommand::MoveTab { tab, to } => move_tab_to(app, window, tab, to),
    UiCommand::DuplicateTab { tab } => {
      let url = app
        .windows
        .iter()
        .find(|w| w.id == window)
        .and_then(|w| w.tab(tab))
        .map(|t| t.effective_url().to_string())
        .unwrap_or_default();
      let _ = app.open_tab(window, &url, true);
    }
    UiCommand::PinTab { tab, pinned } => {
      if let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab_mut(tab) {
          tab_state.pinned = pinned;
        }
      }
      app.push_tabs(window);
    }
    UiCommand::MuteTab { tab, muted } => {
      if let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab_mut(tab) {
          tab_state.muted = muted;
          tab_state.eval(&format!(
            "document.querySelectorAll('video,audio').forEach(function(m){{m.muted={muted};}});"
          ));
        }
      }
      app.push_tabs(window);
    }
    UiCommand::DiscardTab { tab } => {
      if let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab_mut(tab) {
          if tab_state.lifecycle.is_mapped() {
            app.toast("The active tab cannot be discarded");
          } else {
            tab_state.discard();
          }
        }
      }
      app.push_tabs(window);
    }

    // ---- page interactions -------------------------------------------------
    UiCommand::ZoomIn { tab } => zoom(app, window, tab, 0.1),
    UiCommand::ZoomOut { tab } => zoom(app, window, tab, -0.1),
    UiCommand::ZoomReset { tab } => set_zoom(app, window, tab, 1.0),
    UiCommand::Find { tab, text, forward } => {
      if let Err(err) = app.ensure_webview(window, tab) {
        eprintln!("[bir] could not wake tab {tab}: {err}");
      }
      let proxy = app.proxy.clone();
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab(tab) {
          if let Some(webview) = &tab_state.webview {
            let js = format!(
              "JSON.stringify(window.bir.find({},{}))",
              serde_json::json!(text),
              forward
            );
            let result = webview.evaluate_script_with_callback(&js, move |value: String| {
              let parsed: serde_json::Value =
                serde_json::from_str(&value).unwrap_or(serde_json::Value::Null);
              let matches = parsed
                .get("matches")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
              let current = parsed
                .get("current")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
              if let Some(proxy) = &proxy {
                let _ = proxy.send_event(AppEvent::FindResult { window, tab, matches, current });
              }
            });
            if let Err(err) = result {
              eprintln!("[bir] find failed: {err}");
            }
          }
        }
      }
    }
    UiCommand::StopFind { tab } => {
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab(tab) {
          tab_state.eval("window.bir.stopFind();");
        }
      }
    }
    UiCommand::Print { tab } => with_webview(app, window, tab, |wv| {
      let _ = wv.print();
    }),
    UiCommand::OpenDevtools { tab } => with_webview(app, window, tab, |wv| {
      wv.open_devtools();
    }),
    UiCommand::RequestFullscreen { tab } => {
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab(tab) {
          tab_state
            .eval("document.documentElement.requestFullscreen && document.documentElement.requestFullscreen();");
        }
      }
    }

    // ---- omnibox -----------------------------------------------------------
    UiCommand::OmniboxInput { tab, text, request_id } => {
      let items = suggestions(app, &text, tab);
      app.push_event(window, UiEvent::Suggestions { request_id, items });
    }
    UiCommand::CancelOmnibox { request_id } => {
      app.push_event(window, UiEvent::Suggestions { request_id, items: Vec::new() });
    }

    // ---- chrome ------------------------------------------------------------
    UiCommand::OpenPanel { panel } => {
      let url = match panel {
        bir_core::ipc::Panel::None => return,
        bir_core::ipc::Panel::NewTab => "bir://newtab".to_string(),
        bir_core::ipc::Panel::History => "bir://history".to_string(),
        bir_core::ipc::Panel::Bookmarks => "bir://bookmarks".to_string(),
        bir_core::ipc::Panel::Downloads => "bir://downloads".to_string(),
        bir_core::ipc::Panel::Settings => "bir://settings".to_string(),
        bir_core::ipc::Panel::Extensions => "bir://extensions".to_string(),
        bir_core::ipc::Panel::About => "bir://about".to_string(),
      };
      match active {
        // Never hijack a pinned tab: the user pinned it to keep it.
        Some(id)
          if app
            .windows
            .iter()
            .find(|w| w.id == window)
            .and_then(|w| w.tab(id))
            .map(|t| t.pinned)
            .unwrap_or(false) =>
        {
          let _ = app.open_tab(window, &url, true);
        }
        Some(id) => navigate(app, window, id, &url),
        None => {
          let _ = app.open_tab(window, &url, true);
        }
      }
    }
    UiCommand::ClosePanel => {
      if let Some(id) = active {
        with_webview(app, window, id, |wv| {
          let _ = wv.go_back();
        });
      }
    }
    UiCommand::SetTheme { theme } => {
      app.settings.appearance.theme = theme;
      let _ = app.settings.save(&app.paths);
      app.push_event(window, UiEvent::Theme { theme });
      for id in app.windows.iter().map(|w| w.id).collect::<Vec<_>>() {
        if let Some(window_state) = app.windows.iter().find(|w| w.id == id) {
          let js = pages::theme_script(&app.settings);
          window_state.send(&js);
        }
      }
    }
    UiCommand::SetSetting { path, value } => {
      app.settings.set_dotted(&path, value);
      apply_settings(app);
      let _ = app.settings.save(&app.paths);
      let settings_json = serde_json::to_value(&app.settings).unwrap_or_default();
      for id in app.windows.iter().map(|w| w.id).collect::<Vec<_>>() {
        app.push_event(id, UiEvent::Settings { settings: settings_json.clone() });
        if let Some(window_state) = app.windows.iter().find(|w| w.id == id) {
          window_state.send(&pages::theme_script(&app.settings));
        }
      }
    }

    // ---- user data ----------------------------------------------------------
    UiCommand::AddBookmark { url, title, parent } => {
      app.bookmarks.add(&url, &title, parent);
      let nodes = app.bookmarks.tree();
      app.push_event(window, UiEvent::Bookmarks { nodes });
      app.toast("Bookmark added");
    }
    UiCommand::RemoveBookmark { id } => {
      app.bookmarks.remove(&id);
      let nodes = app.bookmarks.tree();
      app.push_event(window, UiEvent::Bookmarks { nodes });
    }
    UiCommand::HistorySearch { query, limit } => {
      let entries = app.history.search(&query, limit);
      app.push_event(window, UiEvent::History { entries });
    }
    UiCommand::ClearHistory => {
      app.history.clear();
      let _ = app.history.flush(&app.paths);
      app.push_event(window, UiEvent::History { entries: Vec::new() });
      app.toast("History cleared");
    }
    UiCommand::ClearBrowsingData { history, cookies, cache } => {
      if history {
        app.history.clear();
        let _ = app.history.flush(&app.paths);
      }
      if cookies {
        for window_state in &app.windows {
          for tab_state in &window_state.tabs {
            if let Some(webview) = &tab_state.webview {
              let _ = webview.clear_all_browsing_data();
            }
          }
        }
      }
      if cache {
        let _ = std::fs::remove_dir_all(app.paths.cache_dir());
      }
      app.toast("Browsing data cleared");
    }
    UiCommand::DownloadAction { id, action } => {
      use bir_core::ipc::DownloadAction as Action;
      match action {
        Action::Open | Action::Reveal => {
          if id.is_empty() {
            // No id means "show me the folder everything lands in".
            let dir = app.downloads.dir().to_path_buf();
            open_path(&dir);
          } else if let Some(path) = app.downloads.get(&id).and_then(|i| i.path.clone()) {
            open_path(&path);
          }
        }
        Action::Cancel => app.downloads.cancel(&id),
        Action::Retry => {
          if let Some(url) = app.downloads.get(&id).map(|i| i.url.clone()) {
            let name = app
              .downloads
              .get(&id)
              .map(|i| i.filename.clone())
              .unwrap_or_default();
            let (new_id, path) = app.downloads.start(&url, Some(&name), "");
            app.downloads.began(&new_id, Some(path));
          }
          app.downloads.cancel(&id);
        }
        Action::Remove => app.downloads.remove(&id),
        Action::ClearFinished => app.downloads.clear_finished(),
      }
      let items = app.downloads.items().to_vec();
      app.push_event(window, UiEvent::Downloads { items });
    }

    // ---- permissions --------------------------------------------------------
    UiCommand::SetSitePermission { origin, permission, allow } => {
      app.site_settings.set(&origin, permission, allow);
      if permission == bir_core::site_settings::Permission::ContentBlocking {
        app.blocking.set_exception(&origin, !allow);
      }
      let _ = bir_core::persist(&app.paths, &mut app.site_settings);
      app.toast(&format!("Updated settings for {origin}"));
    }
    UiCommand::AnswerPermission { token, allow, remember } => {
      if let Some((origin, permission, tab)) = app.pending_permissions.remove(&token) {
        let name = format!("{permission:?}");
        if let Ok(mut decisions) = app.permission_decisions.write() {
          decisions.insert(name, allow);
        }
        if remember {
          // wry's handler is not per-origin, so the global answer is what it reads;
          // the per-origin copy is what a future per-site UI would show.
          app.site_settings.set("*", permission, allow);
          if !origin.is_empty() {
            app.site_settings.set(&origin, permission, allow);
          }
          let _ = bir_core::persist(&app.paths, &mut app.site_settings);
        }
        if allow {
          // The request was denied once (the handler cannot wait for the UI), so the
          // page has to ask again before it can succeed.
          if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
            if let Some(tab_state) = window_state.tab(tab) {
              tab_state.eval("location.reload()");
            }
          }
        }
      }
    }

    // ---- extensions ---------------------------------------------------------
    UiCommand::SetExtensionEnabled { id, enabled } => {
      if let Err(err) = app.extensions.set_enabled(&id, enabled) {
        app.toast(&format!("Could not update the extension: {err}"));
      }
      if cfg!(target_os = "windows") {
        let _ = app.extensions.sync_native_dir();
      }
      app.push_extensions();
    }
    UiCommand::InstallExtension { path } => {
      match app.extensions.install(std::path::Path::new(&path)) {
        Ok(id) => {
          if cfg!(target_os = "windows") {
            let _ = app.extensions.sync_native_dir();
          }
          app.push_extensions();
          app.toast(&format!("Installed {id}"));
        }
        Err(err) => app.toast(&format!("Install failed: {err}")),
      }
    }
    UiCommand::InstallExtensionData { name, data } => {
      let bytes = base64ish::decode(&data);
      let staging = app.paths.extensions_staging_dir();
      if let Err(err) = std::fs::create_dir_all(&staging) {
        app.toast(&format!("Could not stage the extension: {err}"));
        return;
      }
      let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
      let path = bir_core::downloads::unique_path(&staging, &safe);
      match std::fs::write(&path, &bytes).and_then(|_| {
        app
          .extensions
          .install(&path)
          .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
      }) {
        Ok(id) => {
          if cfg!(target_os = "windows") {
            let _ = app.extensions.sync_native_dir();
          }
          app.push_extensions();
          app.toast(&format!("Installed {id}"));
        }
        Err(err) => app.toast(&format!("Install failed: {err}")),
      }
    }
    UiCommand::RemoveExtension { id } => {
      if let Err(err) = app.extensions.remove(&id) {
        app.toast(&format!("Could not remove the extension: {err}"));
      }
      app.push_extensions();
    }
    UiCommand::ReloadExtension { id } => {
      match app.extensions.reload_extension(&id) {
        Ok(()) => {
          app.restart_extension(&id);
          app.push_extensions();
          app.toast("Extension reloaded");
        }
        Err(err) => app.toast(&format!("Reload failed: {err}")),
      }
    }
    UiCommand::OpenExtensionOptions { id } => {
      let url = app
        .extensions
        .get(&id)
        .and_then(|e| e.manifest.options_page().cloned())
        .map(|page| format!("bir://{id}/{page}"));
      if let Some(url) = url {
        match active {
          Some(tab_id) => navigate(app, window, tab_id, &url),
          None => {
            let _ = app.open_tab(window, &url, true);
          }
        }
      }
    }

    // ---- windows -------------------------------------------------------------
    UiCommand::WindowAction { action } => {
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        match action {
          bir_core::ipc::WindowAction::Minimize => window_state.window.set_minimized(true),
          bir_core::ipc::WindowAction::ToggleMaximize => {
            window_state.window.set_maximized(!window_state.window.is_maximized())
          }
          bir_core::ipc::WindowAction::ToggleFullscreen => {
            let fullscreen = window_state.window.fullscreen();
            window_state
              .window
              .set_fullscreen(if fullscreen.is_some() { None } else { Some(tao::window::Fullscreen::Borderless(None)) });
          }
          bir_core::ipc::WindowAction::Close => {
            let id = window_state.id;
            app.close_window(id);
          }
        }
      }
    }

    UiCommand::NewWindow { .. } | UiCommand::Quit => {
      // Handled by the event loop, which owns window creation.
    }
  }
}

// ---------------------------------------------------------------- helpers

fn with_webview<F: FnOnce(&wry::WebView)>(
  app: &mut BrowserApp,
  window: WindowId,
  tab: TabId,
  f: F,
) {
  // A discarded tab has no webview; wake it first so the action actually lands.
  if let Err(err) = app.ensure_webview(window, tab) {
    eprintln!("[bir] could not wake tab {tab}: {err}");
  }
  if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
    if let Some(tab_state) = window_state.tab(tab) {
      if let Some(webview) = &tab_state.webview {
        f(webview);
        return;
      }
    }
  }
  eprintln!("[bir] tab {tab} has no webview");
}

fn navigate(app: &mut BrowserApp, window: WindowId, tab: TabId, input: &str) {
  let url = app.resolve_input(input);
  if let Err(err) = app.ensure_webview(window, tab) {
    eprintln!("[bir] could not create the webview: {err}");
    return;
  }
  if let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) {
    if let Some(tab_state) = window_state.tab_mut(tab) {
      tab_state.url = url.clone();
      tab_state.restore_url = url.clone();
      tab_state.loading = true;
      tab_state.favicon.clear();
      tab_state.title = url.clone();
      if let Some(webview) = &tab_state.webview {
        if let Err(err) = webview.load_url(&url) {
          eprintln!("[bir] load_url failed: {err}");
        }
      }
    }
  }
  app.push_event(window, UiEvent::Url { tab, url });
  app.push_tabs(window);
}

fn close_tab(app: &mut BrowserApp, window: WindowId, tab: TabId) {
  let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) else {
    return;
  };
  let was_active = window_state.active_tab().map(|t| t.id) == Some(tab);
  let index = match window_state.index_of(tab) {
    Some(index) => index,
    None => return,
  };
  // Dropping the removed tab drops its webview, which is how memory comes back.
  let _ = window_state.remove(tab);
  if window_state.tabs.is_empty() {
    let url = app.settings.general.new_tab_url.clone();
    let id = app.next_tab_id;
    app.next_tab_id += 1;
    window_state.push(crate::tab::Tab::new(id, &url, false));
    window_state.active = 0;
  } else if was_active {
    window_state.active = index.min(window_state.tabs.len() - 1);
    let active_id = window_state.tabs[window_state.active].id;
    app.activate_tab(window, active_id);
    return;
  } else if window_state.active >= window_state.tabs.len() {
    window_state.active = window_state.tabs.len() - 1;
  }
  app.push_tabs(window);
}

fn move_tab(app: &mut BrowserApp, window: WindowId, tab: TabId, after: TabId) {
  let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) else {
    return;
  };
  let Some(from) = window_state.index_of(tab) else { return };
  let to = match window_state.index_of(after) {
    Some(index) => (index + 1).min(window_state.tabs.len() - 1),
    None => return,
  };
  let tab_state = window_state.tabs.remove(from);
  window_state.tabs.insert(to, tab_state);
  window_state.active = to;
  app.push_tabs(window);
}

fn move_tab_to(app: &mut BrowserApp, window: WindowId, tab: TabId, to: usize) {
  let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) else {
    return;
  };
  let Some(from) = window_state.index_of(tab) else { return };
  let to = to.min(window_state.tabs.len() - 1);
  let tab_state = window_state.tabs.remove(from);
  window_state.tabs.insert(to, tab_state);
  app.push_tabs(window);
}

fn zoom(app: &mut BrowserApp, window: WindowId, tab: TabId, delta: f64) {
  let current = app
    .windows
    .iter()
    .find(|w| w.id == window)
    .and_then(|w| w.tab(tab))
    .map(|t| t.zoom)
    .unwrap_or(1.0);
  set_zoom(app, window, tab, (current + delta).clamp(0.25, 5.0));
}

fn set_zoom(app: &mut BrowserApp, window: WindowId, tab: TabId, factor: f64) {
  if let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) {
    if let Some(tab_state) = window_state.tab_mut(tab) {
      tab_state.zoom = factor;
      if let Some(webview) = &tab_state.webview {
        let _ = webview.zoom(factor);
      }
      // Remember the zoom for the whole site, the way every other browser does.
      let host = tab_state.host();
      if !host.is_empty() {
        app.site_settings.set_zoom(&host, factor);
      }
    }
  }
  app.push_event(window, UiEvent::Zoom { tab, scale: factor });
  app.push_tabs(window);
}

/// Omnibox suggestions, ranked: exact URL, open tabs, bookmarks, history, search.
fn suggestions(app: &BrowserApp, text: &str, _tab: TabId) -> Vec<Suggestion> {
  let trimmed = text.trim();
  if trimmed.is_empty() {
    return Vec::new();
  }
  let mut out: Vec<Suggestion> = Vec::with_capacity(10);

  // 1. If it looks like a URL, offering to navigate is almost always what is wanted.
  if let Some(url) = bir_core::url::normalize(trimmed) {
    out.push(Suggestion {
      kind: SuggestionKind::Url,
      title: trimmed.to_string(),
      url: url.to_string(),
      subtitle: "Visit".to_string(),
      favicon: String::new(),
      score: 100.0,
    });
  }

  // 2. Open tabs — switching is usually the fastest path.
  for window in &app.windows {
    for tab in &window.tabs {
      let haystack = format!("{} {}", tab.title, tab.url).to_ascii_lowercase();
      if haystack.contains(&trimmed.to_ascii_lowercase()) {
        out.push(Suggestion {
          kind: SuggestionKind::Tab,
          title: tab.title.clone(),
          url: tab.effective_url().to_string(),
          subtitle: "Switch to tab".to_string(),
          favicon: tab.favicon.clone(),
          score: 90.0,
        });
      }
    }
  }

  // 3. Bookmarks.
  for bookmark in app.bookmarks.iter_items().take(200) {
    let haystack = format!("{} {}", bookmark.title, bookmark.url).to_ascii_lowercase();
    if haystack.contains(&trimmed.to_ascii_lowercase()) {
      out.push(Suggestion {
        kind: SuggestionKind::Bookmark,
        title: bookmark.title.clone(),
        url: bookmark.url.clone(),
        subtitle: "Bookmark".to_string(),
        favicon: String::new(),
        score: 70.0,
      });
    }
  }

  // 4. History (already ranked by the store's own scoring).
  for entry in app.history.search(trimmed, 6) {
    out.push(Suggestion {
      kind: SuggestionKind::History,
      title: if entry.title.is_empty() {
        entry.url.clone()
      } else {
        entry.title.clone()
      },
      url: entry.url.clone(),
      subtitle: entry.domain.clone(),
      favicon: String::new(),
      score: 60.0,
    });
  }

  // 5. Search with the default engine.
  let engine = app.engines.default_engine();
  out.push(Suggestion {
    kind: SuggestionKind::Search,
    title: trimmed.to_string(),
    url: engine.build_url(trimmed),
    subtitle: format!("Search {}", engine.title),
    favicon: String::new(),
    score: 10.0,
  });

  out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
  out.truncate(9);
  out
}

/// Re-derive everything that is cached from `Settings`.
fn apply_settings(app: &mut BrowserApp) {
  app.blocking.set(
    app.settings.privacy.block_ads || app.settings.privacy.block_trackers,
    app.settings.privacy.block_cosmetic,
  );
  app.blocker_script = Some(bir_net::blocker::BlockerScriptCache::new(
    &app.blocker,
    &bir_net::blocker::BlockerOptions {
      block_network: app.blocking.network_enabled(),
      block_cosmetic: app.blocking.cosmetic_enabled(),
      watch_dom: true,
    },
  ));
  app.scheduler.set_policy(bir_perf::LifecyclePolicy::from_settings(
    &app.settings.performance,
  ));
  // `general.default_search` is the UI-facing copy of the engine choice; the store
  // keeps its own, so they are reconciled here rather than at read time.
  if app.settings.general.default_search != app.engines.default {
    app.engines.default = app.settings.general.default_search.clone();
    let _ = app.engines.save_to(&app.paths);
  }
  if let Ok(mut center) = app.download_center.lock() {
    center.dir = app
      .settings
      .downloads
      .dir
      .clone()
      .unwrap_or_else(|| app.paths.downloads_dir().to_path_buf());
  }
  app.downloads.set_dir(
    app
      .settings
      .downloads
      .dir
      .clone()
      .unwrap_or_else(|| app.paths.downloads_dir().to_path_buf()),
  );
  // Tab layout changes the geometry of every webview in every window.
  let vertical = app.settings.appearance.tab_layout == TabLayout::Vertical;
  for window in &mut app.windows {
    window.vertical_tabs = vertical;
  }
  for id in app.windows.iter().map(|w| w.id).collect::<Vec<_>>() {
    app.relayout(id);
  }
  if let Err(err) = app.settings.save(&app.paths) {
    eprintln!("[bir] could not save settings: {err}");
  }
}

/// Signals from the page bridge.
pub fn handle_page_signal(app: &mut BrowserApp, window: WindowId, tab: TabId, signal: PageSignal) {
  match signal {
    PageSignal::Audible { audible } => {
      if let Some(window_state) = app.windows.iter_mut().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab_mut(tab) {
          if tab_state.audible != audible {
            tab_state.audible = audible;
            drop(tab_state);
            app.push_tabs(window);
          }
        }
      }
    }
    PageSignal::LinkHover { url } => {
      // Kept for the future status-bar; hovered links are not shown in the chrome yet.
      let _ = url;
    }
    PageSignal::Favicon { href } => {
      let proxy = app.proxy.clone();
      if let Some(proxy) = proxy {
        std::thread::Builder::new()
          .name("bir-favicon".into())
          .spawn(move || {
            if let Ok(bytes) = bir_net::fetch::get_bytes(
              &href,
              std::time::Duration::from_secs(5),
              512 * 1024,
            ) {
              if let Some(data_url) = data_url(&bytes) {
                let _ = proxy.send_event(AppEvent::Favicon { window, tab, data_url });
              }
            }
          })
          .ok();
      }
    }
  }
}

/// Build a `data:` URL for a favicon, guessing the MIME type from the bytes.
fn data_url(bytes: &[u8]) -> Option<String> {
  use base64ish::encode;
  let mime = if bytes.starts_with(b"\x89PNG") {
    "image/png"
  } else if bytes.starts_with(b"GIF8") {
    "image/gif"
  } else if bytes.starts_with(b"\xff\xd8\xff") {
    "image/jpeg"
  } else if bytes.starts_with(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
    "image/webp"
  } else if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
    "image/svg+xml"
  } else if bytes.starts_with(&[0, 0, 1, 0]) {
    "image/x-icon"
  } else {
    // Unknown: still hand it over, most engines sniff successfully.
    "image/png"
  };
  Some(format!("data:{mime};base64,{}", encode(bytes)))
}

/// Minimal base64 codec, so the UI crate does not need a dependency for favicons and
/// extension uploads.
mod base64ish {
  const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len() * 4 / 3 + 4);
    for chunk in input.chunks(3) {
      let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
      let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
      out.push(TABLE[(n >> 18) as usize & 63] as char);
      out.push(TABLE[(n >> 12) as usize & 63] as char);
      out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
      out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
  }

  fn value(byte: u8) -> Option<u8> {
    match byte {
      b'A'..=b'Z' => Some(byte - b'A'),
      b'a'..=b'z' => Some(byte - b'a' + 26),
      b'0'..=b'9' => Some(byte - b'0' + 52),
      b'+' => Some(62),
      b'/' => Some(63),
      b'=' => None,
      _ => None,
    }
  }

  pub fn decode(input: &str) -> Vec<u8> {
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4 + 2);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in bytes {
      let Some(value) = value(byte) else { continue };
      buffer = (buffer << 6) | value as u32;
      bits += 6;
      if bits >= 8 {
        bits -= 8;
        out.push((buffer >> bits) as u8);
      }
    }
    out
  }
}

/// Derive a download filename from a URL when the server did not suggest one.
pub fn filename_from_url(url: &str) -> String {
  let path = url.split('?').next().unwrap_or(url);
  let name = path.rsplit('/').next().unwrap_or("download");
  bir_core::downloads::sanitise_filename(name)
}

/// Open a file (or reveal it in the file manager) with the desktop's own handler.
fn open_path(path: &std::path::Path) {
  let path_string = path.to_string_lossy().into_owned();
  let result = if cfg!(target_os = "windows") {
    std::process::Command::new("cmd")
      .args(["/C", "start", "", &path_string])
      .spawn()
      .map(|_| ())
  } else if cfg!(target_vendor = "apple") {
    std::process::Command::new("open").arg(&path_string).spawn().map(|_| ())
  } else {
    std::process::Command::new("xdg-open")
      .arg(&path_string)
      .spawn()
      .map(|_| ())
  };
  if let Err(err) = result {
    eprintln!("[bir] could not open {path_string}: {err}");
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn filenames_come_from_the_last_path_segment() {
    assert_eq!(filename_from_url("https://example.com/a/b/file.zip"), "file.zip");
    assert_eq!(filename_from_url("https://example.com/a/b/file.zip?x=1"), "file.zip");
    assert_eq!(filename_from_url("https://example.com/"), "download");
    // A traversal attempt in a URL must not become a traversal on disk.
    let sneaky = filename_from_url("https://example.com/a/..%2F..%2Fetc/passwd");
    assert!(!sneaky.contains('/'), "{sneaky} kept a separator");
    assert!(!sneaky.contains(".."), "{sneaky} kept a traversal");
  }

  #[test]
  fn base64_round_trips() {
    for input in [b"".as_slice(), b"a".as_slice(), b"ab".as_slice(), b"abc".as_slice(), b"hello world, this is a crx".as_slice()] {
      let encoded = base64ish::encode(input);
      assert_eq!(base64ish::decode(&encoded), input.to_vec(), "failed on {encoded}");
    }
  }

  #[test]
  fn page_signals_are_recognised() {
    let event = route_message("{\"t\":\"audible\",\"audible\":true}", 1, Some(7));
    assert!(matches!(event, AppEvent::PageSignal { tab: 7, .. }));
  }

  #[test]
  fn unrecognised_messages_are_ignored_not_fatal() {
    assert!(matches!(route_message("not json at all", 1, None), AppEvent::Ignored));
    assert!(matches!(route_message("{}", 1, None), AppEvent::Ignored));
  }

  #[test]
  fn commands_decode_from_the_chrome() {
    let event = route_message("{\"t\":\"new_tab\",\"url\":null,\"foreground\":true,\"after\":null}", 3, None);
    match event {
      AppEvent::UiCommand { window, tab, command } => {
        assert_eq!(window, 3);
        assert!(tab.is_none());
        assert!(matches!(command, UiCommand::NewTab { .. }));
      }
      other => panic!("expected a command, got something else"),
    }
  }
}
