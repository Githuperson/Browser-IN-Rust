//! Permission model.
//!
//! Two kinds of permission, exactly as in Chrome:
//!
//! * **API permissions** — named strings like `tabs`, `storage`, `downloads`.
//! * **Host permissions** — match patterns like `*://*.example.com/*`, which gate
//!   anything that reads page content or cookies.
//!
//! BIR installs extensions with the permissions they declare (there is no store review
//! step), so the important job is *explaining* them: [`PermissionSet::risk`] drives the
//! warning the user sees in the extensions page, and [`describe`] turns a raw
//! permission string into a sentence.

use crate::matches::MatchPattern;

/// How much damage an extension could do with the permissions it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
  Low,
  Medium,
  High,
}

impl RiskLevel {
  pub fn label(&self) -> &'static str {
    match self {
      RiskLevel::Low => "Low risk",
      RiskLevel::Medium => "Moderate risk",
      RiskLevel::High => "High risk",
    }
  }
}

pub struct PermissionSet {
  permissions: Vec<String>,
  host_patterns: Vec<MatchPattern>,
}

impl PermissionSet {
  pub fn new(permissions: Vec<String>, host_permissions: Vec<String>) -> Self {
    let host_patterns = host_permissions
      .iter()
      .filter_map(|p| MatchPattern::parse(p))
      .collect();
    Self {
      permissions,
      host_patterns,
    }
  }

  pub fn has(&self, permission: &str) -> bool {
    self
      .permissions
      .iter()
      .any(|p| p.eq_ignore_ascii_case(permission))
  }

  /// Does this extension have access to `url`?
  pub fn allows_url(&self, url: &str) -> bool {
    self.host_patterns.iter().any(|p| p.matches(url))
  }

  /// `<all_urls>` and `*://*/*` are the ones that matter.
  pub fn has_all_hosts(&self) -> bool {
    self.allows_url("https://any.invalid.test/") && self.allows_url("http://any.invalid.test/")
  }

  pub fn permissions(&self) -> &[String] {
    &self.permissions
  }

  pub fn host_patterns(&self) -> &[MatchPattern] {
    &self.host_patterns
  }

  /// Worst-case classification across everything the extension declared.
  pub fn risk(&self) -> RiskLevel {
    let mut risk = RiskLevel::Low;
    for permission in &self.permissions {
      risk = risk.max(match permission.as_str() {
        "storage" | "alarms" | "notifications" | "unlimitedStorage" | "contextMenus"
        | "scripting" | "activeTab" | "commands" | "idle" => RiskLevel::Low,
        "tabs" | "webNavigation" | "downloads" | "clipboardWrite" | "contextualIdentities"
        | "search" | "sessions" | "windows" | "management" => RiskLevel::Medium,
        "cookies" | "history" | "bookmarks" | "downloads.open" | "browsingData"
        | "clipboardRead" | "identity" | "privacy" | "proxy" | "debugger" | "nativeMessaging"
        | "webRequest" | "declarativeNetRequest" | "pageCapture" | "geolocation" => {
          RiskLevel::High
        }
        _ => RiskLevel::Medium,
      });
    }
    if self.has_all_hosts() {
      risk = risk.max(RiskLevel::High);
    } else if !self.host_patterns.is_empty() {
      risk = risk.max(RiskLevel::Medium);
    }
    risk
  }
}

/// Plain-English description of a permission, shown in the install prompt and the
/// extensions page.
pub fn describe(permission: &str) -> &'static str {
  match permission {
    "storage" => "Store its own settings and data",
    "alarms" => "Run on a schedule, even when you are not on the page",
    "notifications" => "Show desktop notifications",
    "contextMenus" => "Add items to the right-click menu",
    "scripting" => "Run code in pages you visit",
    "activeTab" => "Act on the tab you are using when you invoke it",
    "tabs" => "See the titles, URLs and favicons of your open tabs",
    "downloads" => "Start downloads and read your download history",
    "history" => "Read and change your browsing history",
    "bookmarks" => "Read and change your bookmarks",
    "cookies" => "Read and change cookies for sites you visit",
    "browsingData" => "Clear your browsing data",
    "clipboardRead" => "Read text you have copied",
    "geolocation" => "Know your location",
    "webNavigation" => "Follow your navigation between pages",
    "webRequest" | "declarativeNetRequest" => "Inspect and change network requests",
    "unlimitedStorage" => "Store an unlimited amount of data",
    "identity" => "Ask you to sign in to a Google account",
    "proxy" => "Change your proxy settings",
    "nativeMessaging" => "Talk to native applications on your computer",
    "management" => "Manage your other extensions",
    "privacy" => "Change your privacy settings",
    "debugger" => "Attach a debugger to pages",
    "idle" => "Know when your computer is idle",
    "sessions" => "Read and restore your open tabs",
    "search" => "Read and change your search engines",
    "windows" => "Open and arrange browser windows",
    "commands" | "commands" => "Respond to keyboard shortcuts",
    _ => "Use an API BIR does not have a description for",
  }
}

impl std::fmt::Display for RiskLevel {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.label())
  }
}
