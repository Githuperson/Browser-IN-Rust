//! Tiny, dependency-free persistence helpers.
//!
//! A browser writes the same handful of documents forever (settings, bookmarks,
//! session, extension state). SQLite would be the "proper" answer, but it drags a C
//! build into every `cargo build`, costs a few hundred KB of RSS for the connection,
//! and is overkill for documents that fit in memory comfortably. So:
//!
//! * whole-document stores ([`write_atomic`]) — small JSON, rewritten atomically.
//! * append-only stores ([`LineStore`]) — history, which only ever grows at one end.
//!
//! Both are crash-safe: a writer never leaves a half-written file at the real path.

use crate::{Error, Result};
use std::{
  fs::{self, File},
  io::{BufRead, BufReader, Write},
  path::{Path, PathBuf},
};

/// Write `bytes` to `path` via a sibling temp file and an atomic rename.
///
/// The rename is atomic within a directory on every platform we target, so a crash
/// mid-write leaves the previous document intact rather than a truncated one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  let tmp = temp_path(path);
  {
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    // Flush before rename: on some filesystems metadata and data are journalled
    // separately and an unflushed write can survive as a zero-length file.
    file.sync_all()?;
  }
  match fs::rename(&tmp, path) {
    Ok(()) => Ok(()),
    Err(err) => {
      // Windows cannot rename over an existing file in some configurations.
      let _ = fs::remove_file(path);
      fs::rename(&tmp, path).map_err(|_| err)?;
      Ok(())
    }
  }
}

fn temp_path(path: &Path) -> PathBuf {
  let mut name = path
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_else(|| "doc".into());
  name.push_str(&format!(".tmp-{}", std::process::id()));
  path.with_file_name(name)
}

/// Read a whole JSON document, returning `None` when it does not exist yet.
pub fn read_to_string(path: &Path) -> Result<Option<String>> {
  match fs::read_to_string(path) {
    Ok(s) => Ok(Some(s)),
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(err) => Err(err.into()),
  }
}

/// Append-only newline-delimited JSON store.
///
/// Used by history: visits are appended as they happen (O(1), no rewrite of a
/// multi-megabyte document) and compacted in the background once the file grows
/// past a threshold or the in-memory entry cap is exceeded.
pub struct LineStore {
  path: PathBuf,
}

impl LineStore {
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self { path: path.into() }
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Append one already-serialised line and flush it.
  pub fn append_line(&self, line: &str) -> Result<()> {
    if let Some(parent) = self.path.parent() {
      fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&self.path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
  }

  /// Read every line, invoking `sink` for each. Malformed lines are skipped rather
  /// than fatal: one corrupt line should not cost the user their whole history.
  pub fn read_all(&self, mut sink: impl FnMut(&str)) -> Result<()> {
    let file = match File::open(&self.path) {
      Ok(f) => f,
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
      Err(err) => return Err(err.into()),
    };
    for line in BufReader::new(file).lines() {
      let line = line?;
      let line = line.trim();
      if !line.is_empty() {
        sink(line);
      }
    }
    Ok(())
  }

  /// Rewrite the file from `lines`, atomically. Used by compaction.
  pub fn rewrite(&self, lines: &[String]) -> Result<()> {
    let mut doc = String::with_capacity(lines.len() * 96);
    for line in lines {
      doc.push_str(line);
      doc.push('\n');
    }
    write_atomic(&self.path, doc.as_bytes())
  }

  /// Size of the backing file in bytes (0 if missing).
  pub fn len_bytes(&self) -> u64 {
    fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
  }
}

/// Guard against pathological input: never let a corrupt on-disk document OOM us.
pub fn checked_capacity(len: usize, max: usize) -> Result<usize> {
  if len > max {
    return Err(Error::Other(format!(
      "refusing to load {len} records (limit {max}); document looks corrupt"
    )));
  }
  Ok(len)
}
