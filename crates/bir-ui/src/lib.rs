//! `bir-ui` — the browser shell.
//!
//! # Window anatomy
//!
//! Every browser window contains **one webview per tab plus one for the chrome itself**:
//!
//! ```text
//!  ┌──────────────────────────── window ────────────────────────────┐
//!  │  chrome webview   (HTML/CSS/JS: tab strip, omnibox, menus)     │  ← 96 px
//!  ├────────────────────────────────────────────────────────────────┤
//!  │                                                                │
//!  │  content webview  (the page, one per tab, one visible)         │
//!  │                                                                │
//!  └────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The chrome is HTML because that is the only UI toolkit that is genuinely
//! cross-platform here: tao gives us windows and events, not buttons and text fields.
//! It also means the chrome is GPU-composited, DPI-aware, themeable with CSS and
//! animatable for free — the same trade-off Tauri and every Electron-era shell made.
//!
//! # Platform attachment
//!
//! * **Windows / macOS** — webviews are attached with `build_as_child` and positioned
//!   with `set_bounds`.
//! * **Linux** — webviews are added to a `gtk::Fixed` container (`build_gtk`), which is
//!   the only arrangement wry supports on Wayland. X11 and Wayland therefore behave
//!   identically, at the cost of a GTK dependency on that platform.

pub mod app;
pub mod attach;
pub mod commands;
pub mod ext_api;
pub mod pages;
pub mod shortcuts;
pub mod tab;
pub mod window;

pub use app::{AppEvent, BrowserApp, Startup};
pub use tab::Tab;
pub use window::BrowserWindow;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
  #[error("wry error: {0}")]
  Wry(#[from] wry::Error),
  #[error("{0}")]
  Core(#[from] bir_core::Error),
  #[error("{0}")]
  Net(#[from] bir_net::Error),
  #[error("{0}")]
  Ext(#[from] bir_ext::Error),
  #[error("{0}")]
  Io(#[from] std::io::Error),
  #[error("{0}")]
  Other(String),
}
