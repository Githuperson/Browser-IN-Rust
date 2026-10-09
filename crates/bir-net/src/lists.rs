//! Filter list management: built-in lists, remote lists, and on-disk caching.
//!
//! Remote lists are cached under the profile's cache directory and refreshed at most
//! once per [`DEFAULT_MAX_AGE`]. A failure to refresh is not fatal: the previous copy
//! keeps working. That is the right trade-off for a browser — a content blocker that
//! stops working because a list server is down is worse than a slightly stale list.

use std::{
  collections::HashMap,
  path::PathBuf,
  time::{SystemTime, UNIX_EPOCH},
};

use bir_core::{settings::FilterList, ProfilePaths};

use crate::{
  fetch,
  rules::RuleSet,
  Error, Result,
};

/// The two lists compiled into the binary.
pub const BUILTIN_ADS: &str = include_str!("../assets/bir-ads.txt");
pub const BUILTIN_TRACKERS: &str = include_str!("../assets/bir-trackers.txt");

/// How old a cached remote list may get before we refresh it.
pub const DEFAULT_MAX_AGE_SECS: u64 = 3 * 24 * 3600;

/// Never refresh more often than this, even if the user forces it repeatedly.
const MIN_REFRESH_INTERVAL_SECS: u64 = 3600;

/// Where a rule set came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListSource {
  /// Compiled into the binary; identified by its name.
  Builtin(&'static str),
  /// A user-added URL, cached on disk.
  Remote { name: String, url: String },
}

/// Result of a refresh pass.
#[derive(Debug, Default)]
pub struct UpdateOutcome {
  pub updated: Vec<String>,
  pub up_to_date: usize,
  pub failed: Vec<(String, String)>,
}

impl UpdateOutcome {
  pub fn is_empty(&self) -> bool {
    self.updated.is_empty() && self.failed.is_empty()
  }
}

pub struct FilterListManager {
  cache_dir: PathBuf,
}

impl FilterListManager {
  pub fn new(paths: &ProfilePaths) -> Self {
    Self {
      cache_dir: paths.cache_dir().join("filter-lists"),
    }
  }

  /// Load every enabled list into one rule set.
  ///
  /// Built-in lists are always available (they are part of the binary); remote lists
  /// fall back to "not loaded" if they have never been fetched, which the settings page
  /// surfaces as "needs update".
  pub fn load(&self, lists: &[FilterList]) -> Result<RuleSet> {
    let mut set = RuleSet::new();
    for list in lists.iter().filter(|l| l.enabled) {
      if let Some(text) = self.contents(list)? {
        set.extend(&text);
      }
    }
    Ok(set)
  }

  fn contents(&self, list: &FilterList) -> Result<Option<String>> {
    if list.url.is_empty() {
      return Ok(Some(match list.name.as_str() {
        "BIR baseline ads" => BUILTIN_ADS.to_string(),
        "BIR baseline trackers" => BUILTIN_TRACKERS.to_string(),
        _ => String::new(),
      }));
    }
    let path = self.path_for(&list.url);
    match std::fs::read_to_string(&path) {
      Ok(text) => Ok(Some(text)),
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(err) => Err(Error::Core(err.into())),
    }
  }

  /// Refresh remote lists that are missing or stale. Runs on background threads.
  pub fn update(&self, lists: &[FilterList], max_age_secs: u64) -> UpdateOutcome {
    let mut outcome = UpdateOutcome::default();
    let now = now_secs();

    let jobs: Vec<FilterList> = lists
      .iter()
      .filter(|l| l.enabled && !l.url.is_empty())
      .filter(|l| self.age_secs(&l.url).map(|a| a >= max_age_secs).unwrap_or(true))
      .cloned()
      .collect();

    if jobs.is_empty() {
      return outcome;
    }

    let handles: Vec<_> = jobs
      .into_iter()
      .map(|list| {
        let path = self.path_for(&list.url);
        std::thread::Builder::new()
          .name("bir-list-update".into())
          .spawn(move || refresh_one(&list, &path, now))
      })
      .collect();

    for (index, handle) in handles.into_iter().enumerate() {
      let _ = index;
      match handle {
        Ok(handle) => match handle.join() {
          Ok(Ok(name)) => outcome.updated.push(name),
          Ok(Err(RefreshError::Fresh(name))) => {
            let _ = name;
            outcome.up_to_date += 1;
          }
          Ok(Err(RefreshError::Failed(name, reason))) => outcome.failed.push((name, reason)),
          Err(_) => outcome.failed.push(("<unknown>".into(), "thread panicked".into())),
        },
        Err(err) => outcome.failed.push(("<unknown>".into(), err.to_string())),
      }
    }
    outcome
  }

  /// Bytes used by cached lists, for the settings page.
  pub fn cache_bytes(&self) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
      for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
          total += meta.len();
        }
      }
    }
    total
  }

  pub fn clear_cache(&self) {
    let _ = std::fs::remove_dir_all(&self.cache_dir);
  }

  fn path_for(&self, url: &str) -> PathBuf {
    // A stable, filesystem-safe name derived from the URL. Collisions are harmless:
    // two lists sharing a digest would also share their content.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in url.as_bytes() {
      hash ^= *byte as u64;
      hash = hash.wrapping_mul(0x100000001b3);
    }
    self.cache_dir.join(format!("{hash:016x}.txt"))
  }

  fn age_secs(&self, url: &str) -> Option<u64> {
    let path = self.path_for(url);
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now_secs().saturating_sub(secs))
  }
}

enum RefreshError {
  /// Another refresh already happened very recently; skip.
  Fresh(String),
  Failed(String, String),
}

fn refresh_one(list: &FilterList, path: &PathBuf, now: u64) -> std::result::Result<String, RefreshError> {
  let name = list.name.clone();

  if let Ok(meta) = std::fs::metadata(path) {
    if let Ok(modified) = meta.modified() {
      let secs = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
      if now.saturating_sub(secs) < MIN_REFRESH_INTERVAL_SECS {
        return Err(RefreshError::Fresh(name));
      }
    }
  }

  let text = fetch::get_text(&list.url, fetch::DEFAULT_TIMEOUT, fetch::DEFAULT_MAX_BYTES)
    .map_err(|e| RefreshError::Failed(name.clone(), e.to_string()))?;

  if text.trim().is_empty() {
    return Err(RefreshError::Failed(name, "list was empty".into()));
  }

  if let Some(parent) = path.parent() {
    let _ = std::fs::create_dir_all(parent);
  }
  bir_core::store::write_atomic(path, text.as_bytes())
    .map_err(|e| RefreshError::Failed(name.clone(), e.to_string()))?;

  Ok(name)
}

fn now_secs() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0)
}

/// Built-in lists keyed by name — used by the settings UI so users can re-enable a
/// built-in list they previously turned off.
pub fn builtin_names() -> HashMap<&'static str, &'static str> {
  let mut map = HashMap::new();
  map.insert("BIR baseline ads", BUILTIN_ADS);
  map.insert("BIR baseline trackers", BUILTIN_TRACKERS);
  map
}
