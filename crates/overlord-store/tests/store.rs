//! Store behaviour: the streams, the projections, and the invariants
//! only the store can enforce.

use overlord_core::{
  Actor, CheckDraft, CheckId, CommandKind, EntityKey, EntityRef, EntityStatus,
  EntityType, NewCommand, NormalizedRecord, PersonUid, Revision, Severity,
  SubjectKind, SweepId, SystemId, SystemKind, Timestamp, Value,
};
use overlord_store::{Db, NewFact, SweepStart, SweepStatus, error::StoreError};

const T0: &str = "2026-01-15T00:00:00Z";
const T1: &str = "2026-01-16T00:00:00Z";

fn ts(s: &str) -> Timestamp { s.parse().unwrap() }

fn db() -> Db { Db::open_memory().unwrap() }

fn sweep(db: &Db, at: &str) -> SweepId {
  db.write(|w| {
    w.open_sweep(&SweepStart {
      started_at:        ts(at),
      requested:         vec![SystemId::new("gws-prod")],
      pinned_checks:     vec![],
      pinned_norm:       vec![],
      absence_guard_pct: 10,
    })
  })
  .unwrap()
}

fn user_fact(key: &str, status: EntityStatus, mfa: bool) -> NewFact {
  let mut n = NormalizedRecord::new(
    "gws-prod",
    SystemKind::Workspace,
    "user",
    key,
    status,
  );
  n.insert("mfa_enrolled", Value::Bool(mfa));
  NewFact {
    system:       SystemId::new("gws-prod"),
    entity_type:  EntityType::new("user"),
    entity_key:   EntityKey::new(key),
    observed_at:  ts(T0),
    raw:          Some(serde_json::json!({"primaryEmail": key})),
    normalized:   Some(n),
    norm_version: "gworkspace/1".to_owned(),
  }
}

fn cmd(kind: CommandKind, at: &str) -> NewCommand {
  NewCommand::new(Actor::new("cli:test"), kind, ts(at))
}

fn draft(id: &str, condition: &str, severity: Severity) -> CheckDraft {
  CheckDraft {
    id: CheckId::new(id),
    name: id.to_owned(),
    description: None,
    rationale: None,
    remediation: None,
    references: vec![],
    severity,
    weight: None,
    applies_to: SubjectKind::Entity,
    systems: vec![],
    entity_types: vec![],
    condition: condition.to_owned(),
    suppress_if_pending_links: false,
  }
}

// --- the stream sequence ----------------------------------------------

#[test]
fn facts_and_commands_share_one_monotonic_sequence() {
  let db = db();
  let s = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s, &[user_fact("a@x.com", EntityStatus::Active, true)])
  })
  .unwrap();
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::PersonCreate {
        person_uid:   PersonUid::new("P1"),
        display_name: None,
      },
      T0,
    ))
  })
  .unwrap();
  db.write(|w| {
    w.append_facts(s, &[user_fact("b@x.com", EntityStatus::Active, true)])
  })
  .unwrap();

  // The interleaving is recorded, not reconstructed: a command that
  // landed between two batches of facts sits between them.
  let seqs: Vec<(i64, String)> = db
    .read(|r| -> overlord_store::Result<_> {
      let mut stmt = r.conn().prepare(
        "SELECT seq, 'fact' FROM fact
         UNION ALL SELECT seq, 'command' FROM command
         UNION ALL SELECT opened_seq, 'sweep' FROM sweep
         ORDER BY seq",
      )?;
      let rows = stmt.query_map([], |x| Ok((x.get(0)?, x.get(1)?)))?;
      Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap();

  let kinds: Vec<&str> = seqs.iter().map(|(_, k)| k.as_str()).collect();
  assert_eq!(kinds, ["sweep", "fact", "command", "fact"]);
  assert!(
    seqs.windows(2).all(|p| p[0].0 < p[1].0),
    "sequence must be strictly increasing: {seqs:?}"
  );
}

// --- facts and the entity projection ----------------------------------

#[test]
fn an_unchanged_entity_costs_a_reference_not_a_payload_copy() {
  let db = db();
  for at in [T0, T1] {
    let s = sweep(&db, at);
    db.write(|w| {
      w.append_facts(s, &[user_fact("a@x.com", EntityStatus::Active, true)])
    })
    .unwrap();
  }
  let (facts, payloads): (i64, i64) = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(r.conn().query_row(
        "SELECT (SELECT count(*) FROM fact),
                (SELECT count(*) FROM payload)",
        [],
        |x| Ok((x.get(0)?, x.get(1)?)),
      )?)
    })
    .unwrap();
  assert_eq!(facts, 2);
  assert_eq!(payloads, 2, "one raw and one overlay, shared by both facts");
}

#[test]
fn a_tombstone_makes_an_entity_absent() {
  let db = db();
  let s1 = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s1, &[user_fact("a@x.com", EntityStatus::Active, true)])
  })
  .unwrap();
  assert_eq!(db.read(|r| r.entity_states(None)).unwrap().len(), 1);

  let s2 = sweep(&db, T1);
  let gone = EntityRef::new("gws-prod", "user", "a@x.com");
  db.write(|w| w.append_facts(s2, &[NewFact::tombstone(&gone, ts(T1))]))
    .unwrap();

  // Absent entities are excluded from evaluation entirely.
  assert!(db.read(|r| r.entity_states(None)).unwrap().is_empty());
  assert_eq!(
    db.read(|r| r.present_entity_count(&SystemId::new("gws-prod")))
      .unwrap(),
    0
  );
  // The fact stream still has the history.
  let n: i64 = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(
        r.conn()
          .query_row("SELECT count(*) FROM fact", [], |x| x.get(0))?,
      )
    })
    .unwrap();
  assert_eq!(n, 2);
}

#[test]
fn the_latest_fact_wins_and_first_seen_does_not_move() {
  let db = db();
  let s1 = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s1, &[user_fact("a@x.com", EntityStatus::Active, false)])
  })
  .unwrap();
  let s2 = sweep(&db, T1);
  db.write(|w| {
    w.append_facts(s2, &[user_fact("a@x.com", EntityStatus::Suspended, true)])
  })
  .unwrap();

  let states = db.read(|r| r.entity_states(None)).unwrap();
  assert_eq!(states.len(), 1);
  assert_eq!(states[0].normalized.status, EntityStatus::Suspended);
  assert_eq!(states[0].normalized.get("mfa_enrolled"), Value::Bool(true));

  let (first, last): (i64, i64) = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(r.conn().query_row(
        "SELECT first_seen_sweep, last_seen_sweep FROM entity",
        [],
        |x| Ok((x.get(0)?, x.get(1)?)),
      )?)
    })
    .unwrap();
  assert_eq!((first, last), (s1.0, s2.0));
}

// --- commands ---------------------------------------------------------

#[test]
fn a_retried_command_is_a_no_op_returning_the_original_id() {
  let db = db();
  let c = cmd(
    CommandKind::PersonCreate {
      person_uid:   PersonUid::new("P1"),
      display_name: Some("Ada".to_owned()),
    },
    T0,
  )
  .with_idempotency_key("form-abc123");

  let first = db.write(|w| w.append_command(&c)).unwrap();
  let second = db.write(|w| w.append_command(&c)).unwrap();

  assert!(!first.duplicate);
  assert!(second.duplicate);
  assert_eq!(first.id, second.id);

  let n: i64 = db
    .read(|r| -> overlord_store::Result<_> {
      Ok(
        r.conn()
          .query_row("SELECT count(*) FROM command", [], |x| x.get(0))?,
      )
    })
    .unwrap();
  assert_eq!(n, 1, "the second submission appended nothing");
}

#[test]
fn check_upsert_appends_revisions_and_never_enables() {
  let db = db();
  for condition in ["not mfa_enrolled", "status == \"active\""] {
    db.write(|w| {
      w.append_command(&cmd(
        CommandKind::CheckUpsert {
          draft: draft("idp-mfa", condition, Severity::Critical),
        },
        T0,
      ))
    })
    .unwrap();
  }

  let checks = db.read(|r| r.checks()).unwrap();
  assert_eq!(checks.len(), 1);
  assert_eq!(checks[0].revision, Revision(2));
  assert!(!checks[0].enabled, "an upsert must never enable a check");
  assert_eq!(checks[0].draft.condition, "status == \"active\"");

  // Every revision is retained and readable.
  let first = db
    .read(|r| r.check_revision(&CheckId::new("idp-mfa"), Revision(1)))
    .unwrap();
  assert_eq!(first.condition, "not mfa_enrolled");
}

#[test]
fn enabling_without_a_dry_run_is_refused() {
  let db = db();
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::CheckUpsert {
        draft: draft("c", "not mfa_enrolled", Severity::High),
      },
      T0,
    ))
  })
  .unwrap();

  let err = db
    .write(|w| {
      w.append_command(&cmd(
        CommandKind::CheckEnable {
          check_id: CheckId::new("c"),
          revision: Revision(1),
        },
        T0,
      ))
    })
    .unwrap_err();
  assert!(
    matches!(&err, StoreError::Rejected(m) if m.contains("no dry-run")),
    "{err}"
  );

  // With a dry-run for that exact revision it goes through.
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::CheckDryrun {
        check_id:    CheckId::new("c"),
        revision:    Revision(1),
        match_count: 2,
        samples:     vec![],
      },
      T0,
    ))
  })
  .unwrap();
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::CheckEnable {
        check_id: CheckId::new("c"),
        revision: Revision(1),
      },
      T0,
    ))
  })
  .unwrap();
  assert_eq!(db.read(|r| r.enabled_checks()).unwrap().len(), 1);
}

#[test]
fn a_dry_run_does_not_carry_across_a_revision() {
  let db = db();
  let upsert = |c: &str| {
    db.write(|w| {
      w.append_command(&cmd(
        CommandKind::CheckUpsert {
          draft: draft("c", c, Severity::High),
        },
        T0,
      ))
    })
    .unwrap();
  };
  upsert("not mfa_enrolled");
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::CheckDryrun {
        check_id:    CheckId::new("c"),
        revision:    Revision(1),
        match_count: 0,
        samples:     vec![],
      },
      T0,
    ))
  })
  .unwrap();
  upsert("is_admin");

  // Revision 2 has no dry-run of its own, so enabling it is refused —
  // a rewritten rule cannot inherit the old one's evidence.
  let err = db
    .write(|w| {
      w.append_command(&cmd(
        CommandKind::CheckEnable {
          check_id: CheckId::new("c"),
          revision: Revision(2),
        },
        T0,
      ))
    })
    .unwrap_err();
  assert!(matches!(err, StoreError::Rejected(_)), "{err}");
}

#[test]
fn a_merge_keeps_the_retired_uid_resolvable_forever() {
  let db = db();
  let (a, b) = (PersonUid::new("A"), PersonUid::new("B"));
  let entity = EntityRef::new("gws-prod", "user", "ada@x.com");

  // Only an observed account can be linked, so the fact comes first.
  let s = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s, &[user_fact("ada@x.com", EntityStatus::Active, true)])
  })
  .unwrap();

  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::PersonLink {
        person_uid:      b.clone(),
        entity:          entity.clone(),
        from_suggestion: None,
      },
      T0,
    ))
  })
  .unwrap();
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::PersonMerge {
        surviving: a.clone(),
        retired:   b.clone(),
      },
      T1,
    ))
  })
  .unwrap();

  assert_eq!(db.read(|r| r.resolve_person(&b)).unwrap(), a);
  assert_eq!(db.read(|r| r.resolve_person(&a)).unwrap(), a);
  assert_eq!(db.read(|r| r.person_of(&entity)).unwrap(), Some(a));
}

// --- rebuild ----------------------------------------------------------

#[test]
fn rebuilding_reproduces_the_projections() {
  let db = db();
  let s1 = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s1, &[
      user_fact("a@x.com", EntityStatus::Active, false),
      user_fact("b@x.com", EntityStatus::Active, true),
    ])
  })
  .unwrap();
  db.write(|w| w.commit_sweep(s1, SweepStatus::Ok, ts(T0)))
    .unwrap();

  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::CheckUpsert {
        draft: draft("c", "not mfa_enrolled", Severity::Critical),
      },
      T0,
    ))
  })
  .unwrap();
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::PersonLink {
        person_uid:      PersonUid::new("P1"),
        entity:          EntityRef::new("gws-prod", "user", "a@x.com"),
        from_suggestion: None,
      },
      T1,
    ))
  })
  .unwrap();

  let before = dump_projections(&db);

  let report = db.rebuild_projections().unwrap();
  assert_eq!(report.commands, 2);
  assert_eq!(report.facts, 2);
  assert_eq!(report.sweeps, 1);

  // Row for row, column for column — not just the same counts. Command
  // ids are stable across a replay, so the projections that cite them
  // must come back byte-identical.
  assert_eq!(dump_projections(&db), before);
}

/// Every projection row, rendered as stable text and sorted, so two
/// dumps can be compared directly.
fn dump_projections(db: &Db) -> Vec<String> {
  const TABLES: [&str; 6] = [
    "entity",
    "person",
    "link",
    "check_head",
    "check_revision",
    "normalization_ruleset",
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
              ValueRef::Text(v) => {
                s.push_str(&String::from_utf8_lossy(v));
              }
              ValueRef::Blob(v) => {
                s.push_str(&format!("<{} bytes>", v.len()));
              }
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

#[test]
fn a_rebuild_keeps_the_overlay_that_was_live_at_the_time() {
  let db = db();
  let s = sweep(&db, T0);
  let mut fact = user_fact("a@x.com", EntityStatus::Active, true);
  // A field the current ruleset would no longer produce.
  fact
    .normalized
    .as_mut()
    .unwrap()
    .insert("legacy_field", Value::from("kept"));
  db.write(|w| w.append_facts(s, &[fact])).unwrap();
  db.write(|w| w.commit_sweep(s, SweepStatus::Ok, ts(T0)))
    .unwrap();

  db.rebuild_projections().unwrap();

  let states = db.read(|r| r.entity_states(None)).unwrap();
  assert_eq!(
    states[0].normalized.get("legacy_field"),
    Value::from("kept")
  );
  assert_eq!(states[0].normalized.get("mfa_enrolled"), Value::Bool(true));
}
