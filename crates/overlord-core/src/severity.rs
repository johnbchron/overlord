use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::error::ParseRefError;

/// Fixed severity tiers (SPEC.md section 8).
///
/// Severity is entirely operator-defined: overlord imposes no floor or
/// ceiling, and a check may override the tier's default weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
  Critical,
  High,
  Medium,
  Low,
  Info,
}

impl Severity {
  pub const ALL: [Self; 5] = [
    Self::Critical,
    Self::High,
    Self::Medium,
    Self::Low,
    Self::Info,
  ];

  /// Tier default weight. PLAN.md section 8 open question 1: these are a
  /// starting point, and any check may override them.
  #[must_use]
  pub fn default_weight(self) -> i64 {
    match self {
      Self::Critical => 100,
      Self::High => 50,
      Self::Medium => 20,
      Self::Low => 5,
      Self::Info => 1,
    }
  }

  /// Whether the Violations board collapses this tier by default
  /// (SPEC.md section 5).
  #[must_use]
  pub fn collapsed_by_default(self) -> bool {
    matches!(self, Self::Low | Self::Info)
  }

  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Critical => "critical",
      Self::High => "high",
      Self::Medium => "medium",
      Self::Low => "low",
      Self::Info => "info",
    }
  }
}

/// Orders worst-first, so `sort()` puts `critical` at the top of the board.
impl Ord for Severity {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    fn rank(s: Severity) -> u8 {
      match s {
        Severity::Critical => 0,
        Severity::High => 1,
        Severity::Medium => 2,
        Severity::Low => 3,
        Severity::Info => 4,
      }
    }
    rank(*self).cmp(&rank(*other))
  }
}

impl PartialOrd for Severity {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    Some(self.cmp(other))
  }
}

impl fmt::Display for Severity {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl FromStr for Severity {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "critical" => Ok(Self::Critical),
      "high" => Ok(Self::High),
      "medium" => Ok(Self::Medium),
      "low" => Ok(Self::Low),
      "info" => Ok(Self::Info),
      other => Err(ParseRefError::UnknownSeverity(other.to_owned())),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sorts_worst_first() {
    let mut s = vec![Severity::Low, Severity::Critical, Severity::Medium];
    s.sort();
    assert_eq!(s, [Severity::Critical, Severity::Medium, Severity::Low]);
  }

  #[test]
  fn all_tiers_round_trip() {
    for s in Severity::ALL {
      assert_eq!(s.to_string().parse::<Severity>().unwrap(), s);
    }
  }

  #[test]
  fn default_weights_are_strictly_decreasing() {
    let w: Vec<_> = Severity::ALL.iter().map(|s| s.default_weight()).collect();
    assert!(w.windows(2).all(|p| p[0] > p[1]), "{w:?}");
  }
}
