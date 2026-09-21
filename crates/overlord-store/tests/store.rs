//! Store behaviour: the streams, the projections, and the invariants
//! only the store can enforce.

use overlord_core::{
  Actor, CheckDraft, CheckId, CommandKind, Completeness, EntityKey, EntityRef,
  EntityStatus, EntityType, NewCommand, NormalizedRecord, PersonUid, Revision,
  Severity, SubjectKind, SweepId, SystemId, SystemKind, Timestamp, Value,
};
use overlord_store::{
  Db, NewFact, SubjectFilter, SweepStart, SweepStatus, SystemOutcome,
  SystemStatus, error::StoreError,
};

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

// --- who the Users screen lists ---------------------------------------

#[test]
fn every_collected_account_is_listed_even_with_nothing_against_it() {
  let db = db();
  let s = sweep(&db, T0);

  let mut ada = user_fact("ada@x.com", EntityStatus::Active, true);
  ada.normalized.as_mut().unwrap().display_name = Some("Ada Lovelace".into());
  let grace = user_fact("grace@x.com", EntityStatus::Active, false);
  db.write(|w| w.append_facts(s, &[ada, grace])).unwrap();
  db.write(|w| w.commit_sweep(s, SweepStatus::Ok, ts(T0)))
    .unwrap();

  // Grace is linked to a confirmed person; Ada is not.
  let person = PersonUid::new("P");
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::PersonLink {
        person_uid:      person.clone(),
        entity:          EntityRef::new("gws-prod", "user", "grace@x.com"),
        from_suggestion: None,
      },
      T0,
    ))
  })
  .unwrap();

  // Nothing is open against either of them, so the risk ranking is
  // empty — and the roster still has to hold both, exactly once each.
  assert!(db.read(|r| r.top_subjects(50)).unwrap().is_empty());

  let rows = db
    .read(|r| r.all_subjects(SubjectFilter::Everyone, 50))
    .unwrap();
  let uids: Vec<&str> = rows.iter().map(|r| r.person_uid.as_str()).collect();
  assert_eq!(uids, vec!["implicit:gws-prod/user/ada@x.com", "P"]);

  let ada = &rows[0];
  assert!(ada.implicit);
  assert_eq!(ada.score, 0);
  assert_eq!(ada.count, 0);
  assert_eq!(ada.worst_severity, None);
  // The account's own name, which `person` has no row to supply.
  assert_eq!(ada.display_name.as_deref(), Some("Ada Lovelace"));
  assert!(!rows[1].implicit);
}

#[test]
fn a_departed_account_leaves_the_roster() {
  let db = db();
  let s = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s, &[user_fact("alan@x.com", EntityStatus::Active, true)])
  })
  .unwrap();
  db.write(|w| w.commit_sweep(s, SweepStatus::Ok, ts(T0)))
    .unwrap();
  assert_eq!(
    db.read(|r| r.all_subjects(SubjectFilter::Everyone, 50))
      .unwrap()
      .len(),
    1
  );

  // A tombstone makes the entity absent, and an absent account is not
  // somebody evaluation still judges (SPEC.md section 6.4).
  let s = sweep(&db, T1);
  db.write(|w| {
    w.append_facts(s, &[NewFact {
      system:       SystemId::new("gws-prod"),
      entity_type:  EntityType::new("user"),
      entity_key:   EntityKey::new("alan@x.com"),
      observed_at:  ts(T1),
      raw:          None,
      normalized:   None,
      norm_version: "gworkspace/1".to_owned(),
    }])
  })
  .unwrap();
  db.write(|w| w.commit_sweep(s, SweepStatus::Ok, ts(T1)))
    .unwrap();

  assert!(
    db.read(|r| r.all_subjects(SubjectFilter::Everyone, 50))
      .unwrap()
      .is_empty()
  );
}

#[test]
fn a_filtered_roster_is_cut_by_the_limit_after_the_filter_not_before() {
  // The two kinds are ranked together, so a caller that read the worst
  // N subjects and kept the confirmed ones among them would answer
  // "the confirmed persons inside the worst N" — and with enough
  // unlinked accounts above them, that is nobody at all.
  let db = db();
  let s = sweep(&db, T0);

  let mut facts = Vec::new();
  for i in 0..8 {
    // `aaa-…` so these sort ahead of the persons below, as a console
    // full of unlinked accounts does when nothing scores.
    facts.push(user_fact(
      &format!("aaa-{i}@x.com"),
      EntityStatus::Active,
      true,
    ));
  }
  for i in 0..3 {
    facts.push(user_fact(
      &format!("zzz-{i}@x.com"),
      EntityStatus::Active,
      true,
    ));
  }
  db.write(|w| w.append_facts(s, &facts)).unwrap();
  db.write(|w| w.commit_sweep(s, SweepStatus::Ok, ts(T0)))
    .unwrap();

  for i in 0..3 {
    db.write(|w| {
      w.append_command(&cmd(
        CommandKind::PersonLink {
          person_uid:      PersonUid::new(format!("P-{i}")),
          entity:          EntityRef::new(
            "gws-prod",
            "user",
            format!("zzz-{i}@x.com"),
          ),
          from_suggestion: None,
        },
        T0,
      ))
    })
    .unwrap();
  }

  // Eleven subjects: eight unlinked accounts, then three persons.
  let everyone = db
    .read(|r| r.all_subjects(SubjectFilter::Everyone, 50))
    .unwrap();
  assert_eq!(everyone.len(), 11);
  assert!(everyone[..8].iter().all(|r| r.implicit), "{everyone:?}");

  // Filtering the other way round is the bug this holds closed: the
  // worst five subjects are all unlinked, so keeping the confirmed ones
  // among them finds nobody.
  assert_eq!(
    db.read(|r| r.all_subjects(SubjectFilter::Everyone, 5))
      .unwrap()
      .iter()
      .filter(|r| !r.implicit)
      .count(),
    0
  );

  // A limit smaller than the unlinked half must not cost the roster a
  // single confirmed person.
  let confirmed = db
    .read(|r| r.all_subjects(SubjectFilter::Confirmed, 5))
    .unwrap();
  assert_eq!(confirmed.len(), 3, "{confirmed:?}");
  assert!(confirmed.iter().all(|r| !r.implicit), "{confirmed:?}");

  let unlinked = db
    .read(|r| r.all_subjects(SubjectFilter::Unlinked, 50))
    .unwrap();
  assert_eq!(unlinked.len(), 8);
  assert!(unlinked.iter().all(|r| r.implicit), "{unlinked:?}");

  // The limit still bounds what comes back, having been applied to the
  // roster that was asked for.
  assert_eq!(
    db.read(|r| r.all_subjects(SubjectFilter::Unlinked, 5))
      .unwrap()
      .len(),
    5
  );
}

// --- which connector read a system -------------------------------------

fn outcome(system: &str, connector: &str) -> SystemOutcome {
  SystemOutcome {
    system:         SystemId::new(system),
    system_kind:    SystemKind::Sso,
    connector:      connector.to_owned(),
    status:         SystemStatus::Ok,
    completeness:   Completeness::Complete,
    observed_count: 1,
    tombstoned:     0,
    previous_count: None,
    guard_tripped:  false,
    duration_ms:    1,
    error:          None,
  }
}

#[test]
fn the_connector_a_system_was_last_read_through_is_recorded() {
  // A check can be scoped to `connector:<name>`, and evaluation cannot
  // see the configuration file — it runs inside the sweep and replays
  // from the streams — so the connector has to be recorded beside what
  // it reported.
  let db = db();
  let s = sweep(&db, T0);
  db.write(|w| w.record_system(s, &outcome("access-hq", "unifi-access")))
    .unwrap();
  db.write(|w| w.record_system(s, &outcome("okta-prod", "okta")))
    .unwrap();

  let map = db.read(|r| r.system_connectors()).unwrap();
  assert_eq!(map[&SystemId::new("access-hq")], "unifi-access");
  assert_eq!(map[&SystemId::new("okta-prod")], "okta");
  assert!(!map.contains_key(&SystemId::new("never-swept")));
}

#[test]
fn a_system_moved_to_another_connector_is_scoped_by_the_current_one() {
  // The latest sweep wins. Otherwise a system read through a connector
  // once, years ago, would stay in that connector's scope forever.
  let db = db();
  let first = sweep(&db, T0);
  db.write(|w| w.record_system(first, &outcome("access-hq", "unifi-access")))
    .unwrap();
  let second = sweep(&db, T1);
  db.write(|w| w.record_system(second, &outcome("access-hq", "unifi-v2")))
    .unwrap();

  let map = db.read(|r| r.system_connectors()).unwrap();
  assert_eq!(map[&SystemId::new("access-hq")], "unifi-v2");
  assert_eq!(map.len(), 1, "one row per system, not one per sweep");
}

// --- identity policy (SPEC.md section 6.4) ----------------------------

fn phone_fact(key: &str) -> NewFact {
  let n = NormalizedRecord::new(
    "voip",
    SystemKind::Mdm,
    "phone",
    key,
    EntityStatus::Active,
  );
  NewFact {
    system:       SystemId::new("voip"),
    entity_type:  EntityType::new("phone"),
    entity_key:   EntityKey::new(key),
    observed_at:  ts(T0),
    raw:          Some(serde_json::json!({ "key": key })),
    normalized:   Some(n),
    norm_version: "fixture/1".to_owned(),
  }
}

#[test]
fn a_non_person_type_is_kept_off_the_users_roster() {
  let db = db();
  let s = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s, &[
      user_fact("ada@x.com", EntityStatus::Active, true),
      phone_fact("SEP0001"),
      phone_fact("SEP0002"),
    ])
  })
  .unwrap();
  db.write(|w| w.commit_sweep(s, SweepStatus::Ok, ts(T0)))
    .unwrap();

  // Without a policy every unlinked entity is an implicit person, which
  // is the behaviour orphan-account checks depend on.
  assert_eq!(
    db.read(|r| r.all_subjects(SubjectFilter::Everyone, 50))
      .unwrap()
      .len(),
    3
  );

  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::identity_policy([EntityType::new("phone")]),
      T1,
    ))
  })
  .unwrap();

  let rows = db
    .read(|r| r.all_subjects(SubjectFilter::Everyone, 50))
    .unwrap();
  let uids: Vec<&str> = rows.iter().map(|r| r.person_uid.as_str()).collect();
  assert_eq!(uids, vec!["implicit:gws-prod/user/ada@x.com"]);
}

#[test]
fn the_policy_is_read_back_as_the_set_it_was_written_with() {
  let db = db();
  assert!(
    db.read(|r| r.non_person_entity_types()).unwrap().is_empty(),
    "a store with no policy command admits every type, which is what every \
     store did before the policy existed"
  );

  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::identity_policy([
        EntityType::new("phone"),
        EntityType::new("device"),
      ]),
      T0,
    ))
  })
  .unwrap();
  assert_eq!(
    db.read(|r| r.non_person_entity_types()).unwrap(),
    [EntityType::new("device"), EntityType::new("phone")]
      .into_iter()
      .collect()
  );

  // One row, replaced: the policy is current state, and its history is
  // the command stream rather than an accumulation here.
  db.write(|w| {
    w.append_command(&cmd(
      CommandKind::identity_policy([EntityType::new("phone")]),
      T1,
    ))
  })
  .unwrap();
  assert_eq!(
    db.read(|r| r.non_person_entity_types()).unwrap(),
    [EntityType::new("phone")].into_iter().collect()
  );
}

// --- browsing and searching entities ----------------------------------

use overlord_store::{EntityFilter, fts_query};

fn device_fact(key: &str, model: &str, firmware: &str) -> NewFact {
  let mut n = NormalizedRecord::new(
    "ucm-devices",
    SystemKind::Mdm,
    "phone-device",
    key,
    EntityStatus::Active,
  );
  n.insert("model", Value::String(model.to_owned()));
  n.insert("firmware_version", Value::String(firmware.to_owned()));
  n.insert("vendor", Value::String("Grandstream".to_owned()));
  NewFact {
    system:       SystemId::new("ucm-devices"),
    entity_type:  EntityType::new("phone-device"),
    entity_key:   EntityKey::new(key),
    observed_at:  ts(T0),
    raw:          Some(serde_json::json!({
      "device": { "mac": key, "notes": "reception desk" }
    })),
    normalized:   Some(n),
    norm_version: "grandstream-ucm-device/1".to_owned(),
  }
}

/// The shared `outcome` helper fixes one system kind; the browse tests
/// need two, because telling them apart is the point of the facet.
fn outcome_of(
  system: &str,
  connector: &str,
  kind: SystemKind,
) -> SystemOutcome {
  SystemOutcome {
    system_kind: kind,
    ..outcome(system, connector)
  }
}

/// A store holding two systems read through two connectors.
fn browsable() -> Db {
  let db = db();
  let s = sweep(&db, T0);
  db.write(|w| {
    w.append_facts(s, &[
      user_fact("ada@x.com", EntityStatus::Active, true),
      user_fact("grace@x.com", EntityStatus::Suspended, false),
      device_fact("000b82aabbcc", "GRP2615", "1.0.11.76"),
      device_fact("000b82aabbcd", "GRP2612", "1.0.9.10"),
    ])
  })
  .unwrap();
  db.write(|w| {
    w.record_system(
      s,
      &outcome_of("gws-prod", "google-workspace", SystemKind::Workspace),
    )?;
    w.record_system(
      s,
      &outcome_of("ucm-devices", "grandstream-ucm", SystemKind::Mdm),
    )
  })
  .unwrap();
  db.write(|w| w.commit_sweep(s, SweepStatus::Ok, ts(T0)))
    .unwrap();
  db
}

fn keys(rows: &[overlord_store::EntityRow]) -> Vec<String> {
  let mut v: Vec<String> = rows
    .iter()
    .map(|r| r.entity.entity_key.to_string())
    .collect();
  v.sort();
  v
}

#[test]
fn every_entity_is_listed_whatever_its_type() {
  // The Users roster is person-shaped and excludes non-person types by
  // policy; this is the screen that does not.
  let db = browsable();
  let rows = db.read(|r| r.entities(&EntityFilter::default())).unwrap();
  assert_eq!(rows.len(), 4);
}

#[test]
fn entities_filter_by_system_connector_kind_and_type() {
  let db = browsable();

  let by_system = db
    .read(|r| {
      r.entities(&EntityFilter {
        systems: vec![SystemId::new("ucm-devices")],
        ..Default::default()
      })
    })
    .unwrap();
  assert_eq!(keys(&by_system), ["000b82aabbcc", "000b82aabbcd"]);

  let by_connector = db
    .read(|r| {
      r.entities(&EntityFilter {
        connectors: vec!["google-workspace".to_owned()],
        ..Default::default()
      })
    })
    .unwrap();
  assert_eq!(keys(&by_connector), ["ada@x.com", "grace@x.com"]);
  assert_eq!(
    by_connector[0].connector.as_deref(),
    Some("google-workspace")
  );

  let by_kind = db
    .read(|r| {
      r.entities(&EntityFilter {
        kinds: vec![SystemKind::Mdm],
        ..Default::default()
      })
    })
    .unwrap();
  assert_eq!(keys(&by_kind), ["000b82aabbcc", "000b82aabbcd"]);
  assert_eq!(by_kind[0].system_kind, Some(SystemKind::Mdm));

  let by_type = db
    .read(|r| {
      r.entities(&EntityFilter {
        entity_types: vec![EntityType::new("user")],
        ..Default::default()
      })
    })
    .unwrap();
  assert_eq!(keys(&by_type), ["ada@x.com", "grace@x.com"]);
}

#[test]
fn the_facets_narrow_together_rather_than_widening() {
  // Two facets that disagree return nothing, which is what an operator
  // reading them as "and" expects.
  let db = browsable();
  let rows = db
    .read(|r| {
      r.entities(&EntityFilter {
        connectors: vec!["grandstream-ucm".to_owned()],
        entity_types: vec![EntityType::new("user")],
        ..Default::default()
      })
    })
    .unwrap();
  assert!(rows.is_empty());
}

#[test]
fn search_reaches_the_normalized_overlay_and_the_raw_payload() {
  let db = browsable();
  let find = |q: &str| {
    db.read(|r| {
      r.entities(&EntityFilter {
        query: Some(q.to_owned()),
        ..Default::default()
      })
    })
    .map(|rows| keys(&rows))
    .unwrap()
  };

  // A normalized field.
  assert_eq!(find("GRP2615"), ["000b82aabbcc"]);
  // A value only the vendor payload carries.
  assert_eq!(find("reception"), ["000b82aabbcc", "000b82aabbcd"]);
  // The entity key itself, including a partial one.
  assert_eq!(find("000b82aabbcd"), ["000b82aabbcd"]);
  // Case-insensitive, and a prefix is enough.
  assert_eq!(find("grandstr"), ["000b82aabbcc", "000b82aabbcd"]);
  assert_eq!(find("ADA"), ["ada@x.com"]);
  // Terms are ANDed: both must appear on the same entity.
  assert!(find("GRP2615 GRP2612").is_empty());
  assert_eq!(find("grandstream GRP2612"), ["000b82aabbcd"]);
}

#[test]
fn search_indexes_values_rather_than_the_json_around_them() {
  // Searching `vendor` should not return every entity that has a
  // `vendor` key. Keys are structure; an operator is looking for
  // content.
  let db = browsable();
  let rows = db
    .read(|r| {
      r.entities(&EntityFilter {
        query: Some("firmware_version".to_owned()),
        ..Default::default()
      })
    })
    .unwrap();
  assert!(rows.is_empty(), "{:?}", keys(&rows));
}

#[test]
fn punctuation_in_the_search_box_is_not_a_database_error() {
  // FTS5 syntax has its own operators, and an email address alone has
  // enough punctuation to fail the query outright. Every one of these
  // must return results or nothing — never an error.
  let db = browsable();
  for q in [
    "ada@x.com",
    "\"unterminated",
    "a AND b",
    "NOT ada",
    "*",
    "^ada",
    "()",
    "col:umn",
    "   ",
    "",
  ] {
    db.read(|r| {
      r.entities(&EntityFilter {
        query: Some(q.to_owned()),
        ..Default::default()
      })
    })
    .unwrap_or_else(|e| panic!("{q:?} failed: {e}"));
  }
  // And an address does find its account rather than being dropped.
  let rows = db
    .read(|r| {
      r.entities(&EntityFilter {
        query: Some("ada@x.com".to_owned()),
        ..Default::default()
      })
    })
    .unwrap();
  assert_eq!(keys(&rows), ["ada@x.com"]);
}

#[test]
fn an_empty_search_is_no_filter_rather_than_no_results() {
  assert!(fts_query("").is_none());
  assert!(fts_query("   ").is_none());
  assert!(fts_query("!!! ---").is_none());
  assert_eq!(fts_query("Ada"), Some("\"ada\"*".to_owned()));
  // An identifier stays one term, because the index holds it as one
  // token. Splitting here would match every entity sharing any fragment.
  assert_eq!(fts_query("ada@x.com"), Some("\"ada@x.com\"*".to_owned()));
  assert_eq!(fts_query("1.0.11.76"), Some("\"1.0.11.76\"*".to_owned()));
  // Punctuation around a term is trimmed rather than kept as part of it.
  assert_eq!(fts_query("  (ada)  "), Some("\"ada\"*".to_owned()));
  assert_eq!(
    fts_query("GRP2615 grandstream"),
    Some("\"grp2615\"* \"grandstream\"*".to_owned())
  );
}

#[test]
fn identifiers_are_searchable_whole_and_by_prefix() {
  let db = browsable();
  let find = |q: &str| {
    db.read(|r| {
      r.entities(&EntityFilter {
        query: Some(q.to_owned()),
        ..Default::default()
      })
    })
    .map(|rows| keys(&rows))
    .unwrap()
  };

  // Whole: the exact firmware, and only the handset running it.
  assert_eq!(find("1.0.11.76"), ["000b82aabbcc"]);
  // By prefix, which is how the box behaves as it is typed.
  assert_eq!(find("1.0.11"), ["000b82aabbcc"]);
  assert_eq!(find("ada"), ["ada@x.com"]);
  assert_eq!(find("ada@x"), ["ada@x.com"]);
  // A fragment that is not a prefix does not match, which is the price
  // of keeping identifiers whole and is the right side of the trade.
  assert!(find("x.com").is_empty());
}

#[test]
fn the_index_follows_the_latest_fact_rather_than_the_first() {
  // "Current state" is the latest fact (SPEC.md section 6.1). A search
  // index that kept the first would answer questions about a firmware
  // version the handset no longer runs.
  let db = browsable();
  let s2 = sweep(&db, T1);
  db.write(|w| {
    w.append_facts(s2, &[device_fact("000b82aabbcc", "GRP2615", "9.9.9.9")])
  })
  .unwrap();
  db.write(|w| w.commit_sweep(s2, SweepStatus::Ok, ts(T1)))
    .unwrap();

  let find = |q: &str| {
    db.read(|r| {
      r.entities(&EntityFilter {
        query: Some(q.to_owned()),
        ..Default::default()
      })
    })
    .map(|rows| keys(&rows))
    .unwrap()
  };
  assert_eq!(find("9.9.9.9"), ["000b82aabbcc"]);
  assert!(find("1.0.11.76").is_empty(), "the superseded value lingers");
  // And the other handset, on 1.0.9.10, is not dragged in by sharing a
  // digit group with 9.9.9.9 — a version is one token, not four.
  assert_eq!(find("1.0.9.10"), ["000b82aabbcd"]);
}

#[test]
fn an_absent_entity_is_excluded_by_default_and_findable_on_request() {
  let db = browsable();
  let s2 = sweep(&db, T1);
  db.write(|w| {
    w.append_facts(s2, &[NewFact {
      system:       SystemId::new("ucm-devices"),
      entity_type:  EntityType::new("phone-device"),
      entity_key:   EntityKey::new("000b82aabbcd"),
      observed_at:  ts(T1),
      raw:          None,
      normalized:   None,
      norm_version: "grandstream-ucm-device/1".to_owned(),
    }])
  })
  .unwrap();
  db.write(|w| w.commit_sweep(s2, SweepStatus::Ok, ts(T1)))
    .unwrap();

  let present = db.read(|r| r.entities(&EntityFilter::default())).unwrap();
  assert!(!keys(&present).contains(&"000b82aabbcd".to_owned()));

  let gone = db
    .read(|r| {
      r.entities(&EntityFilter {
        present: Some(false),
        ..Default::default()
      })
    })
    .unwrap();
  assert_eq!(keys(&gone), ["000b82aabbcd"]);
  // A tombstone carries no overlay, so there is no status to report —
  // and the kind still resolves, from the system's last sweep.
  assert_eq!(gone[0].status, None);
  assert_eq!(gone[0].system_kind, Some(SystemKind::Mdm));
  assert!(!gone[0].present);
}

#[test]
fn the_filter_options_come_from_what_the_store_actually_holds() {
  let db = browsable();
  assert_eq!(db.read(|r| r.entity_types()).unwrap(), [
    EntityType::new("phone-device"),
    EntityType::new("user")
  ]);
  assert_eq!(db.read(|r| r.known_connectors()).unwrap(), [
    "google-workspace",
    "grandstream-ucm"
  ]);
}

#[test]
fn a_rebuild_leaves_the_search_index_intact() {
  // The index is maintained by trigger precisely so that replay needs
  // no special case. If that ever stops being true, search silently
  // returns less than the store holds.
  let db = browsable();
  db.rebuild_projections().unwrap();

  let rows = db
    .read(|r| {
      r.entities(&EntityFilter {
        query: Some("grandstream".to_owned()),
        ..Default::default()
      })
    })
    .unwrap();
  assert_eq!(keys(&rows), ["000b82aabbcc", "000b82aabbcd"]);
  assert_eq!(
    db.read(|r| r.entities(&EntityFilter::default()))
      .unwrap()
      .len(),
    4
  );
}
