//! GPU policy.
//!
//! All three platform webviews are GPU-composited by default — WebKitGTK composites
//! through GSK/OpenGL or the Vulkan/DMABUF path, WKWebView always layers through Metal,
//! and WebView2 is Chromium. What this module adds is the ability to *ask* for
//! hardware rasterisation where the platform lets us choose, and to force software
//! rendering where a driver is broken.
//!
//! Two mechanisms, both applied before the first webview is created:
//!
//! * **Windows** — extra Chromium command-line flags via wry's
//!   `WebViewBuilderExtWindows::with_additional_browser_args`.
//! * **Linux** — WebKitGTK environment variables. Note these are *presence*-checked by
//!   WebKit, so the software-mode variables are only ever **set**, never set to `0`
//!   (writing `WEBKIT_DISABLE_COMPOSITING_MODE=0` would disable compositing, because
//!   WebKit only asks whether the variable exists).

use std::sync::Once;

use bir_core::settings::GpuMode;
use serde::{Deserialize, Serialize};

static APPLIED: Once = Once::new();

/// Human-readable description of what the browser is doing about the GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
  pub mode: GpuMode,
  /// e.g. "Metal", "WebKitGTK (hardware)", "software rasterisation".
  pub renderer: String,
  /// Anything the user should know (driver problems, unsupported combinations).
  pub notes: Vec<String>,
}

/// Byte-for-byte the arguments wry passes by default on Windows.
///
/// wry documents that supplying your own additional browser arguments **replaces** its
/// defaults, so they have to be repeated here or we would silently lose them.
const WRY_DEFAULT_ARGS: &str =
  "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

/// Extra Chromium flags for hardware acceleration.
const HARDWARE_ARGS: &str =
  "--enable-gpu-rasterization --ignore-gpu-blocklist --enable-zero-copy --enable-accelerated-video-decode";

/// Extra Chromium flags for software rendering.
const SOFTWARE_ARGS: &str = "--disable-gpu --disable-gpu-compositing";

/// Apply any process-wide GPU settings. Must run before the first webview is created.
///
/// Safe to call more than once; only the first call has an effect.
pub fn apply_gpu_policy(mode: GpuMode) {
  APPLIED.call_once(|| {
    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    {
      match mode {
        GpuMode::Auto => {}
        GpuMode::Hardware => {
          // Nothing to set: WebKitGTK uses hardware compositing unless told otherwise.
          // We do nudge gstreamer towards hardware video decode when available.
          std::env::set_var("GST_VAAPI_ALL_DRIVERS", "1");
        }
        GpuMode::Software => {
          // Presence-checked by WebKitGTK — see the module docs.
          std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
          std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
          std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
          std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
        }
      }
    }

    #[cfg(not(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    )))]
    {
      let _ = mode;
    }
  });
}

/// The `--additional-browser-arguments` value to hand to WebView2 on Windows.
///
/// Returns the wry defaults alone for [`GpuMode::Auto`], so behaviour is unchanged
/// unless the user picked something.
pub fn webview2_extra_args(mode: GpuMode, allow_autoplay: bool) -> String {
  let mut args = String::from(WRY_DEFAULT_ARGS);
  match mode {
    GpuMode::Auto => {}
    GpuMode::Hardware => {
      args.push(' ');
      args.push_str(HARDWARE_ARGS);
    }
    GpuMode::Software => {
      args.push(' ');
      args.push_str(SOFTWARE_ARGS);
    }
  }
  if allow_autoplay {
    args.push_str(" --autoplay-policy=no-user-gesture-required");
  }
  args
}

/// Describe the current GPU situation, as far as we can tell without initialising a
/// graphics API (which we deliberately avoid: probing GL/Vulkan in-process is exactly
/// the kind of thing that crashes on headless and remote-desktop machines).
pub fn gpu_report() -> GpuInfo {
  let notes = detect_notes();

  #[cfg(target_vendor = "apple")]
  let renderer = "Metal (WKWebView is always GPU-composited)".to_string();

  #[cfg(target_os = "windows")]
  let renderer = "Chromium / WebView2".to_string();

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  let renderer = if has_drm_device() {
    "WebKitGTK (hardware compositing available)".to_string()
  } else {
    "WebKitGTK (no /dev/dri — software compositing)".to_string()
  };

  #[cfg(not(any(
    target_vendor = "apple",
    target_os = "windows",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  )))]
  let renderer = "unknown".to_string();

  GpuInfo {
    mode: GpuMode::Auto,
    renderer,
    notes,
  }
}

fn detect_notes() -> Vec<String> {
  let mut notes = Vec::new();

  // Remote sessions usually have no GPU at all. Say so rather than letting the user
  // wonder why video is choppy.
  if std::env::var("SSH_CONNECTION").is_ok() {
    notes.push("Running over SSH: GPU compositing is unlikely to be available.".into());
  }
  if std::env::var("WAYLAND_DISPLAY").is_err()
    && std::env::var("DISPLAY").is_err()
    && !cfg!(target_os = "windows")
    && !cfg!(target_vendor = "apple")
  {
    notes.push("No display server detected: web views cannot be created.".into());
  }

  #[cfg(all(target_family = "unix", not(target_vendor = "apple")))]
  {
    if !has_drm_device() {
      notes.push(
        "No /dev/dri render node found; WebKitGTK will fall back to software compositing."
          .into(),
      );
    }
  }

  notes
}

#[cfg(target_family = "unix")]
fn has_drm_device() -> bool {
  std::path::Path::new("/dev/dri").exists()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn auto_mode_keeps_wry_defaults() {
    let args = webview2_extra_args(GpuMode::Auto, false);
    assert!(args.contains("msSmartScreenProtection"));
    assert!(!args.contains("--disable-gpu"));
  }

  #[test]
  fn autoplay_flag_is_appended() {
    let args = webview2_extra_args(GpuMode::Auto, true);
    assert!(args.contains("--autoplay-policy=no-user-gesture-required"));
  }

  #[test]
  fn hardware_and_software_differ() {
    let hw = webview2_extra_args(GpuMode::Hardware, false);
    let sw = webview2_extra_args(GpuMode::Software, false);
    assert!(hw.contains("--enable-gpu-rasterization"));
    assert!(sw.contains("--disable-gpu "));
  }
}
