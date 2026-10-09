//! Per-site settings: permissions, zoom level, per-site blocking overrides.
//!
//! Rules are keyed by host *and* eTLD+1. A lookup checks the exact host first, then the
//! registrable domain, so "allow camera on meet.google.com" does not leak to
//! `google.com`, while "block ads on example.com" does apply to `www.example.com`.

use crate::{store, url::registrable_domain, Error, Persistent, ProfilePaths, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
  Camera,
  Microphone,
  Geolocation,
  Notifications,
  Midi,
  ClipboardRead,
  Popups,
  Javascript,
  Autoplay,
  ThirdPartyCookies,
  /// Disable content blocking for this site (the "this site is broken" escape hatch).
  ContentBlocking,
  /// Force the dark-mode stylesheet transform on this site.
  DarkMode,
  /// `getDisplayMedia` — screen/window/tab capture.
  ScreenCapture,
  /// Device orientation and motion sensors.
  Sensors,
}

impl Permission {
  /// Label shown in the permission bubble and the site-settings page.
  pub fn label(&self) -> &'static str {
    match self {
      Permission::Camera => "Use your camera",
      Permission::Microphone => "Use your microphone",
      Permission::Geolocation => "Know your location",
      Permission::Notifications => "Show notifications",
      Permission::Midi => "Access MIDI devices",
      Permission::ClipboardRead => "Read the clipboard",
      Permission::Popups => "Open pop-up windows",
      Permission::Javascript => "Run JavaScript",
      Permission::Autoplay => "Autoplay media",
      Permission::ThirdPartyCookies => "Store third-party cookies",
      Permission::ContentBlocking => "Block ads and trackers",
      Permission::DarkMode => "Force dark mode",
      Permission::ScreenCapture => "Capture your screen",
      Permission::Sensors => "Read device motion sensors",
    }
  }

  /// Permissions that default to on; everything else defaults to off/ask.
  pub fn default_allow(&self) -> bool {
    matches!(self, Permission::Javascript | Permission::Popups)
  }

  pub const ALL: &'static [Permission] = &[
    Permission::Camera,
    Permission::Microphone,
    Permission::Geolocation,
    Permission::Notifications,
    Permission::Midi,
    Permission::ClipboardRead,
    Permission::Popups,
    Permission::Javascript,
    Permission::Autoplay,
    Permission::ThirdPartyCookies,
    Permission::ContentBlocking,
    Permission::DarkMode,
  ];
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SiteSettings {
  /// host → (permission → allowed)
  hosts: HashMap<String, HashMap<Permission, bool>>,
  /// host → zoom factor
  zoom: HashMap<String, f64>,
  #[serde(skip)]
  dirty: bool,
}

impl SiteSettings {
  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let path = paths.data_dir().join(Self::FILE);
    match store::read_to_string(&path)? {
      Some(doc) => Ok(serde_json::from_str(&doc).unwrap_or_default()),
      None => Ok(Self::default()),
    }
  }

  /// Resolve a permission for `host`: explicit host rule, then eTLD+1, then default.
  pub fn allows(&self, host: &str, permission: Permission) -> bool {
    let host = host.to_ascii_lowercase();
    if let Some(rules) = self.hosts.get(&host) {
      if let Some(allowed) = rules.get(&permission) {
        return *allowed;
      }
    }
    let domain = registrable_domain(&host).to_ascii_lowercase();
    if domain != host {
      if let Some(rules) = self.hosts.get(&domain) {
        if let Some(allowed) = rules.get(&permission) {
          return *allowed;
        }
      }
    }
    permission.default_allow()
  }

  /// The stored decision for a host and permission, if the user has ever made one.
  ///
  /// Unlike [`SiteSettings::allows`] this does not fall back to the permission's
  /// default, so "the user has not decided" can be told apart from "denied".
  pub fn decision(&self, host: &str, permission: Permission) -> Option<bool> {
    let host = host.to_ascii_lowercase();
    self.hosts.get(&host).and_then(|rules| rules.get(&permission)).copied()
  }

  pub fn set(&mut self, host: &str, permission: Permission, allow: bool) {
    let host = host.to_ascii_lowercase();
    self
      .hosts
      .entry(host)
      .or_default()
      .insert(permission, allow);
    self.dirty = true;
  }

  pub fn clear(&mut self, host: &str) {
    self.hosts.remove(&host.to_ascii_lowercase());
    self.dirty = true;
  }

  pub fn clear_all(&mut self) {
    self.hosts.clear();
    self.dirty = true;
  }

  pub fn zoom(&self, host: &str) -> Option<f64> {
    self.zoom.get(&host.to_ascii_lowercase()).copied()
  }

  pub fn set_zoom(&mut self, host: &str, factor: f64) {
    self
      .zoom
      .insert(host.to_ascii_lowercase(), factor.clamp(0.25, 5.0));
    self.dirty = true;
  }

  /// Everything known about one host, for the site-settings page.
  pub fn rules_for(&self, host: &str) -> HashMap<Permission, bool> {
    self
      .hosts
      .get(&host.to_ascii_lowercase())
      .cloned()
      .unwrap_or_default()
  }

  pub fn known_hosts(&self) -> Vec<String> {
    let mut hosts: Vec<String> = self.hosts.keys().cloned().collect();
    hosts.sort();
    hosts
  }
}

impl Persistent for SiteSettings {
  const FILE: &'static str = "site-settings.json";

  fn mark_dirty(&mut self) {
    self.dirty = true;
  }
  fn is_dirty(&self) -> bool {
    self.dirty
  }
  fn clear_dirty(&mut self) {
    self.dirty = false;
  }
  fn to_json(&self) -> Result<String> {
    serde_json::to_string_pretty(self).map_err(Error::from)
  }
}
