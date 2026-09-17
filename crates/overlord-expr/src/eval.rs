use overlord_core::{
  Evidence, NormalizedRecord, SubjectKind, SystemSelector, Timestamp, Value,
};

use crate::{
  ast::{CmpOp, EntitiesOp, Expr, Lit, Path},
  span::Span,
  types::Program,
};

/// Three-valued truth (SPEC.md section 7).
///
/// Only [`Tri::True`] opens a violation, so a check never fires on data
/// that simply was not collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
  True,
  False,
  Null,
}

impl Tri {
  #[must_use]
  pub fn from_bool(b: bool) -> Self { if b { Self::True } else { Self::False } }

  #[must_use]
  pub fn is_true(self) -> bool { matches!(self, Self::True) }

  /// Kleene negation: `not null` is `null`.
  #[must_use]
  pub fn negate(self) -> Self {
    match self {
      Self::True => Self::False,
      Self::False => Self::True,
      Self::Null => Self::Null,
    }
  }
}

/// A mismatch discovered with a subject in hand.
///
/// SPEC.md section 7 insists a mismatched comparison is an error rather
/// than a silent `false`: a clean `false` would hide a data problem
/// behind a passing check. Evaluation stops at the first one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError {
  pub span:    Span,
  pub message: String,
}

impl EvalError {
  fn new(span: Span, message: impl Into<String>) -> Self {
    Self {
      span,
      message: message.into(),
    }
  }
}

impl std::fmt::Display for EvalError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.message)
  }
}

/// One entity's state as evaluation sees it.
#[derive(Debug, Clone)]
pub struct EntityAttrs {
  pub normalized: NormalizedRecord,
  /// The vendor payload, reachable at `raw.<path>`.
  pub raw:        serde_json::Value,
  /// The fact this state came from, recorded in evidence.
  pub fact_id:    i64,
}

impl EntityAttrs {
  fn get(&self, path: &Path) -> Value {
    if path.is_raw() {
      let mut v = Value::from(&self.raw);
      for seg in &path.tail {
        v = v.get_path(seg);
      }
      return v;
    }
    let mut v = self.normalized.get(&path.head);
    for seg in &path.tail {
      v = v.get_path(seg);
    }
    v
  }

  /// Resolve a bare field name plus a dotted tail, for
  /// `entity("idp").profile.title`.
  fn get_named(&self, head: &str, tail: &[String]) -> Value {
    let mut v = if head == "raw" {
      Value::from(&self.raw)
    } else {
      self.normalized.get(head)
    };
    for seg in tail {
      v = v.get_path(seg);
    }
    v
  }
}

/// The result of looking up an operator-designated primary entity.
#[derive(Debug)]
pub enum Primary<'a> {
  Found(&'a EntityAttrs),
  /// No entity of that kind is linked to this person.
  Missing,
  /// More than one candidate and no designated primary. SPEC.md section
  /// 6.4: the selector returns `null` and the check is flagged
  /// `ambiguous` rather than guessing.
  Ambiguous,
}

/// What a check is evaluated against.
///
/// The engine implements this over projections; the expression crate
/// never touches the store.
pub trait Subject {
  fn kind(&self) -> SubjectKind;

  /// The subject's own entity, for an entity-scoped check.
  fn own(&self) -> Option<&EntityAttrs>;

  /// Every linked entity matching a selector, for a person-scoped check.
  fn select(&self, sel: &SystemSelector) -> Vec<&EntityAttrs>;

  /// The designated primary entity for a selector.
  fn primary(&self, sel: &SystemSelector) -> Primary<'_>;
}

/// Everything evaluation is allowed to know beyond the subject.
///
/// One field, deliberately: the sweep's `started_at` is the only clock
/// (SPEC.md section 13), which is what makes a replay reproduce the
/// original result exactly.
#[derive(Debug, Clone, Copy)]
pub struct EvalCtx {
  pub now: Timestamp,
}

impl EvalCtx {
  #[must_use]
  pub fn at(now: Timestamp) -> Self { Self { now } }
}

/// The outcome of evaluating one check against one subject.
#[derive(Debug, Clone)]
pub struct Evaluation {
  pub outcome:   Result<Tri, EvalError>,
  /// The evaluated leaves, in evaluation order. Recorded whatever the
  /// outcome; the caller keeps them when a violation opens and when a
  /// dry-run wants samples.
  pub evidence:  Evidence,
  /// Selectors that matched several entities with no designated primary.
  pub ambiguous: Vec<String>,
}

impl Evaluation {
  #[must_use]
  pub fn opens_violation(&self) -> bool {
    matches!(self.outcome, Ok(Tri::True))
  }
}

/// Evaluate a compiled condition against one subject.
#[must_use]
pub fn eval(
  program: &Program,
  subject: &dyn Subject,
  ctx: &EvalCtx,
) -> Evaluation {
  let mut ev = Ev {
    program,
    subject,
    ctx,
    evidence: Evidence::default(),
    ambiguous: Vec::new(),
    element: Vec::new(),
    quiet: 0,
  };
  let outcome = ev.tri(program.ast());
  Evaluation {
    outcome,
    evidence: ev.evidence,
    ambiguous: ev.ambiguous,
  }
}

struct Ev<'a> {
  program:   &'a Program,
  subject:   &'a dyn Subject,
  ctx:       &'a EvalCtx,
  evidence:  Evidence,
  ambiguous: Vec<String>,
  /// The `where` element stack. Non-empty means paths name element
  /// fields, and that leaves are not worth recording as evidence — an
  /// operator wants `count(groups where external) = 3`, not one line per
  /// group.
  element:   Vec<Value>,
  /// Depth of "this subexpression is plumbing, not evidence". Raised
  /// while evaluating the list operand of `count` / `any` / `all`: the
  /// aggregate is what an operator wants to see, and dumping the whole
  /// list beside it buries the number that mattered.
  quiet:     usize,
}

type R<T> = Result<T, EvalError>;

impl Ev<'_> {
  fn source(&self, span: Span) -> String {
    span.slice(self.program.src()).trim().to_owned()
  }

  fn record(&mut self, span: Span, value: &Value, fact_ids: Vec<i64>) {
    if !self.element.is_empty() || self.quiet > 0 {
      return;
    }
    let expr = self.source(span);
    if self.evidence.leaves.iter().any(|l| l.expr == expr) {
      return;
    }
    self.evidence.leaves.push(overlord_core::EvidenceLeaf {
      expr,
      value: value.clone(),
      fact_ids,
    });
  }

  fn selector(&self, s: &str) -> SystemSelector {
    s.parse()
      .unwrap_or_else(|_| SystemSelector::Id(overlord_core::SystemId::new(s)))
  }

  /// Evaluate to a value.
  fn value(&mut self, e: &Expr) -> R<Value> {
    match e {
      Expr::Lit(l, _) => Ok(match l {
        Lit::Str(s) => Value::String(s.clone()),
        Lit::Num(n) => Value::Number(*n),
        Lit::Bool(b) => Value::Bool(*b),
        Lit::Null => Value::Null,
      }),

      Expr::Path(p) => {
        let v = if let Some(el) = self.element.last() {
          // Predicates see element fields only, never the enclosing
          // subject (SPEC.md section 7).
          let mut v = el.get_path(&p.head);
          for seg in &p.tail {
            v = v.get_path(seg);
          }
          v
        } else if let Some(own) = self.subject.own() {
          own.get(p)
        } else {
          Value::Null
        };
        let facts = self
          .subject
          .own()
          .map(|o| vec![o.fact_id])
          .unwrap_or_default();
        self.record(p.span, &v, facts);
        Ok(v)
      }

      Expr::DaysAgo { days, .. } =>
      {
        #[allow(clippy::cast_possible_truncation)]
        Ok(Value::Timestamp(self.ctx.now.minus_days(*days as i64)))
      }

      Expr::Coalesce { lhs, rhs, .. } => {
        let l = self.value(lhs)?;
        if l.is_null() { self.value(rhs) } else { Ok(l) }
      }

      Expr::Count { list, pred, span } => {
        let Some(items) = self.items(list)? else {
          self.record(*span, &Value::Null, vec![]);
          return Ok(Value::Null);
        };
        let mut n = 0usize;
        for item in items {
          // A null predicate does not count: only `true` counts, the
          // same rule that governs whether a violation opens.
          if self.with_element(item, pred.as_deref())? == Tri::True {
            n += 1;
          }
        }
        let v = Value::from(n);
        self.record(*span, &v, self.subject_facts());
        Ok(v)
      }

      Expr::Entities {
        op: EntitiesOp::Count,
        selector,
        pred,
        span,
        ..
      } => {
        let sel = self.selector(selector);
        let matched: Vec<EntityAttrs> =
          self.subject.select(&sel).into_iter().cloned().collect();
        let mut n = 0usize;
        let mut facts = Vec::new();
        for ent in &matched {
          if self.entity_matches(ent, pred.as_deref())? == Tri::True {
            n += 1;
            facts.push(ent.fact_id);
          }
        }
        let v = Value::from(n);
        self.record(*span, &v, facts);
        Ok(v)
      }

      Expr::EntityField {
        selector,
        tail,
        span,
        ..
      } => {
        let sel = self.selector(selector);
        let (v, facts) = match self.subject.primary(&sel) {
          Primary::Found(ent) => {
            let (head, rest) = tail
              .split_first()
              .map_or((String::new(), &[] as &[String]), |(h, r)| {
                (h.clone(), r)
              });
            (ent.get_named(&head, rest), vec![ent.fact_id])
          }
          Primary::Missing => (Value::Null, vec![]),
          Primary::Ambiguous => {
            let s = selector.clone();
            if !self.ambiguous.contains(&s) {
              self.ambiguous.push(s);
            }
            (Value::Null, vec![])
          }
        };
        self.record(*span, &v, facts);
        Ok(v)
      }

      // Everything else is boolean-valued; go through `tri` and wrap.
      other => Ok(match self.tri(other)? {
        Tri::True => Value::Bool(true),
        Tri::False => Value::Bool(false),
        Tri::Null => Value::Null,
      }),
    }
  }

  fn subject_facts(&self) -> Vec<i64> {
    self
      .subject
      .own()
      .map(|o| vec![o.fact_id])
      .unwrap_or_default()
  }

  /// The elements of a list operand, or `None` when the operand is null.
  fn items(&mut self, list: &Expr) -> R<Option<Vec<Value>>> {
    self.quiet += 1;
    let v = self.value(list);
    self.quiet -= 1;
    let v = v?;
    match v {
      Value::Null => Ok(None),
      Value::List(items) => Ok(Some(items)),
      other => Err(EvalError::new(
        list.span(),
        format!("expected a list, found {}", other.type_name()),
      )),
    }
  }

  fn with_element(&mut self, item: Value, pred: Option<&Expr>) -> R<Tri> {
    let Some(pred) = pred else {
      return Ok(Tri::True);
    };
    self.element.push(item);
    let out = self.tri(pred);
    self.element.pop();
    out
  }

  /// Run a predicate against an entity's overlay rather than a list
  /// element, for `has_entity("workspace" where status == "active")`.
  fn entity_matches(
    &mut self,
    ent: &EntityAttrs,
    pred: Option<&Expr>,
  ) -> R<Tri> {
    let Some(pred) = pred else {
      return Ok(Tri::True);
    };
    let mut fields = std::collections::BTreeMap::new();
    for name in ent.normalized.field_names() {
      fields.insert(name.to_owned(), ent.normalized.get(name));
    }
    fields.insert("raw".to_owned(), Value::from(&ent.raw));
    self.with_element(Value::Object(fields), Some(pred))
  }

  /// Evaluate to three-valued truth.
  fn tri(&mut self, e: &Expr) -> R<Tri> {
    match e {
      Expr::Not(a, _) => Ok(self.tri(a)?.negate()),

      Expr::And(a, b, _) => {
        // Kleene: `false and null` is `false`, so the left side may
        // short-circuit only on `false`.
        let l = self.tri(a)?;
        if l == Tri::False {
          return Ok(Tri::False);
        }
        let r = self.tri(b)?;
        Ok(match (l, r) {
          (_, Tri::False) => Tri::False,
          (Tri::True, Tri::True) => Tri::True,
          _ => Tri::Null,
        })
      }

      Expr::Or(a, b, _) => {
        let l = self.tri(a)?;
        if l == Tri::True {
          return Ok(Tri::True);
        }
        let r = self.tri(b)?;
        Ok(match (l, r) {
          (_, Tri::True) => Tri::True,
          (Tri::False, Tri::False) => Tri::False,
          _ => Tri::Null,
        })
      }

      Expr::Exists(a, _) => Ok(Tri::from_bool(!self.value(a)?.is_null())),

      Expr::IsNull(a, _) => Ok(Tri::from_bool(self.value(a)?.is_null())),

      Expr::Cmp { op, lhs, rhs, span } => {
        let l = self.value(lhs)?;
        let r = self.value(rhs)?;
        compare(*op, &l, &r, *span)
      }

      Expr::In { lhs, rhs, span } => {
        let needle = self.value(lhs)?;
        if needle.is_null() {
          return Ok(Tri::Null);
        }
        let Some(items) = self.items(rhs)? else {
          return Ok(Tri::Null);
        };
        let mut saw_null = false;
        for item in &items {
          match compare(CmpOp::Eq, &needle, item, *span)? {
            Tri::True => return Ok(Tri::True),
            Tri::Null => saw_null = true,
            Tri::False => {}
          }
        }
        Ok(if saw_null { Tri::Null } else { Tri::False })
      }

      Expr::Matches {
        lhs, pattern, span, ..
      } => {
        let v = self.value(lhs)?;
        match &v {
          Value::Null => Ok(Tri::Null),
          Value::String(s) => {
            let re = self.program.regex(pattern).ok_or_else(|| {
              EvalError::new(*span, "the pattern was not compiled")
            })?;
            Ok(Tri::from_bool(re.is_match(s)))
          }
          other => Err(EvalError::new(
            lhs.span(),
            format!("`matches` needs text, found {}", other.type_name()),
          )),
        }
      }

      Expr::Quant {
        all,
        list,
        pred,
        span,
      } => {
        let Some(items) = self.items(list)? else {
          self.record(*span, &Value::Null, vec![]);
          return Ok(Tri::Null);
        };
        let mut saw_null = false;
        let mut hits = 0usize;
        let total = items.len();
        for item in items {
          match self.with_element(item, Some(pred))? {
            Tri::True => {
              hits += 1;
              if !*all {
                let v = Value::Bool(true);
                self.record(*span, &v, self.subject_facts());
                return Ok(Tri::True);
              }
            }
            Tri::False if *all => {
              let v = Value::Bool(false);
              self.record(*span, &v, self.subject_facts());
              return Ok(Tri::False);
            }
            Tri::Null => saw_null = true,
            Tri::False => {}
          }
        }
        let out = if saw_null {
          Tri::Null
        } else if *all {
          // An empty list satisfies `all` vacuously.
          Tri::from_bool(hits == total)
        } else {
          Tri::False
        };
        if out != Tri::Null {
          let v = Value::Bool(out == Tri::True);
          self.record(*span, &v, self.subject_facts());
        }
        Ok(out)
      }

      Expr::Entities {
        op: EntitiesOp::Has,
        selector,
        pred,
        span,
        ..
      } => {
        let sel = self.selector(selector);
        let matched: Vec<EntityAttrs> =
          self.subject.select(&sel).into_iter().cloned().collect();
        let mut saw_null = false;
        let mut facts = Vec::new();
        for ent in &matched {
          match self.entity_matches(ent, pred.as_deref())? {
            Tri::True => {
              facts.push(ent.fact_id);
              let v = Value::Bool(true);
              self.record(*span, &v, facts);
              return Ok(Tri::True);
            }
            Tri::Null => saw_null = true,
            Tri::False => {}
          }
        }
        if saw_null {
          return Ok(Tri::Null);
        }
        let v = Value::Bool(false);
        self.record(*span, &v, vec![]);
        Ok(Tri::False)
      }

      // A value used where a condition is wanted: the static checker
      // allowed it because the type was unknown, so the mismatch is
      // reported here, with the value in hand.
      other => {
        let span = other.span();
        let v = self.value(other)?;
        match v {
          Value::Bool(b) => Ok(Tri::from_bool(b)),
          Value::Null => Ok(Tri::Null),
          ref bad => Err(EvalError::new(
            span,
            format!(
              "expected a yes/no value, found {} ({bad})",
              bad.type_name()
            ),
          )),
        }
      }
    }
  }
}

/// Compare two values under SPEC.md section 7's rules.
fn compare(op: CmpOp, a: &Value, b: &Value, span: Span) -> R<Tri> {
  use std::cmp::Ordering;

  if a.is_null() || b.is_null() {
    return Ok(Tri::Null);
  }

  let ord: Option<Ordering> = match (a, b) {
    (Value::Number(x), Value::Number(y)) => x.partial_cmp(y),
    (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
    (Value::Bool(x), Value::Bool(y)) => {
      if op.is_ordering() {
        return Err(EvalError::new(span, "booleans have no order"));
      }
      Some(x.cmp(y))
    }
    (Value::Timestamp(x), Value::Timestamp(y)) => Some(x.cmp(y)),

    // The one-directional coercion: a string literal reads as a time
    // when the other side is one. It never goes the other way, so a
    // timestamp is never compared as text.
    (Value::Timestamp(t), Value::String(s)) => {
      Some(t.cmp(&parse_time(s, span)?))
    }
    (Value::String(s), Value::Timestamp(t)) => {
      Some(parse_time(s, span)?.cmp(t))
    }

    (Value::List(_) | Value::Object(_), _)
    | (_, Value::List(_) | Value::Object(_)) => {
      return Err(EvalError::new(
        span,
        format!("cannot compare {} with {}", a.type_name(), b.type_name()),
      ));
    }

    _ => {
      return Err(EvalError::new(
        span,
        format!(
          "cannot compare {} with {} — overlord never compares across types \
           silently",
          a.type_name(),
          b.type_name()
        ),
      ));
    }
  };

  let Some(ord) = ord else {
    // Only reachable for a NaN, which no connector should produce.
    return Ok(Tri::Null);
  };

  Ok(Tri::from_bool(match op {
    CmpOp::Eq => ord == Ordering::Equal,
    CmpOp::Ne => ord != Ordering::Equal,
    CmpOp::Lt => ord == Ordering::Less,
    CmpOp::Le => ord != Ordering::Greater,
    CmpOp::Gt => ord == Ordering::Greater,
    CmpOp::Ge => ord != Ordering::Less,
  }))
}

fn parse_time(s: &str, span: Span) -> R<Timestamp> {
  s.parse::<Timestamp>().map_err(|_| {
    EvalError::new(
      span,
      format!("{s:?} is not a time, so it cannot be compared with one"),
    )
  })
}
