//! System and process memory sampling.
//!
//! Sampled on a timer (default 15 s) rather than continuously: reading memory counters
//! is cheap, but acting on a single noisy sample is not. `sysinfo` provides the system
//! view; the process RSS comes from platform code so that we do not depend on
//! `sysinfo`'s (rapidly changing) process-refresh API.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How close to the limit the machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
  /// Plenty of headroom.
  Normal,
  /// Approaching the configured threshold: start sleeping idle tabs.
  Elevated,
  /// At or above the threshold: discard anything eligible.
  Critical,
}

impl Pressure {
  pub fn at_or_above(&self, other: Pressure) -> bool {
    (*self as u8) >= (other as u8)
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MemorySnapshot {
  pub total_bytes: u64,
  pub available_bytes: u64,
  pub used_bytes: u64,
  /// 0–100.
  pub used_percent: u8,
  /// Resident set size of this process, in bytes. `0` when the platform cannot tell us.
  pub process_rss_bytes: u64,
}

impl Default for MemorySnapshot {
  fn default() -> Self {
    Self {
      total_bytes: 0,
      available_bytes: 0,
      used_bytes: 0,
      used_percent: 0,
      process_rss_bytes: 0,
    }
  }
}

impl MemorySnapshot {
  pub fn used_mib(&self) -> u32 {
    (self.used_bytes / (1024 * 1024)) as u32
  }
  pub fn process_rss_mib(&self) -> u32 {
    (self.process_rss_bytes / (1024 * 1024)) as u32
  }
}

pub struct MemorySampler {
  system: sysinfo::System,
  last: MemorySnapshot,
}

impl MemorySampler {
  pub fn new() -> Self {
    Self {
      system: sysinfo::System::new_all(),
      last: MemorySnapshot::default(),
    }
  }

  /// Take a fresh sample. Costs one `sysinfo` refresh plus, every few minutes, one
  /// process lookup — a few tens of microseconds in total.
  pub fn sample(&mut self) -> MemorySnapshot {
    self.system.refresh_memory();

    let total = self.system.total_memory();
    let available = self.system.available_memory();
    let used = self.system.used_memory();
    let used_percent = if total > 0 {
      ((used.saturating_mul(100)) / total).min(100) as u8
    } else {
      0
    };

    self.last = MemorySnapshot {
      total_bytes: total,
      available_bytes: available,
      used_bytes: used,
      used_percent,
      process_rss_bytes: process_rss_bytes(),
    };
    self.last
  }

  pub fn last(&self) -> MemorySnapshot {
    self.last
  }
}

impl Default for MemorySampler {
  fn default() -> Self {
    Self::new()
  }
}

/// Process CPU usage, as a percentage of **one** core, smoothed.
///
/// Sampled on a timer rather than continuously, and rate-limited internally:
///
/// * Linux reads `/proc/self/stat` — free, so every sample is fresh.
/// * macOS and Windows spawn a helper (`ps`, `powershell`), which is far too expensive
///   to do every second, so those platforms take a sample every few seconds and hold the
///   last value in between.
///
/// The first sample is always `0.0`: CPU percentage is a *delta*, and there is nothing
/// to difference against yet.
pub struct CpuSampler {
  last_cpu: Option<(u64, Instant)>,
  smoothed: f32,
  last_sample: Instant,
  min_interval: Duration,
  cores: u32,
}

impl CpuSampler {
  pub fn new() -> Self {
    Self {
      last_cpu: None,
      smoothed: 0.0,
      // Nothing has been measured yet; force the first call to take a baseline.
      last_sample: Instant::now() - Duration::from_secs(60),
      min_interval: Duration::from_secs(5),
      cores: num_cpus().max(1),
    }
  }

  /// Take a sample, returning the smoothed percentage of one core.
  pub fn sample(&mut self) -> f32 {
    let now = Instant::now();
    if now.duration_since(self.last_sample) < self.min_interval {
      return self.smoothed;
    }
    self.last_sample = now;

    let cpu = process_cpu_ticks();
    let current = match self.last_cpu {
      Some((previous_cpu, previous_time)) => {
        let elapsed = now.saturating_duration_since(previous_time);
        let seconds = elapsed.as_secs_f64();
        if seconds <= 0.0 {
          0.0
        } else {
          let used = cpu.saturating_sub(previous_cpu) as f64 / TICKS_PER_SECOND as f64;
          // sysinfo-style: 100% means one core pinned. Multi-threaded workloads can
          // exceed it, which is why the value is not clamped.
          (used / seconds) * 100.0 / self.cores as f64
        }
      }
      None => 0.0,
    };
    self.last_cpu = Some((cpu, now));

    // Exponential smoothing: a single burst should not make the gauge jump, and a
    // single idle moment should not make it look like the browser went quiet.
    self.smoothed = self.smoothed * 0.7 + (current as f32) * 0.3;
    if self.smoothed < 0.05 {
      self.smoothed = 0.0;
    }
    self.smoothed
  }

  /// The last sampled value, without refreshing it.
  pub fn last(&self) -> f32 {
    self.smoothed
  }

  /// Cores we are dividing by, so the UI can explain a value above 100%.
  pub fn cores(&self) -> u32 {
    self.cores
  }
}

impl Default for CpuSampler {
  fn default() -> Self {
    Self::new()
  }
}

/// Clock ticks per second. All platforms we support that report ticks use 100.
const TICKS_PER_SECOND: u64 = 100;

fn num_cpus() -> u32 {
  std::thread::available_parallelism()
    .map(|n| n.get() as u32)
    .unwrap_or(1)
}

/// Total CPU time consumed by this process, in clock ticks.
pub fn process_cpu_ticks() -> u64 {
  cpu_platform()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn cpu_platform() -> u64 {
  // `comm` is parenthesised and may contain spaces or parentheses, so everything after
  // the *last* ')' is the numeric field list. utime is field 14, stime field 15.
  let Ok(text) = std::fs::read_to_string("/proc/self/stat") else {
    return 0;
  };
  let Some(after) = text.rsplit_once(')').map(|(_, rest)| rest) else {
    return 0;
  };
  let fields: Vec<&str> = after.split_whitespace().collect();
  // Index 11 = utime, 12 = stime (0-based, counting from the field after comm).
  let utime = fields.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
  let stime = fields.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
  utime.saturating_add(stime)
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn cpu_platform() -> u64 {
  // `ps` prints CPU time as [[dd-]hh:]mm:ss.
  let pid = std::process::id();
  let Ok(out) = std::process::Command::new("ps")
    .args(["-o", "time=", "-p", &pid.to_string()])
    .output()
  else {
    return 0;
  };
  let text = String::from_utf8_lossy(&out.stdout);
  let text = text.trim();
  let Some(seconds) = parse_ps_time(text) else {
    return 0;
  };
  (seconds * TICKS_PER_SECOND as f64) as u64
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn parse_ps_time(text: &str) -> Option<f64> {
  let mut parts: Vec<&str> = text.split(':').collect();
  if parts.is_empty() {
    return None;
  }
  // The leftmost part may carry days and/or hours: "dd-hh", "hh" or plain minutes.
  let head = parts.remove(0);
  let (days, hours) = match head.split_once('-') {
    Some((days, hours)) => (days.parse::<f64>().unwrap_or(0.0), hours.parse::<f64>().unwrap_or(0.0)),
    None => {
      // No dash: with two separators this is hours, otherwise it is minutes.
      if parts.len() == 2 {
        (0.0, head.parse::<f64>().unwrap_or(0.0))
      } else {
        parts.insert(0, head);
        (0.0, 0.0)
      }
    }
  };
  let minutes: f64 = if parts.len() == 2 {
    parts[0].parse().unwrap_or(0.0)
  } else {
    0.0
  };
  let seconds: f64 = parts.last().and_then(|v| v.parse().ok()).unwrap_or(0.0);
  Some(days * 86400.0 + hours * 3600.0 + minutes * 60.0 + seconds)
}

#[cfg(target_os = "windows")]
fn cpu_platform() -> u64 {
  let pid = std::process::id();
  let Ok(out) = std::process::Command::new("powershell")
    .args([
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      &format!("(Get-Process -Id {pid}).CPU"),
    ])
    .output()
  else {
    return 0;
  };
  // Reported in seconds, with the culture's decimal separator; ',' is as likely as '.'.
  let text = String::from_utf8_lossy(&out.stdout).trim().replace(',', ".");
  let seconds: f64 = text.parse().unwrap_or(0.0);
  (seconds * TICKS_PER_SECOND as f64) as u64
}

#[cfg(not(any(
  target_os = "linux",
  target_os = "android",
  target_os = "macos",
  target_os = "ios",
  target_os = "freebsd",
  target_os = "windows"
)))]
fn cpu_platform() -> u64 {
  0
}

/// Resident set size of the current process, best effort.
///
/// Platform notes:
/// * Linux — parsed from `/proc/self/statm`, which is free and exact.
/// * macOS/BSD — `ps`, which is a process spawn but only runs on the sample timer.
/// * Windows — PowerShell, same reasoning.
///
/// Returns 0 rather than guessing: a memory UI that invents numbers is worse than one
/// that admits it does not know.
pub fn process_rss_bytes() -> u64 {
  rss_platform()
}

/// `statm` counts in pages; every architecture we ship for uses 4 KiB pages, and the
/// value is only ever displayed rounded to MiB.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rss_platform() -> u64 {
  const PAGE_SIZE: u64 = 4096;
  let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
    return 0;
  };
  // Fields: size resident shared text lib data dt
  statm
    .split_whitespace()
    .nth(1)
    .and_then(|pages| pages.parse::<u64>().ok())
    .map(|pages| pages * PAGE_SIZE)
    .unwrap_or(0)
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn rss_platform() -> u64 {
  ps_rss_kib() * 1024
}

#[cfg(target_os = "windows")]
fn rss_platform() -> u64 {
  windows_rss_bytes()
}

#[cfg(not(any(
  target_os = "linux",
  target_os = "android",
  target_os = "macos",
  target_os = "ios",
  target_os = "freebsd",
  target_os = "windows"
)))]
fn rss_platform() -> u64 {
  0
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn ps_rss_kib() -> u64 {
  let pid = std::process::id();
  let output = std::process::Command::new("ps")
    .args(["-o", "rss=", "-p", &pid.to_string()])
    .output();
  match output {
    Ok(out) => String::from_utf8_lossy(&out.stdout)
      .trim()
      .parse::<u64>()
      .unwrap_or(0),
    Err(_) => 0,
  }
}

#[cfg(target_os = "windows")]
fn windows_rss_bytes() -> u64 {
  let pid = std::process::id();
  let output = std::process::Command::new("powershell")
    .args([
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      &format!("(Get-Process -Id {pid}).WorkingSet64"),
    ])
    .output();
  match output {
    Ok(out) => String::from_utf8_lossy(&out.stdout)
      .trim()
      .parse::<u64>()
      .unwrap_or(0),
    Err(_) => 0,
  }
}
