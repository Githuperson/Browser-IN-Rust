//! `bir-net` — content blocking.
//!
//! # Why this crate exists
//!
//! A webview is not Chromium: WebKitGTK, WKWebView and WebView2 expose no
//! `webRequest`-equivalent to the embedder, so the usual "ask the engine to cancel the
//! request" strategy is unavailable. Blocking therefore happens at three layers, and
//! this crate provides all of them:
//!
//! 1. **Navigation blocking (Rust).** Top-level navigations to blocked URLs are
//!    cancelled in `wry`'s navigation handler — this is exact and free.
//! 2. **Cosmetic filtering (CSS).** Element-hiding selectors from filter lists that
//!    apply to the current host are injected as a `<style>` element. CSS is
//!    self-maintaining: it hides nodes inserted later by the page with no observer.
//! 3. **In-page network blocking (JS).** A small script patches `fetch`,
//!    `XMLHttpRequest`, `EventSource`, `sendBeacon` and `WebSocket`, and scans
//!    dynamically inserted `<script>`/`<iframe>`/`<img>` nodes, rejecting anything
//!    that matches the rule set.
//!
//! Layer 3 cannot be perfect — a page can always grab a reference to the original
//! constructors before our script runs. In practice it runs at document start and
//! blocks the overwhelming majority of real-world ad and analytics traffic, at a cost
//! of a few kilobytes of script per navigation.

pub mod blocker;
pub mod engine;
pub mod fetch;
pub mod lists;
pub mod rules;

pub use blocker::BlockerScript;
pub use engine::{BlockDecision, Blocker, BlockerStats, RequestKind};
pub use lists::{FilterListManager, ListSource, UpdateOutcome};
pub use rules::{CosmeticRule, NetworkRule, RuleSet};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
  #[error("{0}")]
  Core(#[from] bir_core::Error),
  #[error("network error: {0}")]
  Network(String),
  #[error("list error: {0}")]
  List(String),
}
