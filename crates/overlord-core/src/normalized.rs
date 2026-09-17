use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
  error::ParseRefError,
  ids::{EntityKey, EntityRef, EntityType, SystemId, SystemKind},
  value::Value,
};

/// The normalized lifecycle state of an account (SPEC.md section 11).
///
/// A connector that cannot map a vendor state to one of these must emit
/// `Unknown` rather than guessing; `status == "active"` appears in nearly
/// every check, so a wrong guess is a wrong board.
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
pub enum EntityStatus {
  Active,
  Suspended,
  Deprovisioned,
  Invited,
  Unknown,
}

impl EntityStatus {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Active => "active",
      Self::Suspended => "suspended",
      Self::Deprovisioned => "deprovisioned",
      Self::Invited => "invited",
      Self::Unknown => "unknown",
    }
  }
}

impl fmt::Display for EntityStatus {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl FromStr for EntityStatus {
  type Err = ParseRefError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "active" => Ok(Self::Active),
      "suspended" => Ok(Self::Suspended),
      "deprovisioned" => Ok(Self::Deprovisioned),
      "invited" => Ok(Self::Invited),
      "unknown" => Ok(Self::Unknown),
      other => Err(ParseRefError::UnknownStatus(other.to_owned())),
    }
  }
}

/// Field names the guaranteed overlay core owns. A connector may not
/// shadow one with a mapped vendor field.
pub const RESERVED_FIELDS: [&str; 6] = [
  "system",
  "system_kind",
  "entity_type",
  "entity_key",
  "display_name",
  "status",
];

/// The normalization overlay for one observed entity (SPEC.md section 11).
///
/// Flat at the root, so a check has exactly one vocabulary and one place
/// to look. Vendor-specific detail stays reachable at `raw.<path>`, which
/// the evaluator supplies separately — the overlay never embeds the raw
/// payload, because the two are stored as separate deduplicated blobs.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedRecord {
  pub system: SystemId,
  pub system_kind: SystemKind,
  pub entity_type: EntityType,
  pub entity_key: EntityKey,
  pub display_name: Option<String>,
  pub status: EntityStatus,
  /// Everything else the connector could map, flat at the root.
  extra: BTreeMap<String, Value>,
}

impl NormalizedRecord {
  #[must_use]
  pub fn new(
    system: impl Into<SystemId>,
    system_kind: SystemKind,
    entity_type: impl Into<EntityType>,
    entity_key: impl Into<EntityKey>,
    status: EntityStatus,
  ) -> Self {
    Self {
      system: system.into(),
      system_kind,
      entity_type: entity_type.into(),
      entity_key: entity_key.into(),
      display_name: None,
      status,
      extra: BTreeMap::new(),
    }
  }

  #[must_use]
  pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
    self.display_name = Some(name.into());
    self
  }

  /// Add a mapped field. Returns `false` and changes nothing if the name
  /// collides with the guaranteed core; the connector test suite asserts
  /// this never happens in practice.
  pub fn insert(
    &mut self,
    name: impl Into<String>,
    value: impl Into<Value>,
  ) -> bool {
    let name = name.into();
    if RESERVED_FIELDS.contains(&name.as_str()) {
      return false;
    }
    self.extra.insert(name, value.into());
    true
  }

  #[must_use]
  pub fn with(
    mut self,
    name: impl Into<String>,
    value: impl Into<Value>,
  ) -> Self {
    self.insert(name, value);
    self
  }

  #[must_use]
  pub fn entity_ref(&self) -> EntityRef {
    EntityRef {
      system: self.system.clone(),
      entity_type: self.entity_type.clone(),
      entity_key: self.entity_key.clone(),
    }
  }

  /// Resolve a root field name. Unknown names are `null`, matching
  /// SPEC.md section 7's "a missing path is null".
  #[must_use]
  pub fn get(&self, name: &str) -> Value {
    match name {
      "system" => Value::String(self.system.as_str().to_owned()),
      "system_kind" => Value::String(self.system_kind.as_str().to_owned()),
      "entity_type" => Value::String(self.entity_type.as_str().to_owned()),
      "entity_key" => Value::String(self.entity_key.as_str().to_owned()),
      "display_name" => Value::from(self.display_name.clone()),
      "status" => Value::String(self.status.as_str().to_owned()),
      other => self.extra.get(other).cloned().unwrap_or(Value::Null),
    }
  }

  /// Every root field name present on this record, core first.
  #[must_use]
  pub fn field_names(&self) -> Vec<&str> {
    RESERVED_FIELDS
      .iter()
      .copied()
      .chain(self.extra.keys().map(String::as_str))
      .collect()
  }

  #[must_use]
  pub fn extra(&self) -> &BTreeMap<String, Value> {
    &self.extra
  }

  /// The flat JSON form that is stored as the fact's overlay.
  fn to_map(&self) -> BTreeMap<String, Value> {
    let mut m = self.extra.clone();
    for name in RESERVED_FIELDS {
      m.insert(name.to_owned(), self.get(name));
    }
    m
  }
}

impl Serialize for NormalizedRecord {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    self.to_map().serialize(s)
  }
}

impl<'de> Deserialize<'de> for NormalizedRecord {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    use serde::de::Error as _;
    let mut m = BTreeMap::<String, Value>::deserialize(d)?;

    fn take_str<E: serde::de::Error>(
      m: &mut BTreeMap<String, Value>,
      k: &str,
    ) -> Result<String, E> {
      match m.remove(k) {
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(E::custom(format!(
          "overlay field {k:?} must be a string, found {}",
          other.type_name()
        ))),
        None => Err(E::custom(format!("overlay is missing {k:?}"))),
      }
    }

    let system = SystemId::new(take_str::<D::Error>(&mut m, "system")?);
    let system_kind = take_str::<D::Error>(&mut m, "system_kind")?
      .parse()
      .map_err(D::Error::custom)?;
    let entity_type =
      EntityType::new(take_str::<D::Error>(&mut m, "entity_type")?);
    let entity_key =
      EntityKey::new(take_str::<D::Error>(&mut m, "entity_key")?);
    let status = take_str::<D::Error>(&mut m, "status")?
      .parse()
      .map_err(D::Error::custom)?;
    let display_name = match m.remove("display_name") {
      Some(Value::String(s)) => Some(s),
      _ => None,
    };

    Ok(Self {
      system,
      system_kind,
      entity_type,
      entity_key,
      display_name,
      status,
      extra: m,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> NormalizedRecord {
    NormalizedRecord::new(
      "gws-prod",
      SystemKind::Workspace,
      "user",
      "ada@example.com",
      EntityStatus::Active,
    )
    .with_display_name("Ada Lovelace")
    .with("mfa_enrolled", false)
    .with("is_admin", true)
    .with(
      "last_login_at",
      Value::Timestamp("2025-11-02T08:00:00Z".parse().unwrap()),
    )
    .with("groups", Value::List(vec![Value::from("eng")]))
  }

  #[test]
  fn overlay_is_flat_at_the_root() {
    let json = serde_json::to_value(sample()).unwrap();
    let obj = json.as_object().unwrap();
    for k in ["system", "status", "mfa_enrolled", "is_admin", "groups"] {
      assert!(obj.contains_key(k), "missing root field {k}");
    }
  }

  #[test]
  fn round_trips_with_field_types_intact() {
    let r = sample();
    let back: NormalizedRecord =
      serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(back, r);
    assert_eq!(back.get("last_login_at").type_name(), "timestamp");
    assert_eq!(back.get("mfa_enrolled"), Value::Bool(false));
  }

  #[test]
  fn unknown_field_reads_as_null() {
    assert_eq!(sample().get("department"), Value::Null);
  }

  #[test]
  fn connectors_cannot_shadow_the_guaranteed_core() {
    let mut r = sample();
    assert!(!r.insert("status", "definitely-active"));
    assert_eq!(r.get("status"), Value::from("active"));
  }

  #[test]
  fn deserializing_an_overlay_without_the_core_fails_loudly() {
    let err =
      serde_json::from_str::<NormalizedRecord>(r#"{"status":"active"}"#)
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing"), "{err}");
  }
}
