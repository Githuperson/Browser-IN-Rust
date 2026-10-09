//! The `bir://` protocol: chrome, internal pages, extension resources.
//!
//! Everything the browser shows that is not a web page is served from here, which means
//! the chrome and the internal pages are ordinary web content: cacheable,
//! GPU-composited, inspectable, themeable with CSS.

use std::{
  borrow::Cow,
  path::PathBuf,
  sync::RwLock,
};

use wry::http::{Request, Response};

use crate::app::BIR_SCHEME;

const CHROME_HTML: &str = include_str!("../assets/chrome.html");
const CHROME_CSS: &str = include_str!("../assets/chrome.css");
const CHROME_JS: &str = include_str!("../assets/chrome.js");
const PAGE_HTML: &str = include_str!("../assets/page.html");
const PAGE_CSS: &str = include_str!("../assets/page.css");
const PAGE_JS: &str = include_str!("../assets/page.js");

/// Serves `bir://` requests from memory plus the extensions directory on disk.
pub struct PageHost {
  extensions_dir: PathBuf,
  /// Cache of extension file reads. Extension resources are small and read on every
  /// navigation, so a short-lived cache is worth it.
  resource_cache: RwLock<std::collections::HashMap<String, (u64, Vec<u8>)>>,
}

impl PageHost {
  pub fn new(extensions_dir: PathBuf) -> Self {
    Self {
      extensions_dir,
      resource_cache: RwLock::new(std::collections::HashMap::new()),
    }
  }

  /// Answer one `bir://` request.
  pub fn serve(&self, request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let uri = request.uri().to_string();
    let path = normalise_path(&uri);
    self.route(&path)
  }

  fn route(&self, path: &str) -> Response<Cow<'static, [u8]>> {
    let (head, rest) = match path.split_once('/') {
      Some((h, r)) => (h, r),
      None => (path, ""),
    };

    match head {
      "chrome" => html(CHROME_HTML.to_string()),
      "chrome.css" => css(CHROME_CSS),
      "chrome.js" => js(CHROME_JS),
      "page.css" => css(PAGE_CSS),
      "page.js" => js(PAGE_JS),
      "background" => html(bir_ext::background::background_html()),
      "error" => html(error_page(rest)),
      // Internal pages keep the flat `bir://<name>` form used everywhere else in the
      // codebase (`classify` turns `bir://settings` into `Internal("settings")`).
      "newtab" | "history" | "bookmarks" | "downloads" | "settings" | "extensions"
      | "about" => html(page_document(head)),
      // Anything else is `<extension-id>/<resource>`.
      other => self.extension_resource(other, rest),
    }
  }

  fn extension_resource(&self, id: &str, path: &str) -> Response<Cow<'static, [u8]>> {
    if id.is_empty() || path.is_empty() {
      return not_found();
    }
    // Reject traversal before touching the filesystem.
    if path.contains("..") || path.starts_with('/') {
      return not_found();
    }
    let file = self.extensions_dir.join(id).join(path);
    if !file.starts_with(&self.extensions_dir) || !file.is_file() {
      return not_found();
    }

    let key = format!("{id}/{path}");
    let modified = std::fs::metadata(&file)
      .and_then(|m| m.modified())
      .map(|t| {
        t.duration_since(std::time::UNIX_EPOCH)
          .map(|d| d.as_secs())
          .unwrap_or(0)
      })
      .unwrap_or(0);

    if let Ok(cache) = self.resource_cache.read() {
      if let Some((stamp, bytes)) = cache.get(&key) {
        if *stamp == modified {
          return typed(&key, bytes.clone());
        }
      }
    }

    let bytes = match std::fs::read(&file) {
      Ok(bytes) => bytes,
      Err(_) => return not_found(),
    };
    if let Ok(mut cache) = self.resource_cache.write() {
      // Bounded: extensions have few resources and a stale entry costs one re-read.
      if cache.len() > 512 {
        cache.clear();
      }
      cache.insert(key.clone(), (modified, bytes.clone()));
    }
    typed(&key, bytes)
  }
}

/// Strip the scheme, keeping `<host>/<path>`.
///
/// The three platforms hand the URI over in different shapes: `bir://path` on macOS and
/// Linux, `http://bir.path` on Windows.
fn normalise_path(uri: &str) -> String {
  let without_scheme = if let Some(rest) = uri.strip_prefix("bir://") {
    rest
  } else if let Some(rest) = uri.strip_prefix("http://bir.") {
    rest
  } else if let Some(rest) = uri.strip_prefix("https://bir.") {
    rest
  } else {
    uri.trim_start_matches("bir://")
  };
  without_scheme.trim_start_matches('/').to_string()
}

fn mime_for(path: &str) -> &'static str {
  match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
    "html" | "htm" => "text/html; charset=utf-8",
    "css" => "text/css; charset=utf-8",
    "js" | "mjs" => "text/javascript; charset=utf-8",
    "json" => "application/json; charset=utf-8",
    "svg" => "image/svg+xml",
    "png" => "image/png",
    "jpg" | "jpeg" => "image/jpeg",
    "gif" => "image/gif",
    "webp" => "image/webp",
    "ico" => "image/x-icon",
    "woff" => "font/woff",
    "woff2" => "font/woff2",
    "ttf" => "font/ttf",
    "txt" => "text/plain; charset=utf-8",
    "wasm" => "application/wasm",
    _ => "application/octet-stream",
  }
}

fn typed(path: &str, bytes: Vec<u8>) -> Response<Cow<'static, [u8]>> {
  Response::builder()
    .status(200)
    .header("Content-Type", mime_for(path))
    .header("Cache-Control", "no-cache")
    .body(Cow::Owned(bytes))
    .unwrap_or_else(|_| not_found())
}

fn html(body: String) -> Response<Cow<'static, [u8]>> {
  Response::builder()
    .status(200)
    .header("Content-Type", "text/html; charset=utf-8")
    .body(Cow::Owned(body.into_bytes()))
    .unwrap_or_else(|_| not_found())
}

fn css(text: &str) -> Response<Cow<'static, [u8]>> {
  Response::builder()
    .status(200)
    .header("Content-Type", "text/css; charset=utf-8")
    .body(Cow::Owned(text.as_bytes().to_vec()))
    .unwrap_or_else(|_| not_found())
}

fn js(text: &str) -> Response<Cow<'static, [u8]>> {
  Response::builder()
    .status(200)
    .header("Content-Type", "text/javascript; charset=utf-8")
    .body(Cow::Owned(text.as_bytes().to_vec()))
    .unwrap_or_else(|_| not_found())
}

fn not_found() -> Response<Cow<'static, [u8]>> {
  Response::builder()
    .status(404)
    .header("Content-Type", "text/plain; charset=utf-8")
    .body(Cow::Borrowed(b"Not found".as_slice()))
    .unwrap_or_else(|_| {
      Response::builder()
        .status(500)
        .body(Cow::Borrowed(b"".as_slice()))
        .expect("static response")
    })
}

/// The shared internal-page document, tagged with the page it should render.
fn page_document(name: &str) -> String {
  PAGE_HTML.replace("{{PAGE}}", name)
}

/// Network-error page. `rest` is a query string with `url` and `reason`.
fn error_page(query: &str) -> String {
  let mut url = String::from("unknown");
  let mut reason = String::from("The page could not be loaded.");
  for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
    match key.as_ref() {
      "url" => url = value.into_owned(),
      "reason" => reason = value.into_owned(),
      _ => {}
    }
  }
  format!(
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{url}</title>
<style>
  body {{ font: 15px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif;
         color: #1c1c1f; background: #f6f6f8; margin: 0; padding: 64px 24px; }}
  .card {{ max-width: 620px; margin: 0 auto; background: #fff; border-radius: 14px;
           padding: 32px; box-shadow: 0 1px 3px rgba(0,0,0,.12); }}
  h1 {{ font-size: 20px; margin: 0 0 8px; }}
  code {{ background: #f0f0f4; padding: 2px 6px; border-radius: 5px; word-break: break-all; }}
  @media (prefers-color-scheme: dark) {{
    body {{ background: #17171a; color: #e6e6ea; }}
    .card {{ background: #22222a; }}
    code {{ background: #303039; }}
  }}
</style></head>
<body><div class="card">
  <h1>This page could not be loaded</h1>
  <p>{reason}</p>
  <p><code>{url}</code></p>
</div></body></html>"#
  )
}

/// "Blocked by content blocking" page.
pub fn blocked_page(url: &str, rule: &str) -> String {
  format!(
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Blocked</title>
<style>
  body {{ font: 15px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif;
         color: #1c1c1f; background: #f6f6f8; margin: 0; padding: 64px 24px; }}
  .card {{ max-width: 620px; margin: 0 auto; background: #fff; border-radius: 14px;
           padding: 32px; box-shadow: 0 1px 3px rgba(0,0,0,.12); }}
  code {{ background: #f0f0f4; padding: 2px 6px; border-radius: 5px; word-break: break-all; }}
  @media (prefers-color-scheme: dark) {{
    body {{ background: #17171a; color: #e6e6ea; }}
    .card {{ background: #22222a; }}
    code {{ background: #303039; }}
  }}
</style></head>
<body><div class="card">
  <h1>Blocked by content blocking</h1>
  <p><code>{url}</code></p>
  <p>Matched rule: <code>{rule}</code></p>
  <p>Turn blocking off for this site in the address-bar menu to continue.</p>
</div></body></html>"#
  )
}

/// Apply the appearance settings before the chrome's own script runs.
pub fn theme_script(settings: &bir_core::Settings) -> String {
  let theme = match settings.appearance.theme {
    bir_core::settings::Theme::System => "system",
    bir_core::settings::Theme::Light => "light",
    bir_core::settings::Theme::Dark => "dark",
  };
  format!(
    "document.documentElement.dataset.theme='{theme}';\
     document.documentElement.dataset.compact='{}';\
     document.documentElement.dataset.tabs='{}';",
    settings.appearance.compact,
    match settings.appearance.tab_layout {
      bir_core::settings::TabLayout::Horizontal => "horizontal",
      bir_core::settings::TabLayout::Vertical => "vertical",
    }
  )
}

/// The JavaScript injected into every content webview at document start.
///
/// Provides the `bir.send()` channel back to Rust, find-in-page, and the small signals
/// the chrome needs (audio playing, hovered link, favicon).
pub fn page_bridge_js() -> String {
  PAGE_BRIDGE_JS.to_string()
}

/// Same bridge, plus the no-op `onEvent` the chrome replaces.
pub fn chrome_runtime_js() -> String {
  format!(
    "{PAGE_BRIDGE_JS}\nwindow.bir.onEvent = window.bir.onEvent || function(){{}};\n"
  )
}

const PAGE_BRIDGE_JS: &str = r#"
(function () {
  if (window.bir) { return; }

  var bir = {};

  bir.send = function (command) {
    try { window.ipc.postMessage(JSON.stringify(command)); } catch (e) {}
  };

  bir.onEvent = bir.onEvent || function () {};

  // --- find in page ---------------------------------------------------------
  var findState = { matches: [], index: -1, text: '' };

  function clearHighlights() {
    var marks = document.querySelectorAll('bir-highlight');
    for (var i = 0; i < marks.length; i++) {
      var mark = marks[i];
      var parent = mark.parentNode;
      if (!parent) { continue; }
      parent.replaceChild(document.createTextNode(mark.textContent), mark);
      parent.normalize();
    }
  }

  function collect(text) {
    var walker = document.createTreeWalker(
      document.body || document.documentElement,
      NodeFilter.SHOW_TEXT,
      { acceptNode: function (node) {
          if (!node.nodeValue || node.nodeValue.toLowerCase().indexOf(text) === -1) {
            return NodeFilter.FILTER_REJECT;
          }
          var tag = node.parentNode ? node.parentNode.nodeName : '';
          return (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT')
            ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT;
        } }
    );
    var ranges = [];
    var lower = text.toLowerCase();
    var node;
    while ((node = walker.nextNode())) {
      var hay = node.nodeValue.toLowerCase();
      var from = 0;
      var at;
      while ((at = hay.indexOf(lower, from)) !== -1) {
        var range = document.createRange();
        range.setStart(node, at);
        range.setEnd(node, at + text.length);
        ranges.push(range);
        from = at + text.length;
      }
    }
    return ranges;
  }

  bir.find = function (text, forward) {
    clearHighlights();
    if (!text) { findState = { matches: [], index: -1, text: '' }; return { matches: 0, current: 0 }; }
    if (text !== findState.text) {
      findState = { matches: collect(text), index: -1, text: text };
    }
    if (!findState.matches.length) { return { matches: 0, current: 0 }; }

    findState.index += forward ? 1 : -1;
    if (findState.index >= findState.matches.length) { findState.index = 0; }
    if (findState.index < 0) { findState.index = findState.matches.length - 1; }

    var range = findState.matches[findState.index];
    var mark = document.createElement('bir-highlight');
    mark.style.background = '#f5d90a';
    mark.style.color = '#000';
    mark.style.borderRadius = '2px';
    try { range.surroundContents(mark); } catch (e) { mark = null; }
    if (mark && mark.scrollIntoView) {
      mark.scrollIntoView({ block: 'center', behavior: 'auto' });
    }
    return { matches: findState.matches.length, current: findState.index + 1 };
  };

  bir.stopFind = function () {
    clearHighlights();
    findState = { matches: [], index: -1, text: '' };
  };

  // --- signals --------------------------------------------------------------
  // Audio: report once when a media element starts playing so the tab can show it.
  function watchMedia() {
    function check() {
      var playing = false;
      try {
        var media = document.querySelectorAll('video,audio');
        for (var i = 0; i < media.length; i++) {
          var el = media[i];
          if (!el.paused && !el.muted && el.currentTime > 0) { playing = true; }
        }
      } catch (e) {}
      if (playing !== lastAudible) {
        lastAudible = playing;
        bir.send({ t: 'audible', audible: playing });
      }
    }
    setInterval(check, 2000);
    check();
  }
  var lastAudible = false;

  document.addEventListener('mouseover', function (event) {
    var link = event.target && event.target.closest ? event.target.closest('a') : null;
    var href = link ? link.href : '';
    if (href !== lastHref) {
      lastHref = href;
      bir.send({ t: 'link_hover', url: href });
    }
  }, true);
  var lastHref = '';

  function reportFavicon() {
    var best = null;
    var links = document.querySelectorAll('link[rel~="icon"], link[rel="shortcut icon"]');
    for (var i = 0; i < links.length; i++) {
      var href = links[i].getAttribute('href');
      if (!href) { continue; }
      best = href;
      if (links[i].getAttribute('sizes') === '32x32' || links[i].getAttribute('sizes') === '64x64') {
        break;
      }
    }
    if (best) {
      try { bir.send({ t: 'favicon', href: new URL(best, location.href).href }); } catch (e) {}
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { reportFavicon(); watchMedia(); });
  } else {
    reportFavicon();
    watchMedia();
  }

  window.bir = bir;
})();
"#;

/// Scheme used to build internal URLs elsewhere in the crate.
pub fn bir_url(path: &str) -> String {
  format!("{BIR_SCHEME}://{path}")
}
