//! Replaying the streams into the projections.
//!
//! SPEC.md section 13: any projection can be rebuilt by replaying the
//! streams, and a test asserts `replay(streams) == live`. Because checks
//! and normalization live in the command stream and each sweep pins the
//! revisions it used, the assertion is total — no input to evaluation
//! lives outside the two streams.
//!
//! Evaluation itself lives in the engine, which depends on this crate
//! rather than the other way round, so replay takes a hook: it calls
//! back at each sweep's commit position, where the live path evaluated.

use overlord_core::{CommandKind, SweepId, Timestamp};
use rusqlite::params;

use crate::{
  db::{Db, Writer},
  error::Result,
};

/// Projections, in an order that respects foreign keys when cleared.
///
/// `sweep` and `sweep_system` are deliberately absent. They record what
/// a connector actually reported during a run — including whether the
/// absence guard tripped, a decision taken with the snapshot in hand —
/// which is not derivable from the streams and would be lost, not
/// rebuilt, by clearing it.
const PROJECTIONS: [&str; 14] = [
  "violation_event",
  "violation",
  "person_score",
  "suggestion",
  "link_primary",
  "link",
  "person_alias",
  "person",
  "check_dryrun",
  "check_revision",
  "check_head",
  "normalization_ruleset",
  "identity_policy",
  "entity",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
  pub commands: usize,
  pub facts:    usize,
  pub sweeps:   usize,
}

/// What replay encountered, in stream-sequence order.
enum Event {
  Command(i64),
  Fact(i64),
  SweepCommitted(SweepId),
}

/// Rebuild every projection derived from the two streams.
///
/// `on_sweep_commit` is called at each sweep's commit position with the
/// projections in exactly the state the live path had at that moment.
/// The engine uses it to re-run evaluation; a store-only caller passes a
/// no-op and gets everything except violations.
///
/// # Errors
/// If a stored command cannot be interpreted — which means the store
/// holds something this binary is too old to understand, and replay must
/// stop rather than silently skip it.
pub fn rebuild_with(
  db: &Db,
  on_sweep_commit: &mut dyn FnMut(&Writer<'_>, SweepId) -> Result<()>,
) -> Result<RebuildReport> {
  db.write(|w| {
    let tx = w.conn();
    for table in PROJECTIONS {
      tx.execute(&format!("DELETE FROM {table}"), [])?;
    }

    // One merged timeline. Ordering by the shared sequence is what makes
    // this faithful: a command that landed while a sweep was running
    // replays in the same position it was appended.
    let events = {
      let mut stmt = tx.prepare(
        "SELECT seq, 0 AS kind, id FROM command
         UNION ALL
         SELECT seq, 1 AS kind, id FROM fact
         UNION ALL
         SELECT committed_seq, 2 AS kind, id FROM sweep
           WHERE committed_seq IS NOT NULL
         ORDER BY seq",
      )?;
      let rows = stmt.query_map([], |r| {
        let kind: i64 = r.get(1)?;
        let id: i64 = r.get(2)?;
        Ok(match kind {
          0 => Event::Command(id),
          1 => Event::Fact(id),
          _ => Event::SweepCommitted(SweepId(id)),
        })
      })?;
      rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut report = RebuildReport::default();
    for event in events {
      match event {
        Event::Command(id) => {
          replay_command(w, id)?;
          report.commands += 1;
        }
        Event::Fact(id) => {
          replay_fact(w, id)?;
          report.facts += 1;
        }
        Event::SweepCommitted(sweep) => {
          on_sweep_commit(w, sweep)?;
          report.sweeps += 1;
        }
      }
    }
    Ok(report)
  })
}

/// Rebuild everything the streams alone determine.
///
/// Violations are *not* rebuilt: they are produced by evaluation, which
/// lives in the engine. Use the engine's rebuild for a full one.
///
/// # Errors
/// As [`rebuild_with`].
pub fn rebuild(db: &Db) -> Result<RebuildReport> {
  rebuild_with(db, &mut |_, _| Ok(()))
}

fn replay_command(w: &Writer<'_>, id: i64) -> Result<()> {
  let (at, kind, args): (String, String, String) = w.conn().query_row(
    "SELECT at, kind, args FROM command WHERE id = ?1",
    [id],
    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
  )?;
  let at: Timestamp = at.parse()?;
  let kind = CommandKind::from_parts(&kind, serde_json::from_str(&args)?)?;
  w.project_command(id, at, &kind)
}

fn replay_fact(w: &Writer<'_>, id: i64) -> Result<()> {
  let row = w.conn().query_row(
    "SELECT sweep_id, system, entity_type, entity_key, present, raw_hash,
            norm_hash
       FROM fact WHERE id = ?1",
    [id],
    |r| {
      Ok((
        r.get::<_, i64>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
        r.get::<_, String>(3)?,
        r.get::<_, i64>(4)? != 0,
        r.get::<_, Option<String>>(5)?,
        r.get::<_, Option<String>>(6)?,
      ))
    },
  )?;
  let (sweep_id, system, entity_type, entity_key, present, raw_hash, norm_hash) =
    row;

  // The stored overlay is authoritative: a rebuild reproduces the
  // overlay that was live at the time rather than recomputing it with
  // today's normalization ruleset (SPEC.md section 6.1).
  let normalized = match &norm_hash {
    Some(h) => Some(crate::db::get_payload(w.conn(), h)?),
    None => None,
  };

  w.conn().execute(
    "INSERT INTO entity (
       system, entity_type, entity_key, present, latest_fact_id,
       normalized, raw_hash, first_seen_sweep, last_seen_sweep)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
     ON CONFLICT (system, entity_type, entity_key) DO UPDATE SET
       present         = excluded.present,
       latest_fact_id  = excluded.latest_fact_id,
       normalized      = excluded.normalized,
       raw_hash        = excluded.raw_hash,
       last_seen_sweep = excluded.last_seen_sweep
     WHERE excluded.latest_fact_id > entity.latest_fact_id",
    params![
      system,
      entity_type,
      entity_key,
      i32::from(present),
      id,
      normalized,
      raw_hash,
      sweep_id,
    ],
  )?;
  Ok(())
}
