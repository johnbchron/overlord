//! End-to-end: fixture scenarios through the sweep engine, the
//! evaluator, and the violation lifecycle.

use overlord_connect::Registry;
use overlord_connector_fixture::FixtureConnector;
use overlord_core::{
  Actor, CheckDraft, CheckId, CommandKind, EntityRef, NewCommand, PersonUid,
  Severity, SubjectKind, SubjectRef, SuppressReason, SystemId, Timestamp,
  ViolationState,
};
use overlord_engine::{
  SweepOutcome, SweepPlan, SweepProgress, SystemConfig, checks, run_sweep,
  run_sweep_with_progress,
};
use overlord_store::{Db, SweepStatus, SystemStatus};

/// A fixed instant, so `days_ago` windows are stable in tests.
const NOW: &str = "2026-02-01T00:00:00Z";

fn ts(s: &str) -> Timestamp { s.parse().unwrap() }

fn actor() -> Actor { Actor::new("cli:test") }

fn registry() -> Registry { Registry::new().with(FixtureConnector::boxed()) }

fn fixture(name: &str) -> String {
  format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn system(id: &str, scenario: &str, stage: usize) -> SystemConfig {
  SystemConfig {
    id:        SystemId::new(id),
    connector: "fixture".to_owned(),
    config:    serde_json::json!({
      "path": fixture(scenario),
      "stage": stage
    }),
  }
}

fn plan(systems: Vec<SystemConfig>, at: &str) -> SweepPlan {
  SweepPlan::new(systems).at(ts(at))
}

/// The same, with a guard threshold suited to a tiny population.
///
/// The baseline fixture has four accounts, so a single departure is 25%
/// of the system and the default 10% guard refuses it. That is the spec's
/// rule applied literally, and it is a real edge of the design at small
/// N — see PROGRESS.md. Tests that are about something else say so here
/// rather than working around it.
fn plan_guard(systems: Vec<SystemConfig>, at: &str, pct: u32) -> SweepPlan {
  let mut p = plan(systems, at);
  p.absence_guard_pct = pct;
  p
}

async fn sweep(db: &Db, plan: &SweepPlan) -> SweepOutcome {
  run_sweep(db, &registry(), plan).await.unwrap()
}

fn draft(
  id: &str,
  condition: &str,
  severity: Severity,
  applies_to: SubjectKind,
) -> CheckDraft {
  CheckDraft {
    id: CheckId::new(id),
    name: id.to_owned(),
    description: None,
    rationale: None,
    remediation: None,
    references: vec![],
    severity,
    weight: None,
    applies_to,
    systems: vec![],
    entity_types: vec![],
    condition: condition.to_owned(),
    suppress_if_pending_links: false,
  }
}

/// Author, dry-run and enable a check — the order SPEC.md section 7
/// requires.
fn install(db: &Db, d: &CheckDraft) {
  let at = ts(NOW);
  let rev = checks::upsert(db, &actor(), d, at, None).unwrap();
  checks::dry_run(db, &actor(), &d.id, rev, at).unwrap();
  checks::enable(db, &actor(), &d.id, at).unwrap();
}

fn mfa_missing() -> CheckDraft {
  draft(
    "mfa-missing",
    "status == \"active\" and not mfa_enrolled",
    Severity::Critical,
    SubjectKind::Entity,
  )
}

fn dormant_admin() -> CheckDraft {
  draft(
    "dormant-admin",
    "is_admin and (last_login_at is null or last_login_at < days_ago(90))",
    Severity::High,
    SubjectKind::Entity,
  )
}

fn orphan_workspace() -> CheckDraft {
  draft(
    "orphan-workspace",
    "has_entity(\"workspace\" where status == \"active\") and not \
     has_entity(\"idp\")",
    Severity::High,
    SubjectKind::Person,
  )
}

/// Active violations, as `(check, subject key)` pairs, sorted.
fn active(db: &Db) -> Vec<(String, String)> {
  let mut v = db
    .read(|r| -> overlord_store::Result<_> {
      r.violations(&[ViolationState::Open, ViolationState::Acknowledged], 500)
    })
    .unwrap()
    .into_iter()
    .map(|v| {
      let subject = match &v.subject {
        SubjectRef::Entity(e) => e.entity_key.to_string(),
        SubjectRef::Person(p) => p.to_string(),
      };
      (v.check_id.to_string(), subject)
    })
    .collect::<Vec<_>>();
  v.sort();
  v
}

fn state_of(
  db: &Db,
  check: &str,
  subject: &SubjectRef,
) -> Option<ViolationState> {
  db.read(|r| -> overlord_store::Result<_> {
    r.violations(
      &[
        ViolationState::Open,
        ViolationState::Acknowledged,
        ViolationState::Suppressed,
        ViolationState::FalsePositive,
        ViolationState::Resolved,
      ],
      500,
    )
  })
  .unwrap()
  .into_iter()
  .find(|v| v.check_id.as_str() == check && &v.subject == subject)
  .map(|v| v.state)
}

fn ws(key: &str) -> SubjectRef {
  SubjectRef::Entity(EntityRef::new("ws", "user", key))
}

// --- the ordinary case ------------------------------------------------

#[tokio::test]
async fn a_sweep_opens_the_violations_the_facts_justify() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  install(&db, &dormant_admin());

  let out =
    sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;

  assert_eq!(out.status, SweepStatus::Ok);
  assert_eq!(out.systems[0].observed_count, 4);
  assert_eq!(out.systems[0].tombstoned, 0);
  assert_eq!(out.evaluation.opened, 4);

  assert_eq!(active(&db), [
    ("dormant-admin".to_owned(), "grace@example.com".to_owned()),
    (
      "dormant-admin".to_owned(),
      "svc-deploy@example.com".to_owned()
    ),
    ("mfa-missing".to_owned(), "grace@example.com".to_owned()),
    (
      "mfa-missing".to_owned(),
      "svc-deploy@example.com".to_owned()
    ),
  ]);
}

#[tokio::test]
async fn a_fixed_condition_resolves_and_a_departure_resolves_too() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  install(&db, &dormant_admin());

  sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;
  // Stage 1: grace enrols in MFA and logs in; alan has left.
  let out = sweep(
    &db,
    &plan_guard(
      vec![system("ws", "baseline.json", 1)],
      "2026-02-02T00:00:00Z",
      30,
    ),
  )
  .await;

  assert_eq!(out.systems[0].tombstoned, 1, "alan is gone");
  assert_eq!(out.evaluation.resolved, 2, "grace's two violations clear");

  assert_eq!(active(&db), [
    (
      "dormant-admin".to_owned(),
      "svc-deploy@example.com".to_owned()
    ),
    (
      "mfa-missing".to_owned(),
      "svc-deploy@example.com".to_owned()
    ),
  ]);
  assert_eq!(
    state_of(&db, "mfa-missing", &ws("grace@example.com")),
    Some(ViolationState::Resolved)
  );
}

// --- the absence guard (SPEC.md section 10) ---------------------------

#[tokio::test]
async fn a_truncated_snapshot_never_presents_as_mass_deprovisioning() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());

  sweep(&db, &plan(vec![system("ws", "mass-absence.json", 0)], NOW)).await;
  assert_eq!(db.read(|r| r.counts()).unwrap().entities, 10);

  // Stage 1 claims a *complete* snapshot containing one of ten accounts.
  let out =
    sweep(&db, &plan(vec![system("ws", "mass-absence.json", 1)], NOW)).await;

  assert!(out.systems[0].guard_tripped, "the guard must refuse this");
  assert_eq!(out.systems[0].tombstoned, 0, "nothing may be tombstoned");
  assert_eq!(out.status, SweepStatus::Partial);
  assert_eq!(
    db.read(|r| r.counts()).unwrap().entities,
    10,
    "all ten accounts are still present"
  );
  assert!(
    out
      .warnings
      .iter()
      .any(|w| w.contains("confirm the snapshot")),
    "{:?}",
    out.warnings
  );
}

#[tokio::test]
async fn a_departure_within_the_guards_threshold_is_written() {
  let db = Db::open_memory().unwrap();
  sweep(&db, &plan(vec![system("ws", "mass-absence.json", 0)], NOW)).await;

  // Stage 2 drops one of ten. 10% is not *more than* 10%, so it stands.
  let out =
    sweep(&db, &plan(vec![system("ws", "mass-absence.json", 2)], NOW)).await;
  assert!(!out.systems[0].guard_tripped);
  assert_eq!(out.systems[0].tombstoned, 1);
  assert_eq!(db.read(|r| r.counts()).unwrap().entities, 9);
}

#[tokio::test]
async fn a_partial_snapshot_never_tombstones_whatever_it_omits() {
  let db = Db::open_memory().unwrap();
  sweep(&db, &plan(vec![system("ws", "truncated.json", 0)], NOW)).await;
  assert_eq!(db.read(|r| r.counts()).unwrap().entities, 4);

  let out =
    sweep(&db, &plan(vec![system("ws", "truncated.json", 1)], NOW)).await;
  assert_eq!(out.systems[0].status, SystemStatus::Partial);
  assert_eq!(out.systems[0].tombstoned, 0);
  assert_eq!(db.read(|r| r.counts()).unwrap().entities, 4);
  assert!(
    out
      .warnings
      .iter()
      .any(|w| w.contains("snapshot is partial")),
    "{:?}",
    out.warnings
  );
}

// --- per-system transactionality --------------------------------------

#[tokio::test]
async fn one_failing_connector_does_not_lose_another_systems_results() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());

  let out = sweep(
    &db,
    &plan(
      vec![
        system("broken", "unreachable.json", 1),
        system("ws", "baseline.json", 0),
      ],
      NOW,
    ),
  )
  .await;

  let broken = out
    .systems
    .iter()
    .find(|s| s.system.as_str() == "broken")
    .unwrap();
  let ws_sys = out
    .systems
    .iter()
    .find(|s| s.system.as_str() == "ws")
    .unwrap();
  assert_eq!(broken.status, SystemStatus::Failed);
  assert!(broken.error.as_ref().unwrap().contains("unreachable"));
  assert_eq!(ws_sys.status, SystemStatus::Ok);
  assert_eq!(ws_sys.observed_count, 4);
  assert_eq!(out.status, SweepStatus::Partial);

  // The healthy system's violations opened regardless.
  assert_eq!(active(&db).len(), 2);
}

// --- person scope and implicit persons (SPEC.md section 6.4) ----------

#[tokio::test]
async fn orphan_accounts_are_found_before_any_linking_happens() {
  let db = Db::open_memory().unwrap();
  install(&db, &orphan_workspace());

  sweep(
    &db,
    &plan(
      vec![
        system("ws", "baseline.json", 0),
        system("idp", "idp.json", 0),
      ],
      NOW,
    ),
  )
  .await;

  // Nothing is linked yet, so every entity is an implicit singleton
  // person. ada and grace each hold only a workspace account *as an
  // implicit person*, so they look like orphans too — which is exactly
  // why linking is the operator's next job, and why the check is
  // person-scoped rather than entity-scoped.
  let orphans: Vec<String> = active(&db).into_iter().map(|(_, s)| s).collect();
  assert!(
    orphans.iter().any(|s| s.contains("svc-deploy@example.com")),
    "{orphans:?}"
  );
  assert!(
    orphans.iter().all(|s| s.starts_with("implicit:")),
    "{orphans:?}"
  );
}

#[tokio::test]
async fn confirming_a_link_never_changes_the_total_score() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  install(&db, &dormant_admin());

  sweep(
    &db,
    &plan(
      vec![
        system("ws", "baseline.json", 0),
        system("idp", "idp.json", 0),
      ],
      NOW,
    ),
  )
  .await;

  let before = db.write(|w| w.total_score()).unwrap();
  let scores_before: i64 = db
    .read(|r| r.top_subjects(500))
    .unwrap()
    .iter()
    .map(|s| s.score)
    .sum();
  assert_eq!(before, scores_before, "scores must account for everything");

  // Link grace's two accounts to one person.
  let uid = PersonUid::new("P-grace");
  for system_id in ["ws", "idp"] {
    db.write(|w| {
      w.append_command(&NewCommand::new(
        actor(),
        CommandKind::PersonLink {
          person_uid:      uid.clone(),
          entity:          EntityRef::new(
            system_id,
            "user",
            "grace@example.com",
          ),
          from_suggestion: None,
        },
        ts(NOW),
      ))
    })
    .unwrap();
  }
  db.write(|w| w.recompute_scores()).unwrap();

  let after: i64 = db
    .read(|r| r.top_subjects(500))
    .unwrap()
    .iter()
    .map(|s| s.score)
    .sum();
  assert_eq!(
    after, scores_before,
    "linking merges two scores; it must not reveal or lose one"
  );

  let graces = db
    .read(|r| r.top_subjects(500))
    .unwrap()
    .into_iter()
    .find(|s| s.person_uid == uid)
    .expect("the linked person should now be scored");
  assert!(!graces.implicit);
  assert_eq!(graces.worst_severity, Some(Severity::Critical));
}

// --- the lifecycle (SPEC.md section 9) --------------------------------

#[tokio::test]
async fn an_acknowledgement_persists_across_sweeps() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;

  let subject = ws("grace@example.com");
  db.write(|w| {
    w.append_command(&NewCommand::new(
      actor(),
      CommandKind::ViolationAcknowledge {
        check_id: CheckId::new("mfa-missing"),
        subject:  subject.clone(),
      },
      ts(NOW),
    ))
  })
  .unwrap();
  assert_eq!(
    state_of(&db, "mfa-missing", &subject),
    Some(ViolationState::Acknowledged)
  );

  // The same facts again: the overlay survives.
  sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;
  assert_eq!(
    state_of(&db, "mfa-missing", &subject),
    Some(ViolationState::Acknowledged)
  );
}

#[tokio::test]
async fn a_regression_starts_clean_rather_than_pre_silenced() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  let subject = ws("grace@example.com");

  sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;
  db.write(|w| {
    w.append_command(&NewCommand::new(
      actor(),
      CommandKind::ViolationAcknowledge {
        check_id: CheckId::new("mfa-missing"),
        subject:  subject.clone(),
      },
      ts(NOW),
    ))
  })
  .unwrap();

  // Fixed (stage 1), then broken again (stage 0).
  sweep(&db, &plan(vec![system("ws", "baseline.json", 1)], NOW)).await;
  assert_eq!(
    state_of(&db, "mfa-missing", &subject),
    Some(ViolationState::Resolved)
  );

  let out =
    sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;
  assert_eq!(out.evaluation.regressed, 1);
  assert_eq!(
    state_of(&db, "mfa-missing", &subject),
    Some(ViolationState::Open),
    "a reopened violation must not inherit the acknowledgement"
  );

  // The earlier episode and its acknowledgement stay in the history.
  let events: Vec<String> = db
    .read(|r| -> overlord_store::Result<_> {
      let mut stmt = r.conn().prepare(
        "SELECT kind FROM violation_event
          WHERE check_id = 'mfa-missing' AND subject_ref = ?1
          ORDER BY id",
      )?;
      let rows =
        stmt.query_map([subject.to_string()], |x| x.get::<_, String>(0))?;
      Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap();
  assert_eq!(events, ["opened", "acknowledged", "cleared", "regressed"]);
}

#[tokio::test]
async fn a_suppression_expires_against_the_sweeps_own_clock() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  let subject = ws("grace@example.com");

  sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;
  db.write(|w| {
    w.append_command(&NewCommand::new(
      actor(),
      CommandKind::ViolationSuppress {
        check_id: CheckId::new("mfa-missing"),
        subject:  subject.clone(),
        reason:   SuppressReason::AcceptedRisk,
        until:    Some(ts("2026-03-01T00:00:00Z")),
      },
      ts(NOW),
    ))
  })
  .unwrap();

  // A sweep before the expiry leaves it suppressed.
  sweep(
    &db,
    &plan(
      vec![system("ws", "baseline.json", 0)],
      "2026-02-15T00:00:00Z",
    ),
  )
  .await;
  assert_eq!(
    state_of(&db, "mfa-missing", &subject),
    Some(ViolationState::Suppressed)
  );

  // A sweep after it reopens the violation, because the condition holds.
  let out = sweep(
    &db,
    &plan(
      vec![system("ws", "baseline.json", 0)],
      "2026-03-02T00:00:00Z",
    ),
  )
  .await;
  assert_eq!(out.evaluation.expired, 1);
  assert_eq!(
    state_of(&db, "mfa-missing", &subject),
    Some(ViolationState::Open)
  );
}

#[tokio::test]
async fn disabling_a_check_resolves_its_violations_at_once() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;
  assert_eq!(active(&db).len(), 2);

  checks::disable(&db, &actor(), &CheckId::new("mfa-missing"), ts(NOW))
    .unwrap();
  assert!(active(&db).is_empty(), "the board is honest immediately");

  let reason: String = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(r.conn().query_row(
        "SELECT resolve_reason FROM violation LIMIT 1",
        [],
        |x| x.get(0),
      )?)
    })
    .unwrap();
  assert_eq!(reason, "check_disabled");
}

// --- the contract (SPEC.md section 13) --------------------------------

#[tokio::test]
async fn replaying_the_streams_reproduces_the_live_projections() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  install(&db, &dormant_admin());
  install(&db, &orphan_workspace());

  // A history with sweeps, a partial sweep, operator commands landing
  // between them, a regression, and a merge.
  sweep(
    &db,
    &plan(
      vec![
        system("ws", "baseline.json", 0),
        system("idp", "idp.json", 0),
      ],
      NOW,
    ),
  )
  .await;

  let subject = ws("grace@example.com");
  db.write(|w| {
    w.append_command(&NewCommand::new(
      actor(),
      CommandKind::ViolationAcknowledge {
        check_id: CheckId::new("mfa-missing"),
        subject:  subject.clone(),
      },
      ts("2026-02-01T06:00:00Z"),
    ))
  })
  .unwrap();

  db.write(|w| {
    w.append_command(&NewCommand::new(
      actor(),
      CommandKind::PersonLink {
        person_uid:      PersonUid::new("P-ada"),
        entity:          EntityRef::new("ws", "user", "ada@example.com"),
        from_suggestion: None,
      },
      ts("2026-02-01T07:00:00Z"),
    ))
  })
  .unwrap();

  // A partial sweep of one system only.
  sweep(
    &db,
    &plan(
      vec![system("ws", "baseline.json", 1)],
      "2026-02-02T00:00:00Z",
    ),
  )
  .await;
  // ... and the regression.
  sweep(
    &db,
    &plan(
      vec![system("ws", "baseline.json", 0)],
      "2026-02-03T00:00:00Z",
    ),
  )
  .await;

  let before = dump(&db);
  assert!(
    before.iter().any(|r| r.starts_with("violation:")),
    "the fixture should have produced violations to compare"
  );

  let report = overlord_engine::rebuild(&db).unwrap();
  assert!(report.sweeps >= 3, "{report:?}");

  assert_eq!(dump(&db), before, "replay(streams) must equal live");
}

/// Every projection row, as stable sorted text.
fn dump(db: &Db) -> Vec<String> {
  const TABLES: [&str; 9] = [
    "entity",
    "identity_policy",
    "person",
    "person_alias",
    "link",
    "check_head",
    "check_revision",
    "violation",
    "person_score",
  ];
  let mut out = Vec::new();
  for table in TABLES {
    let mut rows = db
      .read(|r| -> overlord_store::Result<_> {
        let mut stmt = r.conn().prepare(&format!("SELECT * FROM {table}"))?;
        let cols = stmt.column_count();
        let rows = stmt.query_map([], |row| {
          use rusqlite::types::ValueRef;
          let mut s = format!("{table}: ");
          for i in 0..cols {
            match row.get_ref(i)? {
              ValueRef::Null => s.push_str("NULL"),
              ValueRef::Integer(v) => s.push_str(&v.to_string()),
              ValueRef::Real(v) => s.push_str(&v.to_string()),
              ValueRef::Text(v) => s.push_str(&String::from_utf8_lossy(v)),
              ValueRef::Blob(v) => s.push_str(&format!("<{} bytes>", v.len())),
            }
            s.push('|');
          }
          Ok(s)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
      })
      .unwrap();
    rows.sort();
    out.extend(rows);
  }
  out
}

/// The absence guard is a percentage, which is sharp at small N.
///
/// SPEC.md section 10 specifies a share of the system's entities with a
/// 10% default, and this is that rule applied literally: in a four-person
/// system one ordinary departure is 25%, so the guard refuses it and asks
/// the operator to confirm. That is safe, but it means a small tenant
/// sees the guard on nearly every real departure. Recorded here as
/// behaviour rather than fixed silently — see PROGRESS.md.
#[tokio::test]
async fn the_percentage_guard_is_sharp_in_a_small_system() {
  let db = Db::open_memory().unwrap();
  sweep(&db, &plan(vec![system("ws", "baseline.json", 0)], NOW)).await;

  let out = sweep(
    &db,
    &plan(
      vec![system("ws", "baseline.json", 1)],
      "2026-02-02T00:00:00Z",
    ),
  )
  .await;

  assert!(out.systems[0].guard_tripped, "one of four is 25%");
  assert_eq!(out.systems[0].tombstoned, 0);
  assert_eq!(
    db.read(|r| r.counts()).unwrap().entities,
    4,
    "the departed account is still present, pending confirmation"
  );
}

// --- "new since last sweep" is per system (SPEC.md sections 8, 10) ----

/// Violations flagged as new, as `(check, subject key)`, sorted.
fn new_since(db: &Db) -> Vec<(String, String)> {
  let mut v = db
    .read(|r| -> overlord_store::Result<_> {
      r.violations(&[ViolationState::Open, ViolationState::Acknowledged], 500)
    })
    .unwrap()
    .into_iter()
    .filter(|v| v.new_since)
    .map(|v| {
      let subject = match &v.subject {
        SubjectRef::Entity(e) => {
          format!("{}/{}", e.system, e.entity_key)
        }
        SubjectRef::Person(p) => p.to_string(),
      };
      (v.check_id.to_string(), subject)
    })
    .collect::<Vec<_>>();
  v.sort();
  v
}

#[tokio::test]
async fn restricting_a_sweep_does_not_manufacture_change_elsewhere() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());

  // Sweep both systems. Everything found is new.
  sweep(
    &db,
    &plan(
      vec![
        system("ws", "baseline.json", 0),
        system("idp", "idp.json", 0),
      ],
      NOW,
    ),
  )
  .await;

  let after_full = new_since(&db);
  assert_eq!(after_full, [
    ("mfa-missing".to_owned(), "idp/grace@example.com".to_owned()),
    ("mfa-missing".to_owned(), "ws/grace@example.com".to_owned()),
    (
      "mfa-missing".to_owned(),
      "ws/svc-deploy@example.com".to_owned()
    ),
  ]);

  // Now sweep only `ws`, with the same facts. Nothing about `idp`
  // changed, and nothing looked at it — so its section must not move.
  sweep(
    &db,
    &plan(
      vec![system("ws", "baseline.json", 0)],
      "2026-02-02T00:00:00Z",
    ),
  )
  .await;

  assert_eq!(
    new_since(&db),
    [("mfa-missing".to_owned(), "idp/grace@example.com".to_owned())],
    "idp's newest finding is still its newest; ws's are no longer new because \
     a later sweep re-confirmed them"
  );

  // The bug this replaces: comparing against the latest sweep overall
  // would have emptied the section entirely, because nothing opened in
  // sweep 2 — a restriction to `ws` silently erasing `idp`'s news.
  let latest = db.read(|r| r.latest_sweep()).unwrap().unwrap();
  let globally_new = db
    .read(|r| -> overlord_store::Result<_> {
      r.violations(&[ViolationState::Open, ViolationState::Acknowledged], 500)
    })
    .unwrap()
    .into_iter()
    .filter(|v| v.opened_sweep == latest)
    .count();
  assert_eq!(globally_new, 0, "the global comparison finds nothing");
}

#[tokio::test]
async fn a_violation_opening_in_the_latest_sweep_of_its_system_is_new() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());

  // Stage 1 of the baseline has grace enrolled, so only the robot fails.
  sweep(&db, &plan(vec![system("ws", "baseline.json", 1)], NOW)).await;
  assert_eq!(new_since(&db), [(
    "mfa-missing".to_owned(),
    "ws/svc-deploy@example.com".to_owned()
  )]);

  // Stage 0 has grace unenrolled again: a genuinely new violation, on a
  // system this sweep did cover.
  sweep(
    &db,
    &plan_guard(
      vec![system("ws", "baseline.json", 0)],
      "2026-02-02T00:00:00Z",
      30,
    ),
  )
  .await;
  assert_eq!(
    new_since(&db),
    [("mfa-missing".to_owned(), "ws/grace@example.com".to_owned())],
    "the robot's violation is standing, not new"
  );
}

/// One progress event, flattened to a string so the sequence can be
/// compared in one assertion.
fn label(progress: &SweepProgress) -> String {
  match progress {
    SweepProgress::Opened { systems, .. } => format!("opened {systems}"),
    SweepProgress::SystemStarted {
      system,
      index,
      total,
    } => {
      format!("start {system} {index}/{total}")
    }
    SweepProgress::SystemFinished {
      system,
      index,
      total,
    } => {
      format!("done {system} {index}/{total}")
    }
    SweepProgress::Detail { note, .. } => format!("detail: {note}"),
    SweepProgress::Evaluating { systems } => format!("evaluating {systems}"),
  }
}

#[tokio::test]
async fn progress_narrates_each_system_in_order() {
  use std::sync::{Arc, Mutex};

  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());

  let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
  let sink = Arc::clone(&seen);
  run_sweep_with_progress(
    &db,
    &registry(),
    &plan(
      vec![
        system("ws", "baseline.json", 0),
        system("idp", "baseline.json", 0),
      ],
      NOW,
    ),
    move |progress| sink.lock().unwrap().push(label(&progress)),
  )
  .await
  .unwrap();

  let seen = seen.lock().unwrap().clone();
  assert_eq!(seen, [
    "opened 2".to_owned(),
    "start ws 1/2".to_owned(),
    "done ws 1/2".to_owned(),
    "start idp 2/2".to_owned(),
    "done idp 2/2".to_owned(),
    "evaluating 2".to_owned(),
  ]);
}

#[tokio::test]
async fn nothing_is_new_in_a_system_that_has_never_been_swept() {
  let db = Db::open_memory().unwrap();
  install(&db, &mfa_missing());
  assert!(new_since(&db).is_empty());
  assert!(
    db.read(|r| r.latest_sweep_per_system()).unwrap().is_empty(),
    "no system has a benchmark to compare against yet"
  );
}

// --- scoping a check to a connector ------------------------------------

/// A connector is the third answer to "which systems", and the one the
/// other two cannot give: a kind is every system of that category, an id
/// is one system, and neither says "every console this adapter reads".
#[tokio::test]
async fn a_check_can_be_scoped_to_the_connector_that_read_the_system() {
  let db = Db::open_memory().unwrap();
  let systems = vec![
    system("gws-prod", "baseline.json", 0),
    system("okta-prod", "idp.json", 0),
  ];
  sweep(&db, &plan(systems.clone(), NOW)).await;

  let mut scoped = mfa_missing();
  scoped.id = CheckId::new("mfa-missing-fixture");
  scoped.systems = vec!["connector:fixture".parse().unwrap()];
  install(&db, &scoped);

  let mut elsewhere = mfa_missing();
  elsewhere.id = CheckId::new("mfa-missing-elsewhere");
  elsewhere.systems = vec!["connector:unifi-access".parse().unwrap()];
  install(&db, &elsewhere);

  let out = sweep(&db, &plan(systems, "2026-02-02T00:00:00Z")).await;
  assert!(
    out.evaluation.errors.is_empty(),
    "{:?}",
    out.evaluation.errors
  );

  let open = db
    .read(|r| r.violations(&[ViolationState::Open], 500))
    .unwrap()
    .into_iter()
    .filter(|v| v.check_id.as_str().starts_with("mfa-missing-"))
    .collect::<Vec<_>>();

  // Both systems are read by `fixture`, so the scoped check behaves
  // exactly like an unscoped one.
  assert!(
    open
      .iter()
      .any(|v| v.check_id.as_str() == "mfa-missing-fixture"),
    "{open:?}"
  );
  // And nothing is read by `unifi-access`, so that check selects nobody
  // rather than everybody.
  assert!(
    !open
      .iter()
      .any(|v| v.check_id.as_str() == "mfa-missing-elsewhere"),
    "{open:?}"
  );
}

/// The same selector inside a condition, which is where "does this
/// person hold an account on any console of this kind" gets asked.
#[tokio::test]
async fn a_connector_selector_works_inside_a_condition_too() {
  let db = Db::open_memory().unwrap();
  let systems = vec![system("gws-prod", "baseline.json", 0)];
  sweep(&db, &plan(systems.clone(), NOW)).await;

  let holds = draft(
    "held-by-fixture",
    "has_entity(\"connector:fixture\")",
    Severity::Low,
    SubjectKind::Person,
  );
  install(&db, &holds);

  let missing = draft(
    "held-by-unifi",
    "has_entity(\"connector:unifi-access\")",
    Severity::Low,
    SubjectKind::Person,
  );
  install(&db, &missing);

  let out = sweep(&db, &plan(systems, "2026-02-02T00:00:00Z")).await;
  assert!(
    out.evaluation.errors.is_empty(),
    "{:?}",
    out.evaluation.errors
  );

  let open = db
    .read(|r| r.violations(&[ViolationState::Open], 500))
    .unwrap();
  assert!(
    open
      .iter()
      .any(|v| v.check_id.as_str() == "held-by-fixture"),
    "{open:?}"
  );
  assert!(
    !open.iter().any(|v| v.check_id.as_str() == "held-by-unifi"),
    "{open:?}"
  );
}

// --- a dry-run answers the sweep's question -----------------------------

/// The count an operator reads before enabling has to be the count the
/// sweep will act on. It was not: the dry-run evaluated every subject in
/// the world while the sweep applies the check's scope, so a rule scoped
/// to nothing dry-ran full and then opened nothing, with no screen
/// explaining the gap.
#[tokio::test]
async fn a_dry_run_counts_only_the_subjects_a_sweep_would_evaluate() {
  let db = Db::open_memory().unwrap();
  let systems = vec![system("gws-prod", "baseline.json", 0)];
  sweep(&db, &plan(systems.clone(), NOW)).await;

  let mut scoped = mfa_missing();
  scoped.id = CheckId::new("mfa-missing-elsewhere");
  scoped.systems = vec!["okta-prod".parse().unwrap()];

  let rev = checks::upsert(&db, &actor(), &scoped, ts(NOW), None).unwrap();
  let dry = checks::dry_run(&db, &actor(), &scoped.id, rev, ts(NOW)).unwrap();
  assert_eq!(dry.in_scope, 0, "no gws-prod account is an okta-prod one");
  assert_eq!(dry.match_count, 0);

  // And the sweep agrees, which is the whole point.
  checks::enable(&db, &actor(), &scoped.id, ts(NOW)).unwrap();
  let out = sweep(&db, &plan(systems.clone(), "2026-02-02T00:00:00Z")).await;
  assert_eq!(out.evaluation.opened, 0, "{:?}", out.evaluation);

  // Unscoped, the same rule matches — so the zero above is the scope,
  // not the condition.
  let mut open = mfa_missing();
  open.id = CheckId::new("mfa-missing-anywhere");
  let rev = checks::upsert(&db, &actor(), &open, ts(NOW), None).unwrap();
  let dry = checks::dry_run(&db, &actor(), &open.id, rev, ts(NOW)).unwrap();
  assert!(dry.in_scope > 0, "{dry:?}");
  assert!(dry.match_count > 0, "{dry:?}");

  checks::enable(&db, &actor(), &open.id, ts(NOW)).unwrap();
  let out = sweep(&db, &plan(systems, "2026-02-03T00:00:00Z")).await;
  assert_eq!(
    out.evaluation.opened as u64, dry.match_count,
    "the dry-run count is what the sweep opens"
  );
}

/// `suppress_if_pending_links` is a skip the sweep applies and the
/// dry-run did not, so a check written to stay quiet until identity work
/// is done still predicted noise.
#[tokio::test]
async fn a_dry_run_honours_suppress_if_pending_links() {
  let db = Db::open_memory().unwrap();
  let systems = vec![
    system("gws-prod", "identity-ws.json", 0),
    system("okta-prod", "identity-idp.json", 0),
  ];
  sweep(&db, &plan(systems, NOW)).await;

  let mut d = draft(
    "everyone",
    "entity_type == \"user\"",
    Severity::Low,
    SubjectKind::Entity,
  );
  let rev = checks::upsert(&db, &actor(), &d, ts(NOW), None).unwrap();
  let loud = checks::dry_run(&db, &actor(), &d.id, rev, ts(NOW)).unwrap();

  d.id = CheckId::new("everyone-quiet");
  d.suppress_if_pending_links = true;
  let rev = checks::upsert(&db, &actor(), &d, ts(NOW), None).unwrap();
  let quiet = checks::dry_run(&db, &actor(), &d.id, rev, ts(NOW)).unwrap();

  // The fixtures propose links, so the quiet check has fewer subjects
  // to answer for — exactly as the sweep would.
  assert!(
    quiet.in_scope < loud.in_scope,
    "pending links must narrow the dry-run: {quiet:?} vs {loud:?}"
  );
  assert!(quiet.match_count < loud.match_count, "{quiet:?}");
}

// --- identity policy (SPEC.md section 6.4) ----------------------------

fn phones() -> SystemConfig { system("voip", "phones.json", 0) }

/// A person check that every implicit singleton answers `true` to.
///
/// Deliberately unscoped, because that is the case the policy exists
/// for: a rule written about accounts, with no `entity_types` on it,
/// which a fleet of handsets would otherwise each be handed.
fn no_idp() -> CheckDraft {
  draft(
    "no-idp",
    "not has_entity(\"idp\")",
    Severity::High,
    SubjectKind::Person,
  )
}

fn not_active() -> CheckDraft {
  draft(
    "phone-not-active",
    "status != \"active\"",
    Severity::Medium,
    SubjectKind::Entity,
  )
}

fn set_policy(db: &Db, types: &[&str]) {
  overlord_engine::sync_identity_policy(
    db,
    &types
      .iter()
      .map(|t| overlord_core::EntityType::new(*t))
      .collect::<Vec<_>>(),
    &actor(),
    ts(NOW),
  )
  .unwrap();
}

fn open_subjects(db: &Db, check: &str) -> Vec<String> {
  let mut v = db
    .read(|r| -> overlord_store::Result<_> {
      r.violations(&[ViolationState::Open, ViolationState::Acknowledged], 500)
    })
    .unwrap()
    .into_iter()
    .filter(|v| v.check_id.as_str() == check)
    .map(|v| v.subject.to_string())
    .collect::<Vec<_>>();
  v.sort();
  v
}

#[tokio::test]
async fn without_a_policy_every_handset_is_its_own_implicit_person() {
  // The behaviour the policy exists to change, asserted first so the
  // test below is a difference rather than a claim.
  let db = Db::open_memory().unwrap();
  install(&db, &no_idp());
  sweep(&db, &plan(vec![phones()], NOW)).await;

  assert_eq!(
    open_subjects(&db, "no-idp").len(),
    3,
    "an unscoped person check should reach all three handsets"
  );
}

#[tokio::test]
async fn a_non_person_type_is_not_an_implicit_person() {
  let db = Db::open_memory().unwrap();
  install(&db, &no_idp());
  install(&db, &not_active());
  set_policy(&db, &["phone"]);

  sweep(&db, &plan(vec![phones()], NOW)).await;

  assert!(
    open_subjects(&db, "no-idp").is_empty(),
    "handsets are not people, so a person check must not reach them"
  );
  // The other half of the point: they are still subjects, just not
  // person-shaped ones.
  assert_eq!(
    open_subjects(&db, "phone-not-active"),
    vec!["entity/voip/phone/SEP001A2B3C4D03".to_owned()],
    "an entity check must still fire on the handset that is not active"
  );
}

#[tokio::test]
async fn turning_the_policy_on_resolves_the_violations_it_orphans() {
  // Otherwise the board would keep three open person violations whose
  // subjects evaluation no longer builds, and nothing would ever clear
  // them.
  let db = Db::open_memory().unwrap();
  install(&db, &no_idp());
  sweep(&db, &plan(vec![phones()], NOW)).await;
  assert_eq!(open_subjects(&db, "no-idp").len(), 3);

  set_policy(&db, &["phone"]);
  sweep(&db, &plan(vec![phones()], "2026-02-02T00:00:00Z")).await;

  assert!(open_subjects(&db, "no-idp").is_empty());
  let reason: String = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(r.conn().query_row(
        "SELECT resolve_reason FROM violation WHERE check_id = 'no-idp'
          LIMIT 1",
        [],
        |row| row.get(0),
      )?)
    })
    .unwrap();
  assert_eq!(reason, "subject_absent");
}

#[tokio::test]
async fn a_policy_that_did_not_change_appends_nothing() {
  // The reconcile runs on every invocation. If a steady state appended
  // a command, the stream would fill with changes that changed nothing
  // and the policy's own history would be unreadable.
  let db = Db::open_memory().unwrap();
  assert!(
    overlord_engine::sync_identity_policy(
      &db,
      &[overlord_core::EntityType::new("phone")],
      &actor(),
      ts(NOW),
    )
    .unwrap()
  );

  for spelling in [vec!["phone"], vec!["phone"]] {
    assert!(
      !overlord_engine::sync_identity_policy(
        &db,
        &spelling
          .iter()
          .map(|t| overlord_core::EntityType::new(*t))
          .collect::<Vec<_>>(),
        &actor(),
        ts(NOW),
      )
      .unwrap(),
      "an unchanged policy must not append"
    );
  }

  let n: i64 = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(r.conn().query_row(
        "SELECT count(*) FROM command WHERE kind = 'identity.policy'",
        [],
        |row| row.get(0),
      )?)
    })
    .unwrap();
  assert_eq!(n, 1);
}

#[tokio::test]
async fn the_policy_replays_with_everything_else() {
  // SPEC.md section 13: the policy decides which subjects exist, so if
  // it did not replay, `replay(streams) == live` would be false for
  // every store that used one.
  let db = Db::open_memory().unwrap();
  install(&db, &no_idp());
  install(&db, &not_active());

  sweep(&db, &plan(vec![phones()], NOW)).await;
  set_policy(&db, &["phone"]);
  sweep(&db, &plan(vec![phones()], "2026-02-02T00:00:00Z")).await;

  let before = dump(&db);
  assert!(before.iter().any(|r| r.starts_with("identity_policy:")));
  overlord_engine::rebuild(&db).unwrap();
  assert_eq!(dump(&db), before, "replay(streams) must equal live");
}
