//! Keyboard shortcuts.
//!
//! The chrome is a webview and the page is a webview, so browser-level shortcuts are not
//! consumed by either of them on their own: this module is the single place where a key
//! press turns into a browser action, whatever has focus.
//!
//! The chrome's own script also handles the keys it needs (arrow keys in the omnibox,
//! Escape closing the find bar) because a text field must see its own input first.

use bir_core::ipc::{Panel, TabId, UiCommand, WindowId};
use tao::{
  event::{ElementState, KeyEvent},
  keyboard::{Key, ModifiersState},
};

use crate::{app::BrowserApp, commands};

/// Shortcut state lives on the app so it survives across windows.
pub fn handle(app: &mut BrowserApp, window: WindowId, event: &KeyEvent) {
  if event.state != ElementState::Pressed {
    return;
  }
  let modifiers = app.modifiers;
  let primary = if cfg!(target_vendor = "apple") {
    modifiers.super_key()
  } else {
    modifiers.control_key()
  };
  let shift = modifiers.shift_key();
  let alt = modifiers.alt_key();

  let active: Option<TabId> = app
    .windows
    .iter()
    .find(|w| w.id == window)
    .and_then(|w| w.active_tab())
    .map(|t| t.id);

  /// Send a command through the same path the chrome uses.
  macro_rules! command {
    ($command:expr) => {{
      commands::handle(app, window, active, $command);
    }};
  }

  // --- number keys select tabs ---------------------------------------------
  if primary && !shift && !alt {
    if let Key::Character(text) = &event.logical_key {
      if let Some(digit) = text.chars().next().and_then(|c| c.to_digit(10)) {
        let tabs = app
          .windows
          .iter()
          .find(|w| w.id == window)
          .map(|w| w.tab_views())
          .unwrap_or_default();
        // 9 means "the last tab", as in every other browser.
        let index = if digit == 9 {
          tabs.len().saturating_sub(1)
        } else {
          digit.saturating_sub(1) as usize
        };
        if let Some(tab) = tabs.get(index) {
          app.activate_tab(window, tab.id);
        }
        return;
      }
    }
  }

  // --- named keys ----------------------------------------------------------
  match &event.logical_key {
    Key::F5 => {
      if let Some(tab) = active {
        command!(UiCommand::Reload { tab });
      }
      return;
    }
    Key::F11 => {
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        let fullscreen = window_state.window.fullscreen();
        window_state
          .window
          .set_fullscreen(if fullscreen.is_some() {
            None
          } else {
            Some(tao::window::Fullscreen::Borderless(None))
          });
      }
      return;
    }
    Key::F12 => {
      if let Some(tab) = active {
        command!(UiCommand::OpenDevtools { tab });
      }
      return;
    }
    Key::Escape => {
      eval_chrome(app, window, "window.bir.stopFind && window.bir.stopFind()");
      return;
    }
    Key::ArrowLeft => {
      if alt || (cfg!(target_vendor = "apple") && primary) {
        if let Some(tab) = active {
          command!(UiCommand::Back { tab });
        }
        return;
      }
    }
    Key::ArrowRight => {
      if alt || (cfg!(target_vendor = "apple") && primary) {
        if let Some(tab) = active {
          command!(UiCommand::Forward { tab });
        }
        return;
      }
    }
    Key::Backspace => {
      if !primary && !alt {
        if let Some(tab) = active {
          command!(UiCommand::Back { tab });
        }
        return;
      }
    }
    Key::Tab if primary => {
      step_tab(app, window, !shift);
      return;
    }
    _ => {}
  }

  // --- character keys ------------------------------------------------------
  let Key::Character(text) = &event.logical_key else {
    return;
  };
  let key = text.chars().next().unwrap_or('\0').to_ascii_lowercase();

  if primary && shift {
    match key {
      't' => {
        // Reopening a closed tab needs a closed-tab stack; until then the shortcut is a
        // normal new tab rather than a silent no-op.
        command!(UiCommand::NewTab {
          url: None,
          foreground: true,
          after: None,
        });
      }
      'n' => {
        // A new window needs the event-loop target, so it goes back through the loop.
        new_window(app, window, active, true);
      }
      'r' => {
        if let Some(tab) = active {
          command!(UiCommand::Reload { tab });
        }
      }
      'i' | 'j' | 'c' => {
        if let Some(tab) = active {
          command!(UiCommand::OpenDevtools { tab });
        }
      }
      '[' | ']' => {
        // Ctrl+[ / Ctrl+] switch tabs, which is what Chrome does.
        step_tab(app, window, key == ']');
      }
      'a' => {
        command!(UiCommand::OpenPanel { panel: Panel::Extensions });
      }
      'o' => {
        command!(UiCommand::OpenPanel { panel: Panel::Bookmarks });
      }
      'p' => {
        command!(UiCommand::OpenPanel { panel: Panel::Downloads });
      }
      'g' => {
        eval_chrome(app, window, "window.bir.focusFind && window.bir.focusFind()");
      }
      _ => {}
    }
    return;
  }

  if primary {
    match key {
      't' => {
        command!(UiCommand::NewTab {
          url: None,
          foreground: true,
          after: None,
        });
      }
      'n' => {
        new_window(app, window, active, false);
      }
      'w' => {
        if let Some(tab) = active {
          command!(UiCommand::CloseTab { tab });
        }
      }
      'l' => eval_chrome(app, window, "window.bir.focusOmnibox && window.bir.focusOmnibox()"),
      'd' => {
        if let Some(tab) = active {
          let (url, title) = app
            .windows
            .iter()
            .find(|w| w.id == window)
            .and_then(|w| w.tab(tab))
            .map(|t| (t.effective_url().to_string(), t.title.clone()))
            .unwrap_or_default();
          command!(UiCommand::AddBookmark { url, title, parent: None });
        }
      }
      'f' => eval_chrome(app, window, "window.bir.focusFind && window.bir.focusFind()"),
      'g' => eval_chrome(app, window, "window.bir.findNext && window.bir.findNext()"),
      'p' => {
        if let Some(tab) = active {
          command!(UiCommand::Print { tab });
        }
      }
      'r' => {
        if let Some(tab) = active {
          command!(UiCommand::Reload { tab });
        }
      }
      '[' => step_tab(app, window, false),
      ']' => step_tab(app, window, true),
      '+' | '=' => {
        if let Some(tab) = active {
          command!(UiCommand::ZoomIn { tab });
        }
      }
      '-' => {
        if let Some(tab) = active {
          command!(UiCommand::ZoomOut { tab });
        }
      }
      '0' => {
        if let Some(tab) = active {
          command!(UiCommand::ZoomReset { tab });
        }
      }
      ',' => command!(UiCommand::OpenPanel { panel: Panel::Settings }),
      'j' => command!(UiCommand::OpenPanel { panel: Panel::Downloads }),
      'h' => command!(UiCommand::OpenPanel { panel: Panel::History }),
      _ => {}
    }
  }
}

fn new_window(app: &BrowserApp, window: WindowId, tab: Option<TabId>, private: bool) {
  if let Some(proxy) = &app.proxy {
    let _ = proxy.send_event(crate::app::AppEvent::UiCommand {
      window,
      tab,
      command: UiCommand::NewWindow { private },
    });
  }
}

/// Move to the next or previous tab, wrapping around.
fn step_tab(app: &mut BrowserApp, window: WindowId, forward: bool) {
  let Some(window_state) = app.windows.iter().find(|w| w.id == window) else {
    return;
  };
  if window_state.tabs.len() < 2 {
    return;
  }
  let count = window_state.tabs.len();
  let current = window_state.active.min(count - 1);
  let next = if forward {
    (current + 1) % count
  } else {
    (current + count - 1) % count
  };
  let id = window_state.tabs[next].id;
  app.activate_tab(window, id);
}

fn eval_chrome(app: &BrowserApp, window: WindowId, js: &str) {
  if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
    let _ = window_state.chrome.evaluate_script(js);
  }
}

/// Update the tracked modifier state.
pub fn modifiers_changed(app: &mut BrowserApp, state: ModifiersState) {
  app.modifiers = state;
}
