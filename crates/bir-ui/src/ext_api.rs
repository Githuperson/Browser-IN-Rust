//! The extension API server: answers `browser.*` / `chrome.*` calls.
//!
//! Extension code never touches the browser directly. It sends
//! `{"t":"ext","ext":"<id>","req":N,"ns":"tabs.query","args":[...]}` over the IPC
//! channel, this module resolves it against real browser state, and the reply is
//! evaluated back into the webview that asked (`__birExt.resolve(...)`).
//!
//! Two rules govern what is answerable here:
//!
//! * **Everything is per-extension.** An extension only ever sees its own storage, and
//!   it only gets tabs/windows data if its manifest asked for the permission.
//! * **APIs that need a real network stack reject.** `webRequest`,
//!   `declarativeNetRequest` and friends cannot be implemented on top of a system
//!   webview, so they reject with a clear message instead of silently doing nothing.

use std::collections::HashMap;

use bir_core::{ipc::{TabId, WindowId}, time};
use bir_ext::{
  background::Alarm,
  bridge::{self, BridgeRequest},
  registry,
};
use serde_json::{json, Value};

use crate::{app::BrowserApp, commands};

/// Menu items registered by extensions, kept here because the overlay lives in the page.
pub struct MenuRegistry {
  items: Vec<(String, Value)>,
}

impl MenuRegistry {
  pub fn new() -> Self {
    Self { items: Vec::new() }
  }

  pub fn replace(&mut self, ext: &str, menus: Vec<Value>) {
    self.items.retain(|(owner, _)| owner != ext);
    for menu in menus {
      self.items.push((ext.to_string(), menu));
    }
  }

  pub fn clear(&mut self, ext: &str) {
    self.items.retain(|(owner, _)| owner != ext);
  }

  /// Menus owned by one extension, as the JSON the page overlay consumes.
  pub fn for_extension(&self, ext: &str) -> Vec<Value> {
    self
      .items
      .iter()
      .filter(|(owner, _)| owner == ext)
      .map(|(_, menu)| menu.clone())
      .collect()
  }
}

impl Default for MenuRegistry {
  fn default() -> Self {
    Self::new()
  }
}

/// Answer one API call.
pub fn handle(app: &mut BrowserApp, window: WindowId, tab: Option<TabId>, request: BridgeRequest) {
  let ext = request.ext.clone();
  let ns = request.ns.clone();
  let (ok, value) = resolve(app, window, tab, &ext, &ns, &request.args);
  app.reply_to(
    window,
    tab,
    &bridge::encode_resolve(request.req, ok, value),
  );
}

fn resolve(
  app: &mut BrowserApp,
  window: WindowId,
  tab: Option<TabId>,
  ext: &str,
  ns: &str,
  args: &[Value],
) -> (bool, Value) {
  let arg = |index: usize| -> Value { args.get(index).cloned().unwrap_or(Value::Null) };

  match ns {
    // ------------------------------------------------------------ runtime
    "runtime.getManifest" => (true, app.extensions.manifest_json(ext)),
    "runtime.getURL" => {
      let path = arg(0).as_str().unwrap_or("").to_string();
      (true, json!(format!("bir://{ext}/{path}")))
    }
    "runtime.id" => (true, json!(ext)),
    "runtime.sendMessage" => {
      // Delivered to every context of this extension; the first reply wins inside the
      // runtime script's own listener bookkeeping.
      let message = arg(0);
      app.dispatch_extension_event(ext, "runtime.onMessage", &message);
      (true, Value::Null)
    }
    "runtime.openOptionsPage" => {
      let page = app
        .extensions
        .get(ext)
        .and_then(|e| e.manifest.options_page().cloned());
      match page {
        Some(page) => {
          // The chrome turns this into a real navigation; URL resolution stays in Rust.
          let url = format!("bir://{ext}/{page}");
          (true, json!({ "url": url }))
        }
        None => err("this extension has no options page"),
      }
    }
    "runtime.lastError" => (true, Value::Null),

    // ------------------------------------------------------------ storage
    "storage.local.get" | "storage.sync.get" | "storage.local.set" | "storage.sync.set"
    | "storage.local.remove" | "storage.sync.remove" | "storage.local.clear"
    | "storage.sync.clear" | "storage.local.getBytesInUse" | "storage.sync.getBytesInUse" => {
      storage(app, ext, ns, &arg(0), &arg(1))
    }

    // ------------------------------------------------------------ tabs
    "tabs.query" => {
      if !has(app, ext, "tabs") {
        return err("the \"tabs\" permission is required");
      }
      (true, json!(tabs_json(app, window, Some(&arg(0)))))
    }
    "tabs.get" => {
      if !has(app, ext, "tabs") {
        return err("the \"tabs\" permission is required");
      }
      let id = arg(0).as_u64().unwrap_or(0) as TabId;
      match app.windows.iter().find_map(|w| w.tab(id)).map(tab_json) {
        Some(value) => (true, value),
        None => err("no such tab"),
      }
    }
    "tabs.getCurrent" => match tab.and_then(|id| {
      app
        .windows
        .iter()
        .find(|w| w.id == window)
        .and_then(|w| w.tab(id))
        .map(tab_json)
    }) {
      Some(value) => (true, value),
      None => (true, Value::Null),
    },
    "tabs.create" => {
      let create = arg(0);
      let url = create
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("bir://newtab");
      let active = create.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
      match app.open_tab(window, url, active) {
        Ok(id) => (true, json!({ "id": id })),
        Err(error) => err(&error.to_string()),
      }
    }
    "tabs.update" => {
      let id = arg(0).as_u64().unwrap_or(0) as TabId;
      let props = arg(1);
      if props.get("active").and_then(|v| v.as_bool()).unwrap_or(false) {
        app.activate_tab(window, id);
      }
      if let Some(url) = props.get("url").and_then(|v| v.as_str()) {
        if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
          if let Some(tab_state) = window_state.tab(id) {
            tab_state.eval(&format!(
              "location.href={}",
              serde_json::to_string(url).unwrap_or_default()
            ));
          }
        }
      }
      if props.get("muted").and_then(|v| v.as_bool()).is_some() {
        commands::handle(
          app,
          window,
          Some(id),
          bir_core::ipc::UiCommand::MuteTab {
            tab: id,
            muted: props.get("muted").and_then(|v| v.as_bool()).unwrap_or(false),
          },
        );
      }
      (true, json!({ "id": id }))
    }
    "tabs.remove" => {
      let ids = tab_ids(&arg(0), tab);
      for id in ids {
        commands::handle(
          app,
          window,
          Some(id),
          bir_core::ipc::UiCommand::CloseTab { tab: id },
        );
      }
      (true, Value::Null)
    }
    "tabs.duplicate" => {
      let id = arg(0).as_u64().unwrap_or(0) as TabId;
      commands::handle(
        app,
        window,
        Some(id),
        bir_core::ipc::UiCommand::DuplicateTab { tab: id },
      );
      (true, Value::Null)
    }
    "tabs.reload" => {
      let id = arg(0).as_u64().unwrap_or(0) as TabId;
      commands::handle(
        app,
        window,
        Some(id),
        bir_core::ipc::UiCommand::Reload { tab: id },
      );
      (true, Value::Null)
    }
    "tabs.sendMessage" => {
      let id = arg(0).as_u64().unwrap_or(0) as TabId;
      let message = arg(1);
      let js = bridge::encode_event(ext, "runtime.onMessage", &message);
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab(id) {
          tab_state.eval(&js);
        }
      }
      (true, Value::Null)
    }
    "tabs.executeScript" | "scripting.executeScript" => {
      let id = if ns.starts_with("scripting") {
        arg(0)
          .get("target")
          .and_then(|t| t.get("tabId"))
          .and_then(|v| v.as_u64())
          .unwrap_or(0) as TabId
      } else {
        arg(0).as_u64().unwrap_or(0) as TabId
      };
      let files = if ns.starts_with("scripting") {
        arg(0)
          .get("files")
          .and_then(|v| v.as_array())
          .cloned()
          .unwrap_or_default()
          .into_iter()
          .map(|v| v.as_str().unwrap_or("").to_string())
          .collect::<Vec<_>>()
      } else {
        Vec::new()
      };
      let code = if ns.starts_with("scripting") {
        arg(0)
          .get("func")
          .map(|v| v.to_string())
          .or_else(|| {
            arg(0)
              .get("injectImmediately")
              .map(|_| String::new())
          })
      } else {
        arg(1)
          .get("code")
          .and_then(|v| v.as_str())
          .map(|s| s.to_string())
      };

      let target = if id != 0 { Some(id) } else { tab };
      let Some(id) = target else { return err("no target tab") };

      let mut injected = Vec::new();
      for file in files {
        if let Some(bytes) = app.extensions.get(ext).and_then(|e| e.read_resource(&file)) {
          injected.push(String::from_utf8_lossy(&bytes).into_owned());
        }
      }
      if let Some(code) = code {
        if !code.is_empty() {
          injected.push(code);
        }
      }
      if injected.is_empty() {
        return err("nothing to execute");
      }
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab(id) {
          for body in injected {
            tab_state.eval(&bridge::wrap_script(ext, &body));
          }
        }
      }
      (true, json!([]))
    }
    "tabs.insertCSS" | "scripting.insertCSS" | "scripting.removeCSS" => {
      let id = arg(0).as_u64().unwrap_or(0) as TabId;
      if ns.ends_with("removeCSS") {
        if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
          if let Some(tab_state) = window_state.tab(id) {
            tab_state.eval(&bridge::encode_remove_css());
          }
        }
        return (true, Value::Null);
      }
      let css = arg(1)
        .get("code")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
      let files: Vec<String> = arg(1)
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
      if let Some(window_state) = app.windows.iter().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab(id) {
          if let Some(css) = css {
            tab_state.eval(&bridge::encode_insert_css(&css));
          }
          for file in files {
            if let Some(bytes) = app.extensions.get(ext).and_then(|e| e.read_resource(&file)) {
              tab_state.eval(&bridge::encode_insert_css(
                &String::from_utf8_lossy(&bytes),
              ));
            }
          }
        }
      }
      (true, Value::Null)
    }

    // ------------------------------------------------------------ windows
    "windows.get" | "windows.getAll" | "windows.create" | "windows.update" => {
      if !has(app, ext, "tabs") {
        return err("the \"tabs\" permission is required");
      }
      match ns {
        "windows.getAll" => (true, json!(windows_json(app))),
        "windows.get" => {
          let id = arg(0).as_u64().unwrap_or(0) as WindowId;
          match windows_json(app).into_iter().find(|w| {
            w.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as WindowId == id
          }) {
            Some(value) => (true, value),
            None => err("no such window"),
          }
        }
        _ => (true, json!(windows_json(app).first().cloned().unwrap_or(Value::Null))),
      }
    }

    // ------------------------------------------------------------ alarms
    "alarms.create" => {
      let name = arg(0).as_str().unwrap_or_default().to_string();
      let info = arg(1);
      let when = info.get("when").and_then(|v| v.as_f64());
      let delay = info
        .get("delayInMinutes")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
      let period = info.get("periodInMinutes").and_then(|v| v.as_f64());
      let now = time::now_secs();
      let at = match when {
        Some(epoch_ms) => (epoch_ms / 1000.0) as u64,
        None => now + (delay * 60.0) as u64,
      };
      app.alarms.retain(|a| !(a.extension_id == ext && a.name == name));
      app.alarms.push(Alarm {
        extension_id: ext.to_string(),
        name: name.clone(),
        scheduled_at: at,
        period_minutes: period,
      });
      (true, Value::Null)
    }
    "alarms.clear" => {
      let name = arg(0).as_str().unwrap_or_default();
      app.alarms.retain(|a| !(a.extension_id == ext && a.name == name));
      (true, json!(true))
    }
    "alarms.clearAll" => {
      app.alarms.retain(|a| a.extension_id != ext);
      (true, json!(true))
    }
    "alarms.get" | "alarms.getAll" => {
      let all: Vec<Value> = app
        .alarms
        .iter()
        .filter(|a| a.extension_id == ext)
        .map(|a| alarm_json(a))
        .collect();
      match ns {
        "alarms.get" => {
          let name = arg(0).as_str().unwrap_or_default();
          match all.into_iter().find(|a| {
            a.get("name").and_then(|v| v.as_str()).unwrap_or_default() == name
          }) {
            Some(value) => (true, value),
            None => (true, Value::Null),
          }
        }
        _ => (true, json!(all)),
      }
    }

    // ------------------------------------------------------------ context menus
    "contextMenus.create" => {
      let menus = arg(0);
      let mut list = app.menus.for_extension(ext);
      list.push(menus);
      app.menus.replace(ext, list);
      push_menus(app, ext);
      (true, Value::Null)
    }
    "contextMenus.remove" => {
      let id = arg(0).as_str().unwrap_or_default();
      let list: Vec<Value> = app
        .menus
        .for_extension(ext)
        .into_iter()
        .filter(|m| m.get("id").and_then(|v| v.as_str()).unwrap_or_default() != id)
        .collect();
      app.menus.replace(ext, list);
      push_menus(app, ext);
      (true, Value::Null)
    }
    "contextMenus.removeAll" => {
      app.menus.clear(ext);
      push_menus(app, ext);
      (true, Value::Null)
    }
    "contextMenus.update" => {
      let id = arg(0).as_str().unwrap_or_default();
      let update = arg(1);
      let mut list = app.menus.for_extension(ext);
      for menu in list.iter_mut() {
        if menu.get("id").and_then(|v| v.as_str()).unwrap_or_default() == id {
          if let Value::Object(patch) = update.clone() {
            if let Value::Object(existing) = menu.clone() {
              let mut merged = existing;
              merged.extend(patch);
              *menu = Value::Object(merged);
            }
          }
        }
      }
      app.menus.replace(ext, list);
      push_menus(app, ext);
      (true, Value::Null)
    }

    // ------------------------------------------------------------ i18n
    "i18n.getMessage" => {
      let key = arg(0).as_str().unwrap_or_default();
      let messages = app.extensions.messages_json(ext);
      let value = messages
        .get(key)
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
      let subs = arg(1);
      let mut out = value.to_string();
      if let Some(items) = subs.as_array() {
        for (index, item) in items.iter().enumerate() {
          out = out.replace(&format!("${}", index + 1), &item.as_str().unwrap_or_default());
        }
      }
      (true, json!(out))
    }
    "i18n.getUILanguage" => (true, json!("en")),

    // ------------------------------------------------------------ permissions
    "permissions.contains" | "permissions.request" => {
      let permissions = arg(0)
        .get("permissions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
      let all = permissions
        .iter()
        .all(|p| p.as_str().map(|name| has(app, ext, name)).unwrap_or(false));
      (true, json!(all))
    }

    // ------------------------------------------------------------ commands
    "commands.getAll" => (true, json!([])),

    // ------------------------------------------------------------ notifications
    "notifications.create" => {
      let title = arg(0)
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
      let message = arg(0)
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
      app.toast(&format!("{title}\n{message}"));
      (true, json!(arg(0).get("type").cloned().unwrap_or(json!("basic"))))
    }
    "notifications.clear" | "notifications.getAll" => (true, Value::Null),

    // ------------------------------------------------------------ rejections
    ns if ns.starts_with("webRequest") || ns.starts_with("declarativeNetRequest") => {
      err("network interception APIs are not supported by the system webview; use the built-in content blocker instead")
    }
    ns if ns.starts_with("cookies") => err("cookie APIs are not supported on a system webview"),
    ns if ns.starts_with("downloads") => {
      // Downloads are real, but they are managed by the browser UI rather than by
      // extensions; exposing them is a later step.
      err("downloads API is not implemented yet")
    }
    ns if ns.starts_with("bookmarks") || ns.starts_with("history") => {
      err("bookmarks and history APIs are not implemented yet")
    }
    ns if ns.starts_with("offscreen") || ns.starts_with("debugger") => {
      err("this API cannot be provided on top of a system webview")
    }

    other => err(&format!("unknown API: {other}")),
  }
}

// ---------------------------------------------------------------- storage

fn storage(
  app: &mut BrowserApp,
  ext: &str,
  ns: &str,
  first: &Value,
  second: &Value,
) -> (bool, Value) {
  let path = app.extensions.storage_path(ext);
  let mut data = registry::load_storage(&path);

  let area_is_sync = ns.contains(".sync");
  let key = |value: &Value| -> String {
    let mut prefix = String::new();
    if area_is_sync {
      prefix.push_str("sync:");
    }
    match value {
      Value::String(s) => format!("{prefix}{s}"),
      Value::Array(items) => format!(
        "{prefix}{}",
        items
          .iter()
          .map(|i| i.as_str().unwrap_or_default())
          .collect::<Vec<_>>()
          .join("\u{0}")
      ),
      Value::Object(map) => format!(
        "{prefix}{}",
        map.keys().cloned().collect::<Vec<_>>().join("\u{0}")
      ),
      _ => prefix,
    }
  };

  let result = match ns {
    ns if ns.ends_with(".get") => match first {
      Value::Null => json!(flatten(&data, area_is_sync)),
      Value::String(name) => {
        let mut out = serde_json::Map::new();
        if let Some(value) = data.get(&format!("{}{}", if area_is_sync { "sync:" } else { "" }, name)) {
          out.insert(name.clone(), value.clone());
        }
        Value::Object(out)
      }
      Value::Object(spec) => {
        let mut out = serde_json::Map::new();
        for (name, default) in spec {
          let stored = data
            .get(&format!(
              "{}{}",
              if area_is_sync { "sync:" } else { "" },
              name
            ))
            .cloned()
            .unwrap_or_else(|| default.clone());
          out.insert(name.clone(), stored);
        }
        Value::Object(out)
      }
      Value::Array(names) => {
        let mut out = serde_json::Map::new();
        for name in names {
          let name = name.as_str().unwrap_or_default();
          if let Some(value) = data.get(&format!(
            "{}{}",
            if area_is_sync { "sync:" } else { "" },
            name
          )) {
            out.insert(name.to_string(), value.clone());
          }
        }
        Value::Object(out)
      }
      _ => json!({}),
    },
    ns if ns.ends_with(".set") => {
      if let Value::Object(items) = first {
        for (name, value) in items {
          data.insert(
            format!("{}{}", if area_is_sync { "sync:" } else { "" }, name),
            value.clone(),
          );
        }
      }
      let _ = second;
      Value::Null
    }
    ns if ns.ends_with(".remove") => {
      match first {
        Value::String(name) => {
          data.remove(&format!(
            "{}{}",
            if area_is_sync { "sync:" } else { "" },
            name
          ));
        }
        Value::Array(names) => {
          for name in names {
            let name = name.as_str().unwrap_or_default();
            data.remove(&format!(
              "{}{}",
              if area_is_sync { "sync:" } else { "" },
              name
            ));
          }
        }
        _ => {}
      }
      Value::Null
    }
    ns if ns.ends_with(".clear") => {
      if area_is_sync {
        data.retain(|k, _| !k.starts_with("sync:"));
      } else {
        data.retain(|k, _| k.starts_with("sync:"));
      }
      Value::Null
    }
    ns if ns.ends_with(".getBytesInUse") => {
      let bytes: usize = data
        .iter()
        .filter(|(k, _)| k.starts_with("sync:") == area_is_sync)
        .map(|(k, v)| k.len() + serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
        .sum();
      json!(bytes)
    }
    _ => Value::Null,
  };

  if let Err(error) = registry::save_storage(&path, &data) {
    return err(&error.to_string());
  }
  (true, result)
}

fn flatten(data: &HashMap<String, Value>, sync: bool) -> Value {
  let mut out = serde_json::Map::new();
  for (key, value) in data {
    let prefix = if sync { "sync:" } else { "" };
    if let Some(rest) = key.strip_prefix(prefix) {
      if sync == key.starts_with("sync:") {
        out.insert(rest.to_string(), value.clone());
      }
    }
  }
  Value::Object(out)
}

// ------------------------------------------------------------------ models

fn tab_json(tab: &crate::tab::Tab) -> Value {
  json!({
    "id": tab.id,
    "url": tab.effective_url(),
    "title": tab.title,
    "active": tab.lifecycle.is_mapped(),
    "pinned": tab.pinned,
    "mutedInfo": { "muted": tab.muted },
    "audible": tab.audible,
    "discarded": !tab.has_webview(),
    "status": if tab.loading { "loading" } else { "complete" },
    "favIconUrl": tab.favicon,
    "index": 0,
    "windowId": 0,
  })
}

fn tabs_json(app: &BrowserApp, window: WindowId, query: Option<&Value>) -> Vec<Value> {
  let query = query.unwrap_or(&Value::Null);
  let active_only = query.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
  let current_window = query
    .get("currentWindow")
    .and_then(|v| v.as_bool())
    .unwrap_or(true);
  let url_patterns: Vec<String> = query
    .get("url")
    .and_then(|v| v.as_array())
    .map(|items| {
      items
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
    })
    .unwrap_or_default();

  let mut out = Vec::new();
  for window_state in &app.windows {
    if current_window && window_state.id != window {
      continue;
    }
    for (index, tab) in window_state.tabs.iter().enumerate() {
      if active_only && index != window_state.active {
        continue;
      }
      if !url_patterns.is_empty()
        && !url_patterns.iter().any(|pattern| {
          bir_ext::matches::MatchPattern::parse(pattern)
            .map(|p| p.matches(tab.effective_url()))
            .unwrap_or(false)
        })
      {
        continue;
      }
      let mut value = tab_json(tab);
      if let Value::Object(ref mut map) = value {
        map.insert("index".to_string(), json!(index));
        map.insert("windowId".to_string(), json!(window_state.id));
      }
      out.push(value);
    }
  }
  out
}

fn windows_json(app: &BrowserApp) -> Vec<Value> {
  app
    .windows
    .iter()
    .map(|window| {
      json!({
        "id": window.id,
        "focused": window.window.is_focused(),
        "incognito": window.private,
        "state": if window.window.is_maximized() { "maximized" } else { "normal" },
        "tabs": window.tabs.iter().map(tab_json).collect::<Vec<_>>(),
      })
    })
    .collect()
}

fn alarm_json(alarm: &Alarm) -> Value {
  json!({
    "name": alarm.name,
    "scheduledTime": alarm.scheduled_at as f64 * 1000.0,
    "periodInMinutes": alarm.period_minutes,
  })
}

fn has(app: &BrowserApp, ext: &str, permission: &str) -> bool {
  app.extensions.has_permission(ext, permission)
}

fn push_menus(app: &BrowserApp, ext: &str) {
  let menus = app.menus.for_extension(ext);
  let js = bridge::encode_set_menus(ext, &json!(menus));
  for window in &app.windows {
    for tab in &window.tabs {
      if let Some(webview) = &tab.webview {
        let _ = webview.evaluate_script(&js);
      }
    }
  }
}

fn tab_ids(argument: &Value, fallback: Option<TabId>) -> Vec<TabId> {
  match argument {
    Value::Number(number) => number
      .as_u64()
      .map(|id| vec![id as TabId])
      .unwrap_or_default(),
    Value::Array(items) => items.iter().filter_map(|v| v.as_u64()).map(|v| v as TabId).collect(),
    _ => fallback.into_iter().collect(),
  }
}

fn err(message: &str) -> (bool, Value) {
  (false, json!({ "message": message }))
}
