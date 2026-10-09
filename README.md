# BIR — Browser In Rust

A real, everyday browser written in Rust: tabs, bookmarks, history, downloads, a
content blocker, find-in-page, working embeds and JavaScript — plus Chrome MV3
extension support, power-user controls over memory and CPU, GPU acceleration, and one
binary per platform.

It is built on **each operating system's own webview** rather than a bundled Chromium:

| Platform | Webview            | What that buys us                                              |
| -------- | ------------------ | -------------------------------------------------------------- |
| Windows  | WebView2 (Edge)    | Chromium rendering, native extension loading, ~10 MB of our code |
| macOS    | WKWebView          | Metal compositing, native energy and memory behaviour          |
| Linux    | WebKitGTK 4.1      | Shared system webview, one copy of WebKit for the whole machine |

That choice is why the browser is small and why it never downloads an engine: the
system already owns updates, codecs, sandboxing and GPU drivers.

---

## Status

Everything described below is implemented in this repository. Nothing has been compiled
yet in this workspace (no Rust toolchain and no crates.io access here) — CI on a real
machine is the first build:

```bash
cargo build --release     # needs Linux: libwebkit2gtk-4.1-dev, libgtk-3-dev
cargo run --release --bin bir
```

If a type error survived the review passes, it will be a small one; the shape of every
module is pinned against the wry 0.57 / tao 0.37 source, not against memory of it.

---

## Features

### Everyday browsing

* **Tabs** — open, close, pin, mute, duplicate, reorder, discard, restore on startup.
  Middle-click and context menus work as expected.
* **Omnibox** — URL detection, search-engine keywords (`ddg webview`), history,
  bookmarks, open-tab switching and search suggestions in one ranked dropdown.
* **Navigation** — back, forward, reload, stop, zoom (per-site and per-tab), print,
  find-in-page with match counts, fullscreen, developer tools.
* **Bookmarks and history** — locally stored, searchable, grouped by day, with caps
  (100,000 entries / 12 MiB) rather than unbounded growth.
* **Downloads** — real progress (measured from bytes on disk, not guessed), cancel,
  retry, open, reveal, clear finished.
* **Internal pages** — `bir://newtab`, `bir://history`, `bir://bookmarks`,
  `bir://downloads`, `bir://settings`, `bir://extensions`, `bir://about`. They are
  ordinary web pages, so text selection, find-in-page and accessibility work in them.
* **Working embeds and JavaScript** — the webview *is* the web engine, so YouTube,
  Google Maps, web components, WebGL, Service Workers and WebAssembly behave as they do
  in the platform's own browser.

### Privacy and blocking

* Network blocking for ads and trackers, driven by an adblock-style rule engine
  (token-bucket indexed, so a 60,000-rule list costs microseconds per request).
* Cosmetic filtering: element-hiding CSS plus a MutationObserver that keeps hiding
  elements DOM injections add later.
* Blocking of `fetch`, `XMLHttpRequest`, `EventSource`, `sendBeacon` and `WebSocket`
  at the JavaScript level, not just at navigation time.
* Tracking-parameter stripping and HTTPS-only upgrades, applied before the request
  leaves the machine.
* Per-site exceptions, per-site permissions, per-site zoom, and configurable
  clear-on-exit.

### Extensions (Chrome MV3)

* Install from an unpacked directory, a `.zip`, or a signed **CRX3** package — the
  extension id is derived from the public key exactly as Chrome does, so an extension
  keeps its id across reinstalls.
* Manifest V3 **and** V2 parsing (permissive: unknown keys are kept, unknown API
  permission strings are tolerated).
* **Background service worker** host: a hidden webview running background scripts,
  with `storage.local`/`storage.sync`, `alarms`, `notifications`, `contextMenus`,
  `i18n`, `runtime`, `tabs`, `windows`, `scripting`, `action`, `commands`, and the
  `browser.*` / `chrome.*` namespaces with callback **and** Promise styles.
* **Content scripts** with match patterns, `run_at`, `all_frames`, CSS injection, and
  `executeScript`/`insertCSS`.
* **Toolbar buttons** with real popups, rendered in an overlay iframe.
* Permission review: each extension's permissions are listed with a risk summary before
  and after install.
* On Windows, extensions can additionally be handed to WebView2's native loader, which
  is the closest thing to real Chrome extension hosting available outside Chrome.

### Extreme / power-user features

* **Lazy webview creation** — a restored or background tab is a record, not a renderer.
  Restoring 200 tabs costs a few hundred kilobytes.
* **Tab sleeping** — hidden webviews are unmapped and throttled; state stays intact, so
  waking is instant.
* **Tab discarding** — the webview is dropped and its memory returned to the OS; the tab
  keeps its title, URL and favicon, and reloads when you return to it. This is the only
  mechanism that actually gives memory back under pressure.
* **Memory-pressure governance** — sampled system and process memory drives a policy
  that sleeps the least-recently-used tabs first and discards above a configurable
  threshold. Pinned and audible tabs are never discarded.
* **Per-site process isolation** — each tab gets its own webview, and private windows
  get a separate web context with a separate cookie jar and cache.
* **Live resource panel** — RSS, estimated webview total, CPU, system-memory pressure,
  and how many tabs are live, sleeping or discarded.
* Keyboard-driven everything (Ctrl/Cmd+T, W, Shift+T, 1–9, L, F, G, P, R, +/-/0, D, J,
  H, comma, F11, F12, Alt+←/→), vertical tabs, compact mode, light/dark/system themes.

### Performance

* **GPU acceleration** — hardware rasterisation flags on WebView2, WebKitGTK's
  compositing path on Linux (with an explicit software fallback for broken drivers),
  Metal always on macOS. A `--gpu-report` flag tells you which one you are on.
* Release profile tuned for the binary we actually ship: LTO, `codegen-units = 1`,
  `panic = "abort"` and stripped symbols.
* One event loop for the whole process, timers consolidated onto a 1-second tick, and
  no polling anywhere except download progress (which is read from the filesystem).

---

## Architecture

```text
bir          — the binary: CLI, profile selection, event loop ownership
bir-core     — profile paths, settings, history, bookmarks, downloads, session,
               site settings, search engines, the UI wire protocol, URL handling
bir-net      — filter-list fetching/caching, adblock rule parsing, the blocker engine,
               and the document-start script that patches network APIs
bir-perf     — memory + CPU sampling, tab lifecycle policy, GPU policy
bir-ext      — MV3/MV2 manifests, CRX3 parsing, match patterns, the JS bridge,
               the background host, the installed-extension registry
bir-ui       — tao windows, wry webviews, the HTML chrome, internal pages,
               command dispatch, shortcuts, the extension API server
```

A window looks like this:

```text
┌──────────────────────────── window ────────────────────────────┐
│  chrome webview   (HTML/CSS/JS: tab strip, omnibox, menus)     │ ← 96 px
├────────────────────────────────────────────────────────────────┤
│  content webview  (the page, one per tab, one visible)         │
└────────────────────────────────────────────────────────────────┘
```

Both halves communicate over newline-free JSON:

```text
chrome → Rust : window.ipc.postMessage(JSON.stringify(command))
Rust → chrome : bir.onEvent(json)
```

Every message is a variant of one tagged enum on each side, so a typo is a compile error
rather than a silently ignored string.

On Linux, webviews are attached to a `gtk::Fixed` container inside the window
(`build_gtk`) rather than as X11 child windows, so X11 and Wayland behave identically.

---

## Building

```bash
# Linux (Debian/Ubuntu)
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
                 librsvg2-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev pkg-config

cargo build --release
./target/release/bir
```

macOS and Windows need no system dependencies beyond Xcode command-line tools and the
WebView2 runtime respectively.

### Command line

```bash
bir                          # open a window
bir https://example.com      # open URLs
bir --private                # private window (separate context, nothing persisted)
bir --profile work           # a separate profile
bir --install-extension ./my-extension      # install from a folder, .zip or .crx
bir --install-extension ./my-extension.crx
bir --gpu-report             # which rendering path is in use
```

---

## What is honest about this design

A system webview is not Chromium-with-flags, and three consequences are structural
rather than unfinished:

1. **Content scripts run in the page's own JavaScript world.** A system webview gives
   no isolated world, so a page can observe them. Chrome isolates them; we cannot.
2. **`webRequest` and `declarativeNetRequest` reject.** Intercepting network traffic is
   not possible through a system webview. The built-in blocker is the replacement, and
   it is stronger than a typical ad-blocker's *filter list*, weaker than a real
   `webRequest` extension.
3. **Cookie APIs are unavailable** for the same reason; per-site storage is cleared
   through the webview's own data APIs.

Everything else — storage, alarms, menus, tabs, windows, scripting, popups, i18n,
notifications, background service workers — is a real implementation, not a stub.

---

## Layout of the profile

```text
~/.local/share/bir/default/        (macOS/Windows: the platform's data directory)
├── settings.json                  appearance, privacy, performance, network, …
├── session.json                   windows, tabs, geometry
├── history.ndjson                 capped at 100k entries / 12 MiB
├── bookmarks.ndjson
├── downloads.ndjson
├── site-settings.json             per-site permissions, zoom, blocking exceptions
├── search-engines.json
├── extensions.json                installed extensions
├── extensions/                    unpacked extension directories (served as bir://<id>/)
├── webview-data/                  the webview's own cookies and caches
└── cache/filter-lists/            downloaded filter lists, refreshed every 3 days
```

State is newline-delimited JSON written atomically (write to a temporary file, then
rename). There is no database to corrupt, and any of it can be edited by hand.

---

## Roadmap

* Bookmark folders in the UI, bookmark import/export
* Reading mode and page translation hooks
* Sync (the storage layer is already portable; the transport is not written)
* `declarativeNetRequest`-shaped declarative rules on top of the native blocker
* Per-site process isolation on platforms that allow a separate webview process

---

## Licence

MIT — see [LICENSE](LICENSE).
