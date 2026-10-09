//! The JavaScript bridge and the message protocol that drives it.
//!
//! One runtime script is injected into every webview (content and background). It
//! exposes a `browser`/`chrome` object per extension whose methods are thin wrappers
//! around messages sent over wry's IPC channel:
//!
//! ```text
//!   extension JS  ──postMessage({t:"ext", ext, req, ns, args})──▶  Rust
//!   extension JS  ◀──evaluateScript("__birExt.resolve(req, ok, value)")──  Rust
//! ```
//!
//! Every API method returns a Promise **and** accepts a trailing callback, so MV2-style
//! (`chrome.tabs.query({}, cb)`) and MV3-style (`await browser.tabs.query({})`)
//! extensions both work.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::RunAt;

/// An API call made by extension code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeRequest {
  /// Extension id the call came from.
  pub ext: String,
  /// Monotonic request id, echoed back in the reply.
  pub req: u64,
  /// Dotted namespace, e.g. `storage.local.get` or `tabs.query`.
  pub ns: String,
  pub args: Vec<Value>,
}

/// Raw envelope on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum BridgeEnvelope {
  Ext(BridgeRequest),
  /// A context-menu item was clicked in the page overlay.
  MenuClicked {
    ext: String,
    menu_item_id: String,
    info: Value,
  },
}

pub fn decode(raw: &str) -> Option<BridgeEnvelope> {
  serde_json::from_str(raw).ok()
}

/// Encode the reply to a [`BridgeRequest`].
pub fn encode_resolve(req: u64, ok: bool, value: Value) -> String {
  let payload = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
  format!("__birExt.resolve({req},{ok},{payload})")
}

/// Push an event (`runtime.onMessage`, `alarms.onAlarm`, ...) into an extension.
pub fn encode_event(ext: &str, event: &str, payload: &Value) -> String {
  let ext = serde_json::to_string(ext).unwrap_or_else(|_| "\"\"".to_string());
  let event = serde_json::to_string(event).unwrap_or_else(|_| "\"\"".to_string());
  let payload = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
  format!("__birExt.dispatch({ext},{event},{payload})")
}

/// Register or replace the context-menu items owned by an extension.
pub fn encode_set_menus(ext: &str, menus: &Value) -> String {
  let ext = serde_json::to_string(ext).unwrap_or_else(|_| "\"\"".to_string());
  let menus = serde_json::to_string(menus).unwrap_or_else(|_| "[]".to_string());
  format!("__birExt.setMenus({ext},{menus})")
}

/// Tell the runtime about an extension: its manifest, locale messages and world.
///
/// `world` is `"content"` or `"background"`; the difference is which namespaces the
/// shell will answer requests for.
pub fn encode_init(ext: &str, world: &str, manifest: &Value, messages: &Value) -> String {
  let ext = serde_json::to_string(ext).unwrap_or_else(|_| "\"\"".to_string());
  let world = serde_json::to_string(world).unwrap_or_else(|_| "\"\"".to_string());
  let manifest = serde_json::to_string(manifest).unwrap_or_else(|_| "{}".to_string());
  let messages = serde_json::to_string(messages).unwrap_or_else(|_| "{}".to_string());
  format!("__birExt.init({{ext:{ext},world:{world},manifest:{manifest},messages:{messages}}})")
}

/// A content script (or background script) ready to be evaluated.
#[derive(Debug, Clone)]
pub struct ScriptInjection {
  pub extension_id: String,
  pub run_at: RunAt,
  pub all_frames: bool,
  /// Already-read file contents, inlined because `evaluate_script` cannot reference
  /// files on disk.
  pub js: Vec<String>,
  pub css: Vec<String>,
}

/// Wrap a content script body so it runs in its own function scope with the extension
/// API in reach.
///
/// An IIFE rather than `new Function`: `new Function` is blocked by any page whose CSP
/// omits `unsafe-eval`, and a plain function expression evaluated by the webview is
/// not. Real Chrome content scripts live in an isolated world — see the crate docs for
/// why we cannot offer that here.
pub fn wrap_script(extension_id: &str, body: &str) -> String {
  format!(
    // One closing brace for the function, then the IIFE's closing paren and the call.
    "(function(browser,chrome,__birExtId){{\n{body}\n})(__birExt.api({id}),__birExt.api({id}),{id});",
    id = serde_json::to_string(extension_id).unwrap_or_else(|_| "\"\"".to_string())
  )
}

/// Build a script that appends a stylesheet to the current document.
///
/// Used for `scripting.insertCSS` and for declarative content-script CSS, neither of
/// which can reference a file on disk from inside `evaluate_script`.
pub fn encode_insert_css(css: &str) -> String {
  let css = serde_json::to_string(css).unwrap_or_else(|_| "\"\"".to_string());
  format!(
    "(function(){{var e=document.createElement('style');e.id='bir-ext-css';e.textContent={css};\
     (document.head||document.documentElement).appendChild(e);})()"
  )
}

/// Build a script that removes every stylesheet inserted by [`encode_insert_css`].
pub fn encode_remove_css() -> String {
  "(function(){var n=document.querySelectorAll('style#bir-ext-css');for(var i=0;i<n.length;i++){n[i].remove();}})()".to_string()
}

/// Build the `runContentScripts` call for a batch of injections.
pub fn encode_run_content_scripts(scripts: &[ScriptInjection]) -> String {
  let payload = serde_json::to_string(scripts).unwrap_or_else(|_| "[]".to_string());
  format!("__birExt.runContentScripts({payload})")
}

/// The runtime injected into every webview that can host extension code.
///
/// Written as ES5-plus-Promise: no optional chaining, no `let`, no arrow functions in
/// the parts that run on older WebKitGTK builds.
pub const EXTENSION_RUNTIME_JS: &str = r#"
(function () {
  if (window.__birExt) { return; }

  var nextReq = 1;
  var pending = {};
  var contexts = {};
  var menus = [];

  function ctx(id) {
    if (!contexts[id]) {
      contexts[id] = { id: id, listeners: {}, lastError: null, manifest: {}, messages: {}, world: 'content' };
    }
    return contexts[id];
  }

  function post(msg) {
    try { window.ipc.postMessage(JSON.stringify(msg)); } catch (e) {}
  }

  function rejectWith(message) {
    return Promise.reject(new Error(message));
  }

  function call(id, ns, args) {
    return new Promise(function (resolve, reject) {
      var req = nextReq++;
      pending[req] = { resolve: resolve, reject: reject };
      post({ t: 'ext', ext: id, req: req, ns: ns, args: args || [] });
    });
  }

  // Promise + callback API, so both `chrome.*` and `browser.*` styles work.
  function method(id, ns) {
    return function () {
      var args = Array.prototype.slice.call(arguments);
      var cb = (args.length && typeof args[args.length - 1] === 'function') ? args.pop() : null;
      var p = call(id, ns, args);
      if (cb) {
        p.then(function (v) { ctx(id).lastError = null; cb(v); }, function (e) {
          ctx(id).lastError = e;
          try { cb(); } catch (err) {}
        });
        return undefined;
      }
      return p;
    };
  }

  function listeners(id, event) {
    return {
      addListener: function (fn) {
        var l = ctx(id).listeners;
        if (!l[event]) { l[event] = []; }
        l[event].push(fn);
      },
      removeListener: function (fn) {
        var l = ctx(id).listeners[event] || [];
        var i = l.indexOf(fn);
        if (i >= 0) { l.splice(i, 1); }
      },
      hasListener: function (fn) {
        return (ctx(id).listeners[event] || []).indexOf(fn) >= 0;
      }
    };
  }

  function unsupported(id, ns) {
    var msg = 'BIR: ' + ns + ' is not available in a webview-based browser';
    return {
      addListener: function () { rejectWith(msg); },
      getMatchedRules: function () { return rejectWith(msg); },
      updateDynamicRules: function () { return rejectWith(msg); }
    };
  }

  function storageArea(id, area) {
    return {
      get: method(id, 'storage.' + area + '.get'),
      set: method(id, 'storage.' + area + '.set'),
      remove: method(id, 'storage.' + area + '.remove'),
      clear: method(id, 'storage.' + area + '.clear'),
      getBytesInUse: method(id, 'storage.' + area + '.getBytesInUse')
    };
  }

  function buildApi(id) {
    var c = ctx(id);
    var api = {
      runtime: {
        id: id,
        getURL: function (p) { return 'bir://' + id + '/' + String(p).replace(/^\/+/, ''); },
        getManifest: function () { return c.manifest; },
        sendMessage: method(id, 'runtime.sendMessage'),
        openOptionsPage: method(id, 'runtime.openOptionsPage'),
        onMessage: listeners(id, 'runtime.onMessage'),
        onInstalled: listeners(id, 'runtime.onInstalled'),
        onStartup: listeners(id, 'runtime.onStartup'),
        get lastError() { return c.lastError; }
      },
      extension: {
        getURL: function (p) { return 'bir://' + id + '/' + String(p).replace(/^\/+/, ''); },
        getBackgroundPage: function () { return null; }
      },
      storage: {
        local: storageArea(id, 'local'),
        sync: storageArea(id, 'sync'),
        onChanged: listeners(id, 'storage.onChanged')
      },
      tabs: {
        query: method(id, 'tabs.query'),
        get: method(id, 'tabs.get'),
        getCurrent: method(id, 'tabs.getCurrent'),
        create: method(id, 'tabs.create'),
        update: method(id, 'tabs.update'),
        remove: method(id, 'tabs.remove'),
        duplicate: method(id, 'tabs.duplicate'),
        reload: method(id, 'tabs.reload'),
        sendMessage: method(id, 'tabs.sendMessage'),
        onUpdated: listeners(id, 'tabs.onUpdated'),
        onCreated: listeners(id, 'tabs.onCreated'),
        onActivated: listeners(id, 'tabs.onActivated'),
        onRemoved: listeners(id, 'tabs.onRemoved')
      },
      scripting: {
        executeScript: method(id, 'scripting.executeScript'),
        insertCSS: method(id, 'scripting.insertCSS'),
        removeCSS: method(id, 'scripting.removeCSS')
      },
      alarms: {
        create: method(id, 'alarms.create'),
        clear: method(id, 'alarms.clear'),
        clearAll: method(id, 'alarms.clearAll'),
        get: method(id, 'alarms.get'),
        getAll: method(id, 'alarms.getAll'),
        onAlarm: listeners(id, 'alarms.onAlarm')
      },
      notifications: {
        create: method(id, 'notifications.create'),
        clear: method(id, 'notifications.clear'),
        getAll: method(id, 'notifications.getAll')
      },
      contextMenus: {
        create: method(id, 'contextMenus.create'),
        update: method(id, 'contextMenus.update'),
        remove: method(id, 'contextMenus.remove'),
        removeAll: method(id, 'contextMenus.removeAll'),
        onClicked: listeners(id, 'contextMenus.onClicked')
      },
      cookies: {
        get: method(id, 'cookies.get'),
        getAll: method(id, 'cookies.getAll'),
        set: method(id, 'cookies.set'),
        remove: method(id, 'cookies.remove')
      },
      commands: {
        getAll: method(id, 'commands.getAll'),
        onCommand: listeners(id, 'commands.onCommand')
      },
      windows: {
        get: method(id, 'windows.get'),
        getAll: method(id, 'windows.getAll'),
        create: method(id, 'windows.create'),
        update: method(id, 'windows.update')
      },
      i18n: {
        getMessage: function (key, substitutions) {
          var template = c.messages[key];
          if (template === undefined) { return ''; }
          var subs = substitutions || [];
          if (typeof subs === 'string') { subs = [subs]; }
          return String(template).replace(/\$([A-Z_0-9]+|\d)\$/g, function (m, name) {
            if (/^\d+$/.test(name)) { return subs[parseInt(name, 10) - 1] || ''; }
            return subs[name] || '';
          }).replace(/\$(\d)/g, function (m, d) { return subs[parseInt(d, 10) - 1] || ''; });
        },
        getUILanguage: function () { return 'en'; }
      },
      webRequest: unsupported(id, 'webRequest'),
      declarativeNetRequest: unsupported(id, 'declarativeNetRequest')
    };
    return api;
  }

  // ---------------------------------------------------------------- page menus
  function menuMatches(menu, info) {
    if (!menu.contexts || !menu.contexts.length) { return true; }
    for (var i = 0; i < menu.contexts.length; i++) {
      var want = menu.contexts[i];
      if (want === 'all') { return true; }
      if (want === 'link' && info.linkUrl) { return true; }
      if (want === 'image' && info.srcUrl) { return true; }
      if (want === 'selection' && info.selectionText) { return true; }
      if (want === 'page') { return true; }
      if (want === 'editable' && info.editable) { return true; }
    }
    return false;
  }

  var overlay = null;

  function closeOverlay() {
    if (overlay && overlay.parentNode) { overlay.parentNode.removeChild(overlay); }
    overlay = null;
  }

  function showMenu(x, y, info) {
    closeOverlay();
    var visible = menus.filter(function (m) { return !m.parentId && menuMatches(m, info); });
    if (!visible.length) { return false; }

    overlay = document.createElement('div');
    overlay.setAttribute('data-bir', 'context-menu');
    var s = overlay.style;
    s.position = 'fixed';
    s.left = x + 'px';
    s.top = y + 'px';
    s.zIndex = '2147483647';
    s.background = '#ffffff';
    s.color = '#1a1a1a';
    s.border = '1px solid rgba(0,0,0,.18)';
    s.borderRadius = '8px';
    s.boxShadow = '0 8px 26px rgba(0,0,0,.28)';
    s.padding = '4px';
    s.minWidth = '190px';
    s.font = '13px/1.4 system-ui, -apple-system, "Segoe UI", sans-serif';
    s.userSelect = 'none';

    visible.forEach(function (menu) {
      var item = document.createElement('div');
      item.textContent = menu.title || menu.id;
      var is = item.style;
      is.padding = '6px 12px';
      is.borderRadius = '5px';
      is.cursor = 'pointer';
      is.whiteSpace = 'nowrap';
      item.addEventListener('mouseenter', function () { is.background = '#f0f0f5'; });
      item.addEventListener('mouseleave', function () { is.background = 'transparent'; });
      item.addEventListener('click', function (event) {
        event.stopPropagation();
        event.preventDefault();
        post({ t: 'menu_clicked', ext: menu.ext, menu_item_id: menu.id, info: info });
        closeOverlay();
      });
      overlay.appendChild(item);
    });

    document.documentElement.appendChild(overlay);
    setTimeout(function () {
      document.addEventListener('mousedown', function away(e) {
        if (overlay && !overlay.contains(e.target)) {
          closeOverlay();
          document.removeEventListener('mousedown', away, true);
        }
      }, true);
    }, 0);
    return true;
  }

  document.addEventListener('contextmenu', function (event) {
    if (!menus.length) { return; }
    var target = event.target || event.srcElement;
    var anchor = target && target.closest ? target.closest('a') : null;
    var selection = '';
    try { selection = String(window.getSelection() || '').slice(0, 200); } catch (e) {}
    var info = {
      menuItemId: null,
      pageUrl: location.href,
      linkUrl: anchor && anchor.href ? anchor.href : null,
      srcUrl: target && target.tagName === 'IMG' ? target.src : null,
      selectionText: selection,
      editable: !!(target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)),
      x: event.clientX,
      y: event.clientY
    };
    if (showMenu(event.clientX, event.clientY, info)) {
      event.preventDefault();
    }
  }, true);

  // ---------------------------------------------------------------- runtime API
  window.__birExt = {
    init: function (cfg) {
      if (!cfg || !cfg.ext) { return; }
      var c = ctx(cfg.ext);
      c.manifest = cfg.manifest || {};
      c.messages = cfg.messages || {};
      if (cfg.world) { c.world = cfg.world; }
    },

    resolve: function (req, ok, value) {
      var entry = pending[req];
      if (!entry) { return; }
      delete pending[req];
      if (ok) {
        entry.resolve(value);
      } else {
        var message = (value && value.message) ? value.message : String(value);
        entry.reject(new Error(message));
      }
    },

    dispatch: function (extId, event, payload) {
      var list = ctx(extId).listeners[event] || [];
      for (var i = 0; i < list.length; i++) {
        try {
          list[i](payload, { id: extId }, function () {});
        } catch (e) {
          if (window.console) { console.error('[bir:' + extId + '] listener for ' + event + ' threw', e); }
        }
      }
      return list.length > 0;
    },

    setMenus: function (extId, items) {
      menus = menus.filter(function (m) { return m.ext !== extId; });
      (items || []).forEach(function (item) {
        menus.push({
          ext: extId,
          id: item.id,
          title: item.title,
          contexts: item.contexts,
          parentId: item.parentId || null
        });
      });
    },

    // Only CSS is applied here. JavaScript is delivered by the shell with
    // `evaluate_script` (see `wrap_script`), because evaluating it inside the page
    // would need `unsafe-eval` in the page's CSP — and webview-driven evaluation does
    // not.
    runContentScripts: function (list) {
      for (var i = 0; i < (list || []).length; i++) {
        var entry = list[i];
        var c = ctx(entry.extension_id || entry.ext);
        c.world = 'content';
        try {
          (entry.css || []).forEach(function (css) {
            if (!css) { return; }
            var style = document.createElement('style');
            style.setAttribute('data-bir', 'content-css');
            style.textContent = css;
            (document.head || document.documentElement).appendChild(style);
          });
        } catch (e) {}
      }
    },

    api: buildApi
  };
})();
"#;
