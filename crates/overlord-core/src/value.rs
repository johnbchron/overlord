use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{error::CoreError, time::Timestamp};

/// The value model shared by normalization and the expression language.
///
/// SPEC.md section 7 names six types: `string`, `number`, `boolean`,
/// `timestamp`, `list`, `null`. `Object` is the seventh, and it is not
/// directly comparable: it exists because list elements have fields
/// (`count(groups where external)`) and because `raw.<path>` walks vendor
/// payloads. A check can reach *through* an object but never compare one.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
  Null,
  Bool(bool),
  Number(f64),
  String(String),
  /// Distinct from `String`: normalization decides what is a timestamp,
  /// so `last_login_at < "2025-01-01"` is a timestamp comparison rather
  /// than a lexicographic one.
  Timestamp(Timestamp),
  List(Vec<Value>),
  Object(std::collections::BTreeMap<String, Value>),
}

/// The key that tags a timestamp in stored JSON.
///
/// A normalized overlay round-trips through a `TEXT` column, and JSON has
/// no timestamp type. Without a tag, `last_login_at` would come back from
/// the store as a plain string and a rebuild would silently re-type it,
/// turning a timestamp comparison into a lexicographic one — which would
/// break SPEC.md section 13's promise that a rebuild reproduces the
/// overlay that was live at the time. Only the overlay uses this encoding;
/// `raw` is converted from [`serde_json::Value`] and never deserialized as
/// a [`Value`], so a vendor field literally named `$ts` is never mistaken
/// for one.
const TS_TAG: &str = "$ts";

impl Value {
  #[must_use]
  pub fn is_null(&self) -> bool { matches!(self, Self::Null) }

  /// The type name used in validation errors.
  #[must_use]
  pub fn type_name(&self) -> &'static str {
    match self {
      Self::Null => "null",
      Self::Bool(_) => "boolean",
      Self::Number(_) => "number",
      Self::String(_) => "string",
      Self::Timestamp(_) => "timestamp",
      Self::List(_) => "list",
      Self::Object(_) => "object",
    }
  }

  #[must_use]
  pub fn as_bool(&self) -> Option<bool> {
    match self {
      Self::Bool(b) => Some(*b),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_number(&self) -> Option<f64> {
    match self {
      Self::Number(n) => Some(*n),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_str(&self) -> Option<&str> {
    match self {
      Self::String(s) => Some(s),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_list(&self) -> Option<&[Value]> {
    match self {
      Self::List(l) => Some(l),
      _ => None,
    }
  }

  /// Walk a dotted path. A missing path is `null` (SPEC.md section 7),
  /// never an error — absence is data, and the three-valued logic is what
  /// decides whether it matters.
  ///
  /// Indexing into a list by number is deliberately unsupported: vendor
  /// array order is not stable, so a check that depended on it would be
  /// non-deterministic.
  #[must_use]
  pub fn get_path(&self, path: &str) -> Self {
    let mut cur = self;
    for seg in path.split('.') {
      match cur {
        Self::Object(map) => match map.get(seg) {
          Some(v) => cur = v,
          None => return Self::Null,
        },
        _ => return Self::Null,
      }
    }
    cur.clone()
  }

  /// Inverse of the [`Serialize`] impl: recognises the timestamp tag.
  fn from_tagged_json(j: &serde_json::Value) -> Result<Self, CoreError> {
    Ok(match j {
      serde_json::Value::Object(o) => {
        if let (1, Some(s)) =
          (o.len(), o.get(TS_TAG).and_then(serde_json::Value::as_str))
        {
          Self::Timestamp(s.parse()?)
        } else {
          Self::Object(
            o.iter()
              .map(|(k, v)| Ok((k.clone(), Self::from_tagged_json(v)?)))
              .collect::<Result<_, CoreError>>()?,
          )
        }
      }
      serde_json::Value::Array(a) => Self::List(
        a.iter()
          .map(Self::from_tagged_json)
          .collect::<Result<_, CoreError>>()?,
      ),
      other => Self::from(other),
    })
  }
}

impl Serialize for Value {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::{SerializeMap, SerializeSeq};
    match self {
      Self::Null => s.serialize_unit(),
      Self::Bool(b) => s.serialize_bool(*b),
      Self::Number(n) => s.serialize_f64(*n),
      Self::String(v) => s.serialize_str(v),
      Self::Timestamp(t) => {
        let mut m = s.serialize_map(Some(1))?;
        m.serialize_entry(TS_TAG, &t.to_string())?;
        m.end()
      }
      Self::List(l) => {
        let mut seq = s.serialize_seq(Some(l.len()))?;
        for v in l {
          seq.serialize_element(v)?;
        }
        seq.end()
      }
      Self::Object(o) => {
        let mut m = s.serialize_map(Some(o.len()))?;
        for (k, v) in o {
          m.serialize_entry(k, v)?;
        }
        m.end()
      }
    }
  }
}

impl<'de> Deserialize<'de> for Value {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let j = serde_json::Value::deserialize(d)?;
    Self::from_tagged_json(&j).map_err(serde::de::Error::custom)
  }
}

impl fmt::Display for Value {
  /// Human-facing rendering, used in evidence and dry-run samples.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Null => f.write_str("null"),
      Self::Bool(b) => write!(f, "{b}"),
      Self::Number(n) => {
        // Integral values print without a trailing `.0`; a count is the
        // most common number a violation shows.
        #[allow(clippy::cast_possible_truncation)]
        if n.fract() == 0.0 && n.abs() < 1e15 {
          write!(f, "{}", *n as i64)
        } else {
          write!(f, "{n}")
        }
      }
      Self::String(s) => write!(f, "{s:?}"),
      Self::Timestamp(t) => write!(f, "{t}"),
      Self::List(l) => {
        f.write_str("[")?;
        for (i, v) in l.iter().enumerate() {
          if i > 0 {
            f.write_str(", ")?;
          }
          write!(f, "{v}")?;
        }
        f.write_str("]")
      }
      Self::Object(m) => {
        f.write_str("{")?;
        for (i, (k, v)) in m.iter().enumerate() {
          if i > 0 {
            f.write_str(", ")?;
          }
          write!(f, "{k}: {v}")?;
        }
        f.write_str("}")
      }
    }
  }
}

impl From<bool> for Value {
  fn from(b: bool) -> Self { Self::Bool(b) }
}

impl From<f64> for Value {
  fn from(n: f64) -> Self { Self::Number(n) }
}

impl From<i64> for Value {
  #[allow(clippy::cast_precision_loss)]
  fn from(n: i64) -> Self { Self::Number(n as f64) }
}

impl From<usize> for Value {
  #[allow(clippy::cast_precision_loss)]
  fn from(n: usize) -> Self { Self::Number(n as f64) }
}

impl From<String> for Value {
  fn from(s: String) -> Self { Self::String(s) }
}

impl From<&str> for Value {
  fn from(s: &str) -> Self { Self::String(s.to_owned()) }
}

impl From<Timestamp> for Value {
  fn from(t: Timestamp) -> Self { Self::Timestamp(t) }
}

impl<T: Into<Value>> From<Option<T>> for Value {
  fn from(o: Option<T>) -> Self { o.map_or(Self::Null, Into::into) }
}

impl From<&serde_json::Value> for Value {
  /// Vendor payloads arrive as JSON. JSON has no timestamp type, so
  /// strings stay strings here; promoting a field to
  /// [`Value::Timestamp`] is normalization's job, not the parser's.
  fn from(j: &serde_json::Value) -> Self {
    match j {
      serde_json::Value::Null => Self::Null,
      serde_json::Value::Bool(b) => Self::Bool(*b),
      serde_json::Value::Number(n) => {
        n.as_f64().map_or(Self::Null, Self::Number)
      }
      serde_json::Value::String(s) => Self::String(s.clone()),
      serde_json::Value::Array(a) => {
        Self::List(a.iter().map(Self::from).collect())
      }
      serde_json::Value::Object(o) => Self::Object(
        o.iter().map(|(k, v)| (k.clone(), Self::from(v))).collect(),
      ),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn json(s: &str) -> Value {
    Value::from(&serde_json::from_str::<serde_json::Value>(s).unwrap())
  }

  fn round_trip(v: &Value) -> Value {
    serde_json::from_str(&serde_json::to_string(v).unwrap()).unwrap()
  }

  #[test]
  fn missing_path_is_null_not_an_error() {
    let v = json(r#"{"profile": {"dept": "eng"}}"#);
    assert_eq!(v.get_path("profile.dept"), Value::from("eng"));
    assert_eq!(v.get_path("profile.missing"), Value::Null);
    assert_eq!(v.get_path("nope.nope.nope"), Value::Null);
  }

  #[test]
  fn path_through_a_scalar_is_null() {
    assert_eq!(json(r#"{"a": 1}"#).get_path("a.b"), Value::Null);
  }

  #[test]
  fn json_strings_do_not_become_timestamps() {
    let v = json(r#"{"t": "2025-01-01T00:00:00Z"}"#);
    assert_eq!(v.get_path("t").type_name(), "string");
  }

  #[test]
  fn timestamps_survive_a_json_round_trip() {
    let v = Value::Timestamp("2026-01-15T09:30:00Z".parse().unwrap());
    let back = round_trip(&v);
    assert_eq!(back, v, "a stored overlay must not re-type its timestamps");
    assert_eq!(back.type_name(), "timestamp");
  }

  #[test]
  fn nested_and_listed_timestamps_survive_too() {
    let t = Value::Timestamp("2026-01-15T09:30:00Z".parse().unwrap());
    let v = Value::List(vec![
      t.clone(),
      Value::Object([("seen".to_owned(), t)].into_iter().collect()),
    ]);
    assert_eq!(round_trip(&v), v);
  }

  #[test]
  fn ordinary_strings_stay_strings_across_a_round_trip() {
    let v = Value::String("2026-01-15T09:30:00Z".to_owned());
    assert_eq!(round_trip(&v).type_name(), "string");
  }

  #[test]
  fn an_object_with_other_keys_is_not_a_timestamp() {
    let v = json(r#"{"$ts": "2026-01-15T09:30:00Z", "other": 1}"#);
    assert_eq!(round_trip(&v).type_name(), "object");
  }

  #[test]
  fn integral_numbers_render_without_a_decimal_point() {
    assert_eq!(Value::from(3_i64).to_string(), "3");
    assert_eq!(Value::from(2.5).to_string(), "2.5");
  }
}
