//! Browsing history.
//!
//! Storage is append-only NDJSON with in-memory dedupe by URL. That keeps the common
//! case (visiting a page) at O(1) instead of rewriting a multi-megabyte document, while
//! still allowing compaction whenever the file or the entry cap gets large.

use crate::{store::LineStore, time, url::UrlInfo, Error, Persistent, ProfilePaths, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Upper bound on entries we will hold in memory / on disk.
///
/// 100k visits is roughly 10 MB of NDJSON and covers years of typical use. Going past
/// it is what makes a browser's history page feel like a spreadsheet.
pub const DEFAULT_HISTORY_CAP: usize = 100_000;

/// Rewrite the backing file once it grows past this, even if the entry cap is fine.
const COMPACT_BYTES: u64 = 12 * 1024 * 1024;

/// Rewrite once this many visits are pending, so a crash loses at most this much.
const MAX_PENDING: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
  pub url: String,
  pub title: String,
  /// Seconds since epoch of the most recent visit.
  pub visit_at: u64,
  pub visit_count: u32,
  /// Incremented when the user typed the URL rather than clicking a link; used to
  /// boost omnibox ranking.
  pub typed_count: u32,
  /// eTLD+1, pre-computed so the history page can group without parsing.
  pub domain: String,
}

impl HistoryEntry {
  pub fn new(url: &str, title: &str) -> Self {
    let domain = UrlInfo::parse(url)
      .map(|i| i.registrable_domain)
      .unwrap_or_default();
    Self {
      url: url.to_string(),
      title: title.to_string(),
      visit_at: time::now_secs(),
      visit_count: 1,
      typed_count: 0,
      domain,
    }
  }
}

pub struct HistoryStore {
  /// Newest last.
  entries: Vec<HistoryEntry>,
  /// url → index into `entries`.
  index: HashMap<String, usize>,
  /// Visits that have not reached disk yet.
  pending: Vec<HistoryEntry>,
  needs_rewrite: bool,
  cap: usize,
}

impl HistoryStore {
  pub fn new() -> Self {
    Self {
      entries: Vec::new(),
      index: HashMap::new(),
      pending: Vec::new(),
      needs_rewrite: false,
      cap: DEFAULT_HISTORY_CAP,
    }
  }

  /// Load from the profile, keeping the newest `cap` entries.
  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let mut store = Self::new();
    let lines = LineStore::new(paths.data_dir().join(Self::FILE));
    let mut seen: HashMap<String, usize> = HashMap::new();
    lines.read_all(|line| {
      if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
        if let Some(&idx) = seen.get(&entry.url) {
          // Later lines are newer: merge counts, keep the newest timestamp.
          let existing = &mut store.entries[idx];
          existing.visit_count += entry.visit_count;
          existing.typed_count += entry.typed_count;
          if entry.visit_at > existing.visit_at {
            existing.visit_at = entry.visit_at;
            if !entry.title.is_empty() {
              existing.title = entry.title;
            }
          }
        } else {
          seen.insert(entry.url.clone(), store.entries.len());
          store.entries.push(entry);
        }
      }
    })?;

    if store.entries.len() > store.cap {
      store.compact_in_place();
      store.needs_rewrite = true;
    }
    store.reindex();
    Ok(store)
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Record a visit. `typed` should be true when the user typed the address.
  ///
  /// Internal pages and `data:` URLs are ignored: they are noise in a history view.
  pub fn record(&mut self, url: &str, title: &str, typed: bool) {
    if url.is_empty() || url.starts_with("bir://") || url.starts_with("data:") {
      return;
    }
    if let Some(&idx) = self.index.get(url) {
      let entry = &mut self.entries[idx];
      entry.visit_at = time::now_secs();
      entry.visit_count = entry.visit_count.saturating_add(1);
      if typed {
        entry.typed_count = entry.typed_count.saturating_add(1);
      }
      if !title.is_empty() {
        entry.title = title.to_string();
      }
      self.needs_rewrite = true;
      return;
    }

    let mut entry = HistoryEntry::new(url, title);
    if typed {
      entry.typed_count = 1;
    }
    self.pending.push(entry.clone());
    self.index.insert(url.to_string(), self.entries.len());
    self.entries.push(entry);

    if self.entries.len() > self.cap {
      self.compact_in_place();
    }
  }

  /// Ranked search used by the omnibox and the history page.
  ///
  /// Scoring is deliberately simple and allocation-light: exact host prefix beats a
  /// title prefix, which beats a substring anywhere, which beats recency alone.
  pub fn search(&self, query: &str, limit: usize) -> Vec<HistoryEntry> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
      let mut recent: Vec<HistoryEntry> = self
        .entries
        .iter()
        .rev()
        .take(limit)
        .cloned()
        .collect();
      recent.sort_by(|a, b| b.visit_at.cmp(&a.visit_at));
      return recent;
    }

    let mut scored: Vec<(f32, &HistoryEntry)> = self
      .entries
      .iter()
      .filter_map(|entry| {
        let url_l = entry.url.to_ascii_lowercase();
        let title_l = entry.title.to_ascii_lowercase();
        let mut score = if url_l == query {
          100.0
        } else if url_l.starts_with(&query) {
          80.0
        } else if entry.domain.starts_with(&query) {
          60.0
        } else if title_l.starts_with(&query) {
          50.0
        } else if title_l.contains(&query) || url_l.contains(&query) {
          30.0
        } else {
          return None;
        };
        // Recency and frequency nudge, both bounded so they can never outrank a
        // genuinely better textual match.
        let age_days = time::now_secs().saturating_sub(entry.visit_at) / 86_400;
        score += 10.0 / (1.0 + age_days as f32);
        score += (entry.visit_count.min(50) as f32) / 10.0;
        score += (entry.typed_count.min(10) as f32) / 2.0;
        Some((score, entry))
      })
      .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored.into_iter().map(|(_, e)| e.clone()).collect()
  }

  /// Domains with the most visits — data for the new-tab page's "top sites".
  pub fn top_domains(&self, limit: usize) -> Vec<(String, u32)> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for entry in &self.entries {
      if entry.domain.is_empty() {
        continue;
      }
      *counts.entry(entry.domain.clone()).or_default() += entry.visit_count;
    }
    let mut out: Vec<(String, u32)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.truncate(limit);
    out
  }

  pub fn delete(&mut self, urls: &[String]) {
    for url in urls {
      if let Some(&idx) = self.index.get(url) {
        self.entries[idx].url.clear(); // tombstone; removed by compaction
      }
    }
    self.compact_in_place();
    self.needs_rewrite = true;
  }

  pub fn clear(&mut self) {
    self.entries.clear();
    self.index.clear();
    self.pending.clear();
    self.needs_rewrite = true;
  }

  /// Persist. Appends pending visits when possible, rewrites when required.
  pub fn flush(&mut self, paths: &ProfilePaths) -> Result<()> {
    if !self.needs_rewrite && self.pending.is_empty() {
      return Ok(());
    }
    let lines = LineStore::new(paths.data_dir().join(Self::FILE));
    let must_rewrite = self.needs_rewrite
      || self.pending.len() >= MAX_PENDING
      || lines.len_bytes() > COMPACT_BYTES;

    if must_rewrite {
      let encoded: Vec<String> = self
        .entries
        .iter()
        .filter(|e| !e.url.is_empty())
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();
      lines.rewrite(&encoded)?;
    } else {
      for entry in self.pending.drain(..) {
        let line = serde_json::to_string(&entry)?;
        lines.append_line(&line)?;
      }
    }
    self.pending.clear();
    self.needs_rewrite = false;
    Ok(())
  }

  fn compact_in_place(&mut self) {
    self.entries.retain(|e| !e.url.is_empty());
    self.entries.sort_by(|a, b| a.visit_at.cmp(&b.visit_at));
    let overflow = self.entries.len().saturating_sub(self.cap);
    if overflow > 0 {
      self.entries.drain(..overflow);
    }
    self.reindex();
  }

  fn reindex(&mut self) {
    self.index.clear();
    for (idx, entry) in self.entries.iter().enumerate() {
      self.index.insert(entry.url.clone(), idx);
    }
  }
}

impl Default for HistoryStore {
  fn default() -> Self {
    Self::new()
  }
}

impl Persistent for HistoryStore {
  const FILE: &'static str = "history.ndjson";

  fn mark_dirty(&mut self) {
    self.needs_rewrite = true;
  }
  fn is_dirty(&self) -> bool {
    self.needs_rewrite || !self.pending.is_empty()
  }
  fn clear_dirty(&mut self) {
    self.needs_rewrite = false;
    self.pending.clear();
  }
  fn to_json(&self) -> Result<String> {
    // History never goes through the whole-document path; [`HistoryStore::flush`]
    // handles it. Provided only so the trait is total.
    serde_json::to_string(&self.entries).map_err(Error::from)
  }
}
