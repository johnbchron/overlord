use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// An instant, always UTC, stored and displayed as ISO-8601.
///
/// SPEC.md section 13: overlord records time only. Evaluation's sole clock
/// is a sweep's `started_at`; nothing else may call a system clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(jiff::Timestamp);

impl Timestamp {
  /// Read the wall clock.
  ///
  /// Legitimate callers are narrow: recording `command.at`, a sweep's
  /// `started_at`, and connector observation times. The evaluator must
  /// never call this — see `clippy.toml`, which denies the underlying
  /// constructor outside this module.
  #[must_use]
  #[allow(clippy::disallowed_methods)]
  pub fn now() -> Self {
    Self(jiff::Timestamp::now())
  }

  #[must_use]
  pub const fn from_jiff(ts: jiff::Timestamp) -> Self {
    Self(ts)
  }

  #[must_use]
  pub const fn as_jiff(self) -> jiff::Timestamp {
    self.0
  }

  /// This instant minus `days`, for the expression language's
  /// `days_ago(n)` (SPEC.md section 7).
  ///
  /// overlord is UTC-only, so a day is exactly 24 hours and the
  /// calculation needs no time zone. Saturates rather than wrapping; a
  /// check asking for a million days ago means "everything", not a panic.
  #[must_use]
  pub fn minus_days(self, days: i64) -> Self {
    let span = jiff::Span::new().try_hours(days.saturating_mul(24));
    match span.and_then(|s| self.0.checked_sub(s)) {
      Ok(ts) => Self(ts),
      Err(_) => Self(jiff::Timestamp::MIN),
    }
  }

  /// Whether `self` is at or after `other`.
  #[must_use]
  pub fn reached(self, other: Self) -> bool {
    self.0 >= other.0
  }
}

/// Fixed-width UTC ISO-8601, nanosecond precision.
///
/// This is the only rendering overlord stores, and the width is the
/// point. Timestamps live in `TEXT` columns, and the store both orders
/// by them (the board sorts by `opened_at`) and compares them
/// (suppression expiry is `suppress_until <= started_at`). With a
/// variable-precision rendering, `…:00Z` and `…:00.5Z` compare
/// backwards, because `.` sorts before `Z` — a suppression would expire
/// early or late by up to a second, silently. Padding the fraction makes
/// lexicographic order and chronological order the same thing, so the
/// SQL is correct by construction rather than by care.
fn print_fixed(ts: jiff::Timestamp) -> String {
  static PRINTER: jiff::fmt::temporal::DateTimePrinter =
    jiff::fmt::temporal::DateTimePrinter::new().precision(Some(9));
  let mut out = String::with_capacity(30);
  PRINTER
    .print_timestamp(&ts, &mut out)
    .expect("writing to a String cannot fail");
  out
}

impl fmt::Display for Timestamp {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&print_fixed(self.0))
  }
}

impl FromStr for Timestamp {
  type Err = CoreError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    s.parse::<jiff::Timestamp>()
      .map(Self)
      .map_err(|e| CoreError::Timestamp(e.to_string()))
  }
}

impl Serialize for Timestamp {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(&self.0)
  }
}

impl<'de> Deserialize<'de> for Timestamp {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let s = String::deserialize(d)?;
    s.parse().map_err(serde::de::Error::custom)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
  }

  #[test]
  fn round_trips_through_iso8601() {
    let t = ts("2026-01-15T09:30:00Z");
    assert_eq!(ts(&t.to_string()), t);
  }

  #[test]
  fn minus_days_is_exact_in_utc() {
    assert_eq!(
      ts("2026-01-15T00:00:00Z").minus_days(90),
      ts("2025-10-17T00:00:00Z")
    );
  }

  #[test]
  fn minus_days_saturates_instead_of_panicking() {
    let t = ts("2026-01-15T00:00:00Z").minus_days(i64::MAX);
    assert_eq!(t, Timestamp::from_jiff(jiff::Timestamp::MIN));
  }

  #[test]
  fn text_order_matches_chronological_order() {
    // The store orders and compares timestamps as SQL text, so the
    // rendering must be sortable. A variable-precision one is not:
    // `.` sorts before `Z`, so a sub-second timestamp would compare as
    // earlier than the whole second it follows.
    let earlier = ts("2026-01-15T00:00:00Z");
    let later = ts("2026-01-15T00:00:00.5Z");
    assert!(earlier < later);
    assert!(
      earlier.to_string() < later.to_string(),
      "{} should sort before {}",
      earlier,
      later
    );
    assert_eq!(earlier.to_string().len(), later.to_string().len());
    assert_eq!(ts(&later.to_string()), later, "and still round-trip");
  }

  #[test]
  fn offsets_normalize_to_utc_for_comparison() {
    assert_eq!(ts("2026-01-15T01:00:00+01:00"), ts("2026-01-15T00:00:00Z"));
  }
}
