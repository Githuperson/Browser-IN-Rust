//! Wall clock helpers. Deliberately no `chrono` dependency: we need "seconds since
//! epoch" for storage and "3 minutes ago" for the UI, and nothing else.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch. Saturates at 0 for pre-1970 clocks.
pub fn now_secs() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0)
}

/// Milliseconds since the Unix epoch — used for monotonic-ish ordering of events
/// that happen inside the same second (downloads, tab activation).
pub fn now_millis() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .unwrap_or(0)
}

/// Human "time ago" string, coarse on purpose.
pub fn ago(secs: u64) -> String {
  let now = now_secs();
  let delta = now.saturating_sub(secs);
  match delta {
    0..=59 => "just now".to_string(),
    60..=3599 => format!("{} min ago", delta / 60),
    3600..=86_399 => {
      let h = delta / 3600;
      format!("{h} hour{} ago", plural(h))
    }
    86_400..=604_799 => {
      let d = delta / 86_400;
      format!("{d} day{} ago", plural(d))
    }
    604_800..=2_591_999 => {
      let w = delta / 604_800;
      format!("{w} week{} ago", plural(w))
    }
    _ => {
      let m = delta / 2_592_000;
      format!("{m} month{} ago", plural(m))
    }
  }
}

/// `14:05` / `09:32` — clock time for anything that happened today.
pub fn clock(secs: u64) -> String {
  // Civil-from-days (Howard Hinnant's algorithm) — avoids pulling in a date crate.
  let (y, m, d) = civil_from_days((secs / 86_400) as i64);
  let _ = (y, m, d);
  let secs_of_day = secs % 86_400;
  format!("{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60)
}

/// `2026-10-09` — ISO date, used for history day headers.
pub fn iso_date(secs: u64) -> String {
  let (y, m, d) = civil_from_days((secs / 86_400) as i64);
  format!("{y:04}-{m:02}-{d:02}")
}

/// True when both timestamps fall on the same calendar day (UTC).
pub fn same_day(a: u64, b: u64) -> bool {
  a / 86_400 == b / 86_400
}

fn plural(n: u64) -> &'static str {
  if n == 1 {
    ""
  } else {
    "s"
  }
}

/// Convert days-since-epoch into (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
  let z = days + 719_468;
  let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
  let doe = (z - era * 146_097) as i64; // [0, 146096]
  let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
  let y = yoe + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
  let mp = (5 * doy + 2) / 153; // [0, 11]
  let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
  let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
  (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn civil_conversion_matches_known_dates() {
    // 1970-01-01 is day 0; 2026-01-01 is day 20454; 2026-10-09 is +281 days.
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(20_454), (2026, 1, 1));
    let (y, m, d) = civil_from_days(20_735);
    assert_eq!((y, m, d), (2026, 10, 9), "got {y}-{m}-{d}");
  }

  #[test]
  fn clock_formats() {
    // 12:34 UTC on 2026-10-09
    let secs = 20_735 * 86_400 + 12 * 3600 + 34 * 60;
    assert_eq!(clock(secs), "12:34");
    assert_eq!(iso_date(secs), "2026-10-09");
  }
}
