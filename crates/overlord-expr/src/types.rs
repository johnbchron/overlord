use std::collections::BTreeMap;

use overlord_core::{SubjectKind, SystemSelector};
use regex::Regex;

use crate::{
  ast::{CmpOp, Expr, Lit},
  parser::parse,
  span::{Diagnostic, Span},
};

/// A static type in the check language (SPEC.md section 7).
///
/// `Unknown` is the type of anything read from a normalization overlay.
/// Overlays are per-connector and evolve with their rulesets, so the
/// checker has no schema to check a field name against; what it *can* do
/// is reject a comparison whose two sides are both known and
/// incompatible, which is where the real mistakes are. Everything else is
/// deferred to evaluation, where a mismatch is an error rather than a
/// silent `false` — the spec is explicit about that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
  Bool,
  Number,
  String,
  Timestamp,
  List,
  Object,
  Null,
  Unknown,
}

impl Ty {
  #[must_use]
  pub fn name(self) -> &'static str {
    match self {
      Self::Bool => "boolean",
      Self::Number => "number",
      Self::String => "string",
      Self::Timestamp => "timestamp",
      Self::List => "list",
      Self::Object => "object",
      Self::Null => "null",
      Self::Unknown => "a value read from the overlay",
    }
  }

  /// Whether a value of this type can stand where a condition is wanted.
  fn is_condition(self) -> bool {
    matches!(self, Self::Bool | Self::Null | Self::Unknown)
  }

  fn is_listish(self) -> bool {
    matches!(self, Self::List | Self::Null | Self::Unknown)
  }
}

/// What the checker knows about the check being validated.
#[derive(Debug, Clone)]
pub struct Schema {
  /// Entity-scoped checks read the subject's own overlay; person-scoped
  /// ones reach entities only through the selectors.
  pub applies_to: SubjectKind,
}

impl Schema {
  #[must_use]
  pub fn new(applies_to: SubjectKind) -> Self {
    Self { applies_to }
  }
}

/// A validated condition, ready to evaluate.
///
/// Holding the compiled regexes here is what makes SPEC.md section 7's
/// "a check cannot hang a sweep" true in two ways: the patterns are RE2
/// (linear time, no backtracking), and they are compiled once when the
/// check is saved rather than once per subject per sweep.
#[derive(Debug, Clone)]
pub struct Program {
  src: String,
  ast: Expr,
  applies_to: SubjectKind,
  regexes: BTreeMap<String, Regex>,
}

impl Program {
  #[must_use]
  pub fn src(&self) -> &str {
    &self.src
  }

  #[must_use]
  pub fn ast(&self) -> &Expr {
    &self.ast
  }

  #[must_use]
  pub fn applies_to(&self) -> SubjectKind {
    self.applies_to
  }

  #[must_use]
  pub fn regex(&self, pattern: &str) -> Option<&Regex> {
    self.regexes.get(pattern)
  }

  /// Every system selector the condition names, for the Rules screen's
  /// "what does this check touch" column.
  #[must_use]
  pub fn selectors(&self) -> Vec<SystemSelector> {
    let mut out = Vec::new();
    collect_selectors(&self.ast, &mut out);
    out.sort();
    out.dedup();
    out
  }
}

fn collect_selectors(e: &Expr, out: &mut Vec<SystemSelector>) {
  let mut push = |s: &str| {
    if let Ok(sel) = s.parse::<SystemSelector>() {
      out.push(sel);
    }
  };
  match e {
    Expr::Entities { selector, pred, .. } => {
      push(selector);
      if let Some(p) = pred {
        collect_selectors(p, out);
      }
    }
    Expr::EntityField { selector, .. } => push(selector),
    Expr::Not(a, _) | Expr::Exists(a, _) | Expr::IsNull(a, _) => {
      collect_selectors(a, out);
    }
    Expr::And(a, b, _)
    | Expr::Or(a, b, _)
    | Expr::Cmp { lhs: a, rhs: b, .. }
    | Expr::In { lhs: a, rhs: b, .. }
    | Expr::Coalesce { lhs: a, rhs: b, .. } => {
      collect_selectors(a, out);
      collect_selectors(b, out);
    }
    Expr::Matches { lhs, .. } => collect_selectors(lhs, out),
    Expr::Count { list, pred, .. } => {
      collect_selectors(list, out);
      if let Some(p) = pred {
        collect_selectors(p, out);
      }
    }
    Expr::Quant { list, pred, .. } => {
      collect_selectors(list, out);
      collect_selectors(pred, out);
    }
    Expr::Lit(..) | Expr::Path(_) | Expr::DaysAgo { .. } => {}
  }
}

/// Parse and type-check a condition.
///
/// # Errors
/// A parse error is returned alone, because everything after it is
/// guesswork. Type errors are returned together: they are independent,
/// and an operator fixing a rule wants to see all of them at once.
pub fn compile(src: &str, schema: &Schema) -> Result<Program, Vec<Diagnostic>> {
  let ast = parse(src).map_err(|d| vec![d])?;
  let mut cx = Cx {
    schema,
    in_predicate: false,
    errors: Vec::new(),
    regexes: BTreeMap::new(),
  };
  let ty = cx.check(&ast);

  if !ty.is_condition() {
    cx.errors.push(
      Diagnostic::new(
        ast.span(),
        format!(
          "a condition must be a yes/no question, but this is {}",
          ty.name()
        ),
      )
      .with_help(match ty {
        Ty::Number => "compare it, for example `count(groups) > 0`",
        _ => {
          "only `true` opens a violation, so the condition must be \
              boolean"
        }
      }),
    );
  }

  if cx.errors.is_empty() {
    Ok(Program {
      src: src.to_owned(),
      ast,
      applies_to: schema.applies_to,
      regexes: cx.regexes,
    })
  } else {
    cx.errors.sort_by_key(|d| d.span.start);
    Err(cx.errors)
  }
}

struct Cx<'a> {
  schema: &'a Schema,
  /// Inside a `where`, paths name element fields, so the rule that a
  /// person-scoped check has no attributes of its own does not apply.
  in_predicate: bool,
  errors: Vec<Diagnostic>,
  regexes: BTreeMap<String, Regex>,
}

impl Cx<'_> {
  fn error(&mut self, span: Span, msg: impl Into<String>) {
    self.errors.push(Diagnostic::new(span, msg));
  }

  fn error_help(
    &mut self,
    span: Span,
    msg: impl Into<String>,
    help: impl Into<String>,
  ) {
    self.errors.push(Diagnostic::new(span, msg).with_help(help));
  }

  /// Check an operand that must read as a condition.
  fn condition(&mut self, e: &Expr, what: &str) {
    let ty = self.check(e);
    if !ty.is_condition() {
      self.error(
        e.span(),
        format!(
          "{what} must be a yes/no question, but this is {}",
          ty.name()
        ),
      );
    }
  }

  fn predicate(&mut self, e: &Expr) {
    let outer = std::mem::replace(&mut self.in_predicate, true);
    self.condition(e, "a `where` predicate");
    self.in_predicate = outer;
  }

  fn person_only(&mut self, span: Span, what: &str) -> bool {
    if self.schema.applies_to == SubjectKind::Person {
      return true;
    }
    self.error_help(
      span,
      format!("`{what}` is only available to a person-scoped check"),
      "set `applies_to` to `person`, or read the field directly — an \
       entity-scoped check already has one entity's overlay",
    );
    false
  }

  #[allow(clippy::too_many_lines)]
  fn check(&mut self, e: &Expr) -> Ty {
    match e {
      Expr::Lit(l, _) => match l {
        Lit::Str(_) => Ty::String,
        Lit::Num(_) => Ty::Number,
        Lit::Bool(_) => Ty::Bool,
        Lit::Null => Ty::Null,
      },

      Expr::Path(p) => {
        // A person has no overlay of its own: it is a set of entities
        // (SPEC.md section 6.4). Reading `status` on one would silently
        // mean nothing, so it is rejected with the rewrite.
        if !self.in_predicate && self.schema.applies_to == SubjectKind::Person {
          self.error_help(
            p.span,
            format!(
              "a person-scoped check has no field `{}` of its own",
              p.source()
            ),
            format!(
              "read it from an entity: `entity(\"idp\").{}`, or test it \
               with `has_entity(\"idp\" where {})`",
              p.source(),
              p.source()
            ),
          );
        }
        Ty::Unknown
      }

      Expr::Not(a, span) => {
        self.condition(a, "`not`");
        let _ = span;
        Ty::Bool
      }

      Expr::Exists(a, _) => {
        self.check(a);
        Ty::Bool
      }

      Expr::IsNull(a, _) => {
        self.check(a);
        Ty::Bool
      }

      Expr::And(a, b, _) => {
        self.condition(a, "the left side of `and`");
        self.condition(b, "the right side of `and`");
        Ty::Bool
      }

      Expr::Or(a, b, _) => {
        self.condition(a, "the left side of `or`");
        self.condition(b, "the right side of `or`");
        Ty::Bool
      }

      Expr::Cmp { op, lhs, rhs, span } => {
        let (lt, rt) = (self.check(lhs), self.check(rhs));
        self.check_comparison(*op, lt, rt, lhs, rhs, *span);
        Ty::Bool
      }

      Expr::In { lhs, rhs, span } => {
        let lt = self.check(lhs);
        let rt = self.check(rhs);
        if !rt.is_listish() {
          self.error_help(
            rhs.span(),
            format!("`in` needs a list on the right, found {}", rt.name()),
            "write `department in [\"eng\", \"ops\"]`-style membership \
             against a list field",
          );
        }
        if matches!(lt, Ty::List | Ty::Object) {
          self.error(
            lhs.span(),
            format!("cannot test {} for membership", lt.name()),
          );
        }
        let _ = span;
        Ty::Bool
      }

      Expr::Matches {
        lhs,
        pattern,
        pat_span,
        ..
      } => {
        let lt = self.check(lhs);
        if !matches!(lt, Ty::String | Ty::Null | Ty::Unknown) {
          self.error(
            lhs.span(),
            format!("`matches` needs text on the left, found {}", lt.name()),
          );
        }
        if !self.regexes.contains_key(pattern) {
          match Regex::new(pattern) {
            Ok(re) => {
              self.regexes.insert(pattern.clone(), re);
            }
            Err(err) => {
              // The compiler's own message names the offending
              // construct; keeping it beats paraphrasing.
              let detail = err
                .to_string()
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("invalid pattern")
                .trim()
                .to_owned();
              self.error_help(
                *pat_span,
                "the pattern is not a valid regular expression",
                detail,
              );
            }
          }
        }
        Ty::Bool
      }

      Expr::Coalesce { lhs, rhs, .. } => {
        let lt = self.check(lhs);
        let rt = self.check(rhs);
        match (lt, rt) {
          (Ty::Null, t) => t,
          (a, b) if a == b => a,
          _ => Ty::Unknown,
        }
      }

      Expr::Count { list, pred, .. } => {
        let lt = self.check(list);
        if !lt.is_listish() {
          self.error(
            list.span(),
            format!("`count` needs a list, found {}", lt.name()),
          );
        }
        if let Some(p) = pred {
          self.predicate(p);
        }
        Ty::Number
      }

      Expr::Quant {
        all, list, pred, ..
      } => {
        let lt = self.check(list);
        if !lt.is_listish() {
          self.error(
            list.span(),
            format!(
              "`{}` needs a list, found {}",
              if *all { "all" } else { "any" },
              lt.name()
            ),
          );
        }
        self.predicate(pred);
        Ty::Bool
      }

      Expr::Entities {
        op,
        selector,
        sel_span,
        pred,
        ..
      } => {
        if self.person_only(*sel_span, op.as_str())
          && selector.trim().is_empty()
        {
          self.error(*sel_span, "the selector is empty");
        }
        if let Some(p) = pred {
          self.predicate(p);
        }
        match op {
          crate::ast::EntitiesOp::Has => Ty::Bool,
          crate::ast::EntitiesOp::Count => Ty::Number,
        }
      }

      Expr::EntityField {
        selector,
        sel_span,
        tail,
        ..
      } => {
        if self.person_only(*sel_span, "entity") && selector.trim().is_empty() {
          self.error(*sel_span, "the selector is empty");
        }
        if tail.is_empty() {
          self.error(*sel_span, "`entity(...)` must be followed by a field");
        }
        Ty::Unknown
      }

      Expr::DaysAgo { days, span } => {
        if !days.is_finite() || *days < 0.0 {
          self.error_help(
            *span,
            "`days_ago` needs a number of days in the past",
            "`days_ago(90)` is ninety days before the sweep started",
          );
        } else if days.fract() != 0.0 {
          self.error_help(
            *span,
            "`days_ago` needs a whole number of days",
            "a fractional day would be rounded, which makes the rule's \
             meaning unclear",
          );
        }
        Ty::Timestamp
      }
    }
  }

  /// SPEC.md section 7: a string literal coerces to a timestamp in a
  /// timestamp comparison, one-directionally. Any other mismatch is a
  /// validation error, not a silent `false`.
  fn check_comparison(
    &mut self,
    op: CmpOp,
    lt: Ty,
    rt: Ty,
    lhs: &Expr,
    rhs: &Expr,
    span: Span,
  ) {
    // Null on either side makes the whole comparison null, which is
    // well-defined and usually a mistake worth a nudge rather than an
    // error.
    if lt == Ty::Null || rt == Ty::Null {
      self.error_help(
        span,
        "comparing with `null` is always null, so this can never open a \
         violation",
        "use `is null` or `exists` to test for absence",
      );
      return;
    }

    for (ty, e) in [(lt, lhs), (rt, rhs)] {
      if matches!(ty, Ty::List | Ty::Object) {
        self.error_help(
          e.span(),
          format!("cannot compare {}", ty.name()),
          "compare a field of it, or use `count(...)` / `any(... where \
           ...)`",
        );
        return;
      }
    }

    if op.is_ordering() && (lt == Ty::Bool || rt == Ty::Bool) {
      self.error(span, "booleans have no order");
      return;
    }

    // The timestamp/string coercion, and its one-directionality: a
    // literal that cannot be read as a time is caught here rather than
    // failing per-subject at sweep time.
    let timestampish = |a: Ty, b: Ty| a == Ty::Timestamp && b == Ty::String;
    if timestampish(lt, rt) || timestampish(rt, lt) {
      let (lit_expr, _) = if lt == Ty::Timestamp {
        (rhs, lhs)
      } else {
        (lhs, rhs)
      };
      if let Expr::Lit(Lit::Str(s), lit_span) = lit_expr
        && s.parse::<overlord_core::Timestamp>().is_err()
      {
        {
          self.error_help(
            *lit_span,
            format!("{s:?} is not a time"),
            "a timestamp literal is ISO-8601, for example \
             \"2025-01-01T00:00:00Z\"",
          );
        }
      }
      return;
    }

    if lt == Ty::Unknown || rt == Ty::Unknown {
      return; // deferred to evaluation
    }

    if lt != rt {
      self.error_help(
        span,
        format!("cannot compare {} with {}", lt.name(), rt.name()),
        "overlord never compares across types silently — it would turn a \
         data problem into a clean `false`",
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entity(src: &str) -> Result<Program, Vec<Diagnostic>> {
    compile(src, &Schema::new(SubjectKind::Entity))
  }

  fn person(src: &str) -> Result<Program, Vec<Diagnostic>> {
    compile(src, &Schema::new(SubjectKind::Person))
  }

  fn msgs(r: Result<Program, Vec<Diagnostic>>) -> Vec<String> {
    r.err()
      .unwrap_or_default()
      .into_iter()
      .map(|d| d.message)
      .collect()
  }

  #[test]
  fn the_spec_examples_type_check_in_their_declared_scope() {
    entity("status == \"active\" and not mfa_enrolled").unwrap();
    entity(
      "is_admin and (last_login_at is null or last_login_at < days_ago(90))",
    )
    .unwrap();
    entity("external_sharing == true or count(groups where external) > 0")
      .unwrap();
    person(
      "has_entity(\"workspace\" where status == \"active\") and not \
       has_entity(\"idp\")",
    )
    .unwrap();
    person("count_entities(\"mdm\") > 1").unwrap();
    person("entity(\"idp\").mfa_enrolled").unwrap();
  }

  #[test]
  fn a_condition_must_be_a_yes_no_question() {
    let m = msgs(entity("count(groups)"));
    assert!(m[0].contains("yes/no question"), "{m:?}");
  }

  #[test]
  fn known_type_mismatches_are_rejected() {
    let m = msgs(entity("count(groups) == \"two\""));
    assert!(m[0].contains("cannot compare"), "{m:?}");
  }

  #[test]
  fn unknown_overlay_fields_defer_to_evaluation() {
    // `department == 3` may be nonsense, but the checker has no schema
    // to prove it, and evaluation reports the mismatch as an error.
    entity("department == 3").unwrap();
  }

  #[test]
  fn a_bad_timestamp_literal_is_caught_when_the_check_is_saved() {
    let m = msgs(entity(
      "last_login_at < days_ago(90) and \
                         days_ago(30) > \"last tuesday\"",
    ));
    assert!(m.iter().any(|s| s.contains("is not a time")), "{m:?}");
  }

  #[test]
  fn a_valid_timestamp_literal_compares_against_a_timestamp() {
    entity("days_ago(90) < \"2025-01-01T00:00:00Z\"").unwrap();
  }

  #[test]
  fn person_selectors_are_rejected_in_an_entity_check() {
    let m = msgs(entity("has_entity(\"idp\")"));
    assert!(m[0].contains("person-scoped check"), "{m:?}");
  }

  #[test]
  fn bare_fields_are_rejected_in_a_person_check_with_a_rewrite() {
    let d = person("status == \"active\"").unwrap_err();
    assert!(d[0].message.contains("no field `status` of its own"));
    assert!(
      d[0]
        .help
        .as_ref()
        .unwrap()
        .contains("entity(\"idp\").status")
    );
  }

  #[test]
  fn predicates_may_read_element_fields_in_a_person_check() {
    person("has_entity(\"workspace\" where status == \"active\")").unwrap();
  }

  #[test]
  fn an_invalid_regex_is_caught_when_the_check_is_saved() {
    let d = entity("entity_key matches \"(unclosed\"").unwrap_err();
    assert!(d[0].message.contains("not a valid regular expression"));
    assert!(d[0].help.is_some());
  }

  #[test]
  fn a_valid_regex_is_compiled_once_and_kept() {
    let p = entity("entity_key matches \"^svc-\\\\d+$\"").unwrap();
    assert!(p.regex("^svc-\\d+$").is_some());
  }

  #[test]
  fn comparing_with_null_is_flagged_rather_than_silently_useless() {
    let m = msgs(entity("last_login_at == null"));
    assert!(m[0].contains("always null"), "{m:?}");
  }

  #[test]
  fn several_independent_type_errors_are_reported_together() {
    let d = entity("count(groups) == \"two\" and 1 < true").unwrap_err();
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d[0].span.start < d[1].span.start, "sorted by position");
  }

  #[test]
  fn a_list_valued_overlay_field_defers_to_evaluation() {
    // Nothing in the language has a statically known list type: an
    // overlay field is `Unknown` until a subject is in hand. So
    // `groups == "eng"` is accepted here and reported as an error at
    // evaluation, where the value is visible. The `Ty::List` guards in
    // `check_comparison` exist for the day a list literal or an inferred
    // overlay schema makes that type reachable.
    assert!(msgs(entity("groups == \"eng\"")).is_empty());
  }

  #[test]
  fn selectors_are_reported_for_the_rules_screen() {
    let p =
      person("has_entity(\"workspace\") and entity(\"okta-prod\").is_admin")
        .unwrap();
    let names: Vec<_> = p.selectors().iter().map(ToString::to_string).collect();
    assert!(names.contains(&"workspace".to_owned()), "{names:?}");
    assert!(names.contains(&"okta-prod".to_owned()), "{names:?}");
  }

  #[test]
  fn days_ago_rejects_nonsense() {
    assert!(!msgs(entity("last_login_at < days_ago(-5)")).is_empty());
    assert!(!msgs(entity("last_login_at < days_ago(1.5)")).is_empty());
  }
}
