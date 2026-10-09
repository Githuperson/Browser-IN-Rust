//! Adblock-style rule parsing.
//!
//! Supported syntax (the subset that covers essentially all real-world lists):
//!
//! | syntax                | meaning                                        |
//! |-----------------------|------------------------------------------------|
//! | `\|\|example.com^`     | domain-anchored block                          |
//! | `\|\|example.com/ads/` | domain-anchored with a path                    |
//! | `\|\|example.com^$script` | restricted to one content type              |
//! | `@@\|\|example.com^`   | exception (wins over blocks unless `important`) |
//! | `example.com##.ad`     | element hiding (cosmetic)                      |
//! | `example.com#@#.ad`    | element-hiding exception                       |
//! | `##.ad`                | global element hiding                          |
//! | `! comment`            | ignored                                        |
//!
//! Deliberately **not** supported: `/regex/` rules, `$csp`/`$redirect`/`$removeheader`
//! and `##+js()` scriptlets. Regular-expression rules are rare in the lists people
//! actually enable and would force a regex engine into the binary; scriptlets are
//! executable content from a third party, which we will not run.

use std::collections::HashSet;

/// Content types a network rule can be restricted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
  Document,
  Script,
  Image,
  Stylesheet,
  Object,
  XmlHttpRequest,
  SubDocument,
  Font,
  Media,
  WebSocket,
  Ping,
  Beacon,
  Other,
}

impl ContentType {
  pub const ALL: &'static [ContentType] = &[
    ContentType::Document,
    ContentType::Script,
    ContentType::Image,
    ContentType::Stylesheet,
    ContentType::Object,
    ContentType::XmlHttpRequest,
    ContentType::SubDocument,
    ContentType::Font,
    ContentType::Media,
    ContentType::WebSocket,
    ContentType::Ping,
    ContentType::Beacon,
    ContentType::Other,
  ];

  fn bit(self) -> u32 {
    1 << (self as u32)
  }

  fn from_str(name: &str) -> Option<Self> {
    Some(match name {
      "document" | "doc" | "frame" | "main_frame" => ContentType::Document,
      "script" | "js" => ContentType::Script,
      "image" | "images" => ContentType::Image,
      "stylesheet" | "css" => ContentType::Stylesheet,
      "object" | "object-subrequest" => ContentType::Object,
      "xmlhttprequest" | "xhr" => ContentType::XmlHttpRequest,
      "subdocument" | "sub_frame" => ContentType::SubDocument,
      "font" => ContentType::Font,
      "media" => ContentType::Media,
      "websocket" => ContentType::WebSocket,
      "ping" => ContentType::Ping,
      "beacon" => ContentType::Beacon,
      "other" => ContentType::Other,
      _ => return None,
    })
  }
}

/// Parsed options from the `$...` suffix of a rule.
#[derive(Debug, Clone, Default)]
pub struct Options {
  /// `Some(true)` = `$third-party`, `Some(false)` = `$~third-party`, `None` = any.
  pub third_party: Option<bool>,
  /// Bitmask of [`ContentType`]; `0` means "all types".
  pub types: u32,
  /// `domain=` includes. Empty when the rule applies everywhere.
  pub domains: Vec<String>,
  /// `domain=~example.com` excludes.
  pub excluded_domains: Vec<String>,
  /// `$important` — an exception rule cannot override it.
  pub important: bool,
  /// `$match-case` — the pattern is matched case-sensitively.
  pub match_case: bool,
}

impl Options {
  /// Does this rule apply to `kind`?
  pub fn matches_type(&self, kind: ContentType) -> bool {
    self.types == 0 || (self.types & kind.bit()) != 0
  }

  /// Does this rule apply to `host` (the *page* host, i.e. the first-party)?
  pub fn matches_domain(&self, host: &str, domain: &str) -> bool {
    if self.excluded_domains.iter().any(|d| host_matches(d, host)) {
      return false;
    }
    if self.domains.is_empty() {
      return true;
    }
    self.domains.iter().any(|d| host_matches(d, host) || host_matches(d, domain))
  }
}

/// `example.com` matches `example.com` and `www.example.com`; a bare `com` matches
/// nothing (it is not a registrable domain).
fn host_matches(rule_host: &str, host: &str) -> bool {
  if rule_host.is_empty() || host.is_empty() {
    return false;
  }
  host == rule_host || host.ends_with(&format!(".{rule_host}"))
}

/// A rule that can block or allow a network request.
#[derive(Debug, Clone)]
pub struct NetworkRule {
  /// Pattern with anchors and the trailing `^` removed.
  pub pattern: String,
  /// `||` prefix: the pattern starts at a host boundary.
  pub anchored_domain: bool,
  /// `|` prefix: the pattern starts at the beginning of the URL.
  pub anchored_start: bool,
  /// `|` suffix: the pattern must reach the end of the URL.
  pub anchored_end: bool,
  /// The rule ended with `^`: whatever follows must be a separator or the end.
  pub require_separator_after: bool,
  /// `@@` exception rule.
  pub allow: bool,
  pub options: Options,
  /// Original text, kept for the "why was this blocked" UI.
  pub raw: String,
}

/// A rule that hides elements.
#[derive(Debug, Clone)]
pub struct CosmeticRule {
  /// Hosts the rule applies to; empty means "every site".
  pub domains: Vec<String>,
  /// `~host` entries.
  pub excluded: Vec<String>,
  pub selector: String,
  /// `#@#` — un-hides elements hidden by another rule.
  pub exception: bool,
}

/// Everything parsed out of one or more filter lists.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
  pub network: Vec<NetworkRule>,
  pub cosmetic: Vec<CosmeticRule>,
  /// Rules we could not understand, counted so the settings page can be honest about
  /// coverage instead of silently ignoring them.
  pub skipped: usize,
}

impl RuleSet {
  pub fn new() -> Self {
    Self::default()
  }

  /// Parse a filter list. One rule per line; blank lines and comments are ignored.
  pub fn parse(text: &str) -> Self {
    let mut set = Self::new();
    set.extend(text);
    set
  }

  pub fn extend(&mut self, text: &str) {
    for line in text.lines() {
      self.push(line);
    }
  }

  pub fn push(&mut self, line: &str) {
    let line = line.trim();
    if line.is_empty()
      || line.starts_with('!')
      || line.starts_with('[')
      || line.starts_with("# ")
    {
      return;
    }

    // Cosmetic rules: the separator is the earliest of ##, #@#, #?# (procedural,
    // unsupported) and #$# (style injection, unsupported).
    let separators = ["##", "#@#", "#?#", "#$#", "#%#"];
    let mut best: Option<(usize, &str)> = None;
    for sep in separators {
      if let Some(idx) = line.find(sep) {
        if best.map(|(b, _)| idx < b).unwrap_or(true) {
          best = Some((idx, sep));
        }
      }
    }

    if let Some((idx, sep)) = best {
      let domains_part = &line[..idx];
      let selector = &line[idx + sep.len()..];
      if selector.is_empty() {
        self.skipped += 1;
        return;
      }
      // Procedural / scriptlet / style rules: counted, not applied.
      if matches!(sep, "#?#" | "#$#" | "#%#") || selector.starts_with("+js(") {
        self.skipped += 1;
        return;
      }
      let (mut domains, excluded) = split_domain_list(domains_part);
      if domains.iter().any(|d| d.contains('*')) {
        // Wildcard domain parts are rare; treat them as global to stay safe rather
        // than accidentally matching everything.
        domains.retain(|d| !d.contains('*'));
      }
      self.cosmetic.push(CosmeticRule {
        domains,
        excluded,
        selector: selector.to_string(),
        exception: sep == "#@#",
      });
      return;
    }

    match parse_network(line) {
      Some(rule) => self.network.push(rule),
      None => self.skipped += 1,
    }
  }

  pub fn network_count(&self) -> usize {
    self.network.len()
  }

  pub fn cosmetic_count(&self) -> usize {
    self.cosmetic.len()
  }
}

/// Parse a single network rule. Returns `None` for anything unusable.
fn parse_network(line: &str) -> Option<NetworkRule> {
  let mut rest = line.trim();

  let allow = if let Some(stripped) = rest.strip_prefix("@@") {
    rest = stripped;
    true
  } else {
    false
  };

  // Split off options at the last `$`. A `$` is only an options separator when what
  // follows it looks like options — patterns legitimately contain `$`.
  let mut options = Options::default();
  if let Some(idx) = rest.rfind('$') {
    let candidate = &rest[idx + 1..];
    if looks_like_options(candidate) {
      options = parse_options(candidate);
      rest = &rest[..idx];
    }
  }

  if rest.is_empty() {
    return None;
  }

  let anchored_domain = rest.starts_with("||");
  if anchored_domain {
    rest = &rest[2..];
  } else if rest.starts_with('|') {
    rest = &rest[1..];
  }

  let mut anchored_end = false;
  let mut require_separator_after = false;
  if rest.ends_with('^') {
    require_separator_after = true;
    rest = &rest[..rest.len() - 1];
  } else if rest.ends_with('|') {
    anchored_end = true;
    rest = &rest[..rest.len() - 1];
  }

  let pattern = rest.trim().to_ascii_lowercase();
  if pattern.is_empty() {
    return None;
  }

  Some(NetworkRule {
    pattern,
    anchored_domain,
    anchored_start: line.starts_with('|') && !anchored_domain,
    anchored_end,
    require_separator_after,
    allow,
    options,
    raw: line.to_string(),
  })
}

/// Option names we recognise. Anything else makes the `$...` suffix be treated as part
/// of the pattern rather than as options.
const KNOWN_OPTIONS: &[&str] = &[
  "third-party",
  "domain",
  "important",
  "match-case",
  "collapse",
  "elemhide",
  "generichide",
  "popup",
  "document",
  "doc",
  "script",
  "js",
  "image",
  "images",
  "stylesheet",
  "css",
  "object",
  "object-subrequest",
  "xmlhttprequest",
  "xhr",
  "subdocument",
  "sub_frame",
  "font",
  "media",
  "websocket",
  "ping",
  "beacon",
  "other",
  "all",
  "frame",
  "main_frame",
  "empty",
  "mp4",
  "inline-script",
  "badfilter",
  "csp",
  "redirect",
  "network",
];

fn looks_like_options(candidate: &str) -> bool {
  if candidate.is_empty() {
    return false;
  }
  candidate
    .split(',')
    .all(|part| {
      let name = part.trim().trim_start_matches('~').split('=').next().unwrap_or("");
      KNOWN_OPTIONS.contains(&name)
    })
}

fn parse_options(raw: &str) -> Options {
  let mut options = Options::default();
  for part in raw.split(',') {
    let part = part.trim();
    if part.is_empty() {
      continue;
    }
    let negated = part.starts_with('~');
    let part = part.trim_start_matches('~');

    if let Some(value) = part.strip_prefix("domain=") {
      let (include, exclude) = split_domain_list(value);
      options.domains = include;
      options.excluded_domains = exclude;
      continue;
    }

    if let Some(kind) = ContentType::from_str(part) {
      if !negated {
        options.types |= kind.bit();
      }
      continue;
    }

    match part {
      "third-party" => options.third_party = Some(!negated),
      "important" => options.important = !negated,
      "match-case" => options.match_case = !negated,
      _ => {}
    }
  }
  options
}

/// Split `a.com|b.com|~c.com` into includes and excludes.
fn split_domain_list(raw: &str) -> (Vec<String>, Vec<String>) {
  let mut include = Vec::new();
  let mut exclude = Vec::new();
  for entry in raw.split('|') {
    let entry = entry.trim().to_ascii_lowercase();
    if entry.is_empty() {
      continue;
    }
    if let Some(domain) = entry.strip_prefix('~') {
      if !domain.is_empty() {
        exclude.push(domain.to_string());
      }
    } else {
      include.push(entry);
    }
  }
  (include, exclude)
}

/// A separator, in ABP syntax: anything that is not a letter, digit, `_`, `-`, `.` or
/// `%`. `^` in a rule matches a separator or the end of the URL.
fn is_separator(c: char) -> bool {
  !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '%')
}

/// Match a pattern against a URL, honouring `*`, `^` and the anchors parsed above.
///
/// Uses the classic star-backtracking algorithm: linear for the common case (no `*`),
/// and bounded by `pattern.len() * url.len()` in the worst case, which for filter-rule
/// sized inputs is microseconds.
pub fn pattern_matches(rule: &NetworkRule, url: &str) -> bool {
  let pattern = &rule.pattern;
  let haystack = if rule.options.match_case {
    url.to_string()
  } else {
    url.to_ascii_lowercase()
  };
  let haystack: &str = haystack.as_ref();

  // Fast path: no wildcards at all.
  if !pattern.contains('*') && !pattern.contains('^') {
    return match find_at(&haystack, pattern, rule) {
      Some(end) => check_tail(&haystack, end, rule),
      None => false,
    };
  }

  // General path with `*` and `^`.
  let pattern_chars: Vec<char> = pattern.chars().collect();
  let url_chars: Vec<char> = haystack.chars().collect();
  let mut pi = 0usize;
  let mut ui = 0usize;
  let mut star_pi: Option<usize> = None;
  let mut star_ui = 0usize;

  while ui < url_chars.len() {
    if pi < pattern_chars.len() && pattern_chars[pi] == '*' {
      star_pi = Some(pi);
      star_ui = ui;
      pi += 1;
    } else if pi < pattern_chars.len() && single_char_matches(pattern_chars[pi], url_chars[ui]) {
      pi += 1;
      ui += 1;
    } else if let Some(star) = star_pi {
      pi = star + 1;
      star_ui += 1;
      ui = star_ui;
    } else {
      return false;
    }
  }
  while pi < pattern_chars.len() && pattern_chars[pi] == '*' {
    pi += 1;
  }
  if pi != pattern_chars.len() {
    // A trailing `^` may legitimately match end-of-url.
    if pi + 1 == pattern_chars.len() && pattern_chars[pi] == '^' {
      return true;
    }
    return false;
  }
  true
}

fn single_char_matches(pattern_char: char, url_char: char) -> bool {
  if pattern_char == '^' {
    return is_separator(url_char);
  }
  pattern_char == url_char
}

/// Locate `needle` in `haystack`, respecting the rule's start anchors.
fn find_at(haystack: &str, needle: &str, rule: &NetworkRule) -> Option<usize> {
  if needle.is_empty() {
    return Some(0);
  }
  if rule.anchored_start {
    return if haystack.starts_with(needle) {
      Some(needle.len())
    } else {
      None
    };
  }

  let mut from = 0usize;
  loop {
    let idx = haystack[from..].find(needle)?;
    let absolute = from + idx;
    if rule.anchored_domain && !is_host_boundary(haystack, absolute) {
      from = absolute + 1;
      if from >= haystack.len() {
        return None;
      }
      continue;
    }
    return Some(absolute + needle.len());
  }
}

/// `||example.com^` must start at a host boundary: the start of the string, or right
/// after `://`, `/`, `.`, `?`, `=`, `&`, `:`.
fn is_host_boundary(haystack: &str, index: usize) -> bool {
  if index == 0 {
    return true;
  }
  let bytes = haystack.as_bytes();
  let prev = bytes[index - 1] as char;
  if prev.is_ascii_alphanumeric() || prev == '-' || prev == '_' || prev == '%' {
    return false;
  }
  true
}

/// Check whatever follows the match: `^` requires a separator or end, `|` requires end.
fn check_tail(haystack: &str, end: usize, rule: &NetworkRule) -> bool {
  if rule.anchored_end {
    return end == haystack.len();
  }
  if rule.require_separator_after {
    if end == haystack.len() {
      return true;
    }
    let next = haystack.as_bytes()[end] as char;
    return is_separator(next);
  }
  true
}

/// Tokens that appear in nearly every URL and therefore make useless bucket keys.
const STOP_TOKENS: &[&str] = &[
  "http", "https", "www", "index", "html", "htm", "php", "aspx", "json", "css", "min",
  "static", "assets", "content", "upload", "uploads", "files", "file", "image", "images",
  "img", "png", "jpg", "jpeg", "gif", "svg", "webp", "woff", "woff2", "ajax", "query",
  "page", "views", "main", "default", "share", "video", "videos", "media", "webkit",
  "mozilla", "chrome", "safari", "android", "iphone", "mobile", "utf", "amp", "v1", "v2",
];

/// Choose the bucket key for a rule: the longest useful alphanumeric run in its
/// pattern, so that a lookup only ever considers rules whose token actually occurs in
/// the URL being tested.
pub fn token_for(pattern: &str) -> Option<String> {
  let mut best: Option<&str> = None;
  for candidate in pattern.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
    let candidate = candidate.trim_matches('-');
    if candidate.len() < 5 || candidate.len() > 32 {
      continue;
    }
    if STOP_TOKENS.contains(&candidate.to_ascii_lowercase().as_str()) {
      continue;
    }
    match best {
      Some(current) if current.len() >= candidate.len() => {}
      _ => best = Some(candidate),
    }
  }
  best.map(|t| t.to_ascii_lowercase())
}

/// Every candidate token in a URL, so the matcher can look up the right buckets.
pub fn url_tokens(url: &str) -> Vec<String> {
  let mut seen = HashSet::new();
  let mut out = Vec::new();
  for candidate in url.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
    let candidate = candidate.trim_matches('-').to_ascii_lowercase();
    if candidate.len() < 5 || candidate.len() > 32 {
      continue;
    }
    if STOP_TOKENS.contains(&candidate.as_str()) {
      continue;
    }
    if seen.insert(candidate.clone()) {
      out.push(candidate);
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_domain_anchored_rules() {
    let rule = parse_network("||doubleclick.net^").unwrap();
    assert!(rule.anchored_domain);
    assert!(rule.require_separator_after);
    assert_eq!(rule.pattern, "doubleclick.net");

    assert!(pattern_matches(&rule, "https://googleads.g.doubleclick.net/pagead/ads"));
    assert!(!pattern_matches(&rule, "https://notdoubleclick.net.evil.com/x"));
    assert!(!pattern_matches(&rule, "https://foo-doubleclick.net/x"));
  }

  #[test]
  fn parses_options() {
    let rule = parse_network("||example.com^$script,third-party").unwrap();
    assert!(rule.options.matches_type(ContentType::Script));
    assert!(!rule.options.matches_type(ContentType::Image));
    assert_eq!(rule.options.third_party, Some(true));

    let rule = parse_network("||example.com^$domain=a.com|~b.com").unwrap();
    assert_eq!(rule.options.domains, vec!["a.com".to_string()]);
    assert_eq!(rule.options.excluded_domains, vec!["b.com".to_string()]);
  }

  #[test]
  fn pattern_with_dollar_is_not_options() {
    // `$` inside the pattern must not be read as an options separator.
    let rule = parse_network("||example.com/promo$50").unwrap();
    assert_eq!(rule.pattern, "example.com/promo$50".to_ascii_lowercase());
  }

  #[test]
  fn wildcards_and_separators() {
    // `*` matches any run of characters, including `/`, exactly as in ABP syntax.
    let rule = parse_network("||example.com/ads^*/track").unwrap();
    assert!(pattern_matches(&rule, "https://example.com/ads/x/y/track"));
    assert!(!pattern_matches(&rule, "https://example.com/ads/x/y/other"));
    assert!(!pattern_matches(&rule, "https://other.example/ads/x/y/track"));
  }

  #[test]
  fn separator_requires_a_boundary() {
    let rule = parse_network("||example.com^").unwrap();
    // `^` means "separator or end", so a longer host must not match.
    assert!(pattern_matches(&rule, "https://example.com/"));
    assert!(!pattern_matches(&rule, "https://example.community/page"));
    assert!(pattern_matches(&rule, "https://ads.example.com/x"));
  }

  #[test]
  fn cosmetic_rules() {
    let set = RuleSet::parse("example.com##.advert\nexample.com#@#.advert\n##div[id^=\"ad-\"]");
    assert_eq!(set.cosmetic.len(), 3);
    assert!(set.cosmetic[0].domains.contains(&"example.com".to_string()));
    assert!(set.cosmetic[1].exception);
    assert!(set.cosmetic[2].domains.is_empty());
  }
}
