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
  /// Raw values that mean "absent", applied before coercion and before
  /// [`Self::default`].
  ///
  /// Vendors spell absence with a sentinel rather than by omitting the
  /// field: Google reports a user who has never signed in as
  /// `lastLoginTime: "1970-01-01T00:00:00.000Z"`, and Entra uses
  /// `"0001-01-01T00:00:00Z"`. Coerced literally, a dormancy check would
  /// read "has never signed in" as "signed in during the Nixon
  /// administration" — the same verdict today, but for a reason the
  /// evidence would state wrongly. Listing the sentinel here makes the
  /// field null, which is what it means, and keeps the mapping in the
  /// ruleset where an operator can revise it.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub null_if: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coerce {
  String,
  Number,
  Boolean,
  Timestamp,
  List,
  /// The part of a string before its first `@`, or the whole string if
  /// it has none.
  ///
  /// An extraction rather than a conversion, and the only one, so it
  /// earns its place by being load-bearing: identity's `username`
  /// signal (SPEC.md section 12) needs a username, and a directory that
  /// ships only `primaryEmail` or `profile.login` has one — spelled as
  /// an address. Without this a connector whose stable key is an opaque
  /// vendor id offers nothing for that signal at all.
  EmailLocal,
}

/// One way of reading a vendor's lifecycle state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusClause {
  pub path: String,
  /// Keyed by the raw value's string form, so `true` maps as `"true"`.
  /// A value absent from the map does not match, and the next clause is
  /// tried.
  pub map:  BTreeMap<String, EntityStatus>,
}

/// How to map a vendor's lifecycle state onto the five overlord knows.
///
/// Clauses are tried in order and the first that maps wins, because not
/// every vendor keeps its answer in one field. Google Workspace spells
/// an account's state across two independent booleans — `archived` and
/// `suspended` — so a single-path rule has to ignore one of them, and
/// ignoring `archived` reads a deprovisioned account as active. Order is
/// the whole of the semantics: put the narrower state first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusRule {
  pub when:    Vec<StatusClause>,
  /// What an unmapped value becomes. A connector that cannot map a
  /// vendor state must land on `unknown` rather than guessing:
  /// `status == "active"` appears in nearly every check, so a wrong
  /// guess is a wrong board.
  pub default: EntityStatus,
}

impl StatusRule {
  /// The one-clause case, which is most of them.
  #[must_use]
  pub fn single(
    path: impl Into<String>,
    map: BTreeMap<String, EntityStatus>,
  ) -> Self {
    Self {
      when:    vec![StatusClause {
        path: path.into(),
        map,
      }],
      default: EntityStatus::Unknown,
    }
  }
}

/// Accepts the one-clause shape a ruleset was written in before clauses
/// existed, so a stored `normalization.upsert` body still reads.
impl<'de> Deserialize<'de> for StatusRule {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
      Clauses {
        when:    Vec<StatusClause>,
        #[serde(default = "unknown_status")]
        default: EntityStatus,
      },
      Single {
        path:    String,
        map:     BTreeMap<String, EntityStatus>,
        #[serde(default = "unknown_status")]
        default: EntityStatus,
      },
    }

    Ok(match Repr::deserialize(d)? {
      Repr::Clauses { when, default } => Self { when, default },
      Repr::Single { path, map, default } => Self {
        when: vec![StatusClause { path, map }],
        default,
      },
    })
  }
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

    // The first clause that maps wins. A clause that finds nothing, or
    // finds a value its map does not name, falls through — that is how
    // `archived: false` hands the question to `suspended`.
    let mut status = None;
    let mut seen: Vec<Value> = Vec::new();
    for clause in &self.status.when {
      let raw_status = raw.get_path(&clause.path);
      if let Some(mapped) =
        status_key(&raw_status).and_then(|k| clause.map.get(&k).copied())
      {
        status = Some(mapped);
        break;
      }
      if !raw_status.is_null() {
        seen.push(raw_status);
      }
    }
    let status = status.unwrap_or_else(|| {
      // Silence when every clause found nothing: absence is not a
      // mapping failure. A value that was present and named by nothing
      // is, and it is how a vendor's new lifecycle state announces
      // itself.
      if !seen.is_empty() {
        let seen: Vec<String> = seen.iter().map(ToString::to_string).collect();
        warnings.push(format!(
          "status {} is not mapped; recorded as {}",
          seen.join(", "),
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
      // A blank name is not a name. Vendors spell "this field is not
      // filled in" as `null` and as `""` interchangeably — Grandstream's
      // own `listAccount` example does both — and the difference must
      // not reach the overlay, because everything downstream treats
      // "there is a display name" as a reason to show it *instead of*
      // the key. `Some("")` is how an entity ends up with an empty,
      // unclickable link where its name belongs.
      && !name.trim().is_empty()
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
  // Sentinels first: a vendor's "never" is absence, so it must reach
  // `default` the same way a missing path does.
  if rule.null_if.iter().any(|s| Value::from(s) == v) {
    v = Value::Null;
  }
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
        Coerce::EmailLocal => "string to take a local part from",
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
    // A bare username is already the local part; a directory that
    // spells logins both ways in one tenant should not lose the ones
    // that are not addresses.
    (Coerce::EmailLocal, Value::String(s)) => match s.split_once('@') {
      Some(("", _)) => failed(warnings, &v),
      Some((local, _)) => Value::String(local.to_owned()),
      None => Value::String(s.clone()),
    },
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
  fn a_blank_display_name_is_absent_rather_than_empty() {
    // Vendors spell "not filled in" as null and as "" interchangeably,
    // and the difference must not reach the overlay: everything
    // downstream reads "there is a display name" as a reason to show it
    // instead of the key, so `Some("")` is how an entity ends up with
    // an empty, unclickable link where its name belongs.
    for name in ["", "   ", "\t"] {
      let n = ruleset()
        .apply(
          &SystemId::new("gws-prod"),
          &serde_json::json!({
            "primaryEmail": "ada@example.com",
            "name": { "fullName": name },
            "suspended": false,
          }),
        )
        .unwrap();
      assert_eq!(
        n.record.display_name, None,
        "{name:?} should not become a display name"
      );
    }

    // A real name is untouched, padding and all — trimming for display
    // is the view's business, and the overlay records what was read.
    let n = ruleset()
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({
          "primaryEmail": "ada@example.com",
          "name": { "fullName": " Ada Lovelace " },
          "suspended": false,
        }),
      )
      .unwrap();
    assert_eq!(n.record.display_name.as_deref(), Some(" Ada Lovelace "));
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
      null_if: Vec::new(),
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

  /// Google's shape: two independent booleans, narrower state first.
  fn two_clause_ruleset() -> Ruleset {
    serde_json::from_value(serde_json::json!({
      "id": "two-clause",
      "version": "1",
      "system_kind": "workspace",
      "entity_type": "user",
      "entity_key": { "path": "primaryEmail" },
      "status": {
        "when": [
          { "path": "archived",  "map": { "true": "deprovisioned" } },
          { "path": "suspended", "map": { "true": "suspended",
                                          "false": "active" } }
        ],
        "default": "unknown"
      }
    }))
    .unwrap()
  }

  #[test]
  fn a_later_clause_answers_what_an_earlier_one_declines() {
    let n = two_clause_ruleset()
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({
          "primaryEmail": "ada@example.com",
          "archived": false,
          "suspended": false
        }),
      )
      .unwrap();
    assert_eq!(n.record.status, EntityStatus::Active);
    // `archived: false` falling through is the design, not a problem.
    assert!(n.warnings.is_empty(), "{:?}", n.warnings);
  }

  #[test]
  fn an_earlier_clause_wins_when_it_maps() {
    // An archived account is also suspended in Google's payload; read
    // by `suspended` alone it would be merely suspended, which is a
    // different remediation.
    let n = two_clause_ruleset()
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({
          "primaryEmail": "ada@example.com",
          "archived": true,
          "suspended": true
        }),
      )
      .unwrap();
    assert_eq!(n.record.status, EntityStatus::Deprovisioned);
  }

  #[test]
  fn a_value_no_clause_names_still_warns() {
    let n = two_clause_ruleset()
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({
          "primaryEmail": "ada@example.com",
          "archived": false,
          "suspended": "pending"
        }),
      )
      .unwrap();
    assert_eq!(n.record.status, EntityStatus::Unknown);
    assert!(n.warnings[0].contains("not mapped"), "{:?}", n.warnings);
  }

  #[test]
  fn the_one_clause_shape_a_stored_ruleset_was_written_in_still_reads() {
    // `normalization.upsert` bodies are stored as opaque JSON. A store
    // written before clauses existed must still replay.
    let old: StatusRule = serde_json::from_value(serde_json::json!({
      "path": "suspended",
      "map": { "true": "suspended", "false": "active" },
      "default": "unknown"
    }))
    .unwrap();
    assert_eq!(old.when.len(), 1);
    assert_eq!(old.when[0].path, "suspended");
    assert_eq!(old.default, EntityStatus::Unknown);
  }

  #[test]
  fn a_vendor_sentinel_for_never_becomes_null() {
    let mut rs = ruleset();
    rs.fields.insert("last_login_at".to_owned(), FieldRule {
      path:    "lastLoginTime".to_owned(),
      coerce:  Some(Coerce::Timestamp),
      default: None,
      null_if: vec![serde_json::json!("1970-01-01T00:00:00.000Z")],
    });
    let n = rs
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({
          "primaryEmail": "ada@example.com",
          "suspended": false,
          "lastLoginTime": "1970-01-01T00:00:00.000Z"
        }),
      )
      .unwrap();
    assert_eq!(n.record.get("last_login_at"), Value::Null);
    // A sentinel is expected data, not a normalization problem.
    assert!(n.warnings.is_empty(), "{:?}", n.warnings);
  }

  #[test]
  fn a_sentinel_does_not_swallow_a_real_value() {
    let mut rs = ruleset();
    rs.fields.insert("last_login_at".to_owned(), FieldRule {
      path:    "lastLoginTime".to_owned(),
      coerce:  Some(Coerce::Timestamp),
      default: None,
      null_if: vec![serde_json::json!("1970-01-01T00:00:00.000Z")],
    });
    let n = rs
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({
          "primaryEmail": "ada@example.com",
          "suspended": false,
          "lastLoginTime": "2026-01-02T03:04:05.000Z"
        }),
      )
      .unwrap();
    assert_eq!(n.record.get("last_login_at").type_name(), "timestamp");
  }

  #[test]
  fn a_username_can_be_taken_from_an_address() {
    let mut rs = ruleset();
    rs.fields.insert("username".to_owned(), FieldRule {
      path:    "primaryEmail".to_owned(),
      coerce:  Some(Coerce::EmailLocal),
      default: None,
      null_if: Vec::new(),
    });
    let n = rs
      .apply(
        &SystemId::new("gws-prod"),
        &serde_json::json!({
          "primaryEmail": "ada.lovelace@example.com",
          "suspended": false
        }),
      )
      .unwrap();
    assert_eq!(
      n.record.get("username"),
      Value::String("ada.lovelace".to_owned())
    );
  }

  #[test]
  fn a_bare_username_is_left_alone() {
    // A directory that spells some logins as addresses and some not
    // should not lose the ones that are not.
    let mut warnings = Vec::new();
    let v = coerce(
      Value::String("ada".to_owned()),
      Coerce::EmailLocal,
      "login",
      &mut warnings,
    );
    assert_eq!(v, Value::String("ada".to_owned()));
    assert!(warnings.is_empty());
  }

  #[test]
  fn a_ruleset_round_trips_through_json() {
    let rs = ruleset();
    let back: Ruleset =
      serde_json::from_str(&serde_json::to_string(&rs).unwrap()).unwrap();
    assert_eq!(back, rs);
  }
}
