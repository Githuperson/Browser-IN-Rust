//! Chrome match patterns (`*://*.example.com/*`) and URL glob matching.
//!
//! Implements the semantics described by Chrome's match-pattern documentation:
//! `*` in the scheme matches `http` and `https` only; `file://` URLs must be matched
//! explicitly; a host of `*.example.com` matches the domain and all its subdomains but
//! not `notexample.com`; the path is a glob.

use url::Url;

/// How the host part of a pattern is compared.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
  /// `*` — every host (of an allowed scheme).
  Any,
  /// `*.example.com` — the domain itself plus any subdomain.
  Suffix(String),
  /// `example.com` or `*.foo.example.com` written out in full — exact match only.
  Exact(String),
}

/// A parsed match pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPattern {
  scheme: Option<String>,
  host: HostPattern,
  path: String,
}

impl MatchPattern {
  pub fn parse(pattern: &str) -> Option<Self> {
    let pattern = pattern.trim();
    if pattern == "<all_urls>" {
      return Some(Self {
        scheme: None,
        host: HostPattern::Any,
        path: "/".into(),
      });
    }

    let (scheme_part, rest) = pattern.split_once("://")?;
    let scheme = match scheme_part {
      "*" => None,
      other if other.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') =>
      {
        Some(other.to_ascii_lowercase())
      }
      _ => return None,
    };

    let (host_part, path_part) = match rest.find('/') {
      Some(index) => (&rest[..index], &rest[index..]),
      None => (rest, "/"),
    };

    let host = if host_part == "*" {
      HostPattern::Any
    } else if let Some(domain) = host_part.strip_prefix("*.") {
      if domain.contains('*') || domain.is_empty() {
        return None;
      }
      HostPattern::Suffix(domain.to_ascii_lowercase())
    } else if host_part.contains('*') {
      return None;
    } else if host_part.is_empty() {
      return None;
    } else {
      HostPattern::Exact(host_part.to_ascii_lowercase())
    };

    let path = if path_part.is_empty() {
      "/".to_string()
    } else {
      path_part.to_string()
    };

    Some(Self { scheme, host, path })
  }

  /// Does this pattern match `url`?
  pub fn matches(&self, url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
      return false;
    };
    let scheme = parsed.scheme().to_ascii_lowercase();

    match (&self.scheme, scheme.as_str()) {
      // A `*` scheme only ever matches http/https, per Chrome's rules.
      (None, "http") | (None, "https") => {}
      (None, _) => return false,
      (Some(expected), actual) => {
        if expected != actual {
          return false;
        }
      }
    }

    let host = match parsed.host_str() {
      Some(host) => host.to_ascii_lowercase(),
      // `file://`, `data:` and friends have no host; only patterns that explicitly
      // opted in can match them.
      None => return matches!(self.host, HostPattern::Any) && scheme == "file",
    };

    let host_ok = match &self.host {
      HostPattern::Any => true,
      HostPattern::Exact(expected) => host == *expected,
      HostPattern::Suffix(domain) => host == *domain || host.ends_with(&format!(".{domain}")),
    };
    if !host_ok {
      return false;
    }

    let path = if parsed.query().is_some() {
      format!("{}?{}", parsed.path(), parsed.query().unwrap_or_default())
    } else {
      parsed.path().to_string()
    };
    glob_match(&self.path, &path)
  }

  /// Human-readable form, for the extensions page.
  pub fn as_str(&self) -> String {
    let scheme = self.scheme.clone().unwrap_or_else(|| "*".into());
    let host = match &self.host {
      HostPattern::Any => "*".to_string(),
      HostPattern::Exact(host) => host.clone(),
      HostPattern::Suffix(domain) => format!("*.{domain}"),
    };
    format!("{scheme}://{host}{}", self.path)
  }
}

/// True when any pattern matches and no exclusion does.
pub fn matches_any(
  includes: &[MatchPattern],
  excludes: &[MatchPattern],
  url: &str,
) -> bool {
  includes.iter().any(|p| p.matches(url)) && !excludes.iter().any(|p| p.matches(url))
}

/// Glob match where `*` matches any run of characters (including `/`).
///
/// Linear with backtracking, bounded by `pattern.len() × text.len()`. Match patterns are
/// short and this runs once per content script per navigation, so it is ample.
pub fn glob_match(pattern: &str, text: &str) -> bool {
  if pattern == "/*" || pattern == "*" {
    return true;
  }
  if !pattern.contains('*') {
    return pattern == text;
  }

  let pattern: Vec<char> = pattern.chars().collect();
  let text: Vec<char> = text.chars().collect();
  let mut pi = 0usize;
  let mut ti = 0usize;
  let mut star_pi: Option<usize> = None;
  let mut star_ti = 0usize;

  while ti < text.len() {
    if pi < pattern.len() && pattern[pi] == '*' {
      star_pi = Some(pi);
      star_ti = ti;
      pi += 1;
    } else if pi < pattern.len() && pattern[pi] == text[ti] {
      pi += 1;
      ti += 1;
    } else if let Some(star) = star_pi {
      pi = star + 1;
      star_ti += 1;
      ti = star_ti;
    } else {
      return false;
    }
  }
  while pi < pattern.len() && pattern[pi] == '*' {
    pi += 1;
  }
  pi == pattern.len()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn host_suffix_matching() {
    let p = MatchPattern::parse("*://*.example.com/*").unwrap();
    assert!(p.matches("https://example.com/a"));
    assert!(p.matches("https://www.example.com/a/b?c=d"));
    assert!(!p.matches("https://notexample.com/a"));
    assert!(!p.matches("https://example.com.evil.net/a"));
  }

  #[test]
  fn scheme_rules() {
    let p = MatchPattern::parse("*://*/*").unwrap();
    assert!(p.matches("https://anything.test/x"));
    assert!(p.matches("http://anything.test/x"));
    assert!(!p.matches("file:///tmp/x.html"));

    let file = MatchPattern::parse("file:///*").unwrap();
    assert!(file.matches("file:///tmp/x.html"));

    let https = MatchPattern::parse("https://*/*").unwrap();
    assert!(https.matches("https://x.test/"));
    assert!(!https.matches("http://x.test/"));
  }

  #[test]
  fn all_urls() {
    let p = MatchPattern::parse("<all_urls>").unwrap();
    assert!(p.matches("https://any.test/"));
    assert!(p.matches("http://any.test/"));
  }

  #[test]
  fn path_globs() {
    let p = MatchPattern::parse("https://x.test/a/*/c").unwrap();
    assert!(p.matches("https://x.test/a/b/c"));
    assert!(p.matches("https://x.test/a/b/b2/c"));
    assert!(!p.matches("https://x.test/a/b/d"));

    let p = MatchPattern::parse("https://x.test/exact").unwrap();
    assert!(p.matches("https://x.test/exact"));
    assert!(!p.matches("https://x.test/exactly"));
  }
}
