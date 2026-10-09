//! The background host page.
//!
//! MV3 replaced persistent background pages with an event-driven service worker. A
//! webview cannot host a `Worker` that outlives a document, so BIR hosts every
//! extension's background scripts in **one hidden webview** — one process, one JS heap,
//! one event loop, shared across extensions.
//!
//! Each extension's scripts run inside their own function scope with a per-extension
//! `browser`/`chrome` object, so they cannot see each other's globals. They are not
//! isolated against each other in the process-isolation sense; the mitigation is that
//! the shell only routes messages to the extension that owns the listener.

use crate::bridge::EXTENSION_RUNTIME_JS;
use serde_json::Value;

/// Path the background host is served from.
pub const BACKGROUND_URL: &str = "bir://background/index.html";

/// The background host document.
///
/// Deliberately tiny: it exists only to give the runtime somewhere to live. It has no
/// DOM content, no stylesheet, and nothing that could accidentally leak between
/// extensions.
pub fn background_html() -> String {
  format!(
    r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>BIR extension host</title>
<script>
{EXTENSION_RUNTIME_JS}
</script>
</head>
<body></body>
</html>"#
  )
}

/// Register one extension in the background host.
pub fn encode_register(
  id: &str,
  manifest: &Value,
  messages: &Value,
) -> String {
  crate::bridge::encode_init(id, "background", manifest, messages)
}

/// Evaluate one background script file for an extension.
pub fn encode_run_background_script(id: &str, body: &str) -> String {
  crate::bridge::wrap_script(id, body)
}

/// Alarm bookkeeping lives in Rust (so it survives a background-webview reload) and is
/// delivered into the page as `alarms.onAlarm` events.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Alarm {
  pub extension_id: String,
  pub name: String,
  /// Seconds between firings. `None` for a one-shot `when` alarm.
  pub period_minutes: Option<f64>,
  /// Next fire time, seconds since epoch.
  pub scheduled_at: u64,
}

impl Alarm {
  pub fn due(&self, now: u64) -> bool {
    now >= self.scheduled_at
  }

  /// Advance to the next firing. Returns `false` for a one-shot alarm, which should
  /// then be dropped.
  pub fn advance(&mut self, now: u64) -> bool {
    match self.period_minutes {
      Some(minutes) if minutes > 0.0 => {
        let period = (minutes * 60.0).max(30.0) as u64;
        self.scheduled_at = now.max(self.scheduled_at) + period;
        true
      }
      _ => false,
    }
  }
}
