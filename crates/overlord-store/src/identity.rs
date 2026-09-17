//! Link suggestions: the one projection identity work reads before it
//! writes anything (SPEC.md section 12).
//!
//! Suggestions are machine-proposed and never applied automatically, so
//! nothing here emits a command or touches `link`. They are recomputed
//! wholesale each sweep, because a suggestion is a statement about the
//! facts that sweep observed and a stale one is worse than none.

use std::collections::BTreeSet;

use overlord_core::{EntityRef, PersonUid, SweepId};
use rusqlite::params;

use crate::{
  db::{Reader, Writer},
  error::Result,
};

/// One proposed link, as the engine computes it and the detail page
/// shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
  pub entity:     EntityRef,
  /// The person the entity might belong to: a confirmed one, or the
  /// implicit singleton of another unlinked entity.
  pub person_uid: PersonUid,
  /// How it was found. Shown to the operator verbatim, so it is a name
  /// rather than a score.
  pub signal:     String,
  /// Why, in enough detail to judge it without leaving the page.
  pub evidence:   serde_json::Value,
}

impl Writer<'_> {
  /// Replace every suggestion with the ones this sweep computed.
  ///
  /// Wholesale, not incremental: an entity whose email changed should
  /// stop being suggested against its old match, and a suggestion the
  /// operator already acted on should simply not come back.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn replace_suggestions(
    &self,
    sweep: SweepId,
    suggestions: &[Suggestion],
  ) -> Result<()> {
    self.conn().execute("DELETE FROM suggestion", [])?;
    for s in suggestions {
      self.conn().execute(
        "INSERT INTO suggestion (
           system, entity_type, entity_key, person_uid, signal, evidence,
           sweep_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (system, entity_type, entity_key, person_uid)
           DO UPDATE SET
             signal = excluded.signal,
             evidence = excluded.evidence,
             sweep_id = excluded.sweep_id",
        params![
          s.entity.system.as_str(),
          s.entity.entity_type.as_str(),
          s.entity.entity_key.as_str(),
          s.person_uid.as_str(),
          &s.signal,
          serde_json::to_string(&s.evidence)?,
          sweep.0,
        ],
      )?;
    }
    Ok(())
  }
}

impl Reader<'_> {
  /// Every unreviewed suggestion, worst-scoring entity first, so the
  /// identity queue leads with the accounts that matter.
  ///
  /// One row per proposed *pair*. A match between two unlinked accounts
  /// is stored from both sides — each account's own page must show it —
  /// but as a queue that would be two rows for one decision, and
  /// confirming either resolves both. The surviving row is the
  /// higher-scoring account's, which is the one an operator would rather
  /// be looking at.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn pending_suggestions(&self, limit: usize) -> Result<Vec<Suggestion>> {
    // Read past the limit before folding the mirrors away, or a page of
    // ten pairs would arrive as five.
    let limit = i64::try_from(limit.saturating_mul(2)).unwrap_or(i64::MAX);
    let mut stmt = self.conn().prepare(
      "SELECT s.system, s.entity_type, s.entity_key, s.person_uid,
              s.signal, s.evidence,
              coalesce(p.score, 0) AS score
         FROM suggestion s
         LEFT JOIN link l
           ON l.system = s.system AND l.entity_type = s.entity_type
          AND l.entity_key = s.entity_key
         LEFT JOIN person_score p
           ON p.person_uid = 'implicit:' || s.system || '/' || s.entity_type
              || '/' || s.entity_key
        WHERE l.person_uid IS NULL
        ORDER BY score DESC, s.system, s.entity_key, s.person_uid
        LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |r| {
      Ok((
        EntityRef::new(
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, String>(2)?,
        ),
        PersonUid::new(r.get::<_, String>(3)?),
        r.get::<_, String>(4)?,
        r.get::<_, String>(5)?,
      ))
    })?;

    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out = Vec::new();
    for row in rows {
      let (entity, person_uid, signal, evidence) = row?;
      // The mirror of this proposal is the same two identities the
      // other way round, so an unordered pair is the key.
      let mine = PersonUid::implicit(&entity).to_string();
      let theirs = person_uid.as_str().to_owned();
      let pair = if mine <= theirs {
        (mine, theirs)
      } else {
        (theirs, mine)
      };
      if !seen.insert(pair) {
        continue;
      }
      out.push(Suggestion {
        entity,
        person_uid,
        signal,
        evidence: serde_json::from_str(&evidence)?,
      });
    }
    Ok(out)
  }

  /// How many entities have at least one unreviewed suggestion.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn pending_suggestion_count(&self) -> Result<i64> {
    Ok(self.conn().query_row(
      "SELECT count(*) FROM (
         SELECT DISTINCT s.system, s.entity_type, s.entity_key
           FROM suggestion s
           LEFT JOIN link l
             ON l.system = s.system AND l.entity_type = s.entity_type
            AND l.entity_key = s.entity_key
          WHERE l.person_uid IS NULL)",
      [],
      |r| r.get(0),
    )?)
  }
}
