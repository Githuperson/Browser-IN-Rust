//! `bir-ext` — the WebExtensions host.
//!
//! # Honest statement of what this is
//!
//! This is a **WebExtensions-compatible extension host built on top of a system
//! webview**, not a copy of Chromium's extension system. That distinction matters:
//!
//! * Chromium-only internals are not available. `webRequest` and
//!   `declarativeNetRequest` cannot be implemented, because none of the three platform
//!   webviews expose request interception to the embedder outside of a native
//!   WebExtension process. Those APIs reject with a clear error instead of failing
//!   silently.
//! * Content scripts run in the page's own JS world (a real isolated world needs
//!   engine-level support). Each script is wrapped in an IIFE so it cannot collide with
//!   page globals, but page scripts *can* see it. This is the same trade-off Firefox
//!   made before it had separate processes.
//! * **On Windows this crate is not needed for the common case**: WebView2 can load
//!   unpacked Chromium extensions natively, and the shell hands every enabled extension
//!   to it (see [`registry::ExtensionRegistry::sync_native_dir`]). Native extensions get
//!   the real Chromium extension process, isolated worlds and full API support. The
//!   compatibility layer here is what macOS and Linux use, and what Windows uses for
//!   APIs WebView2's loader does not expose to us.
//!
//! # What *is* supported
//!
//! MV3 manifests, CRX3 and zip packaging, unpacked directories, content scripts
//! (document_start / document_end / document_idle, `all_frames`, `matches` /
//! `exclude_matches`), background service workers, and the `runtime`, `storage`, `tabs`,
//! `scripting`, `alarms`, `notifications`, `contextMenus`, `cookies` and `i18n` APIs.

pub mod bridge;
pub mod crx;
pub mod manifest;
pub mod matches;
pub mod permissions;
pub mod registry;

pub use bridge::{BridgeMessage, BridgeReply, ContentScriptInjection, EXTENSION_RUNTIME_JS};
pub use crx::{parse_crx, unpack_package, CrxInfo};
pub use manifest::{ContentScriptDecl, Manifest, RunAt};
pub use permissions::{PermissionSet, RiskLevel};
pub use registry::{ExtensionKind, ExtensionRegistry, InstalledExtension};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
  #[error("{0}")]
  Core(#[from] bir_core::Error),
  #[error("io error: {0}")]
  Io(#[from] std::io::Error),
  #[error("json error: {0}")]
  Json(#[from] serde_json::Error),
  #[error("zip error: {0}")]
  Zip(#[from] zip::result::ZipError),
  #[error("extension error: {0}")]
  Extension(String),
}
