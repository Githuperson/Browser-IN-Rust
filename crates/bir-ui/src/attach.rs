//! Platform-specific webview attachment and layout geometry.
//!
//! See the crate docs for why Linux takes a different path. The rest of this module is
//! the arithmetic that keeps the chrome and the content webview from overlapping.

use wry::{WebView, WebViewBuilder};

#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
use wry::WebViewBuilderExtUnix as _;

/// Height of the chrome strip in logical pixels (horizontal tab layout).
pub const CHROME_HEIGHT: f64 = 96.0;

/// Width of the chrome sidebar in logical pixels (vertical tab layout).
pub const SIDEBAR_WIDTH: f64 = 252.0;

/// Where a webview gets attached.
pub enum Surface<'a> {
  /// Windows and macOS: a child of the native window.
  Window(&'a tao::window::Window),
  /// Linux (X11 and Wayland): a child of a GTK fixed container.
  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  Gtk(&'a gtk::Fixed),
}

/// Finish building a webview onto the given surface.
pub fn build<'a>(builder: WebViewBuilder<'a>, surface: Surface<'a>) -> wry::Result<WebView> {
  match surface {
    Surface::Window(window) => builder.build_as_child(window),
    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    Surface::Gtk(container) => builder.build_gtk(container),
  }
}

/// A rectangle in logical pixels, matching what `wry::Rect` wants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
  pub x: f64,
  pub y: f64,
  pub width: f64,
  pub height: f64,
}

impl Frame {
  pub fn to_wry(self) -> wry::Rect {
    wry::Rect {
      position: wry::dpi::LogicalPosition::new(self.x, self.y).into(),
      size: wry::dpi::LogicalSize::new(self.width.max(1.0), self.height.max(1.0)).into(),
    }
  }

  /// Clamp so the frame never produces a zero or negative size, which some platforms
  /// reject outright.
  pub fn clamped(mut self, bounds: (f64, f64)) -> Self {
    self.x = self.x.max(0.0);
    self.y = self.y.max(0.0);
    self.width = self.width.min((bounds.0 - self.x).max(0.0)).max(1.0);
    self.height = self.height.min((bounds.1 - self.y).max(0.0)).max(1.0);
    self
  }
}

/// Chrome and content rectangles for a window of `width × height` logical pixels.
pub fn layout(width: f64, height: f64, vertical_tabs: bool) -> (Frame, Frame) {
  if vertical_tabs {
    let sidebar = SIDEBAR_WIDTH.min(width * 0.5).max(160.0);
    (
      Frame {
        x: 0.0,
        y: 0.0,
        width: sidebar,
        height,
      },
      Frame {
        x: sidebar,
        y: 0.0,
        width: (width - sidebar).max(1.0),
        height,
      },
    )
  } else {
    let chrome_height = CHROME_HEIGHT.min(height * 0.6).max(48.0);
    (
      Frame {
        x: 0.0,
        y: 0.0,
        width,
        height: chrome_height,
      },
      Frame {
        x: 0.0,
        y: chrome_height,
        width,
        height: (height - chrome_height).max(1.0),
      },
    )
  }
}

/// Logical size of a window from its physical size and scale factor.
pub fn logical_size(physical_width: u32, physical_height: u32, scale_factor: f64) -> (f64, f64) {
  let scale = if scale_factor > 0.0 { scale_factor } else { 1.0 };
  (physical_width as f64 / scale, physical_height as f64 / scale)
}

/// Create the GTK container used on Linux, sized to the window and packed into it.
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
pub fn create_container(window: &tao::window::Window, width: i32, height: i32) -> gtk::Fixed {
  use gtk::prelude::*;
  let fixed = gtk::Fixed::new();
  fixed.set_size_request(width.max(1), height.max(1));
  // tao gives every window a default vertical box; pack into it so the container
  // expands with the window instead of sitting at a fixed size.
  match tao::platform::unix::WindowExtUnix::default_vbox(window) {
    Some(vbox) => {
      vbox.pack_start(&fixed, true, true, 0);
    }
    None => {
      tao::platform::unix::WindowExtUnix::gtk_window(window).add(&fixed);
    }
  }
  fixed.show_all();
  fixed
}

/// Stub used on non-Linux builds so call sites do not need `cfg` noise.
#[cfg(not(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
)))]
pub fn create_container(_window: &tao::window::Window, _width: i32, _height: i32) {}
