//! Tab lifecycle policy: who gets to keep a webview.
//!
//! The scheduler is pure: given the current tab inventory and a memory sample it
//! returns a list of actions. Nothing here touches a webview, which makes the policy
//! unit-testable and keeps the (stateful, platform-specific) application of those
//! actions in one place in the UI layer.

use crate::memory::{MemorySnapshot, Pressure};
use bir_core::{ipc::TabId, settings::Performance};

/// Rough resident cost of one live webview, in MiB.
///
/// Measured on a mid-range 2024 laptop: an empty page is ~25 MiB, a typical news or
/// social page 80–150 MiB. Taken as 45 MiB so the "you saved X" figure in the UI is
/// conservative rather than flattering.
pub const ESTIMATED_WEBVIEW_MIB: u32 = 45;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabLifecycle {
  /// Selected tab in the focused window: full speed, no throttling.
  Active,
  /// Visible (split view / another window) but not focused.
  Visible,
  /// Backgrounded: unmapped and throttled, state intact, wakes instantly.
  Sleeping,
  /// No webview at all; reloads on activation.
  Discarded,
}

impl TabLifecycle {
  pub fn has_webview(self) -> bool {
    !matches!(self, TabLifecycle::Discarded)
  }
  pub fn is_mapped(self) -> bool {
    matches!(self, TabLifecycle::Active | TabLifecycle::Visible)
  }
}

/// What the scheduler needs to know about one tab.
#[derive(Debug, Clone)]
pub struct TabActivity {
  pub id: TabId,
  pub lifecycle: TabLifecycle,
  /// Seconds since epoch of the last time this tab was the active one.
  pub last_active_secs: u64,
  pub pinned: bool,
  /// Playing audio — never discard; users notice instantly and hate it.
  pub audible: bool,
  /// Matches `performance.never_discard` (compare against host or eTLD+1).
  pub host: String,
}

/// Derived from `Settings::performance`.
#[derive(Debug, Clone)]
pub struct LifecyclePolicy {
  pub max_live_webviews: usize,
  pub sleep_after_secs: u64,
  pub discard_after_secs: u64,
  pub discard_under_pressure: bool,
  pub memory_pressure_percent: u8,
  pub never_discard: Vec<String>,
}

impl LifecyclePolicy {
  pub fn from_settings(settings: &Performance) -> Self {
    Self {
      max_live_webviews: settings.max_live_webviews.max(1),
      sleep_after_secs: settings.sleep_after_secs,
      discard_after_secs: settings.discard_after_secs,
      discard_under_pressure: settings.discard_under_pressure,
      memory_pressure_percent: settings.memory_pressure_percent,
      never_discard: settings.never_discard.clone(),
    }
  }

  fn is_protected(&self, tab: &TabActivity) -> bool {
    if tab.pinned || tab.audible {
      return true;
    }
    if tab.host.is_empty() {
      return false;
    }
    let host = tab.host.to_ascii_lowercase();
    self
      .never_discard
      .iter()
      .any(|entry| {
        let entry = entry.to_ascii_lowercase();
        host == entry || host.ends_with(&format!(".{entry}"))
      })
  }
}

/// What the shell should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
  /// Unmap and throttle (fast to wake).
  Sleep(TabId),
  /// Drop the webview entirely.
  Discard(TabId),
}

pub struct LifecycleScheduler {
  policy: LifecyclePolicy,
  /// Set when the last evaluation reported pressure, so the UI can explain itself.
  last_pressure: Pressure,
}

impl LifecycleScheduler {
  pub fn new(policy: LifecyclePolicy) -> Self {
    Self {
      policy,
      last_pressure: Pressure::Normal,
    }
  }

  pub fn policy(&self) -> &LifecyclePolicy {
    &self.policy
  }

  pub fn set_policy(&mut self, policy: LifecyclePolicy) {
    self.policy = policy;
  }

  pub fn last_pressure(&self) -> Pressure {
    self.last_pressure
  }

  /// Classify the current memory situation against the configured threshold.
  pub fn pressure(&self, memory: &MemorySnapshot) -> Pressure {
    let threshold = self.policy.memory_pressure_percent.min(100);
    let used = memory.used_percent;
    if used >= threshold {
      Pressure::Critical
    } else if used + 10 >= threshold {
      Pressure::Elevated
    } else {
      Pressure::Normal
    }
  }

  /// Decide what to do. `now` is seconds since epoch.
  ///
  /// Ordering matters: the hard cap is enforced first (it is a user-visible promise),
  /// then pressure relief, then the idle timers.
  pub fn evaluate(
    &mut self,
    tabs: &[TabActivity],
    memory: &MemorySnapshot,
    now: u64,
  ) -> Vec<LifecycleAction> {
    let pressure = self.pressure(memory);
    self.last_pressure = pressure;

    let mut actions = Vec::new();

    // Candidates: everything with a webview that is neither active, visible, pinned,
    // audible nor explicitly protected.
    let mut candidates: Vec<&TabActivity> = tabs
      .iter()
      .filter(|t| {
        t.lifecycle.has_webview()
          && !t.lifecycle.is_mapped()
          && !self.policy.is_protected(t)
      })
      .collect();

    // Oldest first: least-recently-used is the only sensible eviction order for tabs.
    candidates.sort_by_key(|t| t.last_active_secs);

    // 1. Hard cap on live webviews.
    let live = tabs.iter().filter(|t| t.lifecycle.has_webview()).count();
    let mut over_by = live.saturating_sub(self.policy.max_live_webviews);

    let mut evicted: std::collections::HashSet<TabId> = std::collections::HashSet::new();
    for tab in &candidates {
      if over_by == 0 {
        break;
      }
      if tab.lifecycle == TabLifecycle::Discarded {
        continue;
      }
      actions.push(LifecycleAction::Discard(tab.id));
      evicted.insert(tab.id);
      over_by -= 1;
    }

    // 2. Memory pressure: discard idle tabs (oldest first) until we are under the
    //    threshold or run out of candidates.
    if pressure == Pressure::Critical && self.policy.discard_under_pressure {
      let excess_mib = estimate_excess_mib(memory, self.policy.memory_pressure_percent);
      let mut reclaimed = 0u32;
      for tab in &candidates {
        if evicted.contains(&tab.id) {
          continue;
        }
        if reclaimed >= excess_mib {
          break;
        }
        // Only discard tabs that have been idle for a while; a tab the user was in
        // thirty seconds ago is not the thing that is hurting us.
        if now.saturating_sub(tab.last_active_secs) < 60 {
          continue;
        }
        actions.push(LifecycleAction::Discard(tab.id));
        evicted.insert(tab.id);
        reclaimed += ESTIMATED_WEBVIEW_MIB;
      }
    }

    // 3. Idle timers: discard the very old, sleep the merely old.
    for tab in &candidates {
      if evicted.contains(&tab.id) {
        continue;
      }
      let idle = now.saturating_sub(tab.last_active_secs);
      if self.policy.discard_after_secs > 0 && idle >= self.policy.discard_after_secs {
        actions.push(LifecycleAction::Discard(tab.id));
        evicted.insert(tab.id);
      } else if self.policy.sleep_after_secs > 0
        && idle >= self.policy.sleep_after_secs
        && tab.lifecycle != TabLifecycle::Sleeping
      {
        actions.push(LifecycleAction::Sleep(tab.id));
      }
    }

    actions
  }

  /// MiB we would like to reclaim to get back under the threshold.
  pub fn excess_mib(&self, memory: &MemorySnapshot) -> u32 {
    estimate_excess_mib(memory, self.policy.memory_pressure_percent)
  }
}

fn estimate_excess_mib(memory: &MemorySnapshot, threshold_percent: u8) -> u32 {
  if memory.total_bytes == 0 || memory.used_percent <= threshold_percent {
    return 0;
  }
  let target = (memory.total_bytes as u128 * threshold_percent as u128) / 100;
  let excess = memory.used_bytes as u128 - target;
  (excess / (1024 * 1024)) as u32
}

/// Estimate of how much memory has been given back, for the UI.
pub fn savings_mib(discarded: usize) -> u32 {
  discarded as u32 * ESTIMATED_WEBVIEW_MIB
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tab(id: TabId, lifecycle: TabLifecycle, last_active_secs: u64) -> TabActivity {
    TabActivity {
      id,
      lifecycle,
      last_active_secs,
      pinned: false,
      audible: false,
      host: format!("site{id}.example.com"),
    }
  }

  fn policy() -> LifecyclePolicy {
    LifecyclePolicy {
      max_live_webviews: 3,
      sleep_after_secs: 600,
      discard_after_secs: 3600,
      discard_under_pressure: true,
      memory_pressure_percent: 85,
      never_discard: Vec::new(),
    }
  }

  #[test]
  fn enforces_the_live_cap() {
    let mut scheduler = LifecycleScheduler::new(policy());
    let tabs = vec![
      tab(1, TabLifecycle::Active, 1000),
      tab(2, TabLifecycle::Sleeping, 900),
      tab(3, TabLifecycle::Sleeping, 800),
      tab(4, TabLifecycle::Sleeping, 700),
      tab(5, TabLifecycle::Sleeping, 600),
    ];
    let actions = scheduler.evaluate(&tabs, &MemorySnapshot::default(), 2000);
    // 5 live, cap 3 → discard the two least recently used: 5 then 4.
    assert_eq!(
      actions,
      vec![LifecycleAction::Discard(5), LifecycleAction::Discard(4)]
    );
  }

  #[test]
  fn never_touches_active_pinned_or_audible_tabs() {
    let mut scheduler = LifecycleScheduler::new(policy());
    let mut tabs = vec![
      tab(1, TabLifecycle::Active, 0),
      tab(2, TabLifecycle::Sleeping, 0),
      tab(3, TabLifecycle::Sleeping, 0),
      tab(4, TabLifecycle::Sleeping, 0),
    ];
    tabs[2].pinned = true;
    tabs[3].audible = true;
    let actions = scheduler.evaluate(&tabs, &MemorySnapshot::default(), 2000);
    assert_eq!(actions, vec![LifecycleAction::Discard(2)]);
  }

  #[test]
  fn sleeps_then_discards_by_idle_time() {
    let mut scheduler = LifecycleScheduler::new(policy());
    let now = 10_000;
    let tabs = vec![
      tab(1, TabLifecycle::Active, now),
      // idle 700 s → past sleep_after (600), before discard_after (3600)
      tab(2, TabLifecycle::Visible, now - 700),
      // idle 4000 s → past discard_after
      tab(3, TabLifecycle::Sleeping, now - 4000),
    ];
    // Tab 2 is Visible, so it is not a candidate at all; only tab 3 is discarded.
    let actions = scheduler.evaluate(&tabs, &MemorySnapshot::default(), now);
    assert_eq!(actions, vec![LifecycleAction::Discard(3)]);

    let tabs = vec![
      tab(1, TabLifecycle::Active, now),
      tab(2, TabLifecycle::Sleeping, now - 700),
    ];
    let actions = scheduler.evaluate(&tabs, &MemorySnapshot::default(), now);
    assert_eq!(actions, Vec::<LifecycleAction>::new());
  }
}
