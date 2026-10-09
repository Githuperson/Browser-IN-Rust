//! `bir-core` — the half of the browser that has nothing to do with rendering.
//!
//! Everything in here is deliberately free of any webview/GUI dependency so it can be
//! unit-tested headlessly, reused by a future mobile/embedded front end, and reasoned
//! about without a windowing system attached.
//!
//! The one rule this crate follows: **no blocking work on the hot path, no allocation
//! storms per keystroke.** Several types here (history, bookmarks, downloads) are
//! queried on every keystroke in the omnibox, so they keep flat `Vec`s plus small
//! indices instead of deep trees.

pub mod bookmarks;
pub mod downloads;
pub mod history;
pub mod ipc;
pub mod paths;
pub mod search;
pub mod session;
pub mod settings;
pub mod site_settings;
pub mod store;
pub mod time;
pub mod url;

pub use bookmarks::{Bookmark, BookmarkStore};
pub use downloads::{DownloadItem, DownloadManager, DownloadState};
pub use history::{HistoryEntry, HistoryStore};
pub use paths::ProfilePaths;
pub use search::{SearchEngine, SearchEngines};
pub use session::{SessionStore, TabSnapshot, WindowSnapshot};
pub use settings::Settings;
pub use site_settings::{Permission, SiteSettings};
pub use url::{OmniboxInput, UrlInfo};

use thiserror::Error;

/// Result alias used across the whole workspace.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by the browser core.
#[derive(Debug, Error)]
pub enum Error {
  #[error("io error: {0}")]
  Io(#[from] std::io::Error),
  #[error("json error: {0}")]
  Json(#[from] serde_json::Error),
  #[error("url parse error: {0}")]
  Url(#[from] url::ParseError),
  #[error("{0}")]
  Other(String),
}

/// Anything that can be persisted to disk in the profile directory.
///
/// Implementations must be cheap to call repeatedly: the app flushes dirty stores on a
/// timer (and on exit), not on every mutation.
pub trait Persistent: Sized {
  /// File name inside the profile directory, e.g. `bookmarks.json`.
  const FILE: &'static str;

  /// Called after a mutation that should eventually reach disk.
  fn mark_dirty(&mut self);

  /// Whether a flush is pending.
  fn is_dirty(&self) -> bool;

  /// Cleared by [`Persistent::save`] implementations after a successful write.
  fn clear_dirty(&mut self);

  /// Serialise to a JSON document.
  fn to_json(&self) -> Result<String>;
}

/// Convenience helper: write `value` into `paths.data_dir()/<FILE>` atomically.
pub fn persist<T: Persistent>(paths: &ProfilePaths, value: &mut T) -> Result<()> {
  if !value.is_dirty() {
    return Ok(());
  }
  let doc = value.to_json()?;
  let file = paths.data_dir().join(T::FILE);
  store::write_atomic(&file, doc.as_bytes())?;
  value.clear_dirty();
  Ok(())
}
