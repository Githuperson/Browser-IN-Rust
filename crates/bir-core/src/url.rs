//! Omnibox classification, URL normalisation and tracking-parameter stripping.
//!
//! This module is on the hot path: it runs on every keystroke in the address bar. No
//! allocations beyond the returned `String`, no regex, no sorting.

use url::Url;

/// What the user typed into the omnibox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OmniboxInput {
  /// A navigable URL (after normalisation).
  Url(Url),
  /// Free text to send to the default search engine.
  Search(String),
  /// `<keyword> <query>` where `<keyword>` matches a registered search engine shortcut.
  SearchWith { engine: String, query: String },
  /// An internal `bir://` page.
  Internal(String),
}

/// Pre-computed, display-friendly facts about a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlInfo {
  pub raw: String,
  /// Host without `www.`, if any.
  pub host: String,
  /// eTLD+1, used for per-site settings and "is this third party?" checks.
  pub registrable_domain: String,
  pub scheme: String,
  /// `true` for https / bir / about / file.
  pub is_secure: bool,
  /// Host stripped of `www.`, scheme and trailing slash for the address bar.
  pub display: String,
}

impl UrlInfo {
  pub fn parse(raw: &str) -> Option<Self> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let display = display_url(raw);
    let registrable_domain = registrable_domain(&host).to_string();
    let scheme = url.scheme().to_string();
    let is_secure = matches!(scheme.as_str(), "https" | "bir" | "about" | "file" | "data");
    Some(Self {
      raw: raw.to_string(),
      host,
      registrable_domain,
      scheme,
      is_secure,
      display,
    })
  }
}

/// Turn arbitrary user input into a URL, or decide it is a search query.
pub fn classify(input: &str, keywords: &[String]) -> OmniboxInput {
  let trimmed = input.trim();
  if trimmed.is_empty() {
    return OmniboxInput::Search(String::new());
  }

  if let Some(rest) = trimmed.strip_prefix("bir://") {
    return OmniboxInput::Internal(rest.to_string());
  }

  // A keyword search (`gh rust webview`) beats URL heuristics: nobody has a host
  // literally named `gh`, and the ambiguity is resolved by the user registering it.
  if let Some((head, tail)) = trimmed.split_once(' ') {
    let head_lower = head.to_ascii_lowercase();
    if keywords.iter().any(|k| k == &head_lower) && !tail.trim().is_empty() {
      return OmniboxInput::SearchWith {
        engine: head_lower,
        query: tail.trim().to_string(),
      };
    }
  }

  if let Some(url) = normalize(trimmed) {
    return OmniboxInput::Url(url);
  }

  OmniboxInput::Search(trimmed.to_string())
}

/// Normalise user input into an absolute URL, or `None` if it is not URL-shaped.
///
/// Accepts `example.com`, `localhost:3000`, `https://x`, `file:///tmp/a.html`,
/// `about:blank`, and IPv6 literals. Rejects `hello world`, `foo:bar` (bare scheme
/// without a host is treated as text), and anything containing whitespace.
pub fn normalize(input: &str) -> Option<Url> {
  let trimmed = input.trim();
  if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
    return None;
  }

  // Already has a scheme.
  if let Ok(url) = Url::parse(trimmed) {
    if !url.cannot_be_a_base() {
      return Some(url);
    }
    // `about:blank`, `data:...`, `mailto:...` — keep the ones a browser can render.
    if matches!(url.scheme(), "about" | "data" | "file" | "blob") {
      return Some(url);
    }
    return None;
  }

  // No scheme: only treat as a URL when it looks like a host.
  let host_part = trimmed.split('/').next().unwrap_or(trimmed);
  let host_part = host_part.split('?').next().unwrap_or(host_part);
  let host_part = host_part.split('#').next().unwrap_or(host_part);
  let host_only = host_part.rsplit('@').next().unwrap_or(host_part);
  let host_only = match host_only.rsplit_once(':') {
    Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
    _ => host_only,
  };

  if !looks_like_host(host_only) {
    return None;
  }

  Url::parse(&format!("https://{trimmed}")).ok()
}

fn looks_like_host(host: &str) -> bool {
  if host.is_empty() || host.len() > 253 {
    return false;
  }
  if host == "localhost" {
    return true;
  }
  if host.starts_with('[') {
    // IPv6 literal — accept, `Url` will do the real validation.
    return host.contains(']');
  }
  // A dotted name with a plausible TLD. Deliberately permissive about the TLD: new
  // gTLDs appear faster than any bundled list, and the failure mode (a search for a
  // string that happens to look like a domain) is cheap to recover from.
  let mut labels = host.split('.');
  let first = labels.next().unwrap_or("");
  let last = labels.last();
  if first.is_empty() {
    return false;
  }
  match last {
    Some(tld) => {
      !tld.is_empty()
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && host
          .chars()
          .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    }
    None => false,
  }
}

/// Two-label public suffixes that would otherwise be mistaken for the registrable
/// domain. Enough of them to cover the overwhelming majority of real traffic without
/// shipping the full Public Suffix List (~250 KB) into the binary.
const MULTI_LABEL_SUFFIXES: &[&str] = &[
  "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "net.uk", "sch.uk",
  "com.au", "net.au", "org.au", "edu.au", "gov.au",
  "co.nz", "net.nz", "org.nz", "govt.nz",
  "co.jp", "or.jp", "ne.jp", "ac.jp", "go.jp",
  "co.kr", "or.kr", "ne.kr", "go.kr",
  "com.br", "net.br", "org.br", "gov.br",
  "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn",
  "com.mx", "com.ar", "com.tr", "com.tw", "com.hk", "com.sg", "com.ua", "com.pl",
  "co.za", "co.in", "net.in", "org.in", "gen.in",
  "com.es", "com.pt", "com.gr", "com.vn", "com.ph", "com.my", "com.pk",
  "github.io", "gitlab.io", "pages.dev", "vercel.app", "netlify.app",
];

/// Best-effort eTLD+1 ("registrable domain") for a hostname.
///
/// This is the unit we key per-site settings, cookie policy and third-party detection
/// on, so it must be *stable* more than it must be perfect.
pub fn registrable_domain(host: &str) -> &str {
  // `to_ascii_lowercase` only maps ASCII bytes, so `lower.len() == host.len()` always
  // holds and slices computed against `lower` can be applied to `host` directly.
  let host = host.trim_end_matches('.');
  let lower = host.to_ascii_lowercase();

  for suffix in MULTI_LABEL_SUFFIXES {
    let suffix = format!(".{suffix}");
    if lower.len() > suffix.len() && lower.ends_with(&suffix) {
      let rest = &lower[..lower.len() - suffix.len()];
      let base = match rest.rsplit_once('.') {
        Some((_, last)) => last,
        None => rest,
      };
      let start = host.len() - (base.len() + suffix.len());
      return &host[start..];
    }
  }

  let labels: Vec<&str> = host.split('.').collect();
  if labels.len() <= 2 {
    return host;
  }
  let keep_from = labels.len() - 2;
  let start: usize = labels[..keep_from].iter().map(|l| l.len() + 1).sum();
  &host[start..]
}

/// Address-bar representation: no scheme, no `www.`, no trailing slash.
pub fn display_url(raw: &str) -> String {
  let Ok(url) = Url::parse(raw) else {
    return raw.to_string();
  };
  if url.scheme() == "bir" || url.scheme() == "about" {
    return raw.to_string();
  }
  let mut out = String::with_capacity(raw.len());
  let host = url.host_str().unwrap_or("");
  let host = host.strip_prefix("www.").unwrap_or(host);
  out.push_str(host);
  if let Some(port) = url.port() {
    out.push(':');
    out.push_str(&port.to_string());
  }
  let path = url.path().trim_end_matches('/');
  if !path.is_empty() {
    out.push_str(path);
  }
  if let Some(query) = url.query().filter(|q| !q.is_empty()) {
    out.push('?');
    out.push_str(query);
  }
  if out.is_empty() {
    raw.to_string()
  } else {
    out
  }
}

/// Query parameters that exist only to attribute a click to a campaign.
///
/// Removing them is a real privacy win and, conveniently, breaks a meaningful chunk of
/// cross-site referrer-based tracking.
pub const TRACKING_PARAMS: &[&str] = &[
  "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content", "utm_name",
  "utm_id", "utm_reader", "utm_social", "gclid", "dclid", "gbraid", "wbraid", "fbclid",
  "msclkid", "twclid", "igshid", "mc_eid", "mc_cid", "_hsenc", "_hsmi", "hsCtaTracking",
  "vero_id", "s_kwcid", "mkt_tok", "yclid", "ttclid", "trk", "trkCampaign", "scid",
  "si", "spm", "ref_src", "wt_mc", "pk_campaign", "pk_kwd",
];

/// Strip tracking parameters from a URL. Returns the cleaned URL and whether anything
/// changed (so callers can avoid a needless navigation).
pub fn strip_tracking(url: &str) -> (String, bool) {
  let Ok(mut parsed) = Url::parse(url) else {
    return (url.to_string(), false);
  };
  let Some(query) = parsed.query().map(|q| q.to_string()) else {
    return (url.to_string(), false);
  };

  let cleaned: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
    .filter(|(k, _)| {
      let lower = k.to_ascii_lowercase();
      !TRACKING_PARAMS.iter().any(|p| *p == lower)
    })
    .map(|(k, v)| (k.into_owned(), v.into_owned()))
    .collect();

  if cleaned.len()
    == url::form_urlencoded::parse(query.as_bytes()).count()
  {
    return (url.to_string(), false);
  }

  {
    let mut out = parsed.query_pairs_mut();
    out.clear();
    for (k, v) in cleaned {
      out.append_pair(&k, &v);
    }
  }
  (parsed.to_string(), true)
}

/// Upgrade `http:` to `https:` when the site is expected to support it.
pub fn upgrade_to_https(url: &str) -> Option<String> {
  let mut parsed = Url::parse(url).ok()?;
  if parsed.scheme() == "http" && parsed.host_str().is_some() {
    parsed.set_scheme("https").ok()?;
    return Some(parsed.to_string());
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn classifies_urls_and_searches() {
    assert!(matches!(
      classify("example.com", &[]),
      OmniboxInput::Url(_)
    ));
    assert_eq!(classify("hello world", &[]), OmniboxInput::Search("hello world".into()));
    assert_eq!(
      classify("gh rust", &["gh".into()]),
      OmniboxInput::SearchWith { engine: "gh".into(), query: "rust".into() }
    );
    assert!(matches!(classify("bir://settings", &[]), OmniboxInput::Internal(_)));
    // A single word must not be mistaken for a host.
    assert_eq!(classify("rust", &[]), OmniboxInput::Search("rust".into()));
  }

  #[test]
  fn normalises_hosts() {
    assert_eq!(normalize("example.com").unwrap().as_str(), "https://example.com/");
    assert_eq!(normalize("localhost:3000/x").unwrap().as_str(), "https://localhost:3000/x");
    assert!(normalize("hello world").is_none());
    assert!(normalize("foo").is_none());
  }

  #[test]
  fn registrable_domains() {
    assert_eq!(registrable_domain("www.example.com"), "example.com");
    assert_eq!(registrable_domain("a.b.example.co.uk"), "example.co.uk");
    assert_eq!(registrable_domain("foo.github.io"), "foo.github.io");
    assert_eq!(registrable_domain("localhost"), "localhost");
  }

  #[test]
  fn strips_tracking_params() {
    let (clean, changed) = strip_tracking("https://x.com/a?utm_source=n&keep=1&fbclid=z");
    assert!(changed);
    assert_eq!(clean, "https://x.com/a?keep=1");
    let (same, changed) = strip_tracking("https://x.com/a?keep=1");
    assert!(!changed);
    assert_eq!(same, "https://x.com/a?keep=1");
  }

  #[test]
  fn display_strips_cruft() {
    assert_eq!(display_url("https://www.example.com/"), "example.com");
    assert_eq!(display_url("https://example.com/a/b?x=1"), "example.com/a/b?x=1");
    assert_eq!(display_url("bir://settings"), "bir://settings");
  }
}
