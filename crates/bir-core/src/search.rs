//! Search engines and keyword shortcuts.
//!
//! Supports the OpenSearch `{searchTerms}` placeholder as well as the older `%s`, so
//! engines pasted from other browsers or from `opensearch.xml` files just work.

use crate::{store, Error, Persistent, ProfilePaths, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchEngine {
  /// Lowercase identifier, e.g. `duckduckgo`.
  pub name: String,
  /// Display name for the UI.
  pub title: String,
  /// Shortcut typed in the omnibox, e.g. `ddg`. Empty string means "no keyword".
  pub keyword: String,
  pub search_url: String,
  pub suggest_url: Option<String>,
  /// Icon `data:` URL (we cache favicons as data URLs to avoid a network dependency).
  pub icon: Option<String>,
}

impl SearchEngine {
  /// Expand the query template. Percent-encodes the query; the template itself is
  /// trusted (it was typed by the user or shipped by us).
  pub fn build_url(&self, query: &str) -> String {
    let encoded = percent_encode(query);
    self
      .search_url
      .replace("{searchTerms}", &encoded)
      .replace("%s", &encoded)
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchEngines {
  pub engines: Vec<SearchEngine>,
  pub default: String,
}

impl Default for SearchEngines {
  fn default() -> Self {
    Self {
      default: "duckduckgo".into(),
      engines: builtin_engines(),
    }
  }
}

/// The ships-in-the-binary engine list. Chosen for coverage rather than partner deals:
/// a privacy default plus the ones people actually type keywords for.
fn builtin_engines() -> Vec<SearchEngine> {
  vec![
    SearchEngine {
      name: "duckduckgo".into(),
      title: "DuckDuckGo".into(),
      keyword: "d".into(),
      search_url: "https://duckduckgo.com/?q={searchTerms}".into(),
      suggest_url: Some("https://duckduckgo.com/ac/?q={searchTerms}&type=list".into()),
      icon: None,
    },
    SearchEngine {
      name: "google".into(),
      title: "Google".into(),
      keyword: "g".into(),
      search_url: "https://www.google.com/search?q={searchTerms}".into(),
      suggest_url: Some("https://suggestqueries.google.com/complete/search?client=firefox&q={searchTerms}".into()),
      icon: None,
    },
    SearchEngine {
      name: "bing".into(),
      title: "Bing".into(),
      keyword: "b".into(),
      search_url: "https://www.bing.com/search?q={searchTerms}".into(),
      suggest_url: None,
      icon: None,
    },
    SearchEngine {
      name: "startpage".into(),
      title: "Startpage".into(),
      keyword: "sp".into(),
      search_url: "https://www.startpage.com/sp/search?query={searchTerms}".into(),
      suggest_url: None,
      icon: None,
    },
    SearchEngine {
      name: "wikipedia".into(),
      title: "Wikipedia".into(),
      keyword: "w".into(),
      search_url: "https://en.wikipedia.org/wiki/Special:Search?search={searchTerms}".into(),
      suggest_url: None,
      icon: None,
    },
    SearchEngine {
      name: "github".into(),
      title: "GitHub".into(),
      keyword: "gh".into(),
      search_url: "https://github.com/search?q={searchTerms}".into(),
      suggest_url: None,
      icon: None,
    },
    SearchEngine {
      name: "crates".into(),
      title: "crates.io".into(),
      keyword: "cr".into(),
      search_url: "https://crates.io/search?q={searchTerms}".into(),
      suggest_url: None,
      icon: None,
    },
    SearchEngine {
      name: "docs".into(),
      title: "docs.rs".into(),
      keyword: "rs".into(),
      search_url: "https://docs.rs/releases/search?query={searchTerms}".into(),
      suggest_url: None,
      icon: None,
    },
    SearchEngine {
      name: "youtube".into(),
      title: "YouTube".into(),
      keyword: "yt".into(),
      search_url: "https://www.youtube.com/results?search_query={searchTerms}".into(),
      suggest_url: None,
      icon: None,
    },
  ]
}

impl SearchEngines {
  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let path = paths.data_dir().join(Self::FILE);
    let mut engines: Self = match store::read_to_string(&path)? {
      Some(doc) => serde_json::from_str(&doc).unwrap_or_default(),
      None => Self::default(),
    };
    // Merge in any engine added by a newer build, without clobbering user edits.
    for builtin in builtin_engines() {
      if !engines.engines.iter().any(|e| e.name == builtin.name) {
        engines.engines.push(builtin);
      }
    }
    if !engines.engines.iter().any(|e| e.name == engines.default) {
      engines.default = "duckduckgo".into();
    }
    Ok(engines)
  }

  pub fn save_to(&self, paths: &ProfilePaths) -> Result<()> {
    let doc = serde_json::to_string_pretty(self)?;
    store::write_atomic(&paths.data_dir().join(Self::FILE), doc.as_bytes())
  }

  pub fn default_engine(&self) -> &SearchEngine {
    self
      .engines
      .iter()
      .find(|e| e.name == self.default)
      .or_else(|| self.engines.first())
      .expect("search engines are never empty")
  }

  pub fn by_keyword(&self, keyword: &str) -> Option<&SearchEngine> {
    let keyword = keyword.to_ascii_lowercase();
    self
      .engines
      .iter()
      .find(|e| !e.keyword.is_empty() && e.keyword.eq_ignore_ascii_case(&keyword))
      .or_else(|| self.engines.iter().find(|e| e.name == keyword))
  }

  /// Every registered keyword, lowercased — handed to [`crate::url::classify`] so the
  /// omnibox can tell `gh foo` (search) from `foo.com` (URL).
  pub fn keywords(&self) -> Vec<String> {
    self
      .engines
      .iter()
      .filter(|e| !e.keyword.is_empty())
      .map(|e| e.keyword.to_ascii_lowercase())
      .collect()
  }

  pub fn add(&mut self, engine: SearchEngine) {
    if let Some(existing) = self.engines.iter_mut().find(|e| e.name == engine.name) {
      *existing = engine;
    } else {
      self.engines.push(engine);
    }
  }

  pub fn remove(&mut self, name: &str) -> bool {
    if name == self.default {
      return false;
    }
    let before = self.engines.len();
    self.engines.retain(|e| e.name != name);
    self.engines.len() != before
  }
}

/// Minimal `application/x-www-form-urlencoded` encoder for query strings.
pub fn percent_encode(input: &str) -> String {
  let mut out = String::with_capacity(input.len() * 2);
  for byte in input.bytes() {
    match byte {
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
        out.push(byte as char)
      }
      b' ' => out.push('+'),
      _ => out.push_str(&format!("%{byte:02X}")),
    }
  }
  out
}

impl Persistent for SearchEngines {
  const FILE: &'static str = "search-engines.json";

  fn mark_dirty(&mut self) {}
  fn is_dirty(&self) -> bool {
    true
  }
  fn clear_dirty(&mut self) {}
  fn to_json(&self) -> Result<String> {
    serde_json::to_string_pretty(self).map_err(Error::from)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn builds_search_urls() {
    let engines = SearchEngines::default();
    let url = engines.default_engine().build_url("rust webview");
    assert_eq!(url, "https://duckduckgo.com/?q=rust+webview");
    let gh = engines.by_keyword("gh").unwrap();
    assert_eq!(gh.build_url("wry"), "https://github.com/search?q=wry");
  }

  #[test]
  fn percent_encoding_is_correct() {
    assert_eq!(percent_encode("a b&c=d/e"), "a+b%26c%3Dd%2Fe");
    assert_eq!(percent_encode("safe-_.~1"), "safe-_.~1");
  }
}
