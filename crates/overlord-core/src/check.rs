use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
  error::ParseRefError,
  ids::{CheckId, EntityType, Revision, SubjectKind, SystemId, SystemKind},
  severity::Severity,
  violation::Evidence,
};

/// The prefix that makes a selector name a connector rather than a
/// system. Explicit because the alternative is a third namespace
/// silently overlapping the other two.
pub const CONNECTOR_PREFIX: &str = "connector:";

/// A scope restriction naming a system instance, a system kind, or the
/// connector a system is read through (SPEC.md section 7's `systems`,
/// and the argument to `has_entity`).
///
/// The kind and id namespaces overlap, so resolution is ordered and
/// documented: a selector that spells a known system kind means the
/// kind. An operator who names a system instance `idp` cannot select it
/// alone — a trade the spec already accepts by letting one field mean
/// both.
///
/// A connector is the third answer to "which systems", and the one the
/// other two cannot give: `sso` is every access and SSO system, and a
/// system id is one console, but "every console read through
/// `unifi-access`" is neither. It is spelled with a prefix rather than
/// folded into the same bare-word resolution, because a third
/// overlapping namespace would make `unifi-access` mean different things
/// depending on what a deployment happened to name its systems.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SystemSelector {
  Kind(SystemKind),
  Id(SystemId),
  /// `connector:<name>`, as the connector names itself.
  Connector(String),
}

// Serialized as its own spelling, not by `untagged`.
//
// Every variant is a bare string, so `untagged` cannot tell them apart:
// it tries them in declaration order and `Id` accepts anything, so a
// stored `connector:unifi-access` came back as a system id named
// `connector:unifi-access` and silently matched nothing. Going through
// `Display`/`FromStr` makes the round trip the same resolution an
// operator's typed selector gets — and produces byte-identical JSON for
// kinds and ids, so revisions written before this still read.
impl Serialize for SystemSelector {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(self)
  }
}

impl<'de> Deserialize<'de> for SystemSelector {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let s = String::deserialize(d)?;
    s.parse().map_err(serde::de::Error::custom)
  }
}

impl SystemSelector {
  /// Whether this selector matches a concrete system.
  ///
  /// `connector` is the name of the connector that read the system, as
  /// the last sweep recorded it. It is `None` for a system that has
  /// never been swept under a build that recorded one, and a connector
  /// selector never matches that: an unknown connector is not evidence
  /// of a particular one.
  #[must_use]
  pub fn matches(
    &self,
    id: &SystemId,
    kind: SystemKind,
    connector: Option<&str>,
  ) -> bool {
    match self {
      Self::Kind(k) => *k == kind,
      Self::Id(i) => i == id,
      Self::Connector(c) => connector == Some(c.as_str()),
    }
  }
}

impl fmt::Display for SystemSelector {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Kind(k) => write!(f, "{k}"),
      Self::Id(i) => write!(f, "{i}"),
      Self::Connector(c) => write!(f, "{CONNECTOR_PREFIX}{c}"),
    }
  }
}

impl FromStr for SystemSelector {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if s.is_empty() {
      return Err(ParseRefError::Empty);
    }
    if let Some(name) = s.strip_prefix(CONNECTOR_PREFIX) {
      if name.is_empty() {
        return Err(ParseRefError::Empty);
      }
      return Ok(Self::Connector(name.to_owned()));
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
    assert!(s.matches(&SystemId::new("okta-prod"), SystemKind::Idp, None));
    assert!(!s.matches(&SystemId::new("okta-prod"), SystemKind::Mdm, None));
  }

  #[test]
  fn an_unknown_selector_means_a_system_id() {
    let s: SystemSelector = "okta-prod".parse().unwrap();
    assert_eq!(s, SystemSelector::Id(SystemId::new("okta-prod")));
    assert!(s.matches(&SystemId::new("okta-prod"), SystemKind::Idp, None));
    assert!(!s.matches(&SystemId::new("okta-dev"), SystemKind::Idp, None));
  }

  #[test]
  fn a_prefixed_selector_means_the_connector_that_read_the_system() {
    let s: SystemSelector = "connector:unifi-access".parse().unwrap();
    assert_eq!(s, SystemSelector::Connector("unifi-access".to_owned()));
    assert_eq!(s.to_string(), "connector:unifi-access");

    let hq = SystemId::new("access-hq");
    assert!(s.matches(&hq, SystemKind::Sso, Some("unifi-access")));
    // A second console on the same connector, which is the point.
    assert!(s.matches(
      &SystemId::new("access-warehouse"),
      SystemKind::Sso,
      Some("unifi-access")
    ));
    // Another SSO system, which the `sso` kind would have caught.
    assert!(!s.matches(
      &SystemId::new("okta-prod"),
      SystemKind::Sso,
      Some("okta")
    ));
    // A system whose connector was never recorded.
    assert!(!s.matches(&hq, SystemKind::Sso, None));
  }

  #[test]
  fn a_connector_name_is_only_a_connector_when_it_says_so() {
    // Bare, it is a system id — a deployment may well have named a
    // system after its connector, and that spelling must not change
    // meaning underneath it.
    let bare: SystemSelector = "unifi-access".parse().unwrap();
    assert_eq!(bare, SystemSelector::Id(SystemId::new("unifi-access")));
    assert!(!bare.matches(
      &SystemId::new("access-hq"),
      SystemKind::Sso,
      Some("unifi-access")
    ));

    assert!("connector:".parse::<SystemSelector>().is_err());
  }

  #[test]
  fn every_selector_round_trips_through_its_spelling() {
    for s in ["idp", "workspace", "okta-prod", "connector:unifi-access"] {
      let sel: SystemSelector = s.parse().unwrap();
      assert_eq!(sel.to_string(), s);
      assert_eq!(sel.to_string().parse::<SystemSelector>().unwrap(), sel);

      // And through serde, which is how a check revision is stored. A
      // derived `untagged` could not do this: every variant is a bare
      // string, so `Id` swallowed `connector:` selectors on the way
      // back in and they matched nothing.
      let json = serde_json::to_string(&sel).unwrap();
      assert_eq!(json, format!("\"{s}\""));
      assert_eq!(serde_json::from_str::<SystemSelector>(&json).unwrap(), sel);
    }
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
