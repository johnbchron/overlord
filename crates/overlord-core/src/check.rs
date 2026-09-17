use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
  error::ParseRefError,
  ids::{CheckId, EntityType, Revision, SubjectKind, SystemId, SystemKind},
  severity::Severity,
  violation::Evidence,
};

/// A scope restriction naming either a system instance or a system kind
/// (SPEC.md section 7's `systems`, and the argument to `has_entity`).
///
/// The two namespaces overlap, so resolution is ordered and documented:
/// a selector that spells a known system kind means the kind. An operator
/// who names a system instance `idp` cannot select it alone — a trade the
/// spec already accepts by letting one field mean both.
#[derive(
  Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(untagged)]
pub enum SystemSelector {
  Kind(SystemKind),
  Id(SystemId),
}

impl SystemSelector {
  /// Whether this selector matches a concrete system.
  #[must_use]
  pub fn matches(&self, id: &SystemId, kind: SystemKind) -> bool {
    match self {
      Self::Kind(k) => *k == kind,
      Self::Id(i) => i == id,
    }
  }
}

impl fmt::Display for SystemSelector {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Kind(k) => write!(f, "{k}"),
      Self::Id(i) => write!(f, "{i}"),
    }
  }
}

impl FromStr for SystemSelector {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if s.is_empty() {
      return Err(ParseRefError::Empty);
    }
    Ok(
      s.parse::<SystemKind>()
        .map_or_else(|_| Self::Id(SystemId::new(s)), Self::Kind),
    )
  }
}

/// The operator-authored part of a check (SPEC.md section 7).
///
/// `enabled` is deliberately absent: it is set by `check.enable` and
/// `check.disable`, never by an upsert, so editing a rule cannot
/// accidentally turn it on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckDraft {
  pub id: CheckId,
  pub name: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub rationale: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub remediation: Option<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub references: Vec<String>,
  pub severity: Severity,
  /// Overrides [`Severity::default_weight`] for ranking and scoring.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub weight: Option<i64>,
  pub applies_to: SubjectKind,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub systems: Vec<SystemSelector>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub entity_types: Vec<EntityType>,
  /// Expression source (SPEC.md section 7).
  pub condition: String,
  /// Skip subjects that still have unreviewed link suggestions, so an
  /// un-linked-yet account does not generate noise.
  #[serde(default)]
  pub suppress_if_pending_links: bool,
}

impl CheckDraft {
  /// The weight used for ranking and scoring.
  #[must_use]
  pub fn effective_weight(&self) -> i64 {
    self
      .weight
      .unwrap_or_else(|| self.severity.default_weight())
  }
}

/// A check as projected: the latest draft plus the state overlay that
/// only `check.enable` / `check.disable` can change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckRecord {
  #[serde(flatten)]
  pub draft:    CheckDraft,
  pub revision: Revision,
  pub enabled:  bool,
}

impl CheckRecord {
  #[must_use]
  pub fn pinned(&self) -> (CheckId, Revision) {
    (self.draft.id.clone(), self.revision)
  }
}

/// One sample recorded by a dry-run (SPEC.md section 7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DryrunSample {
  pub subject:  crate::ids::SubjectRef,
  pub evidence: Evidence,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_selector_spelling_a_kind_means_the_kind() {
    let s: SystemSelector = "idp".parse().unwrap();
    assert_eq!(s, SystemSelector::Kind(SystemKind::Idp));
    assert!(s.matches(&SystemId::new("okta-prod"), SystemKind::Idp));
    assert!(!s.matches(&SystemId::new("okta-prod"), SystemKind::Mdm));
  }

  #[test]
  fn an_unknown_selector_means_a_system_id() {
    let s: SystemSelector = "okta-prod".parse().unwrap();
    assert_eq!(s, SystemSelector::Id(SystemId::new("okta-prod")));
    assert!(s.matches(&SystemId::new("okta-prod"), SystemKind::Idp));
    assert!(!s.matches(&SystemId::new("okta-dev"), SystemKind::Idp));
  }

  #[test]
  fn weight_falls_back_to_the_tier_default() {
    let mut d = CheckDraft {
      id: CheckId::new("idp-mfa-missing"),
      name: "MFA missing".to_owned(),
      description: None,
      rationale: None,
      remediation: None,
      references: Vec::new(),
      severity: Severity::Critical,
      weight: None,
      applies_to: SubjectKind::Entity,
      systems: Vec::new(),
      entity_types: Vec::new(),
      condition: "status == \"active\" and not mfa_enrolled".to_owned(),
      suppress_if_pending_links: false,
    };
    assert_eq!(d.effective_weight(), 100);
    d.weight = Some(250);
    assert_eq!(d.effective_weight(), 250);
  }
}
