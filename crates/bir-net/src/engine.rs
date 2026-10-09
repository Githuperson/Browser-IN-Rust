//! The matcher: turns a URL into a block/allow decision, and a host into CSS.
//!
//! # Why tokens
//!
//! A modern filter list has 50k–100k rules. Testing every rule against every URL is
//! obviously too slow, so rules are bucketed by a short "token" — the longest
//! distinctive alphanumeric run in the pattern (`||googleadservices.com^` →
//! `googleadservices`). A URL can only match a rule if one of the rule's tokens occurs
//! in it, so a lookup touches a handful of rules instead of the whole list. This is the
//! same idea uBlock Origin and Brave use, with a simpler token choice.

use std::{
  collections::{HashMap, HashSet},
  sync::atomic::{AtomicU64, Ordering},
};

use bir_core::url::registrable_domain;

use crate::rules::{pattern_matches, token_for, url_tokens, ContentType, RuleSet};

/// What kind of request is being checked.
pub type RequestKind = ContentType;

/// Upper bound on hosts shipped to the in-page blocker script.
///
/// ~4k short domains is around 70 KB, which is the point where injecting the blob on
/// every navigation starts to be noticeable on a slow machine.
pub const MAX_BLOB_HOSTS: usize = 4_000;

/// Upper bound on non-host patterns shipped to the in-page blocker script.
pub const MAX_BLOB_PATTERNS: usize = 1_500;

/// Cap on generated element-hiding CSS, so one broken list cannot bloat every page.
const MAX_CSS_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockDecision {
  Allow,
  Blocked { rule: String },
}

impl BlockDecision {
  pub fn is_blocked(&self) -> bool {
    matches!(self, BlockDecision::Blocked { .. })
  }
}

#[derive(Debug, Default)]
pub struct BlockerStats {
  pub network_rules: usize,
  pub cosmetic_rules: usize,
  pub skipped_rules: usize,
  checks: AtomicU64,
  blocks: AtomicU64,
  candidates: AtomicU64,
}

impl BlockerStats {
  pub fn checks(&self) -> u64 {
    self.checks.load(Ordering::Relaxed)
  }
  pub fn blocks(&self) -> u64 {
    self.blocks.load(Ordering::Relaxed)
  }
  /// Average number of rules actually tested per check. The whole point of token
  /// bucketing is to keep this in the low tens rather than the tens of thousands.
  pub fn average_candidates(&self) -> f64 {
    let checks = self.checks();
    if checks == 0 {
      return 0.0;
    }
    self.candidates.load(Ordering::Relaxed) as f64 / checks as f64
  }
  fn record(&self, candidates: usize) {
    self.checks.fetch_add(1, Ordering::Relaxed);
    self.candidates.fetch_add(candidates as u64, Ordering::Relaxed);
  }
  fn record_block(&self) {
    self.blocks.fetch_add(1, Ordering::Relaxed);
  }
}

pub struct Blocker {
  rules: RuleSet,
  /// token → indices into `rules.network`
  by_token: HashMap<String, Vec<u32>>,
  /// Rules with no usable token; always tested.
  untokenized: Vec<u32>,
  /// host (or eTLD+1) → indices into `rules.cosmetic`
  cosmetic_by_domain: HashMap<String, Vec<u32>>,
  cosmetic_global: Vec<u32>,
  host_blob: String,
  pattern_blob: String,
  pub stats: BlockerStats,
}

impl Blocker {
  /// Build the matcher. `O(rules)` once at startup; `check` afterwards is near-O(1).
  pub fn new(rules: RuleSet) -> Self {
    let mut by_token: HashMap<String, Vec<u32>> = HashMap::new();
    let mut untokenized: Vec<u32> = Vec::new();

    for (index, rule) in rules.network.iter().enumerate() {
      let index = index as u32;
      match token_for(&rule.pattern) {
        Some(token) => by_token.entry(token).or_default().push(index),
        None => untokenized.push(index),
      }
    }

    let mut cosmetic_by_domain: HashMap<String, Vec<u32>> = HashMap::new();
    let mut cosmetic_global: Vec<u32> = Vec::new();
    for (index, rule) in rules.cosmetic.iter().enumerate() {
      let index = index as u32;
      if rule.domains.is_empty() {
        cosmetic_global.push(index);
        continue;
      }
      for domain in &rule.domains {
        cosmetic_by_domain
          .entry(domain.clone())
          .or_default()
          .push(index);
      }
    }

    let stats = BlockerStats {
      network_rules: rules.network.len(),
      cosmetic_rules: rules.cosmetic.len(),
      skipped_rules: rules.skipped,
      ..Default::default()
    };

    let host_blob = build_host_blob(&rules);
    let pattern_blob = build_pattern_blob(&rules);

    Self {
      rules,
      by_token,
      untokenized,
      cosmetic_by_domain,
      cosmetic_global,
      host_blob,
      pattern_blob,
      stats,
    }
  }

  /// Decide whether `url` should be blocked.
  ///
  /// `source_host` is the host of the page making the request; `None` means
  /// first-party/unknown. Pass `Document` as `kind` for a top-level navigation.
  pub fn check(
    &self,
    url: &str,
    source_host: Option<&str>,
    kind: RequestKind,
  ) -> BlockDecision {
    let url_lc = url.to_ascii_lowercase();
    let host = host_of(&url_lc);
    let domain = registrable_domain(host);

    let third_party = match source_host {
      Some(source) => registrable_domain(&source.to_ascii_lowercase()) != domain,
      None => false,
    };

    let mut candidate_ids: Vec<u32> = Vec::with_capacity(16);
    for token in url_tokens(&url_lc) {
      if let Some(ids) = self.by_token.get(&token) {
        candidate_ids.extend_from_slice(ids);
      }
    }
    candidate_ids.extend_from_slice(&self.untokenized);
    candidate_ids.sort_unstable();
    candidate_ids.dedup();

    self.stats.record(candidate_ids.len());

    let mut blocked: Option<&crate::rules::NetworkRule> = None;
    let mut exception: Option<&crate::rules::NetworkRule> = None;

    for id in candidate_ids {
      let rule = &self.rules.network[id as usize];
      if !rule.options.matches_type(kind) {
        continue;
      }
      if let Some(wants_third_party) = rule.options.third_party {
        if wants_third_party != third_party {
          continue;
        }
      }
      if !rule.options.matches_domain(host, domain) {
        continue;
      }
      if !pattern_matches(rule, &url_lc) {
        continue;
      }

      if rule.allow {
        exception = Some(rule);
      } else {
        blocked = Some(rule);
        if rule.options.important {
          break;
        }
      }
    }

    match (blocked, exception) {
      (Some(rule), None) => {
        self.stats.record_block();
        BlockDecision::Blocked { rule: rule.raw.clone() }
      }
      (Some(rule), Some(_)) if rule.options.important => {
        self.stats.record_block();
        BlockDecision::Blocked { rule: rule.raw.clone() }
      }
      _ => BlockDecision::Allow,
    }
  }

  /// Convenience wrapper for top-level navigations.
  pub fn check_navigation(&self, url: &str) -> BlockDecision {
    self.check(url, None, RequestKind::Document)
  }

  /// Element-hiding CSS for `host`, ready to be dropped into a `<style>` element.
  pub fn cosmetic_css(&self, host: &str) -> String {
    let host_lc = host.to_ascii_lowercase();
    let domain = registrable_domain(&host_lc).to_string();

    let mut ids: Vec<u32> = Vec::new();
    if let Some(rules) = self.cosmetic_by_domain.get(&host_lc) {
      ids.extend_from_slice(rules);
    }
    if domain != host_lc {
      if let Some(rules) = self.cosmetic_by_domain.get(&domain) {
        ids.extend_from_slice(rules);
      }
    }
    ids.extend_from_slice(&self.cosmetic_global);

    let mut exceptions: HashSet<&str> = HashSet::new();
    for &id in &ids {
      let rule = &self.rules.cosmetic[id as usize];
      if rule.exception {
        exceptions.insert(rule.selector.as_str());
      }
    }

    let mut out = String::with_capacity(1024);
    let mut emitted: HashSet<&str> = HashSet::new();
    for &id in &ids {
      let rule = &self.rules.cosmetic[id as usize];
      if rule.exception || exceptions.contains(rule.selector.as_str()) {
        continue;
      }
      if !emitted.insert(rule.selector.as_str()) {
        continue;
      }
      if out.len() + rule.selector.len() + 32 > MAX_CSS_BYTES {
        break;
      }
      if !out.is_empty() {
        out.push(',');
        out.push('\n');
      }
      out.push_str(&rule.selector);
    }

    if out.is_empty() {
      String::new()
    } else {
      // `!important` because these are user-intent rules: a page's own stylesheet must
      // never be able to reveal what the user chose to hide.
      format!("{out}{{display:none !important;}}")
    }
  }

  /// Newline-separated bare hosts the in-page blocker should reject.
  pub fn blocked_hosts_blob(&self) -> &str {
    &self.host_blob
  }

  /// Newline-separated non-host patterns for the in-page blocker.
  pub fn blocked_patterns_blob(&self) -> &str {
    &self.pattern_blob
  }

  pub fn is_empty(&self) -> bool {
    self.rules.network.is_empty() && self.rules.cosmetic.is_empty()
  }
}

/// Extract the host from a URL without a full parse (this runs per request).
fn host_of(url: &str) -> &str {
  let rest = match url.split_once("://") {
    Some((_scheme, rest)) => rest,
    None => url,
  };
  let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
  let authority = match authority.rsplit_once('@') {
    Some((_userinfo, host)) => host,
    None => authority,
  };
  // Strip the port, but not inside an IPv6 literal.
  if authority.starts_with('[') {
    authority.split('[').nth(1).and_then(|a| a.split(']').next()).unwrap_or(authority)
  } else {
    authority.split(':').next().unwrap_or(authority)
  }
}

fn is_bare_host(pattern: &str) -> bool {
  if pattern.is_empty() || pattern.len() > 253 {
    return false;
  }
  !pattern
    .contains(|c: char| matches!(c, '/' | '*' | '?' | '=' | ':' | '^' | ' ' | '(' | ')'))
    && pattern.contains('.')
    && pattern
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Collect every block rule that is a plain host into a deduped, length-sorted blob.
fn build_host_blob(rules: &RuleSet) -> String {
  let mut hosts: Vec<&str> = rules
    .network
    .iter()
    .filter(|r| !r.allow && is_bare_host(&r.pattern))
    .map(|r| r.pattern.as_str())
    .collect();
  hosts.sort_unstable_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));

  let mut seen: HashSet<&str> = HashSet::new();
  let mut out = String::with_capacity(hosts.len() * 16);
  for host in hosts {
    if seen.insert(host) {
      if !out.is_empty() {
        out.push('\n');
      }
      out.push_str(host);
      if seen.len() >= MAX_BLOB_HOSTS {
        break;
      }
    }
  }
  out
}

/// Everything that is not a bare host: substring patterns, shortest first.
fn build_pattern_blob(rules: &RuleSet) -> String {
  let mut patterns: Vec<&str> = rules
    .network
    .iter()
    .filter(|r| !r.allow && !is_bare_host(&r.pattern))
    .map(|r| r.pattern.as_str())
    .collect();
  patterns.sort_unstable_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));

  let mut seen: HashSet<&str> = HashSet::new();
  let mut out = String::with_capacity(patterns.len() * 24);
  for pattern in patterns {
    if seen.insert(pattern) {
      if !out.is_empty() {
        out.push('\n');
      }
      out.push_str(pattern);
      if seen.len() >= MAX_BLOB_PATTERNS {
        break;
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::rules::RuleSet;

  fn blocker() -> Blocker {
    let set = RuleSet::parse(
      "||doubleclick.net^\n\
       ||google-analytics.com^\n\
       @@||example.com^$document\n\
       ||tracker.example.net^\n\
       example.com##.advert\n\
       ##.global-ad\n",
    );
    Blocker::new(set)
  }

  #[test]
  fn blocks_known_trackers() {
    let b = blocker();
    assert!(b
      .check("https://googleads.g.doubleclick.net/pagead/ads", None, RequestKind::Script)
      .is_blocked());
    assert!(!b
      .check("https://example.com/article", None, RequestKind::Document)
      .is_blocked());
  }

  #[test]
  fn third_party_detection() {
    let b = blocker();
    // Same registrable domain → first party → not "third-party", but this rule has no
    // third-party option so it still blocks.
    assert!(b
      .check("https://tracker.example.net/x", Some("www.example.net"), RequestKind::Script)
      .is_blocked());
  }

  #[test]
  fn exceptions_win() {
    let set = RuleSet::parse("||ads.example.com^\n@@||ads.example.com/allowed^\n");
    let b = Blocker::new(set);
    assert!(b
      .check("https://ads.example.com/banner", None, RequestKind::Image)
      .is_blocked());
    assert!(!b
      .check("https://ads.example.com/allowed/x", None, RequestKind::Image)
      .is_blocked());
  }

  #[test]
  fn cosmetic_css_is_generated() {
    let b = blocker();
    let css = b.cosmetic_css("www.example.com");
    assert!(css.contains(".advert"));
    assert!(css.contains(".global-ad"));
    assert!(css.ends_with("{display:none !important;}"));
  }

  #[test]
  fn host_extraction() {
    assert_eq!(host_of("https://Example.com:8443/a/b?c=d"), "example.com");
    assert_eq!(host_of("https://user:pw@example.com/x"), "example.com");
    assert_eq!(host_of("https://[::1]:8080/x"), "::1");
  }

  #[test]
  fn blobs_are_bounded() {
    let mut set = RuleSet::new();
    for i in 0..10_000 {
      set.push(&format!("||ad{i}.tracker{i % 7}.com^"));
    }
    let b = Blocker::new(set);
    assert!(b.blocked_hosts_blob().lines().count() <= MAX_BLOB_HOSTS);
  }
}
