//! In-page blocking script generation.
//!
//! The generated script is injected at document start. It is intentionally written in
//! ES5-flavoured JS with no optional chaining and no `let`/arrow functions in the hot
//! paths: it has to run correctly on WebKitGTK 2.38, WKWebView and WebView2 alike.

use serde_json::json;

use crate::engine::Blocker;

/// What the injected script should do.
#[derive(Debug, Clone, Default)]
pub struct BlockerOptions {
  /// Reject `fetch`/XHR/`EventSource`/`sendBeacon`/`WebSocket` to blocked URLs.
  pub block_network: bool,
  /// Hide elements with the generated CSS.
  pub block_cosmetic: bool,
  /// Remove `<script>`/`<iframe>`/`<img>` nodes inserted later that point at blocked
  /// URLs. Costs a `MutationObserver`, so it is opt-in.
  pub watch_dom: bool,
}

/// Pre-serialised script prefix, so the (large) rule blobs are encoded once instead of
/// on every navigation.
///
/// A filter list blob is tens of kilobytes; re-encoding it per page load would be pure
/// waste, so the constant half of the payload is built at startup and only the
/// host-specific CSS (a few hundred bytes) is appended per page.
pub struct BlockerScriptCache {
  hosts_json: String,
  patterns_json: String,
  watch_dom: bool,
  cosmetic: bool,
}

impl BlockerScriptCache {
  pub fn new(blocker: &Blocker, options: &BlockerOptions) -> Self {
    let hosts = if options.block_network {
      blocker.blocked_hosts_blob()
    } else {
      ""
    };
    let patterns = if options.block_network {
      blocker.blocked_patterns_blob()
    } else {
      ""
    };
    Self {
      hosts_json: js_string(hosts),
      patterns_json: js_string(patterns),
      watch_dom: options.watch_dom,
      cosmetic: options.block_cosmetic,
    }
  }

  /// Build the script for one page. Only the CSS varies per host.
  pub fn script_for(&self, blocker: &Blocker, host: &str) -> String {
    let css = if self.cosmetic {
      blocker.cosmetic_css(host)
    } else {
      String::new()
    };
    format!(
      "(function(){{window.__birInstallBlocker({{hosts:{},patterns:{},watchDom:{},css:{}}});}})();",
      self.hosts_json,
      self.patterns_json,
      self.watch_dom,
      js_string(&css)
    )
  }
}

/// Escape an arbitrary string as a JavaScript string literal.
fn js_string(value: &str) -> String {
  serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Builder for the document-start blocking script.
pub struct BlockerScript;

impl BlockerScript {
  /// Produce the script for a page on `host`.
  ///
  /// The rule blobs are shared by every page, so callers should cache the result per
  /// (blocking enabled, cosmetic enabled) pair rather than per navigation — see
  /// [`BlockerScriptCache`].
  pub fn build(blocker: &Blocker, host: &str, options: &BlockerOptions) -> String {
    if blocker.is_empty() || (!options.block_network && !options.block_cosmetic) {
      return String::new();
    }

    let css = if options.block_cosmetic {
      blocker.cosmetic_css(host)
    } else {
      String::new()
    };

    let (hosts, patterns) = if options.block_network {
      (blocker.blocked_hosts_blob(), blocker.blocked_patterns_blob())
    } else {
      ("", "")
    };

    // JSON-encoding is deliberate: it is the only escaping that is guaranteed to be
    // valid inside a JS string literal for arbitrary filter-list content.
    let payload = json!({
      "hosts": hosts,
      "patterns": patterns,
      "css": css,
      "watchDom": options.watch_dom,
    })
    .to_string();

    format!("(function(){{window.__birInstallBlocker({payload});}})();")
  }
}

/// The static half of the script — installed once per webview as an initialisation
/// script — plus the per-page driver `window.__birInstallBlocker`.
///
/// Keeping the constant part out of every navigation saves re-parsing ~4 KB of JS per
/// page load.
pub const RUNTIME: &str = r#"
(function () {
  if (window.__birInstallBlocker) { return; }

  var stats = { blocked: 0, hidden: 0 };
  var HOSTS = null;
  var PATTERNS = null;

  function hostOf(url) {
    var s = String(url);
    var i = s.indexOf('://');
    if (i !== -1) { s = s.slice(i + 3); }
    var end = s.length;
    var slash = s.indexOf('/');
    var q = s.indexOf('?');
    var h = s.indexOf('#');
    if (slash !== -1) { end = Math.min(end, slash); }
    if (q !== -1) { end = Math.min(end, q); }
    if (h !== -1) { end = Math.min(end, h); }
    s = s.slice(0, end);
    var at = s.lastIndexOf('@');
    if (at !== -1) { s = s.slice(at + 1); }
    if (s.charAt(0) === '[') {
      var close = s.indexOf(']');
      return close === -1 ? s : s.slice(1, close);
    }
    var colon = s.indexOf(':');
    if (colon !== -1) { s = s.slice(0, colon); }
    return s.toLowerCase();
  }

  function hostBlocked(host) {
    if (!HOSTS || !host) { return false; }
    if (HOSTS.has(host)) { return true; }
    // Walk up the labels: `ads.example.com` is caught by a rule for `example.com`.
    var i = host.indexOf('.');
    while (i !== -1 && i < host.length - 1) {
      var parent = host.slice(i + 1);
      if (HOSTS.has(parent)) { return true; }
      i = host.indexOf('.', i + 1);
    }
    return false;
  }

  function blocked(url) {
    if (!url) { return false; }
    var lower = String(url).toLowerCase();
    if (hostBlocked(hostOf(lower))) { return true; }
    if (PATTERNS) {
      for (var i = 0; i < PATTERNS.length; i++) {
        if (lower.indexOf(PATTERNS[i]) !== -1) { return true; }
      }
    }
    return false;
  }

  function note() {
    stats.blocked++;
  }

  // --- network interception -------------------------------------------------
  var origFetch = window.fetch;
  if (typeof origFetch === 'function') {
    window.fetch = function (input, init) {
      var url = (input && typeof input === 'object' && input.url) ? input.url : input;
      if (blocked(url)) {
        note();
        var err = new TypeError('Failed to fetch');
        return Promise.reject(err);
      }
      return origFetch.apply(window, arguments);
    };
  }

  var OrigXHR = window.XMLHttpRequest;
  if (typeof OrigXHR === 'function') {
    var origOpen = OrigXHR.prototype.open;
    OrigXHR.prototype.open = function (method, url) {
      if (blocked(url)) {
        note();
        this.__birBlocked = true;
      }
      return origOpen.apply(this, arguments);
    };
    var origSend = OrigXHR.prototype.send;
    OrigXHR.prototype.send = function () {
      if (this.__birBlocked) {
        // Fail asynchronously so pages that ignore errors do not spin.
        var self = this;
        setTimeout(function () {
          try { self.dispatchEvent(new Event('error')); } catch (e) {}
        }, 0);
        return;
      }
      return origSend.apply(this, arguments);
    };
  }

  if (typeof window.EventSource === 'function') {
    var OrigES = window.EventSource;
    window.EventSource = function (url, cfg) {
      if (blocked(url)) { note(); throw new Error('blocked'); }
      return new OrigES(url, cfg);
    };
    window.EventSource.prototype = OrigES.prototype;
  }

  if (navigator.sendBeacon) {
    var origBeacon = navigator.sendBeacon.bind(navigator);
    navigator.sendBeacon = function (url, data) {
      if (blocked(url)) { note(); return false; }
      return origBeacon(url, data);
    };
  }

  if (typeof window.WebSocket === 'function') {
    var OrigWS = window.WebSocket;
    window.WebSocket = function (url, protocols) {
      if (blocked(url)) { note(); throw new Error('blocked'); }
      return protocols === undefined ? new OrigWS(url) : new OrigWS(url, protocols);
    };
    window.WebSocket.prototype = OrigWS.prototype;
    window.WebSocket.CONNECTING = OrigWS.CONNECTING;
    window.WebSocket.OPEN = OrigWS.OPEN;
    window.WebSocket.CLOSING = OrigWS.CLOSING;
    window.WebSocket.CLOSED = OrigWS.CLOSED;
  }

  // --- cosmetic hiding ------------------------------------------------------
  function applyCss(css) {
    if (!css) { return; }
    try {
      var style = document.createElement('style');
      style.setAttribute('data-bir', 'blocking');
      style.textContent = css;
      (document.head || document.documentElement).appendChild(style);
      stats.hidden++;
    } catch (e) {}
  }

  // --- dynamic node sweeping ------------------------------------------------
  function sweep(root) {
    if (!root || !root.querySelectorAll) { return; }
    try {
      var nodes = root.querySelectorAll('script[src],iframe[src],img[src],source[src]');
      for (var i = 0; i < nodes.length; i++) {
        var node = nodes[i];
        var src = node.getAttribute('src');
        if (src && blocked(src)) {
          note();
          if (node.parentNode) { node.parentNode.removeChild(node); }
          else { node.setAttribute('src', 'about:blank'); }
        }
      }
    } catch (e) {}
  }

  // --- per-page installation -----------------------------------------------
  window.__birInstallBlocker = function (cfg) {
    if (!cfg) { return; }
    if (cfg.hosts && HOSTS === null) {
      HOSTS = new Set(cfg.hosts.split('\n'));
      HOSTS.delete('');
    }
    if (cfg.patterns && PATTERNS === null) {
      PATTERNS = cfg.patterns.split('\n').filter(function (p) { return p.length > 0; });
    }
    applyCss(cfg.css);
    if (cfg.watchDom && typeof MutationObserver === 'function') {
      try {
        new MutationObserver(function (records) {
          for (var i = 0; i < records.length; i++) {
            var added = records[i].addedNodes;
            for (var j = 0; j < added.length; j++) {
              var node = added[j];
              if (node.nodeType === 1) { sweep(node); }
            }
          }
        }).observe(document.documentElement || document, { childList: true, subtree: true });
      } catch (e) {}
    }
  };

  window.__birBlockStats = function () { return stats; };
  window.__birIsBlocked = blocked;
})();
"#;
