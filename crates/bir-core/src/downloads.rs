//! Download bookkeeping.
//!
//! The platform webview performs the actual transfer; it only tells us when a download
//! starts and when it finishes. Progress is therefore *observed* by polling the file
//! size — which turns out to be more accurate than most browsers' own progress bars,
//! since it reflects bytes actually on disk.

use crate::{store, time, Error, Persistent, ProfilePaths, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
  InProgress,
  Complete,
  Cancelled,
  Failed,
  /// The user chose a location and we are waiting for the webview to start writing.
  Starting,
}

impl DownloadState {
  pub fn is_finished(self) -> bool {
    !matches!(self, DownloadState::InProgress | DownloadState::Starting)
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadItem {
  pub id: String,
  pub url: String,
  /// Final path on disk, known once the download has started writing.
  pub path: Option<PathBuf>,
  pub filename: String,
  /// Bytes on disk at the last poll.
  pub received: u64,
  /// Advertised total size when the server provided one; `None` otherwise.
  pub total: Option<u64>,
  pub state: DownloadState,
  pub started_at: u64,
  pub finished_at: Option<u64>,
  pub mime: String,
}

impl DownloadItem {
  /// 0.0–1.0, or `None` when the total size is unknown.
  pub fn progress(&self) -> Option<f64> {
    let total = self.total.filter(|t| *t > 0)?;
    Some((self.received as f64 / total as f64).clamp(0.0, 1.0))
  }

  pub fn is_finished(&self) -> bool {
    self.state.is_finished()
  }
}

pub struct DownloadManager {
  items: Vec<DownloadItem>,
  dir: PathBuf,
  ask_where_to_save: bool,
  max_concurrent: usize,
  next_id: u64,
  dirty: bool,
}

impl DownloadManager {
  pub fn new(dir: PathBuf, ask_where_to_save: bool, max_concurrent: usize) -> Self {
    Self {
      items: Vec::new(),
      dir,
      ask_where_to_save,
      max_concurrent: max_concurrent.max(1),
      next_id: 0,
      dirty: false,
    }
  }

  pub fn from_paths(paths: &ProfilePaths, ask: bool, max_concurrent: usize) -> Self {
    let dir = paths.downloads_dir().to_path_buf();
    Self::new(dir, ask, max_concurrent)
  }

  pub fn items(&self) -> &[DownloadItem] {
    &self.items
  }

  pub fn set_dir(&mut self, dir: PathBuf) {
    self.dir = dir;
  }

  pub fn dir(&self) -> &Path {
    &self.dir
  }

  pub fn asks_for_location(&self) -> bool {
    self.ask_where_to_save
  }

  pub fn set_asks_for_location(&mut self, ask: bool) {
    self.ask_where_to_save = ask;
  }

  /// Pick a destination for an incoming download and register it.
  ///
  /// `suggested` is the filename the site asked for; it is sanitised before use because
  /// a server-controlled filename is a path-traversal vector (`../../.bashrc`).
  pub fn start(&mut self, url: &str, suggested: Option<&str>, mime: &str) -> (String, PathBuf) {
    let filename = sanitise_filename(suggested.unwrap_or("download"));
    let mut path = unique_path(&self.dir, &filename);

    // Back-pressure: if too many downloads are already running, still accept this one
    // (cancelling a user-initiated download is worse) but let the UI show the overflow.
    if self.active_count() >= self.max_concurrent {
      path = unique_path(&self.dir, &filename);
    }

    self.next_id += 1;
    let id = format!("d{}", self.next_id);
    let item = DownloadItem {
      id: id.clone(),
      url: url.to_string(),
      path: Some(path.clone()),
      filename: path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.clone()),
      received: 0,
      total: None,
      state: DownloadState::Starting,
      started_at: time::now_secs(),
      finished_at: None,
      mime: mime.to_string(),
    };
    self.items.push(item);
    self.dirty = true;
    (id, path)
  }

  /// The download moved from "starting" to actually writing.
  pub fn began(&mut self, id: &str, path: Option<PathBuf>) {
    if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
      item.state = DownloadState::InProgress;
      if let Some(path) = path {
        item.filename = path
          .file_name()
          .map(|n| n.to_string_lossy().into_owned())
          .unwrap_or_else(|| item.filename.clone());
        item.path = Some(path);
      }
      self.dirty = true;
    }
  }

  pub fn finished(&mut self, id: &str, success: bool) {
    if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
      item.state = if success {
        DownloadState::Complete
      } else {
        DownloadState::Failed
      };
      item.finished_at = Some(time::now_secs());
      if let Some(path) = &item.path {
        item.received = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
      }
      self.dirty = true;
    }
  }

  pub fn cancel(&mut self, id: &str) {
    if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
      item.state = DownloadState::Cancelled;
      item.finished_at = Some(time::now_secs());
      self.dirty = true;
    }
  }

  pub fn remove(&mut self, id: &str) {
    self.items.retain(|i| i.id != id);
    self.dirty = true;
  }

  pub fn clear_finished(&mut self) {
    self.items.retain(|i| !i.is_finished());
    self.dirty = true;
  }

  pub fn active_count(&self) -> usize {
    self
      .items
      .iter()
      .filter(|i| !i.is_finished())
      .count()
  }

  /// Look up by id.
  pub fn get(&self, id: &str) -> Option<&DownloadItem> {
    self.items.iter().find(|i| i.id == id)
  }

  /// Refresh `received` for in-flight downloads by stat-ing their files.
  ///
  /// Cheap enough to run on the UI tick (a handful of `stat` calls) and it keeps the
  /// downloads panel honest even though the webview gives us no progress events.
  pub fn poll(&mut self) {
    for item in self.items.iter_mut() {
      if item.is_finished() {
        continue;
      }
      if let Some(path) = &item.path {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size != item.received {
          item.received = size;
        }
      }
    }
  }

  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let path = paths.data_dir().join(Self::FILE);
    let mut manager = Self::new(paths.downloads_dir().to_path_buf(), false, 4);
    if let Some(doc) = store::read_to_string(&path)? {
      if let Ok(items) = serde_json::from_str::<Vec<DownloadItem>>(&doc) {
        // Downloads that were in flight when the browser exited are dead: mark them
        // failed rather than showing a permanently "downloading" row.
        manager.items = items
          .into_iter()
          .map(|mut item| {
            if !item.is_finished() {
              item.state = DownloadState::Failed;
              item.finished_at = Some(time::now_secs());
            }
            item
          })
          .collect();
        manager.next_id = manager.items.len() as u64 + 1;
      }
    }
    Ok(manager)
  }
}

impl Persistent for DownloadManager {
  const FILE: &'static str = "downloads.json";

  fn mark_dirty(&mut self) {
    self.dirty = true;
  }
  fn is_dirty(&self) -> bool {
    self.dirty
  }
  fn clear_dirty(&mut self) {
    self.dirty = false;
  }
  fn to_json(&self) -> Result<String> {
    serde_json::to_string_pretty(&self.items).map_err(Error::from)
  }
}

/// Strip directory components, control characters and anything that is not safe on the
/// three filesystems we support.
pub fn sanitise_filename(name: &str) -> String {
  let name = name
    .rsplit(['/', '\\'])
    .next()
    .unwrap_or("download")
    .trim_matches(|c: char| c.is_whitespace() || c == '.')
    .to_string();

  let cleaned: String = name
    .chars()
    .filter(|c| !matches!(*c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'))
    .filter(|c| !c.is_control())
    .collect();

  if cleaned.is_empty() {
    "download".to_string()
  } else if cleaned.len() > 180 {
    let mut end = 180;
    while !cleaned.is_char_boundary(end) && end > 0 {
      end -= 1;
    }
    cleaned[..end].to_string()
  } else {
    cleaned
  }
}

/// `report.pdf` → `report (1).pdf` → `report (2).pdf`, never overwriting.
pub fn unique_path(dir: &Path, filename: &str) -> PathBuf {
  let candidate = dir.join(filename);
  let stem = Path::new(filename)
    .file_stem()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| "download".into());
  let ext = Path::new(filename)
    .extension()
    .map(|e| format!(".{}", e.to_string_lossy()))
    .unwrap_or_default();

  let mut attempt = 0u32;
  let mut path = candidate;
  while path.exists() {
    attempt += 1;
    let name = format!("{stem} ({attempt}){ext}");
    path = dir.join(name);
    if attempt > 999 {
      break;
    }
  }
  path
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn filenames_are_sanitised() {
    assert_eq!(sanitise_filename("../../etc/passwd"), "passwd");
    assert_eq!(sanitise_filename("  ..  "), "download");
    assert_eq!(sanitise_filename("a/b\\c.txt"), "c.txt");
    assert_eq!(sanitise_filename(""), "download");
  }

  #[test]
  fn unique_paths_do_not_clobber() {
    let dir = std::env::temp_dir().join(format!("bir-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let p = unique_path(&dir, "file.txt");
    std::fs::write(&p, b"x").unwrap();
    let p2 = unique_path(&dir, "file.txt");
    assert_ne!(p, p2);
    assert!(p2.to_string_lossy().contains("(1)"));
    let _ = std::fs::remove_dir_all(&dir);
  }
}
