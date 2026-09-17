//! Identity resolution end to end (SPEC.md section 12).
//!
//! The fixture pair `identity-ws.json` / `identity-idp.json` gives each
//! link signal exactly one person to find and one case that must find
//! nobody, so these tests can assert on the whole suggestion set rather
//! than on a sample of it.

use overlord_connect::Registry;
use overlord_connector_fixture::FixtureConnector;
use overlord_core::{
  Actor, CheckDraft, CheckId, CommandKind, EntityRef, NewCommand, PersonUid,
  Severity, SubjectKind, SubjectRef, SystemId, SystemKind, Timestamp,
  ViolationState,
};
use overlord_engine::{
  SweepOutcome, SweepPlan, SystemConfig, checks, identity, run_sweep,
};
use overlord_store::{Db, ViolationFilter};

const NOW: &str = "2026-02-01T00:00:00Z";
const LATER: &str = "2026-02-02T00:00:00Z";

fn ts(s: &str) -> Timestamp { s.parse().unwrap() }

fn actor() -> Actor { Actor::new("cli:test") }

fn fixture(name: &str) -> String {
  format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn system(id: &str, scenario: &str) -> SystemConfig {
  SystemConfig {
    id:        SystemId::new(id),
    connector: "fixture".to_owned(),
    config:    serde_json::json!({ "path": fixture(scenario), "stage": 0 }),
  }
}

/// The identity pair, swept together. Both systems every time: a
/// suggestion is a statement about two systems at once.
fn both() -> Vec<SystemConfig> {
  vec![
    system("ws", "identity-ws.json"),
    system("idp", "identity-idp.json"),
  ]
}

async fn sweep_at(db: &Db, at: &str) -> SweepOutcome {
  let plan = SweepPlan::new(both()).at(ts(at));
  run_sweep(db, &Registry::new().with(FixtureConnector::boxed()), &plan)
    .await
    .unwrap()
}

fn draft(
  id: &str,
  condition: &str,
  applies_to: SubjectKind,
  pending: bool,
) -> CheckDraft {
  CheckDraft {
    id: CheckId::new(id),
    name: id.to_owned(),
    description: None,
    rationale: None,
    remediation: None,
    references: vec![],
    severity: Severity::High,
    weight: None,
    applies_to,
    systems: vec![],
    entity_types: vec![],
    condition: condition.to_owned(),
    suppress_if_pending_links: pending,
  }
}

fn install(db: &Db, d: &CheckDraft) {
  let at = ts(NOW);
  let rev = checks::upsert(db, &actor(), d, at, None).unwrap();
  checks::dry_run(db, &actor(), &d.id, rev, at).unwrap();
  checks::enable(db, &actor(), &d.id, at).unwrap();
}

fn ws(key: &str) -> EntityRef { EntityRef::new("ws", "user", key) }

fn idp(key: &str) -> EntityRef { EntityRef::new("idp", "user", key) }

/// Every stored suggestion as `(entity, person, signal)`, sorted.
///
/// The stored set, not the queue's: a match between two unlinked
/// accounts is recorded from both sides, because each account's own
/// detail page has to show it. The queue folds the mirrors; these tests
/// are about what was computed.
fn suggestions(db: &Db) -> Vec<(String, String, String)> {
  let mut v = db
    .read(|r| -> overlord_store::Result<_> {
      let mut stmt = r.conn().prepare(
        "SELECT system, entity_type, entity_key, person_uid, signal
           FROM suggestion",
      )?;
      let rows = stmt.query_map([], |row| {
        Ok((
          format!(
            "{}/{}/{}",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?
          ),
          row.get::<_, String>(3)?,
          row.get::<_, String>(4)?,
        ))
      })?;
      Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap();
  v.sort();
  v
}

// --- computing suggestions (task 19) ----------------------------------

#[tokio::test]
async fn each_signal_finds_its_person_and_nothing_else() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  // Nothing is linked, so every candidate person is the implicit
  // singleton of the account on the other side. The set is exhaustive:
  // three people found, by three different signals, in both directions.
  assert_eq!(suggestions(&db), vec![
    (
      "idp/user/ada@example.com".to_owned(),
      "implicit:ws/user/ada@example.com".to_owned(),
      "exact-email".to_owned(),
    ),
    (
      "idp/user/alan@corp.example.com".to_owned(),
      "implicit:ws/user/alan@example.com".to_owned(),
      "username".to_owned(),
    ),
    (
      "idp/user/ghopper@corp.example.com".to_owned(),
      "implicit:ws/user/grace@example.com".to_owned(),
      "directory-id".to_owned(),
    ),
    (
      "ws/user/ada@example.com".to_owned(),
      "implicit:idp/user/ada@example.com".to_owned(),
      "exact-email".to_owned(),
    ),
    (
      "ws/user/alan@example.com".to_owned(),
      "implicit:idp/user/alan@corp.example.com".to_owned(),
      "username".to_owned(),
    ),
    (
      "ws/user/grace@example.com".to_owned(),
      "implicit:idp/user/ghopper@corp.example.com".to_owned(),
      "directory-id".to_owned(),
    ),
  ]);
}

#[tokio::test]
async fn the_queue_shows_one_row_per_pair_not_one_per_side() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  // Six stored suggestions, three decisions. Confirming either side of
  // a pair resolves both, so a queue with both rows would ask the
  // operator the same question twice.
  assert_eq!(suggestions(&db).len(), 6);
  let queue = db.read(|r| r.pending_suggestions(500)).unwrap();
  assert_eq!(queue.len(), 3, "{queue:?}");
}

#[tokio::test]
async fn two_candidates_in_one_system_identify_nobody() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  // Both of Sam's directory accounts carry username "sam". SPEC.md
  // section 6.4 refuses to guess between candidates, and a suggestion is
  // not the place to start — so neither side proposes anything, in
  // either direction.
  let sams: Vec<_> = suggestions(&db)
    .into_iter()
    .filter(|(e, p, _)| e.contains("sam") || p.contains("sam"))
    .collect();
  assert!(sams.is_empty(), "{sams:?}");
}

#[tokio::test]
async fn an_orphan_account_is_proposed_to_nobody() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;
  assert!(
    !suggestions(&db)
      .iter()
      .any(|(e, ..)| e.contains("svc-deploy")),
    "a service account with no counterpart has nobody to be"
  );
}

#[tokio::test]
async fn a_suggestion_is_never_applied() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  assert!(!suggestions(&db).is_empty(), "there should be proposals");
  assert!(
    db.read(|r| r.links()).unwrap().is_empty(),
    "SPEC.md section 12: suggestions are read-only and emit no command"
  );
  let commands: i64 = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(r.conn().query_row(
        "SELECT count(*) FROM command WHERE kind LIKE 'person.%'",
        [],
        |row| row.get(0),
      )?)
    })
    .unwrap();
  assert_eq!(commands, 0);
}

#[tokio::test]
async fn confirming_a_stale_proposal_joins_the_person_rather_than_a_rival() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  // The operator confirms Ada from the workspace side, which creates the
  // person. The proposal pointing the other way still names the *old*
  // implicit uid — suggestions are a snapshot of the last sweep.
  let uid = identity::confirm(
    &db,
    &actor(),
    &ws("ada@example.com"),
    &PersonUid::implicit(&idp("ada@example.com")),
    Some("exact-email".to_owned()),
    ts(NOW),
    None,
  )
  .unwrap();

  // Confirming it must land on the same person, not mint a second one
  // and take an account off the first.
  let again = identity::confirm(
    &db,
    &actor(),
    &idp("ada@example.com"),
    &PersonUid::implicit(&ws("ada@example.com")),
    Some("exact-email".to_owned()),
    ts(LATER),
    None,
  )
  .unwrap();

  assert_eq!(again, uid);
  assert_eq!(db.read(|r| r.links()).unwrap().len(), 2);
  let people: Vec<_> = db
    .read(|r| -> overlord_store::Result<_> {
      let mut stmt = r.conn().prepare("SELECT person_uid FROM person")?;
      let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
      Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap();
  assert_eq!(people, vec![uid.to_string()], "one person, not two");
}

#[tokio::test]
async fn confirming_a_link_retires_the_suggestion() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    Some("Ada Lovelace".to_owned()),
    &ws("ada@example.com"),
    Some("exact-email".to_owned()),
    ts(NOW),
    Some("k1".to_owned()),
  )
  .unwrap();
  identity::link(
    &db,
    &actor(),
    &uid,
    &idp("ada@example.com"),
    Some("exact-email".to_owned()),
    ts(NOW),
    Some("k2".to_owned()),
  )
  .unwrap();

  sweep_at(&db, LATER).await;
  assert!(
    !suggestions(&db).iter().any(|(e, ..)| e.contains("ada")),
    "a linked account has had the operator's attention already"
  );
}

// --- suppress_if_pending_links (task 23) ------------------------------

#[tokio::test]
async fn a_check_can_stay_quiet_about_an_account_with_unreviewed_links() {
  let db = Db::open_memory().unwrap();
  install(
    &db,
    &draft(
      "no-idp",
      "has_entity(\"workspace\") and not has_entity(\"idp\")",
      SubjectKind::Person,
      true,
    ),
  );
  sweep_at(&db, NOW).await;

  let open: Vec<String> = db
    .read(|r| r.violations(&[ViolationState::Open], 500))
    .unwrap()
    .into_iter()
    .map(|v| v.subject.to_string())
    .collect();

  // Every workspace account looks like an orphan before linking is done.
  // Ada, Grace and Alan each have an unreviewed proposal, so the check
  // says nothing about them; Sam and the robot have none, so they open.
  assert!(
    open.iter().any(|s| s.contains("svc-deploy@example.com")),
    "{open:?}"
  );
  assert!(
    open.iter().any(|s| s.contains("sam@example.com")),
    "{open:?}"
  );
  for quiet in ["ada@example.com", "grace@example.com", "alan@example.com"] {
    assert!(
      !open.iter().any(|s| s.contains(quiet)),
      "{quiet} has an unreviewed proposal and should be quiet: {open:?}"
    );
  }
}

// --- promotion carries history (task 22) ------------------------------

/// A person-scoped check that stays true through a link, so the episode
/// has something to be carried *by*.
fn admin_somewhere() -> CheckDraft {
  draft(
    "admin-somewhere",
    "has_entity(\"workspace\" where is_admin)",
    SubjectKind::Person,
    false,
  )
}

#[tokio::test]
async fn promotion_carries_the_episode_and_the_acknowledgement() {
  let db = Db::open_memory().unwrap();
  install(&db, &admin_somewhere());
  sweep_at(&db, NOW).await;

  // Grace is an admin in the workspace and, unlinked, is her own
  // implicit person. The operator sees it and acknowledges it.
  let implicit =
    SubjectRef::Person(PersonUid::implicit(&ws("grace@example.com")));
  db.write(|w| {
    w.append_command(&NewCommand::new(
      actor(),
      CommandKind::ViolationAcknowledge {
        check_id: CheckId::new("admin-somewhere"),
        subject:  implicit.clone(),
      },
      ts(NOW),
    ))
  })
  .unwrap();

  let before = one(&db, &implicit);
  assert_eq!(before.state, ViolationState::Acknowledged);

  // Now identity work happens: both of Grace's accounts become one
  // person.
  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    Some("Grace Hopper".to_owned()),
    &ws("grace@example.com"),
    Some("directory-id".to_owned()),
    ts(NOW),
    None,
  )
  .unwrap();
  identity::link(
    &db,
    &actor(),
    &uid,
    &idp("ghopper@corp.example.com"),
    Some("directory-id".to_owned()),
    ts(NOW),
    None,
  )
  .unwrap();

  let out = sweep_at(&db, LATER).await;
  assert_eq!(out.evaluation.carried, 1, "the episode should have carried");

  // Same episode, same opening, same acknowledgement: the operator's
  // "I've seen it" survives the link that made it one person's problem.
  let after = one(&db, &implicit);
  assert_eq!(after.episode, before.episode);
  assert_eq!(after.opened_at, before.opened_at);
  assert_eq!(after.state, ViolationState::Acknowledged);

  // And it is reachable as the person's, not orphaned under a uid the
  // Users screen no longer lists.
  let theirs = db
    .read(|r| -> overlord_store::Result<_> {
      let subjects = r.person_subject_refs(&uid)?;
      r.violations_where(&ViolationFilter {
        subjects,
        ..ViolationFilter::default()
      })
    })
    .unwrap();
  assert_eq!(theirs.len(), 1);
  assert_eq!(theirs[0].episode, before.episode);
  assert_eq!(db.read(|r| r.resolve_person(&implicit_uid())).unwrap(), uid);
}

fn implicit_uid() -> PersonUid { PersonUid::implicit(&ws("grace@example.com")) }

#[tokio::test]
async fn an_absorbed_duplicate_is_closed_rather_than_counted_twice() {
  let db = Db::open_memory().unwrap();
  // True for every person who holds any account at all, so both of
  // Grace's implicit persons carry an episode before she is one person.
  install(
    &db,
    &draft(
      "has-an-account",
      "has_entity(\"workspace\") or has_entity(\"idp\")",
      SubjectKind::Person,
      false,
    ),
  );
  sweep_at(&db, NOW).await;

  let ws_side =
    SubjectRef::Person(PersonUid::implicit(&ws("grace@example.com")));
  let idp_side =
    SubjectRef::Person(PersonUid::implicit(&idp("ghopper@corp.example.com")));
  assert_eq!(one(&db, &ws_side).state, ViolationState::Open);
  assert_eq!(one(&db, &idp_side).state, ViolationState::Open);
  let before = db.write(|w| w.total_score()).unwrap();

  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    None,
    &ws("grace@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();
  identity::link(
    &db,
    &actor(),
    &uid,
    &idp("ghopper@corp.example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();

  sweep_at(&db, LATER).await;

  // One episode continues; the other described the same person all
  // along and is closed as merged. Leaving both standing would have
  // scored Grace twice for one finding.
  let states = [one(&db, &ws_side).state, one(&db, &idp_side).state];
  assert!(states.contains(&ViolationState::Open), "{states:?}");
  assert!(states.contains(&ViolationState::Resolved), "{states:?}");
  assert_eq!(
    db.read(|r| r.score_of(&uid)).unwrap().1,
    1,
    "one finding, counted once"
  );
  assert!(db.write(|w| w.total_score()).unwrap() < before);
}

/// The single episode recorded against a subject, whatever its state.
fn one(db: &Db, subject: &SubjectRef) -> overlord_store::ViolationRow {
  let rows = db
    .read(|r| {
      r.violations_where(&ViolationFilter {
        states: vec![
          ViolationState::Open,
          ViolationState::Acknowledged,
          ViolationState::Suppressed,
          ViolationState::FalsePositive,
          ViolationState::Resolved,
        ],
        subjects: vec![subject.clone()],
        ..ViolationFilter::default()
      })
    })
    .unwrap();
  assert_eq!(
    rows.len(),
    1,
    "expected one episode for {subject}: {rows:?}"
  );
  rows.into_iter().next().unwrap()
}

// --- the scoring invariant (M3's acceptance bar) ----------------------

#[tokio::test]
async fn linking_moves_score_between_people_but_never_changes_the_total() {
  let db = Db::open_memory().unwrap();
  install(&db, &admin_somewhere());
  install(
    &db,
    &draft(
      "mfa-missing",
      "status == \"active\" and not mfa_enrolled",
      SubjectKind::Entity,
      false,
    ),
  );
  sweep_at(&db, NOW).await;

  let total = |db: &Db| -> i64 {
    db.read(|r| r.top_subjects(500))
      .unwrap()
      .iter()
      .map(|s| s.score)
      .sum()
  };
  let before = total(&db);
  assert!(before > 0, "the fixture should carry some risk");
  assert_eq!(
    before,
    db.write(|w| w.total_score()).unwrap(),
    "every violation must be attributed to exactly one person"
  );

  // Link everyone the suggestions propose, which is the whole point of
  // the identity screen: several people, several signals, one pass.
  for (workspace, directory) in [
    ("ada@example.com", "ada@example.com"),
    ("grace@example.com", "ghopper@corp.example.com"),
    ("alan@example.com", "alan@corp.example.com"),
  ] {
    let uid = identity::link_to_new_person(
      &db,
      &actor(),
      None,
      &ws(workspace),
      None,
      ts(NOW),
      None,
    )
    .unwrap();
    identity::link(&db, &actor(), &uid, &idp(directory), None, ts(NOW), None)
      .unwrap();
  }

  // No sweep in between: linking recomputes the scores itself, so the
  // Users screen is honest the moment the operator acts.
  assert_eq!(
    total(&db),
    before,
    "SPEC.md section 8: linking merges scores rather than revealing them, so \
     identity work is never penalized"
  );
}

// --- the operator verbs (tasks 20 and 21) -----------------------------

#[tokio::test]
async fn unlinking_gives_the_account_back_its_implicit_person() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    None,
    &ws("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();
  let implicit = PersonUid::implicit(&ws("ada@example.com"));
  assert_eq!(db.read(|r| r.resolve_person(&implicit)).unwrap(), uid);

  identity::unlink(&db, &actor(), &uid, &ws("ada@example.com"), ts(NOW), None)
    .unwrap();
  assert_eq!(
    db.read(|r| r.resolve_person(&implicit)).unwrap(),
    implicit,
    "an unlinked account is its own person again"
  );
}

#[tokio::test]
async fn unlinking_an_account_someone_else_holds_is_refused() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;
  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    None,
    &ws("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();

  let err = identity::unlink(
    &db,
    &actor(),
    &PersonUid::new("someone-else"),
    &ws("ada@example.com"),
    ts(NOW),
    None,
  )
  .unwrap_err();
  assert!(err.to_string().contains(uid.as_str()), "{err}");
}

#[tokio::test]
async fn an_account_can_only_be_primary_for_the_person_holding_it() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;
  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    None,
    &ws("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();

  identity::set_primary(
    &db,
    &actor(),
    &uid,
    SystemKind::Workspace,
    &ws("ada@example.com"),
    ts(NOW),
    None,
  )
  .unwrap();

  let err = identity::set_primary(
    &db,
    &actor(),
    &uid,
    SystemKind::Idp,
    &idp("ada@example.com"),
    ts(NOW),
    None,
  )
  .unwrap_err();
  assert!(err.to_string().contains("not linked"), "{err}");
}

#[tokio::test]
async fn linking_an_account_overlord_has_never_seen_is_refused() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;
  let err = identity::link_to_new_person(
    &db,
    &actor(),
    None,
    &ws("typo@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap_err();
  assert!(err.to_string().contains("typo@example.com"), "{err}");
}

#[tokio::test]
async fn a_merge_survives_as_one_person_and_keeps_both_designations() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  let keep = identity::link_to_new_person(
    &db,
    &actor(),
    Some("Ada".to_owned()),
    &ws("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();
  let retire = identity::link_to_new_person(
    &db,
    &actor(),
    Some("A. Lovelace".to_owned()),
    &idp("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();
  identity::set_primary(
    &db,
    &actor(),
    &retire,
    SystemKind::Idp,
    &idp("ada@example.com"),
    ts(NOW),
    None,
  )
  .unwrap();

  identity::merge(&db, &actor(), &keep, &retire, ts(NOW), None).unwrap();

  let detail = db.read(|r| r.person_detail(&keep)).unwrap().unwrap();
  assert_eq!(detail.entities.len(), 2);
  assert_eq!(detail.primaries.len(), 1, "the designation followed");
  assert_eq!(
    db.read(|r| r.resolve_person(&retire)).unwrap(),
    keep,
    "a retired uid resolves forever (SPEC.md section 12)"
  );
}

#[tokio::test]
async fn a_split_moves_the_named_accounts_and_leaves_the_history() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;

  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    Some("Ada".to_owned()),
    &ws("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();
  identity::link(
    &db,
    &actor(),
    &uid,
    &idp("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();

  let new_uid = identity::split(
    &db,
    &actor(),
    &uid,
    Some("Ada (directory)".to_owned()),
    &[idp("ada@example.com")],
    ts(LATER),
    None,
  )
  .unwrap();

  let original = db.read(|r| r.person_detail(&uid)).unwrap().unwrap();
  let departed = db.read(|r| r.person_detail(&new_uid)).unwrap().unwrap();
  assert_eq!(original.entities, vec![ws("ada@example.com")]);
  assert_eq!(departed.entities, vec![idp("ada@example.com")]);
  assert_eq!(
    original.person_uid, uid,
    "the original keeps its uid, and so its history"
  );

  // The provenance is in the stream: the command that minted the new uid
  // names the person it came from.
  let split_cmd: String = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(r.conn().query_row(
        "SELECT args FROM command WHERE kind = 'person.split'",
        [],
        |row| row.get(0),
      )?)
    })
    .unwrap();
  assert!(split_cmd.contains(uid.as_str()), "{split_cmd}");
  assert!(split_cmd.contains(new_uid.as_str()), "{split_cmd}");
}

#[tokio::test]
async fn a_split_cannot_take_an_account_the_person_does_not_hold() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;
  let uid = identity::link_to_new_person(
    &db,
    &actor(),
    None,
    &ws("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();

  let err = identity::split(
    &db,
    &actor(),
    &uid,
    None,
    &[idp("ada@example.com")],
    ts(LATER),
    None,
  )
  .unwrap_err();
  assert!(err.to_string().contains("not linked"), "{err}");
}

#[tokio::test]
async fn an_unlinked_account_is_not_a_person_to_link_to() {
  let db = Db::open_memory().unwrap();
  sweep_at(&db, NOW).await;
  let err = identity::link(
    &db,
    &actor(),
    &PersonUid::implicit(&idp("ada@example.com")),
    &ws("ada@example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap_err();
  assert!(err.to_string().contains("create a person"), "{err}");
}

// --- replay (SPEC.md section 13) --------------------------------------

#[tokio::test]
async fn identity_work_replays_exactly() {
  let db = Db::open_memory().unwrap();
  install(&db, &admin_somewhere());
  sweep_at(&db, NOW).await;

  let keep = identity::link_to_new_person(
    &db,
    &actor(),
    Some("Grace".to_owned()),
    &ws("grace@example.com"),
    Some("directory-id".to_owned()),
    ts(NOW),
    None,
  )
  .unwrap();
  let other = identity::link_to_new_person(
    &db,
    &actor(),
    None,
    &idp("ghopper@corp.example.com"),
    None,
    ts(NOW),
    None,
  )
  .unwrap();
  identity::merge(&db, &actor(), &keep, &other, ts(NOW), None).unwrap();
  identity::set_primary(
    &db,
    &actor(),
    &keep,
    SystemKind::Workspace,
    &ws("grace@example.com"),
    ts(NOW),
    None,
  )
  .unwrap();
  sweep_at(&db, LATER).await;

  let before = dump(&db);
  overlord_engine::rebuild(&db).unwrap();
  assert_eq!(dump(&db), before, "replay(streams) must equal live");
}

/// Every identity projection, as stable sorted text.
fn dump(db: &Db) -> Vec<String> {
  const TABLES: [&str; 6] = [
    "person",
    "person_alias",
    "link",
    "link_primary",
    "suggestion",
    "violation",
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
