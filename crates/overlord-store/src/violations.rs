//! Violation rows and scoring.
//!
//! The state machine of SPEC.md section 9 lives in the engine; this
//! module is the set of row operations it drives, plus the scoring query,
//! which is pure SQL over the projections.

use overlord_core::{
  Evidence, ResolveReason, Severity, SubjectRef, SweepId, Timestamp,
  ViolationEventKind, ViolationState,
};
use rusqlite::{OptionalExtension, params};

use crate::{db::Writer, error::Result};

/// The latest episode of one `(check, subject)` violation.
#[derive(Debug, Clone)]
pub struct Episode {
  pub episode: i64,
  pub state: ViolationState,
  pub revision_open: u32,
  pub suppress_until: Option<Timestamp>,
}

/// Everything needed to open or refresh an episode.
#[derive(Debug, Clone)]
pub struct EpisodeFacts {
  pub check_id: String,
  pub subject: SubjectRef,
  pub severity: Severity,
  pub weight: i64,
  pub revision: u32,
  pub sweep: SweepId,
  pub at: Timestamp,
  pub evidence: Evidence,
  pub stale: bool,
  pub ambiguous: bool,
  pub eval_error: Option<String>,
}

impl Writer<'_> {
  /// The latest episode, whatever its state.
  ///
  /// # Errors
  /// On a SQLite failure or an unreadable stored state.
  pub fn latest_episode(
    &self,
    check_id: &str,
    subject_ref: &str,
  ) -> Result<Option<Episode>> {
    let row: Option<(i64, String, u32, Option<String>)> = self
      .conn()
      .query_row(
        "SELECT episode, state, revision_open, suppress_until
           FROM violation
          WHERE check_id = ?1 AND subject_ref = ?2
          ORDER BY episode DESC LIMIT 1",
        params![check_id, subject_ref],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
      )
      .optional()?;

    let Some((episode, state, revision_open, until)) = row else {
      return Ok(None);
    };
    Ok(Some(Episode {
      episode,
      state: state.parse()?,
      revision_open,
      suppress_until: until.as_deref().map(str::parse).transpose()?,
    }))
  }

  /// Open a new episode.
  ///
  /// SPEC.md section 9: a regression starts clean — a reopened violation
  /// returns to `open`, never to a carried-over `acknowledged` — so a
  /// new episode never inherits an overlay. Prior episodes stay in the
  /// table and in the history, which is what makes the regression loud
  /// rather than pre-silenced.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn open_episode(&self, f: &EpisodeFacts) -> Result<i64> {
    let subject_ref = f.subject.to_string();
    let previous: Option<i64> = self
      .conn()
      .query_row(
        "SELECT max(episode) FROM violation
          WHERE check_id = ?1 AND subject_ref = ?2",
        params![&f.check_id, &subject_ref],
        |r| r.get(0),
      )
      .optional()?
      .flatten();
    let episode = previous.unwrap_or(0) + 1;
    let regression = previous.is_some();

    self.conn().execute(
      "INSERT INTO violation (
         check_id, subject_ref, episode, subject_kind, state, severity,
         weight, opened_sweep, opened_at, revision_open, last_seen_sweep,
         evidence, stale, ambiguous, eval_error)
       VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?7, ?8, ?9, ?7, ?10, ?11,
               ?12, ?13)",
      params![
        &f.check_id,
        &subject_ref,
        episode,
        f.subject.kind().as_str(),
        f.severity.as_str(),
        f.weight,
        f.sweep.0,
        f.at.to_string(),
        f.revision,
        serde_json::to_string(&f.evidence)?,
        i32::from(f.stale),
        i32::from(f.ambiguous),
        f.eval_error.as_deref(),
      ],
    )?;

    self.record_event(
      &f.check_id,
      &subject_ref,
      episode,
      f.at,
      if regression {
        ViolationEventKind::Regressed
      } else {
        ViolationEventKind::Opened
      },
      Some(f.sweep.0),
      None,
      None,
    )?;
    Ok(episode)
  }

  /// Refresh a standing episode with this sweep's evidence.
  ///
  /// The state is untouched: only the operator and the resolution rules
  /// move a violation between states.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn touch_episode(&self, episode: i64, f: &EpisodeFacts) -> Result<()> {
    self.conn().execute(
      "UPDATE violation
          SET last_seen_sweep = ?4, evidence = ?5, stale = ?6,
              ambiguous = ?7, eval_error = ?8, severity = ?9, weight = ?10
        WHERE check_id = ?1 AND subject_ref = ?2 AND episode = ?3",
      params![
        &f.check_id,
        f.subject.to_string(),
        episode,
        f.sweep.0,
        serde_json::to_string(&f.evidence)?,
        i32::from(f.stale),
        i32::from(f.ambiguous),
        f.eval_error.as_deref(),
        f.severity.as_str(),
        f.weight,
      ],
    )?;
    Ok(())
  }

  /// Close an episode. Auto-resolve is the only close (SPEC.md s9).
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn resolve_episode(
    &self,
    check_id: &str,
    subject_ref: &str,
    episode: i64,
    at: Timestamp,
    reason: ResolveReason,
    sweep: Option<SweepId>,
  ) -> Result<()> {
    let reason_str = serde_json::to_value(reason)?
      .as_str()
      .unwrap_or("condition_cleared")
      .to_owned();
    self.conn().execute(
      "UPDATE violation
          SET state = 'resolved', resolved_at = ?4, resolve_reason = ?5,
              overlay_cmd = NULL, overlay_rev = NULL,
              suppress_reason = NULL, suppress_until = NULL
        WHERE check_id = ?1 AND subject_ref = ?2 AND episode = ?3",
      params![check_id, subject_ref, episode, at.to_string(), &reason_str],
    )?;
    self.record_event(
      check_id,
      subject_ref,
      episode,
      at,
      ViolationEventKind::Cleared,
      sweep.map(|s| s.0),
      None,
      Some(&reason_str),
    )
  }

  /// Return an expired suppression to `open`.
  ///
  /// Expiry is evaluated at sweep time against the sweep's start time,
  /// so it replays identically.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn expire_suppression(
    &self,
    check_id: &str,
    subject_ref: &str,
    episode: i64,
    at: Timestamp,
    sweep: SweepId,
  ) -> Result<()> {
    self.conn().execute(
      "UPDATE violation
          SET state = 'open', overlay_cmd = NULL, overlay_rev = NULL,
              suppress_reason = NULL, suppress_until = NULL
        WHERE check_id = ?1 AND subject_ref = ?2 AND episode = ?3",
      params![check_id, subject_ref, episode],
    )?;
    self.record_event(
      check_id,
      subject_ref,
      episode,
      at,
      ViolationEventKind::SuppressionExpired,
      Some(sweep.0),
      None,
      None,
    )
  }

  /// Every episode not yet resolved, as `(check_id, subject_ref,
  /// episode)`. The evaluator uses this to find what a sweep did *not*
  /// re-confirm.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn standing_episodes(&self) -> Result<Vec<(String, String, i64)>> {
    let mut stmt = self.conn().prepare(
      "SELECT check_id, subject_ref, episode FROM violation
        WHERE state != 'resolved'",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
  }

  /// Recompute every subject's risk score (SPEC.md section 8).
  ///
  /// A person's score includes the entity-scoped violations of all its
  /// entities, and an unlinked entity is an implicit person, so
  /// confirming a link merges two scores rather than revealing a new
  /// one: the organisation's total never moves because identity work
  /// happened. A retired uid resolves through its alias for the same
  /// reason.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn recompute_scores(&self) -> Result<()> {
    self.conn().execute("DELETE FROM person_score", [])?;
    self.conn().execute(
      "WITH attributed AS (
         SELECT
           v.weight AS weight,
           v.severity AS severity,
           CASE
             WHEN v.subject_kind = 'person' THEN substr(v.subject_ref, 8)
             ELSE coalesce(
               (SELECT l.person_uid FROM link l
                 WHERE 'entity/' || l.system || '/' || l.entity_type
                       || '/' || l.entity_key = v.subject_ref),
               'implicit:' || substr(v.subject_ref, 8))
           END AS raw_uid
         FROM violation v
         WHERE v.state IN ('open', 'acknowledged')
       ),
       resolved AS (
         SELECT
           coalesce(a.surviving_uid, attributed.raw_uid) AS uid,
           weight,
           CASE severity
             WHEN 'critical' THEN 0 WHEN 'high' THEN 1
             WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4
           END AS rank
         FROM attributed
         LEFT JOIN person_alias a ON a.retired_uid = attributed.raw_uid
       )
       INSERT INTO person_score (
         person_uid, score, violation_count, worst_severity)
       SELECT uid, sum(weight), count(*),
              CASE min(rank)
                WHEN 0 THEN 'critical' WHEN 1 THEN 'high'
                WHEN 2 THEN 'medium' WHEN 3 THEN 'low' ELSE 'info'
              END
         FROM resolved
        GROUP BY uid",
      [],
    )?;
    Ok(())
  }

  /// The sum of every active violation's weight, across the whole
  /// store. Identity work must never move this number.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn total_score(&self) -> Result<i64> {
    let total: Option<i64> = self.conn().query_row(
      "SELECT sum(weight) FROM violation
        WHERE state IN ('open', 'acknowledged')",
      [],
      |r| r.get(0),
    )?;
    Ok(total.unwrap_or(0))
  }
}
