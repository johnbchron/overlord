use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::error::ParseRefError;

/// The separator used by every stringified reference.
///
/// Segments may not contain it; ids come from vendor systems, so this is
/// checked rather than assumed.
const SEP: char = '/';

macro_rules! newtype_str {
  ($(#[$m:meta])* $name:ident) => {
    $(#[$m])*
    #[derive(
      Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize,
      Deserialize,
    )]
    #[serde(transparent)]
    pub struct $name(String);

    impl $name {
      pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }

      pub fn as_str(&self) -> &str { &self.0 }

      pub fn into_inner(self) -> String { self.0 }
    }

    impl fmt::Display for $name {
      fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
      }
    }

    impl From<&str> for $name {
      fn from(s: &str) -> Self { Self(s.to_owned()) }
    }

    impl From<String> for $name {
      fn from(s: String) -> Self { Self(s) }
    }
  };
}

newtype_str!(
  /// A connected system instance, e.g. `okta-prod` (SPEC.md section 4).
  SystemId
);
newtype_str!(
  /// The kind of object observed within a system, e.g. `user`.
  EntityType
);
newtype_str!(
  /// A stable key for an entity within its system.
  EntityKey
);
newtype_str!(
  /// Operator-chosen, never reused (SPEC.md section 7).
  CheckId
);
newtype_str!(
  /// An authenticated OIDC subject, or a named CLI principal.
  Actor
);

/// The category of a connected system. Checks may scope by kind or by id.
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
#[serde(rename_all = "lowercase")]
pub enum SystemKind {
  Idp,
  Workspace,
  Sso,
  Mdm,
}

impl SystemKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Idp => "idp",
      Self::Workspace => "workspace",
      Self::Sso => "sso",
      Self::Mdm => "mdm",
    }
  }
}

impl fmt::Display for SystemKind {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl FromStr for SystemKind {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "idp" => Ok(Self::Idp),
      "workspace" => Ok(Self::Workspace),
      "sso" => Ok(Self::Sso),
      "mdm" => Ok(Self::Mdm),
      other => Err(ParseRefError::UnknownSystemKind(other.to_owned())),
    }
  }
}

/// A monotonic sweep identifier. Ordering facts by sweep id never depends
/// on a connector's clock (SPEC.md section 6.1).
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
#[serde(transparent)]
pub struct SweepId(pub i64);

impl fmt::Display for SweepId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.0)
  }
}

/// A position in the single global stream sequence.
///
/// PLAN.md section 3.1: facts and commands share one monotonic counter so
/// that replay merges the two streams without consulting any clock.
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
#[serde(transparent)]
pub struct Seq(pub i64);

impl fmt::Display for Seq {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.0)
  }
}

/// Monotonic per check id; each `check.upsert` appends one.
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
#[serde(transparent)]
pub struct Revision(pub u32);

impl Revision {
  pub const FIRST: Self = Self(1);

  pub fn next(self) -> Self { Self(self.0 + 1) }
}

impl fmt::Display for Revision {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.0)
  }
}

/// A unified human (SPEC.md section 6.4).
///
/// Confirmed persons carry a ULID. Unlinked entities are additionally
/// evaluated as *implicit singleton persons* whose uid is derived from the
/// entity ref, so cross-system checks work before linking is complete.
#[derive(
  Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct PersonUid(String);

impl PersonUid {
  pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }

  /// Mint a uid for a person the operator created.
  pub fn generate() -> Self { Self(ulid::Ulid::new().to_string()) }

  /// The derived uid of the implicit singleton person for `entity`.
  ///
  /// Prefixed so an implicit uid is never mistaken for a confirmed one,
  /// and so promotion (SPEC.md section 12) is detectable.
  pub fn implicit(entity: &EntityRef) -> Self {
    Self(format!("implicit:{entity}"))
  }

  pub fn is_implicit(&self) -> bool { self.0.starts_with("implicit:") }

  /// The entity behind an implicit uid, if this uid is implicit.
  pub fn implicit_entity(&self) -> Option<EntityRef> {
    let rest = self.0.strip_prefix("implicit:")?;
    EntityRef::from_str(rest).ok()
  }

  pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for PersonUid {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

/// One account or object observed in one system (SPEC.md section 4).
#[derive(
  Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntityRef {
  pub system:      SystemId,
  pub entity_type: EntityType,
  pub entity_key:  EntityKey,
}

impl EntityRef {
  pub fn new(
    system: impl Into<SystemId>,
    entity_type: impl Into<EntityType>,
    entity_key: impl Into<EntityKey>,
  ) -> Self {
    Self {
      system:      system.into(),
      entity_type: entity_type.into(),
      entity_key:  entity_key.into(),
    }
  }
}

impl fmt::Display for EntityRef {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "{}{SEP}{}{SEP}{}",
      self.system, self.entity_type, self.entity_key
    )
  }
}

impl FromStr for EntityRef {
  type Err = ParseRefError;

  /// `system/entity_type/entity_key`.
  ///
  /// Only the first two separators are structural: an entity key may itself
  /// contain `/` (some vendors use path-shaped ids), so the remainder is
  /// taken whole.
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if s.is_empty() {
      return Err(ParseRefError::Empty);
    }
    let mut parts = s.splitn(3, SEP);
    let (Some(system), Some(entity_type), Some(entity_key)) =
      (parts.next(), parts.next(), parts.next())
    else {
      return Err(ParseRefError::Arity {
        expected: 3,
        found:    s.split(SEP).count(),
      });
    };
    if system.is_empty() || entity_type.is_empty() || entity_key.is_empty() {
      return Err(ParseRefError::Empty);
    }
    Ok(Self::new(system, entity_type, entity_key))
  }
}

/// Which kind of subject a check is evaluated against.
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
#[serde(rename_all = "lowercase")]
pub enum SubjectKind {
  Entity,
  Person,
}

impl SubjectKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Entity => "entity",
      Self::Person => "person",
    }
  }
}

impl fmt::Display for SubjectKind {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl FromStr for SubjectKind {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "entity" => Ok(Self::Entity),
      "person" => Ok(Self::Person),
      other => Err(ParseRefError::UnknownKind(other.to_owned())),
    }
  }
}

/// What a violation is about: `(check_id, subject_ref)` is the stable
/// identity that lets overlays persist across sweeps (SPEC.md section 6.5).
#[derive(
  Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SubjectRef {
  Entity(EntityRef),
  Person(PersonUid),
}

impl SubjectRef {
  pub fn kind(&self) -> SubjectKind {
    match self {
      Self::Entity(_) => SubjectKind::Entity,
      Self::Person(_) => SubjectKind::Person,
    }
  }

  pub fn as_entity(&self) -> Option<&EntityRef> {
    match self {
      Self::Entity(e) => Some(e),
      Self::Person(_) => None,
    }
  }

  pub fn as_person(&self) -> Option<&PersonUid> {
    match self {
      Self::Person(p) => Some(p),
      Self::Entity(_) => None,
    }
  }
}

impl fmt::Display for SubjectRef {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Entity(e) => write!(f, "entity{SEP}{e}"),
      Self::Person(p) => write!(f, "person{SEP}{p}"),
    }
  }
}

impl FromStr for SubjectRef {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let (kind, rest) = s.split_once(SEP).ok_or(ParseRefError::Arity {
      expected: 2,
      found:    1,
    })?;
    match kind {
      "entity" => Ok(Self::Entity(EntityRef::from_str(rest)?)),
      "person" => {
        if rest.is_empty() {
          Err(ParseRefError::Empty)
        } else {
          Ok(Self::Person(PersonUid::new(rest)))
        }
      }
      other => Err(ParseRefError::UnknownKind(other.to_owned())),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn entity_ref_round_trips() {
    let e = EntityRef::new("gws-prod", "user", "ada@example.com");
    assert_eq!(EntityRef::from_str(&e.to_string()).unwrap(), e);
  }

  #[test]
  fn entity_key_may_contain_separators() {
    let e = EntityRef::new("intune", "device", "ou=eng/dev/laptop-1");
    let parsed = EntityRef::from_str(&e.to_string()).unwrap();
    assert_eq!(parsed.entity_key.as_str(), "ou=eng/dev/laptop-1");
    assert_eq!(parsed, e);
  }

  #[test]
  fn subject_ref_round_trips_both_arms() {
    let entity =
      SubjectRef::Entity(EntityRef::new("okta", "user", "a@example.com"));
    let person = SubjectRef::Person(PersonUid::new("01J0ABCD"));
    for s in [entity, person] {
      assert_eq!(SubjectRef::from_str(&s.to_string()).unwrap(), s);
    }
  }

  #[test]
  fn implicit_person_uid_carries_its_entity() {
    let e = EntityRef::new("gws-prod", "user", "ada@example.com");
    let uid = PersonUid::implicit(&e);
    assert!(uid.is_implicit());
    assert_eq!(uid.implicit_entity().unwrap(), e);
  }

  #[test]
  fn generated_person_uid_is_not_implicit() {
    assert!(!PersonUid::generate().is_implicit());
  }

  #[test]
  fn bad_refs_are_rejected() {
    assert!(EntityRef::from_str("").is_err());
    assert!(EntityRef::from_str("only-system").is_err());
    assert!(EntityRef::from_str("sys/type/").is_err());
    assert!(SubjectRef::from_str("wat/x").is_err());
  }
}
