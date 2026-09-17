use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{error::ParseRefError, value::Value};

/// The lifecycle states of SPEC.md section 9.
///
/// There is deliberately no manual `resolve`: auto-resolve is the only
/// close, because marking something fixed that the next sweep still sees
/// would be a lie the system then has to reconcile.
#[derive(
  Debug,
  Clone,
  Copy,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  Hash,
  Serialize,
  Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ViolationState {
  Open,
  Acknowledged,
  Suppressed,
  FalsePositive,
  Resolved,
}

impl ViolationState {
  /// Whether this state counts as bad state, and so contributes to a
  /// subject's score (SPEC.md sections 8 and 9).
  ///
  /// `acknowledged` counts: it means seen, not fixed.
  #[must_use]
  pub fn counts_as_bad_state(self) -> bool {
    matches!(self, Self::Open | Self::Acknowledged)
  }

  /// Whether an operator overlay is what put the violation in this state.
  /// An overlay survives a check revision bump but is flagged when the
  /// revision it was applied under is no longer current.
  #[must_use]
  pub fn is_overlay(self) -> bool {
    matches!(
      self,
      Self::Acknowledged | Self::Suppressed | Self::FalsePositive
    )
  }

  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Open => "open",
      Self::Acknowledged => "acknowledged",
      Self::Suppressed => "suppressed",
      Self::FalsePositive => "false_positive",
      Self::Resolved => "resolved",
    }
  }
}

impl fmt::Display for ViolationState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl FromStr for ViolationState {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "open" => Ok(Self::Open),
      "acknowledged" => Ok(Self::Acknowledged),
      "suppressed" => Ok(Self::Suppressed),
      "false_positive" => Ok(Self::FalsePositive),
      "resolved" => Ok(Self::Resolved),
      other => Err(ParseRefError::UnknownViolationState(other.to_owned())),
    }
  }
}

/// Why a violation is suppressed (SPEC.md section 9).
///
/// This replaces the separate exception and override verbs; the
/// distinction between them was only ever the reason.
#[derive(
  Debug,
  Clone,
  Copy,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  Hash,
  Serialize,
  Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SuppressReason {
  AcceptedRisk,
  BadSourceData,
  Expected,
}

impl SuppressReason {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::AcceptedRisk => "accepted_risk",
      Self::BadSourceData => "bad_source_data",
      Self::Expected => "expected",
    }
  }
}

impl fmt::Display for SuppressReason {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl FromStr for SuppressReason {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "accepted_risk" => Ok(Self::AcceptedRisk),
      "bad_source_data" => Ok(Self::BadSourceData),
      "expected" => Ok(Self::Expected),
      other => Err(ParseRefError::UnknownSuppressReason(other.to_owned())),
    }
  }
}

/// Why an episode closed. Recorded on the resolving event so history
/// explains itself without re-deriving anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveReason {
  /// The condition no longer holds.
  ConditionCleared,
  /// The check was disabled (SPEC.md section 6.5).
  CheckDisabled,
  /// The subject's latest fact is a tombstone, so it is out of scope
  /// entirely (SPEC.md section 6.1).
  SubjectAbsent,
  /// The check no longer scopes this subject after a revision.
  OutOfScope,
}

/// One entry in a violation's history (SPEC.md section 6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationEventKind {
  Opened,
  Regressed,
  Acknowledged,
  Suppressed,
  FalsePositive,
  Revoked,
  SuppressionExpired,
  Cleared,
}

impl ViolationEventKind {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Opened => "opened",
      Self::Regressed => "regressed",
      Self::Acknowledged => "acknowledged",
      Self::Suppressed => "suppressed",
      Self::FalsePositive => "false_positive",
      Self::Revoked => "revoked",
      Self::SuppressionExpired => "suppression_expired",
      Self::Cleared => "cleared",
    }
  }
}

impl fmt::Display for ViolationEventKind {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

/// One evaluated leaf that contributed to a condition being true.
///
/// SPEC.md section 7: evidence is what makes the board actionable and the
/// audit trail meaningful, and it is what dry-run shows as samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceLeaf {
  /// The source text of the leaf, e.g. `mfa_enrolled` or
  /// `count(groups where external)`.
  pub expr: String,
  /// What it evaluated to.
  pub value: Value,
  /// The facts the value was read from. Usually one; person-scoped
  /// checks reach across several entities and so several facts.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub fact_ids: Vec<i64>,
}

/// The evidence captured when an episode opens or is re-evaluated.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
  pub leaves: Vec<EvidenceLeaf>,
}

impl Evidence {
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.leaves.is_empty()
  }

  pub fn push(&mut self, expr: impl Into<String>, value: Value) {
    self.leaves.push(EvidenceLeaf {
      expr: expr.into(),
      value,
      fact_ids: Vec::new(),
    });
  }

  /// Attribute every leaf collected so far to `fact_ids`.
  ///
  /// The evaluator knows which facts a subject was built from; the
  /// expression interpreter does not, so attribution happens here rather
  /// than being threaded through evaluation.
  pub fn attribute(&mut self, fact_ids: &[i64]) {
    for leaf in &mut self.leaves {
      if leaf.fact_ids.is_empty() {
        leaf.fact_ids = fact_ids.to_vec();
      }
    }
  }
}

/// Whether a connector returned a full enumeration (SPEC.md section 11).
///
/// Only a complete snapshot may produce tombstones. A partial one carries
/// the reason, which the coverage view shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Completeness {
  Complete,
  Partial { reason: String },
}

impl Completeness {
  #[must_use]
  pub fn is_complete(&self) -> bool {
    matches!(self, Self::Complete)
  }

  #[must_use]
  pub fn reason(&self) -> Option<&str> {
    match self {
      Self::Complete => None,
      Self::Partial { reason } => Some(reason),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_open_and_acknowledged_count_as_bad_state() {
    use ViolationState::{
      Acknowledged, FalsePositive, Open, Resolved, Suppressed,
    };
    assert!(Open.counts_as_bad_state());
    assert!(Acknowledged.counts_as_bad_state(), "seen is not fixed");
    for s in [Suppressed, FalsePositive, Resolved] {
      assert!(!s.counts_as_bad_state(), "{s}");
    }
  }

  #[test]
  fn states_round_trip() {
    for s in [
      ViolationState::Open,
      ViolationState::Acknowledged,
      ViolationState::Suppressed,
      ViolationState::FalsePositive,
      ViolationState::Resolved,
    ] {
      assert_eq!(s.to_string().parse::<ViolationState>().unwrap(), s);
    }
  }

  #[test]
  fn evidence_attribution_does_not_overwrite_explicit_fact_ids() {
    let mut e = Evidence::default();
    e.push("mfa_enrolled", Value::Bool(false));
    e.leaves.push(EvidenceLeaf {
      expr: "entity(\"idp\").status".to_owned(),
      value: Value::from("active"),
      fact_ids: vec![7],
    });
    e.attribute(&[42]);
    assert_eq!(e.leaves[0].fact_ids, [42]);
    assert_eq!(e.leaves[1].fact_ids, [7]);
  }
}
