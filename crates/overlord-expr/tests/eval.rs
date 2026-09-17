//! Evaluation behaviour, exercised through the public surface.

use overlord_core::{
  EntityStatus, NormalizedRecord, SubjectKind, SystemKind, SystemSelector,
  Timestamp, Value,
};
use overlord_expr::{
  EntityAttrs, EvalCtx, Evaluation, Primary, Schema, Subject, Tri, compile,
  eval,
};

const NOW: &str = "2026-01-15T00:00:00Z";

fn now() -> Timestamp { NOW.parse().unwrap() }

fn attrs(
  system: &str,
  kind: SystemKind,
  key: &str,
  fact_id: i64,
) -> EntityAttrs {
  EntityAttrs {
    normalized: NormalizedRecord::new(
      system,
      kind,
      "user",
      key,
      EntityStatus::Active,
    ),
    raw: serde_json::json!({}),
    fact_id,
  }
}

/// An entity-scoped subject.
struct Ent(EntityAttrs);

impl Subject for Ent {
  fn kind(&self) -> SubjectKind { SubjectKind::Entity }

  fn own(&self) -> Option<&EntityAttrs> { Some(&self.0) }

  fn select(&self, _: &SystemSelector) -> Vec<&EntityAttrs> { Vec::new() }

  fn primary(&self, _: &SystemSelector) -> Primary<'_> { Primary::Missing }
}

/// A person-scoped subject: a bag of entities, with an optional
/// designated primary per selector.
#[derive(Default)]
struct Person {
  entities: Vec<EntityAttrs>,
  /// Selectors the operator has designated a primary for.
  primary:  Vec<(String, usize)>,
}

impl Subject for Person {
  fn kind(&self) -> SubjectKind { SubjectKind::Person }

  fn own(&self) -> Option<&EntityAttrs> { None }

  fn select(&self, sel: &SystemSelector) -> Vec<&EntityAttrs> {
    self
      .entities
      .iter()
      .filter(|e| sel.matches(&e.normalized.system, e.normalized.system_kind))
      .collect()
  }

  fn primary(&self, sel: &SystemSelector) -> Primary<'_> {
    if let Some((_, i)) = self
      .primary
      .iter()
      .find(|(s, _)| s.parse::<SystemSelector>().ok().as_ref() == Some(sel))
    {
      return Primary::Found(&self.entities[*i]);
    }
    let matched = self.select(sel);
    match matched.len() {
      0 => Primary::Missing,
      1 => Primary::Found(matched[0]),
      _ => Primary::Ambiguous,
    }
  }
}

fn run(src: &str, subject: &dyn Subject, kind: SubjectKind) -> Evaluation {
  let program = compile(src, &Schema::new(kind))
    .unwrap_or_else(|d| panic!("{}", d[0].render(src)));
  eval(&program, subject, &EvalCtx::at(now()))
}

fn tri(src: &str, subject: &dyn Subject, kind: SubjectKind) -> Tri {
  run(src, subject, kind)
    .outcome
    .unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn ent_tri(src: &str, e: &Ent) -> Tri { tri(src, e, SubjectKind::Entity) }

fn person_tri(src: &str, p: &Person) -> Tri { tri(src, p, SubjectKind::Person) }

fn user(fields: &[(&str, Value)]) -> Ent {
  let mut a = attrs("gws-prod", SystemKind::Workspace, "ada@example.com", 1);
  for (k, v) in fields {
    assert!(
      a.normalized.insert(*k, v.clone()),
      "{k} is a guaranteed core field; set it on the record instead"
    );
  }
  Ent(a)
}

// --- three-valued logic (SPEC.md section 7) ---------------------------

#[test]
fn only_true_opens_a_violation() {
  let u = user(&[("mfa_enrolled", Value::Bool(false))]);
  assert!(run("not mfa_enrolled", &u, SubjectKind::Entity).opens_violation());

  // The field was never collected: the check must stay quiet rather than
  // firing on absence.
  let blank = user(&[]);
  let e = run("not mfa_enrolled", &blank, SubjectKind::Entity);
  assert!(!e.opens_violation());
  assert_eq!(e.outcome.unwrap(), Tri::Null);
}

#[test]
fn and_or_follow_kleene_logic() {
  let t = ("t", Value::Bool(true));
  let f = ("f", Value::Bool(false));
  let u = user(&[t.clone(), f.clone()]); // `n` is absent, so null

  for (src, want) in [
    ("t and t", Tri::True),
    ("t and f", Tri::False),
    ("t and n", Tri::Null),
    ("f and n", Tri::False), // false short-circuits over null
    ("n and f", Tri::False), // ... from either side
    ("n and n", Tri::Null),
    ("t or n", Tri::True),
    ("n or t", Tri::True),
    ("f or n", Tri::Null),
    ("n or n", Tri::Null),
    ("not n", Tri::Null),
    ("not f", Tri::True),
  ] {
    assert_eq!(ent_tri(src, &u), want, "{src}");
  }
}

#[test]
fn absence_can_be_handled_deliberately() {
  let u = user(&[("mfa_enrolled", Value::Bool(true))]);
  assert_eq!(ent_tri("department is null", &u), Tri::True);
  assert_eq!(ent_tri("exists department", &u), Tri::False);
  assert_eq!(ent_tri("exists mfa_enrolled", &u), Tri::True);
  assert_eq!(
    ent_tri("(department ?? \"none\") == \"none\"", &u),
    Tri::True
  );
}

// --- comparisons and coercion ----------------------------------------

#[test]
fn a_string_literal_reads_as_a_time_against_a_timestamp() {
  let u = user(&[(
    "last_login_at",
    Value::Timestamp("2025-06-01T00:00:00Z".parse().unwrap()),
  )]);
  assert_eq!(
    ent_tri("last_login_at < \"2025-12-31T00:00:00Z\"", &u),
    Tri::True
  );
  assert_eq!(
    ent_tri("last_login_at > \"2025-12-31T00:00:00Z\"", &u),
    Tri::False
  );
}

#[test]
fn a_mismatched_comparison_is_an_error_not_a_silent_false() {
  let u = user(&[("department", Value::from("engineering"))]);
  let e = run("department > 3", &u, SubjectKind::Entity);
  let err = e.outcome.unwrap_err();
  assert!(err.message.contains("cannot compare"), "{}", err.message);
  assert!(err.message.contains("never compares across types silently"));
}

#[test]
fn a_non_boolean_condition_is_an_error_with_the_value_shown() {
  let u = user(&[("department", Value::from("engineering"))]);
  let err = run("department", &u, SubjectKind::Entity)
    .outcome
    .unwrap_err();
  assert!(err.message.contains("expected a yes/no value"), "{err}");
  assert!(err.message.contains("engineering"), "{err}");
}

#[test]
fn membership_is_kleene_too() {
  let u = user(&[
    ("department", Value::from("eng")),
    (
      "groups",
      Value::List(vec![Value::from("eng"), Value::from("ops")]),
    ),
  ]);
  assert_eq!(ent_tri("department in groups", &u), Tri::True);
  assert_eq!(ent_tri("\"hr\" in groups", &u), Tri::False);
  assert_eq!(ent_tri("department in missing_list", &u), Tri::Null);
}

#[test]
fn matches_uses_a_linear_time_engine() {
  let u = Ent(attrs("gws-prod", SystemKind::Workspace, "svc-42", 1));
  assert_eq!(
    ent_tri("entity_key matches \"^svc-\\\\d+$\"", &u),
    Tri::True
  );

  // A pattern that would make a backtracking engine hang. RE2 semantics
  // mean a check cannot hang a sweep (SPEC.md section 7).
  let long = "a".repeat(40);
  let u = user(&[("login_name", Value::from(long))]);
  // Wall-clock timing is the point of this test, so the determinism
  // lint is lifted here rather than in library code.
  #[allow(clippy::disallowed_methods)]
  let start = std::time::Instant::now();
  assert_eq!(ent_tri("login_name matches \"(a+)+$\"", &u), Tri::True);
  assert!(start.elapsed().as_millis() < 500, "regex took too long");
}

// --- relative time ----------------------------------------------------

#[test]
fn days_ago_is_pinned_to_the_sweeps_start_time() {
  let u = user(&[(
    "last_login_at",
    Value::Timestamp("2025-10-01T00:00:00Z".parse().unwrap()),
  )]);
  // 2026-01-15 minus 90 days is 2025-10-17, so an October login is
  // dormant; minus 180 days it is not.
  assert_eq!(ent_tri("last_login_at < days_ago(90)", &u), Tri::True);
  assert_eq!(ent_tri("last_login_at < days_ago(180)", &u), Tri::False);
}

#[test]
fn the_dormant_admin_example_behaves() {
  let src =
    "is_admin and (last_login_at is null or last_login_at < days_ago(90))";
  let never = user(&[("is_admin", Value::Bool(true))]);
  assert_eq!(ent_tri(src, &never), Tri::True);

  let recent = user(&[
    ("is_admin", Value::Bool(true)),
    (
      "last_login_at",
      Value::Timestamp("2026-01-10T00:00:00Z".parse().unwrap()),
    ),
  ]);
  assert_eq!(ent_tri(src, &recent), Tri::False);

  let not_admin = user(&[("is_admin", Value::Bool(false))]);
  assert_eq!(ent_tri(src, &not_admin), Tri::False);
}

// --- collections ------------------------------------------------------

fn with_groups(groups: Vec<Value>) -> Ent {
  user(&[("groups", Value::List(groups))])
}

fn group(name: &str, external: bool) -> Value {
  Value::Object(
    [
      ("name".to_owned(), Value::from(name)),
      ("external".to_owned(), Value::Bool(external)),
    ]
    .into_iter()
    .collect(),
  )
}

#[test]
fn count_any_all_over_a_predicate() {
  let u = with_groups(vec![
    group("eng", false),
    group("partners", true),
    group("vendors", true),
  ]);
  assert_eq!(ent_tri("count(groups) == 3", &u), Tri::True);
  assert_eq!(ent_tri("count(groups where external) == 2", &u), Tri::True);
  assert_eq!(ent_tri("any(groups where external)", &u), Tri::True);
  assert_eq!(ent_tri("all(groups where not external)", &u), Tri::False);
  assert_eq!(ent_tri("not any(groups where external)", &u), Tri::False);
}

#[test]
fn all_over_an_empty_list_is_vacuously_true() {
  let u = with_groups(vec![]);
  assert_eq!(ent_tri("all(groups where external)", &u), Tri::True);
  assert_eq!(ent_tri("any(groups where external)", &u), Tri::False);
  assert_eq!(ent_tri("count(groups) == 0", &u), Tri::True);
}

#[test]
fn a_missing_list_makes_the_collection_null() {
  let u = user(&[]);
  assert_eq!(ent_tri("any(groups where external)", &u), Tri::Null);
  assert_eq!(ent_tri("count(groups) > 0", &u), Tri::Null);
}

#[test]
fn a_predicate_cannot_see_the_enclosing_subject() {
  // `is_admin` is a subject field, not an element field, so inside the
  // predicate it is null — not the subject's value.
  let mut u = with_groups(vec![group("eng", false)]);
  u.0.normalized.insert("is_admin", Value::Bool(true));
  assert_eq!(ent_tri("any(groups where is_admin)", &u), Tri::Null);
  assert_eq!(ent_tri("is_admin", &u), Tri::True);
}

#[test]
fn a_null_element_predicate_does_not_count() {
  let u = with_groups(vec![
    group("eng", false),
    Value::Object([("name".to_owned(), Value::from("mystery"))].into()),
  ]);
  // The second group has no `external` field at all.
  assert_eq!(ent_tri("count(groups where external) == 0", &u), Tri::True);
  assert_eq!(ent_tri("any(groups where external)", &u), Tri::Null);
}

// --- person-scoped selectors ------------------------------------------

#[test]
fn the_orphan_workspace_account_example_behaves() {
  let src = "has_entity(\"workspace\" where status == \"active\") and not \
             has_entity(\"idp\")";

  let orphan = Person {
    entities: vec![attrs("gws-prod", SystemKind::Workspace, "ada", 1)],
    ..Person::default()
  };
  assert_eq!(person_tri(src, &orphan), Tri::True);

  let linked = Person {
    entities: vec![
      attrs("gws-prod", SystemKind::Workspace, "ada", 1),
      attrs("okta-prod", SystemKind::Idp, "ada", 2),
    ],
    ..Person::default()
  };
  assert_eq!(person_tri(src, &linked), Tri::False);
}

#[test]
fn a_selector_matches_a_system_id_as_well_as_a_kind() {
  let p = Person {
    entities: vec![attrs("okta-prod", SystemKind::Idp, "ada", 1)],
    ..Person::default()
  };
  assert_eq!(person_tri("has_entity(\"okta-prod\")", &p), Tri::True);
  assert_eq!(person_tri("has_entity(\"okta-dev\")", &p), Tri::False);
  assert_eq!(person_tri("has_entity(\"idp\")", &p), Tri::True);
}

#[test]
fn count_entities_counts_matching_entities() {
  let p = Person {
    entities: vec![
      attrs("intune", SystemKind::Mdm, "laptop", 1),
      attrs("intune", SystemKind::Mdm, "phone", 2),
    ],
    ..Person::default()
  };
  assert_eq!(person_tri("count_entities(\"mdm\") > 1", &p), Tri::True);
}

#[test]
fn an_ambiguous_primary_is_null_and_flagged_rather_than_guessed() {
  let mut a = attrs("okta-prod", SystemKind::Idp, "ada", 1);
  a.normalized.insert("mfa_enrolled", Value::Bool(false));
  let mut b = attrs("okta-prod", SystemKind::Idp, "ada-admin", 2);
  b.normalized.insert("mfa_enrolled", Value::Bool(true));

  let p = Person {
    entities: vec![a, b],
    ..Person::default()
  };
  let e = run("not entity(\"idp\").mfa_enrolled", &p, SubjectKind::Person);
  assert_eq!(e.outcome.unwrap(), Tri::Null, "must not guess an entity");
  assert_eq!(e.ambiguous, ["idp"]);
}

#[test]
fn a_designated_primary_resolves_the_ambiguity() {
  let mut a = attrs("okta-prod", SystemKind::Idp, "ada", 1);
  a.normalized.insert("mfa_enrolled", Value::Bool(false));
  let mut b = attrs("okta-prod", SystemKind::Idp, "ada-admin", 2);
  b.normalized.insert("mfa_enrolled", Value::Bool(true));

  let p = Person {
    entities: vec![a, b],
    primary:  vec![("idp".to_owned(), 0)],
  };
  let e = run("not entity(\"idp\").mfa_enrolled", &p, SubjectKind::Person);
  assert_eq!(e.outcome.unwrap(), Tri::True);
  assert!(e.ambiguous.is_empty());
}

#[test]
fn a_missing_primary_is_null_but_not_ambiguous() {
  let p = Person::default();
  let e = run("entity(\"idp\").mfa_enrolled", &p, SubjectKind::Person);
  assert_eq!(e.outcome.unwrap(), Tri::Null);
  assert!(e.ambiguous.is_empty(), "absent is not ambiguous");
}

// --- evidence ---------------------------------------------------------

#[test]
fn evidence_records_the_leaves_that_made_the_condition_true() {
  let u = user(&[("mfa_enrolled", Value::Bool(false))]);
  let e = run(
    "status == \"active\" and not mfa_enrolled",
    &u,
    SubjectKind::Entity,
  );
  assert!(e.opens_violation());
  let leaves: Vec<_> =
    e.evidence.leaves.iter().map(|l| l.expr.as_str()).collect();
  assert_eq!(leaves, ["status", "mfa_enrolled"]);
  assert_eq!(e.evidence.leaves[1].value, Value::Bool(false));
  assert_eq!(
    e.evidence.leaves[0].fact_ids,
    [1],
    "evidence names its fact"
  );
}

#[test]
fn evidence_omits_a_short_circuited_branch() {
  let u = user(&[("is_admin", Value::Bool(true))]);
  let e = run("is_admin or count(groups) > 0", &u, SubjectKind::Entity);
  let leaves: Vec<_> =
    e.evidence.leaves.iter().map(|l| l.expr.as_str()).collect();
  assert_eq!(leaves, ["is_admin"], "the untaken branch is not evidence");
}

#[test]
fn evidence_reports_an_aggregate_rather_than_every_element() {
  let u = with_groups(vec![group("a", true), group("b", true)]);
  let e = run("count(groups where external) > 1", &u, SubjectKind::Entity);
  assert!(e.opens_violation());
  let leaves: Vec<_> =
    e.evidence.leaves.iter().map(|l| l.expr.as_str()).collect();
  assert_eq!(leaves, ["count(groups where external)"]);
  assert_eq!(e.evidence.leaves[0].value, Value::from(2_i64));
}

#[test]
fn person_evidence_names_the_facts_it_read() {
  let p = Person {
    entities: vec![attrs("gws-prod", SystemKind::Workspace, "ada", 7)],
    ..Person::default()
  };
  let e = run("has_entity(\"workspace\")", &p, SubjectKind::Person);
  assert!(e.opens_violation());
  assert_eq!(e.evidence.leaves[0].fact_ids, [7]);
}

// --- the raw escape hatch ---------------------------------------------

#[test]
fn raw_reaches_the_vendor_payload() {
  let mut u = user(&[]);
  u.0.raw = serde_json::json!({"profile": {"costCenter": "R&D"}});
  assert_eq!(ent_tri("raw.profile.costCenter == \"R&D\"", &u), Tri::True);
  assert_eq!(ent_tri("raw.profile.missing is null", &u), Tri::True);
}
