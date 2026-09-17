//! Normalization: raw vendor payload to the guaranteed overlay.
//!
//! SPEC.md section 11 requires normalization to be versioned and
//! operator-visible: a connector ships a default ruleset, and every
//! change — shipped or operator-made — is a `normalization.upsert`
//! revision in the command stream. That rules out expressing the mapping
//! in Rust, because an operator cannot edit a compiled function. So a
//! ruleset is data: JSON, stored in the stream, applied here.

use std::collections::BTreeMap;

use overlord_core::{
  EntityStatus, NormalizedRecord, RESERVED_FIELDS, SystemId, SystemKind,
  Timestamp, Value,
};
use serde::{Deserialize, Serialize};

/// How to read one field out of a raw payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldRule {
  /// Dotted path into the raw payload.
  pub path:    String,
  /// What the value should become. Absent means "leave it as it is".
  #[serde(rename = "as", default)]
  pub coerce:  Option<Coerce>,
  /// Used when the path is missing. Absent means the field is null,
  /// which is the right default: null is data that was not collected,
  /// and the check language handles it deliberately.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub default: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Coerce {
  String,
  Number,
  Boolean,
  Timestamp,
  List,
}

/// How to map a vendor's lifecycle state onto the five overlord knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusRule {
  pub path:    String,
  /// Keyed by the raw value's string form, so `true` maps as `"true"`.
  pub map:     BTreeMap<String, EntityStatus>,
  /// What an unmapped value becomes. A connector that cannot map a
  /// vendor state must land on `unknown` rather than guessing:
  /// `status == "active"` appears in nearly every check, so a wrong
  /// guess is a wrong board.
  #[serde(default = "unknown_status")]
  pub default: EntityStatus,
}

fn unknown_status() -> EntityStatus { EntityStatus::Unknown }

/// A versioned normalization ruleset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ruleset {
  pub id:           String,
  pub version:      String,
  pub system_kind:  SystemKind,
  pub entity_type:  String,
  /// Where the stable key within the system comes from.
  pub entity_key:   FieldRule,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub display_name: Option<FieldRule>,
  pub status:       StatusRule,
  /// Everything else, flat at the root of the overlay.
  #[serde(default)]
  pub fields:       BTreeMap<String, FieldRule>,
}

/// The result of normalizing one payload.
#[derive(Debug, Clone)]
pub struct Normalized {
  pub record:   NormalizedRecord,
  /// Values the ruleset could not convert. Surfaced on the coverage
  /// view rather than dropped: a timestamp that silently became null
  /// would make a dormancy check quietly wrong.
  pub warnings: Vec<String>,
}

/// Why a payload could not be normalized at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizeError(pub String);

impl std::fmt::Display for NormalizeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

impl Ruleset {
  /// Apply this ruleset to one raw payload.
  ///
  /// # Errors
  /// Only when the entity key is missing or empty: an entity with no
  /// stable key cannot be tracked across sweeps, so it is refused
  /// rather than given a synthetic one.
  pub fn apply(
    &self,
    system: &SystemId,
    raw: &serde_json::Value,
  ) -> Result<Normalized, NormalizeError> {
    let raw = Value::from(raw);
    let mut warnings = Vec::new();

    let key = read(&raw, &self.entity_key, &mut warnings);
    let key = match &key {
      Value::String(s) if !s.is_empty() => s.clone(),
      Value::Number(n) => n.to_string(),
      _ => {
        return Err(NormalizeError(format!(
          "no entity key at {:?}; an entity with no stable key cannot be \
           tracked across sweeps",
          self.entity_key.path
        )));
      }
    };

    let status_raw = raw.get_path(&self.status.path);
    let status = status_key(&status_raw)
      .and_then(|k| self.status.map.get(&k).copied())
      .unwrap_or_else(|| {
        if !status_raw.is_null() {
          warnings.push(format!(
            "status {status_raw} is not mapped; recorded as {}",
            self.status.default
          ));
        }
        self.status.default
      });

    let mut record = NormalizedRecord::new(
      system.clone(),
      self.system_kind,
      self.entity_type.clone(),
      key,
      status,
    );

    if let Some(rule) = &self.display_name
      && let Value::String(name) = read(&raw, rule, &mut warnings)
    {
      record.display_name = Some(name);
    }

    for (name, rule) in &self.fields {
      if RESERVED_FIELDS.contains(&name.as_str()) {
        warnings.push(format!(
          "field {name:?} shadows a guaranteed overlay field and was ignored"
        ));
        continue;
      }
      let value = read(&raw, rule, &mut warnings);
      record.insert(name.clone(), value);
    }

    Ok(Normalized { record, warnings })
  }

  /// `(id, version)`, as a sweep pins it.
  #[must_use]
  pub fn pin(&self) -> (String, String) {
    (self.id.clone(), self.version.clone())
  }
}

/// The lookup key for a raw status value.
fn status_key(v: &Value) -> Option<String> {
  match v {
    Value::Null => None,
    Value::String(s) => Some(s.clone()),
    Value::Bool(b) => Some(b.to_string()),
    other => Some(other.to_string()),
  }
}

fn read(raw: &Value, rule: &FieldRule, warnings: &mut Vec<String>) -> Value {
  let mut v = raw.get_path(&rule.path);
  if v.is_null()
    && let Some(d) = &rule.default
  {
    v = Value::from(d);
  }
  match rule.coerce {
    None => v,
    Some(c) => coerce(v, c, &rule.path, warnings),
  }
}

fn coerce(
  v: Value,
  to: Coerce,
  path: &str,
  warnings: &mut Vec<String>,
) -> Value {
  if v.is_null() {
    return Value::Null;
  }
  let failed = |warnings: &mut Vec<String>, v: &Value| {
    warnings.push(format!(
      "{path}: {v} is not a {}, recorded as null",
      match to {
        Coerce::String => "string",
        Coerce::Number => "number",
        Coerce::Boolean => "boolean",
        Coerce::Timestamp => "timestamp",
        Coerce::List => "list",
      }
    ));
    Value::Null
  };

  match (to, &v) {
    (Coerce::String, Value::String(_))
    | (Coerce::Number, Value::Number(_))
    | (Coerce::Boolean, Value::Bool(_))
    | (Coerce::List, Value::List(_))
    | (Coerce::Timestamp, Value::Timestamp(_)) => v,

    // Vendors spell booleans as strings often enough to be worth
    // handling, but only for the two unambiguous spellings.
    (Coerce::Boolean, Value::String(s)) => match s.as_str() {
      "true" | "True" | "TRUE" => Value::Bool(true),
      "false" | "False" | "FALSE" => Value::Bool(false),
      _ => failed(warnings, &v),
    },
    (Coerce::String, Value::Number(_) | Value::Bool(_)) => {
      Value::String(v.to_string())
    }
    (Coerce::Number, Value::String(s)) => s
      .parse()
      .map_or_else(|_| failed(warnings, &v), Value::Number),
    (Coerce::Timestamp, Value::String(s)) => s
      .parse::<Timestamp>()
      .map_or_else(|_| failed(warnings, &v), Value::Timestamp),
    // A single value where a list is expected is a list of one; vendors
    // do this for optional repeated fields.
    (Coerce::List, _) => Value::List(vec![v]),
    _ => failed(warnings, &v),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ruleset() -> Ruleset {
    serde_json::from_value(serde_json::json!({
      "id": "gworkspace-default",
      "version": "1",
      "system_kind": "workspace",
      "entity_type": "user",
      "entity_key": { "path": "primaryEmail" },
      "display_name": { "path": "name.fullName" },
      "status": {
        "path": "suspended",
        "map": { "true": "suspended", "false": "active" },
        "default": "unknown"
      },
      "fields": {
        "mfa_enrolled": { "path": "isEnrolledIn2Sv", "as": "boolean" },
        "is_admin": { "path": "isAdmin", "as": "boolean" },
        "last_login_at": { "path": "lastLoginTime", "as": "timestamp" },
        "aliases": { "path": "aliases", "as": "list" }
      }
    }))
    .unwrap()
  }

  fn apply(raw: serde_json::Value) -> Normalized {
    ruleset().apply(&SystemId::new("gws-prod"), &raw).unwrap()
  }

  #[test]
  fn maps_a_vendor_payload_onto_the_guaranteed_core() {
    let n = apply(serde_json::json!({
      "primaryEmail": "ada@example.com",
      "name": { "fullName": "Ada Lovelace" },
      "suspended": false,
      "isEnrolledIn2Sv": true,
      "isAdmin": false,
      "lastLoginTime": "2026-01-02T03:04:05Z",
      "aliases": ["ada.l@example.com"]
    }));
    assert!(n.warnings.is_empty(), "{:?}", n.warnings);
    let r = &n.record;
    assert_eq!(r.entity_key.as_str(), "ada@example.com");
    assert_eq!(r.display_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(r.status, EntityStatus::Active);
    assert_eq!(r.get("mfa_enrolled"), Value::Bool(true));
    assert_eq!(r.get("last_login_at").type_name(), "timestamp");
    assert_eq!(r.get("aliases").as_list().unwrap().len(), 1);
  }

  #[test]
  fn a_missing_field_is_null_not_a_guess() {
    let n = apply(serde_json::json!({
      "primaryEmail": "ada@example.com",
      "suspended": false
    }));
    assert_eq!(n.record.get("mfa_enrolled"), Value::Null);
    assert!(n.warnings.is_empty(), "absence is not a warning");
  }

  #[test]
  fn an_unmappable_timestamp_warns_rather_than_vanishing() {
    let n = apply(serde_json::json!({
      "primaryEmail": "ada@example.com",
      "suspended": false,
      "lastLoginTime": "never"
    }));
    assert_eq!(n.record.get("last_login_at"), Value::Null);
    assert_eq!(n.warnings.len(), 1);
    assert!(
      n.warnings[0].contains("not a timestamp"),
      "{:?}",
      n.warnings
    );
  }

  #[test]
  fn an_unmapped_status_becomes_unknown_and_warns() {
    let n = apply(serde_json::json!({
      "primaryEmail": "ada@example.com",
      "suspended": "archived"
    }));
    assert_eq!(n.record.status, EntityStatus::Unknown);
    assert!(n.warnings[0].contains("not mapped"), "{:?}", n.warnings);
  }

  #[test]
  fn an_entity_with_no_stable_key_is_refused() {
    let err = ruleset()
      .apply(&SystemId::new("gws-prod"), &serde_json::json!({"x": 1}))
      .unwrap_err();
    assert!(err.0.contains("no entity key"), "{err}");
  }

  #[test]
  fn a_rule_cannot_shadow_the_guaranteed_core() {
    let mut rs = ruleset();
    rs.fields.insert("status".to_owned(), FieldRule {
      path:    "whatever".to_owned(),
      coerce:  None,
      default: None,
    });
    let n = rs
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({"primaryEmail": "a", "suspended": false}),
      )
      .unwrap();
    assert_eq!(n.record.status, EntityStatus::Active);
    assert!(n.warnings[0].contains("shadows"), "{:?}", n.warnings);
  }

  #[test]
  fn a_ruleset_round_trips_through_json() {
    let rs = ruleset();
    let back: Ruleset =
      serde_json::from_str(&serde_json::to_string(&rs).unwrap()).unwrap();
    assert_eq!(back, rs);
  }
}
