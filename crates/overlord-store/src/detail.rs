//! Read models for the detail screens (SPEC.md section 5).
//!
//! [`crate::read`] answers the questions evaluation asks: what is the
//! current state, what is open, who is worst. These answer the questions
//! an operator asks once a row on the board has caught their eye — what
//! happened to this violation, what does this account actually look like,
//! what did that sweep see. They are wordier and less hot, so they live
//! apart rather than crowding the evaluation path.

use overlord_core::{
  Actor, CheckDraft, CheckId, DryrunSample, EntityRef, EntityStatus, Evidence,
  NormalizedRecord, PersonUid, Revision, Severity, SubjectRef, SweepId,
  SystemId, SystemKind, Timestamp, ViolationEventKind, ViolationState,
};
use rusqlite::{OptionalExtension, params};

use crate::{
  db::{Reader, get_payload},
  error::Result,
  streams::{SweepStatus, SystemStatus},
};

// --- sweeps -----------------------------------------------------------

/// One system's line in a sweep's coverage table (SPEC.md section 10).
#[derive(Debug, Clone)]
pub struct CoverageRow {
  pub system:         SystemId,
  pub system_kind:    SystemKind,
  pub status:         SystemStatus,
  pub complete:       bool,
  pub observed_count: i64,
  pub tombstoned:     i64,
  /// What the previous sweep of this system counted, so a connector that
  /// has gone quiet is visible as a delta rather than only as a number.
  pub previous_count: Option<i64>,
  pub guard_tripped:  bool,
  pub duration_ms:    i64,
  pub error:          Option<String>,
}

impl CoverageRow {
  /// The change in entity count since this system's previous sweep.
  /// `None` on a system's first sweep, where there is nothing to compare.
  #[must_use]
  pub fn delta(&self) -> Option<i64> {
    self.previous_count.map(|p| self.observed_count - p)
  }
}

/// A run, as the Sweeps screen lists it.
#[derive(Debug, Clone)]
pub struct SweepRow {
  pub id:          SweepId,
  pub started_at:  Timestamp,
  pub finished_at: Option<Timestamp>,
  pub status:      SweepStatus,
  /// The systems the operator asked for. Empty means every configured
  /// system, which is how an unrestricted run records itself.
  pub requested:   Vec<SystemId>,
  pub coverage:    Vec<CoverageRow>,
  pub facts:       i64,
}

impl SweepRow {
  /// Whether this run is still going, which is what the UI polls on.
  #[must_use]
  pub fn running(&self) -> bool { self.status == SweepStatus::Running }
}

impl Reader<'_> {
  /// Recent runs, newest first, each with its coverage.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn sweeps(&self, limit: usize) -> Result<Vec<SweepRow>> {
    let mut stmt = self
      .conn()
      .prepare("SELECT id FROM sweep ORDER BY id DESC LIMIT ?1")?;
    let ids = stmt
      .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
        r.get::<_, i64>(0)
      })?
      .collect::<rusqlite::Result<Vec<_>>>()?;
    ids
      .into_iter()
      .map(|id| {
        self.sweep(SweepId(id))?.ok_or_else(|| {
          crate::error::StoreError::not_found(format!("sweep {id}"))
        })
      })
      .collect()
  }

  /// One run with its coverage, or `None` if it is unknown.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn sweep(&self, id: SweepId) -> Result<Option<SweepRow>> {
    let row: Option<(String, Option<String>, String, String)> = self
      .conn()
      .query_row(
        "SELECT started_at, finished_at, status, requested
           FROM sweep WHERE id = ?1",
        [id.0],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
      )
      .optional()?;
    let Some((started_at, finished_at, status, requested)) = row else {
      return Ok(None);
    };

    let facts: i64 = self.conn().query_row(
      "SELECT count(*) FROM fact WHERE sweep_id = ?1",
      [id.0],
      |r| r.get(0),
    )?;

    let requested: Vec<String> = serde_json::from_str(&requested)?;

    Ok(Some(SweepRow {
      id,
      started_at: started_at.parse()?,
      finished_at: finished_at.map(|f| f.parse()).transpose()?,
      status: status.parse()?,
      requested: requested.into_iter().map(SystemId::new).collect(),
      coverage: self.coverage(id)?,
      facts,
    }))
  }

  /// What each system reported during one run.
  ///
  /// # Errors
  /// On a SQLite failure or an unreadable stored enum.
  pub fn coverage(&self, sweep: SweepId) -> Result<Vec<CoverageRow>> {
    let mut stmt = self.conn().prepare(
      "SELECT system, system_kind, status, complete, observed_count,
              tombstoned, previous_count, guard_tripped, duration_ms, error
         FROM sweep_system WHERE sweep_id = ?1 ORDER BY system",
    )?;
    let rows = stmt.query_map([sweep.0], |r| {
      Ok((
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
        r.get::<_, i64>(3)? != 0,
        r.get::<_, i64>(4)?,
        r.get::<_, i64>(5)?,
        r.get::<_, Option<i64>>(6)?,
        r.get::<_, i64>(7)? != 0,
        r.get::<_, i64>(8)?,
        r.get::<_, Option<String>>(9)?,
      ))
    })?;
    let mut out = Vec::new();
    for row in rows {
      let (
        system,
        kind,
        status,
        complete,
        observed_count,
        tombstoned,
        previous_count,
        guard_tripped,
        duration_ms,
        error,
      ) = row?;
      out.push(CoverageRow {
        system: SystemId::new(system),
        system_kind: kind.parse()?,
        status: status.parse()?,
        complete,
        observed_count,
        tombstoned,
        previous_count,
        guard_tripped,
        duration_ms,
        error,
      });
    }
    Ok(out)
  }
}

// --- systems ----------------------------------------------------------

/// A connected system as the Systems screen shows it (SPEC.md section 5:
/// deliberately lightweight, not an asset inventory).
#[derive(Debug, Clone)]
pub struct SystemRow {
  pub system:       SystemId,
  pub system_kind:  SystemKind,
  /// Entities currently present. Absent ones are excluded, so this is
  /// what a check would actually evaluate over.
  pub entities:     i64,
  /// The last run that reached this system at all, successfully or not.
  pub last_sweep:   Option<SweepId>,
  pub last_at:      Option<Timestamp>,
  pub last_status:  Option<SystemStatus>,
  /// The last run that came back `ok`, which is the freshness an
  /// operator actually cares about — a failing connector keeps
  /// appearing in sweeps without refreshing anything.
  pub last_ok_at:   Option<Timestamp>,
  pub last_error:   Option<String>,
  pub guard_recent: bool,
}

impl Reader<'_> {
  /// Every system overlord has observed, with its freshness.
  ///
  /// # Errors
  /// On a SQLite failure or an unreadable stored enum.
  pub fn systems(&self) -> Result<Vec<SystemRow>> {
    let mut out = Vec::new();
    for (system, system_kind) in self.known_systems()? {
      let entities: i64 = self.conn().query_row(
        "SELECT count(*) FROM entity WHERE system = ?1 AND present = 1",
        [system.as_str()],
        |r| r.get(0),
      )?;

      let last: Option<(i64, String, String, Option<String>, i64)> = self
        .conn()
        .query_row(
          "SELECT s.id, s.started_at, ss.status, ss.error, ss.guard_tripped
             FROM sweep_system ss
             JOIN sweep s ON s.id = ss.sweep_id
            WHERE ss.system = ?1
            ORDER BY s.id DESC LIMIT 1",
          [system.as_str()],
          |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;

      let last_ok: Option<String> = self
        .conn()
        .query_row(
          "SELECT s.started_at
             FROM sweep_system ss
             JOIN sweep s ON s.id = ss.sweep_id
            WHERE ss.system = ?1 AND ss.status = 'ok'
            ORDER BY s.id DESC LIMIT 1",
          [system.as_str()],
          |r| r.get(0),
        )
        .optional()?;

      let (last_sweep, last_at, last_status, last_error, guard_recent) =
        match last {
          Some((id, at, status, error, guard)) => (
            Some(SweepId(id)),
            Some(at.parse()?),
            Some(status.parse::<SystemStatus>()?),
            error,
            guard != 0,
          ),
          None => (None, None, None, None, false),
        };

      out.push(SystemRow {
        system,
        system_kind,
        entities,
        last_sweep,
        last_at,
        last_status,
        last_ok_at: last_ok.map(|a| a.parse()).transpose()?,
        last_error,
        guard_recent,
      });
    }
    Ok(out)
  }
}

// --- violation history ------------------------------------------------

/// One entry in a violation's history (SPEC.md section 6.3).
#[derive(Debug, Clone)]
pub struct EventRow {
  pub at:     Timestamp,
  pub kind:   ViolationEventKind,
  pub sweep:  Option<SweepId>,
  /// The operator behind the event, for a command-driven one. A
  /// sweep-driven event has none: nobody decided it, a fact did.
  pub actor:  Option<Actor>,
  pub note:   Option<String>,
  pub detail: Option<String>,
}

/// One episode of a violation, with everything the detail view shows.
#[derive(Debug, Clone)]
pub struct EpisodeRow {
  pub episode:         i64,
  pub state:           ViolationState,
  pub severity:        Severity,
  pub weight:          i64,
  pub opened_at:       Timestamp,
  pub opened_sweep:    SweepId,
  pub revision_open:   Revision,
  pub last_seen_sweep: SweepId,
  pub resolved_at:     Option<Timestamp>,
  pub resolve_reason:  Option<String>,
  pub suppress_reason: Option<String>,
  pub suppress_until:  Option<Timestamp>,
  pub evidence:        Evidence,
  pub stale:           bool,
  pub ambiguous:       bool,
  pub eval_error:      Option<String>,
  pub events:          Vec<EventRow>,
}

impl Reader<'_> {
  /// Every episode of one `(check, subject)` violation, newest first.
  ///
  /// SPEC.md section 9: a regression opens a new episode rather than
  /// reviving the old one, and prior episodes stay visible with their
  /// acknowledgements — so this returns the whole chain, not the
  /// current state.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn violation_episodes(
    &self,
    check: &CheckId,
    subject: &SubjectRef,
  ) -> Result<Vec<EpisodeRow>> {
    let subject_ref = subject.to_string();
    let mut stmt = self.conn().prepare(
      "SELECT episode, state, severity, weight, opened_at, opened_sweep,
              revision_open, last_seen_sweep, resolved_at, resolve_reason,
              suppress_reason, suppress_until, evidence, stale, ambiguous,
              eval_error
         FROM violation
        WHERE check_id = ?1 AND subject_ref = ?2
        ORDER BY episode DESC",
    )?;
    let rows = stmt.query_map(params![check.as_str(), &subject_ref], |r| {
      Ok((
        r.get::<_, i64>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
        r.get::<_, i64>(3)?,
        r.get::<_, String>(4)?,
        r.get::<_, i64>(5)?,
        r.get::<_, u32>(6)?,
        r.get::<_, i64>(7)?,
        r.get::<_, Option<String>>(8)?,
        r.get::<_, Option<String>>(9)?,
        r.get::<_, Option<String>>(10)?,
        r.get::<_, Option<String>>(11)?,
        r.get::<_, String>(12)?,
        r.get::<_, i64>(13)? != 0,
        r.get::<_, i64>(14)? != 0,
        r.get::<_, Option<String>>(15)?,
      ))
    })?;

    let mut out = Vec::new();
    for row in rows {
      let r = row?;
      out.push(EpisodeRow {
        episode:         r.0,
        state:           r.1.parse()?,
        severity:        r.2.parse()?,
        weight:          r.3,
        opened_at:       r.4.parse()?,
        opened_sweep:    SweepId(r.5),
        revision_open:   Revision(r.6),
        last_seen_sweep: SweepId(r.7),
        resolved_at:     r.8.map(|a| a.parse()).transpose()?,
        resolve_reason:  r.9,
        suppress_reason: r.10,
        suppress_until:  r.11.map(|a| a.parse()).transpose()?,
        evidence:        serde_json::from_str(&r.12)?,
        stale:           r.13,
        ambiguous:       r.14,
        eval_error:      r.15,
        events:          self.violation_events(check, &subject_ref, r.0)?,
      });
    }
    Ok(out)
  }

  /// The event log for one episode, oldest first.
  ///
  /// # Errors
  /// On a SQLite failure or an unreadable stored enum.
  fn violation_events(
    &self,
    check: &CheckId,
    subject_ref: &str,
    episode: i64,
  ) -> Result<Vec<EventRow>> {
    let mut stmt = self.conn().prepare(
      "SELECT e.at, e.kind, e.sweep_id, e.detail, c.actor, c.note
         FROM violation_event e
         LEFT JOIN command c ON c.id = e.command_id
        WHERE e.check_id = ?1 AND e.subject_ref = ?2 AND e.episode = ?3
        ORDER BY e.id",
    )?;
    let rows =
      stmt.query_map(params![check.as_str(), subject_ref, episode], |r| {
        Ok((
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, Option<i64>>(2)?,
          r.get::<_, Option<String>>(3)?,
          r.get::<_, Option<String>>(4)?,
          r.get::<_, Option<String>>(5)?,
        ))
      })?;
    let mut out = Vec::new();
    for row in rows {
      let (at, kind, sweep, detail, actor, note) = row?;
      out.push(EventRow {
        at: at.parse()?,
        kind: parse_event_kind(&kind)?,
        sweep: sweep.map(SweepId),
        actor: actor.map(Actor::new),
        note,
        detail,
      });
    }
    Ok(out)
  }
}

/// `ViolationEventKind` has no `FromStr`, and adding one to the core
/// crate for a display path would put a parser where nothing parses.
fn parse_event_kind(s: &str) -> Result<ViolationEventKind> {
  use ViolationEventKind::{
    Acknowledged, Cleared, FalsePositive, Opened, Regressed, Revoked,
    Suppressed, SuppressionExpired,
  };
  Ok(match s {
    "opened" => Opened,
    "regressed" => Regressed,
    "acknowledged" => Acknowledged,
    "suppressed" => Suppressed,
    "false_positive" => FalsePositive,
    "revoked" => Revoked,
    "suppression_expired" => SuppressionExpired,
    "cleared" => Cleared,
    other => {
      return Err(crate::error::StoreError::not_found(format!(
        "violation event kind {other}"
      )));
    }
  })
}

// --- entity and person detail -----------------------------------------

/// One observed account, as its detail page shows it.
#[derive(Debug, Clone)]
pub struct EntityDetail {
  pub entity:      EntityRef,
  pub present:     bool,
  pub status:      EntityStatus,
  /// `None` for an entity whose latest fact is a tombstone: there is no
  /// overlay to show, only the record that it went away.
  pub normalized:  Option<NormalizedRecord>,
  pub raw:         serde_json::Value,
  pub first_seen:  SweepId,
  pub last_seen:   SweepId,
  pub latest_fact: i64,
  /// The confirmed person, if the operator has linked this account.
  pub person:      Option<PersonUid>,
}

/// The `entity` projection row [`Reader::entity_detail`] reads. Named
/// rather than a six-wide tuple so the field order is checked by the
/// compiler instead of by counting.
struct EntityProjection {
  present:    bool,
  fact:       i64,
  normalized: Option<String>,
  raw_hash:   Option<String>,
  first:      SweepId,
  last:       SweepId,
}

/// One fact in an entity's timeline (SPEC.md section 5).
#[derive(Debug, Clone)]
pub struct FactRowSummary {
  pub id:           i64,
  pub sweep:        SweepId,
  pub observed_at:  Timestamp,
  pub present:      bool,
  pub norm_version: String,
  /// Whether the overlay differs from the previous observation. Facts
  /// are content-addressed, so this is a hash comparison rather than a
  /// diff — enough to show the operator which sweeps changed anything.
  pub changed:      bool,
}

impl Reader<'_> {
  /// One entity's current state, or `None` if it was never observed.
  ///
  /// Unlike [`Self::entity_states`], this includes absent entities: the
  /// detail page for a deprovisioned account is exactly where an
  /// operator goes to confirm it really did go away.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn entity_detail(
    &self,
    entity: &EntityRef,
  ) -> Result<Option<EntityDetail>> {
    let row: Option<EntityProjection> = self
      .conn()
      .query_row(
        "SELECT present, latest_fact_id, normalized, raw_hash,
                first_seen_sweep, last_seen_sweep
           FROM entity
          WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3",
        params![
          entity.system.as_str(),
          entity.entity_type.as_str(),
          entity.entity_key.as_str(),
        ],
        |r| {
          Ok(EntityProjection {
            present:    r.get::<_, i64>(0)? != 0,
            fact:       r.get(1)?,
            normalized: r.get(2)?,
            raw_hash:   r.get(3)?,
            first:      SweepId(r.get(4)?),
            last:       SweepId(r.get(5)?),
          })
        },
      )
      .optional()?;
    let Some(row) = row else {
      return Ok(None);
    };

    let normalized: Option<NormalizedRecord> = row
      .normalized
      .map(|n| serde_json::from_str(&n))
      .transpose()?;
    let raw = match row.raw_hash {
      Some(h) => serde_json::from_str(&get_payload(self.conn(), &h)?)?,
      None => serde_json::Value::Null,
    };

    Ok(Some(EntityDetail {
      status: normalized
        .as_ref()
        .map_or(EntityStatus::Unknown, |n| n.status),
      entity: entity.clone(),
      present: row.present,
      normalized,
      raw,
      first_seen: row.first,
      last_seen: row.last,
      latest_fact: row.fact,
      person: self.person_of(entity)?,
    }))
  }

  /// An entity's fact timeline, newest first.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn entity_facts(
    &self,
    entity: &EntityRef,
    limit: usize,
  ) -> Result<Vec<FactRowSummary>> {
    let mut stmt = self.conn().prepare(
      "SELECT id, sweep_id, observed_at, present, norm_version, norm_hash
         FROM fact
        WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3
        ORDER BY sweep_id DESC, id DESC
        LIMIT ?4",
    )?;
    let rows = stmt.query_map(
      params![
        entity.system.as_str(),
        entity.entity_type.as_str(),
        entity.entity_key.as_str(),
        i64::try_from(limit).unwrap_or(i64::MAX),
      ],
      |r| {
        Ok((
          r.get::<_, i64>(0)?,
          r.get::<_, i64>(1)?,
          r.get::<_, String>(2)?,
          r.get::<_, i64>(3)? != 0,
          r.get::<_, String>(4)?,
          r.get::<_, Option<String>>(5)?,
        ))
      },
    )?;

    let mut collected = Vec::new();
    for row in rows {
      collected.push(row?);
    }

    // Rows arrive newest first, so each one is compared with its
    // successor in the vector, which is the observation before it.
    let mut out = Vec::new();
    for (i, (id, sweep, at, present, version, hash)) in
      collected.iter().enumerate()
    {
      let previous = collected.get(i + 1).map(|p| &p.5);
      out.push(FactRowSummary {
        id:           *id,
        sweep:        SweepId(*sweep),
        observed_at:  at.parse()?,
        present:      *present,
        norm_version: version.clone(),
        // The oldest row in the window counts as a change: it is the
        // first time this page has anything to show for the entity.
        changed:      previous.is_none_or(|p| p != hash),
      });
    }
    Ok(out)
  }
}

/// A person as their detail page shows them.
#[derive(Debug, Clone)]
pub struct PersonDetail {
  pub person_uid:   PersonUid,
  pub display_name: Option<String>,
  /// An unlinked entity evaluated as a person in its own right (SPEC.md
  /// section 6.4). It has no `person` row, so everything here is derived
  /// from the uid and the entity behind it.
  pub implicit:     bool,
  pub entities:     Vec<EntityRef>,
  pub primaries:    Vec<(SystemKind, EntityRef)>,
  pub score:        i64,
  pub violations:   i64,
}

impl Reader<'_> {
  /// One person, confirmed or implicit, or `None` if the uid resolves to
  /// nothing overlord has seen.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn person_detail(&self, uid: &PersonUid) -> Result<Option<PersonDetail>> {
    let uid = self.resolve_person(uid)?;

    // An implicit person is one entity, named by its own uid, and has no
    // row in `person` to read.
    if let Some(entity) = uid.implicit_entity() {
      let exists: i64 = self.conn().query_row(
        "SELECT count(*) FROM entity
          WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3",
        params![
          entity.system.as_str(),
          entity.entity_type.as_str(),
          entity.entity_key.as_str(),
        ],
        |r| r.get(0),
      )?;
      if exists == 0 {
        return Ok(None);
      }
      let name = self
        .entity_detail(&entity)?
        .and_then(|d| d.normalized.and_then(|n| n.display_name));
      let (score, violations) = self.score_of(&uid)?;
      return Ok(Some(PersonDetail {
        person_uid: uid,
        display_name: name,
        implicit: true,
        entities: vec![entity],
        primaries: Vec::new(),
        score,
        violations,
      }));
    }

    let name: Option<Option<String>> = self
      .conn()
      .query_row(
        "SELECT display_name FROM person WHERE person_uid = ?1",
        [uid.as_str()],
        |r| r.get(0),
      )
      .optional()?;
    let Some(display_name) = name else {
      return Ok(None);
    };

    let mut stmt = self.conn().prepare(
      "SELECT system, entity_type, entity_key FROM link
        WHERE person_uid = ?1 ORDER BY system, entity_type, entity_key",
    )?;
    let entities = stmt
      .query_map([uid.as_str()], |r| {
        Ok(EntityRef::new(
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, String>(2)?,
        ))
      })?
      .collect::<rusqlite::Result<Vec<_>>>()?;

    let primaries = self
      .primaries()?
      .into_iter()
      .filter(|(p, ..)| *p == uid)
      .map(|(_, kind, entity)| Ok((kind.parse::<SystemKind>()?, entity)))
      .collect::<Result<Vec<_>>>()?;

    let (score, violations) = self.score_of(&uid)?;
    Ok(Some(PersonDetail {
      person_uid: uid,
      display_name,
      implicit: false,
      entities,
      primaries,
      score,
      violations,
    }))
  }

  /// A subject's recorded score, or zeroes if it carries no risk.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn score_of(&self, uid: &PersonUid) -> Result<(i64, i64)> {
    Ok(
      self
        .conn()
        .query_row(
          "SELECT score, violation_count FROM person_score
            WHERE person_uid = ?1",
          [uid.as_str()],
          |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0)),
    )
  }

  /// Unreviewed link suggestions for one entity (SPEC.md section 5:
  /// pending suggestions appear on the detail page).
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn suggestions_for(
    &self,
    entity: &EntityRef,
  ) -> Result<Vec<crate::Suggestion>> {
    let mut stmt = self.conn().prepare(
      "SELECT person_uid, signal, evidence FROM suggestion
        WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3
        ORDER BY person_uid",
    )?;
    let rows = stmt.query_map(
      params![
        entity.system.as_str(),
        entity.entity_type.as_str(),
        entity.entity_key.as_str(),
      ],
      |r| {
        Ok((
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, String>(2)?,
        ))
      },
    )?;
    let mut out = Vec::new();
    for row in rows {
      let (uid, signal, evidence) = row?;
      out.push(crate::Suggestion {
        entity: entity.clone(),
        person_uid: PersonUid::new(uid),
        signal,
        evidence: serde_json::from_str(&evidence)?,
      });
    }
    Ok(out)
  }
}

// --- search -----------------------------------------------------------

/// A hit on the Users screen's search box (SPEC.md section 5: searchable
/// by name, email, or key).
#[derive(Debug, Clone)]
pub struct SubjectHit {
  pub subject:      SubjectRef,
  pub display_name: Option<String>,
  pub detail:       String,
}

impl Reader<'_> {
  /// Entities and confirmed persons matching a substring, case-folded.
  ///
  /// Entities are matched on their key and display name, which covers
  /// "email" without a separate column: for every connector so far the
  /// stable key *is* the email.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn search_subjects(
    &self,
    query: &str,
    limit: usize,
  ) -> Result<Vec<SubjectHit>> {
    let needle = format!("%{}%", query.to_lowercase());
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut out = Vec::new();

    let mut stmt = self.conn().prepare(
      "SELECT person_uid, display_name FROM person
        WHERE lower(coalesce(display_name, '')) LIKE ?1
           OR lower(person_uid) LIKE ?1
        ORDER BY display_name, person_uid LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![&needle, limit], |r| {
      Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
      let (uid, name) = row?;
      out.push(SubjectHit {
        subject:      SubjectRef::Person(PersonUid::new(uid)),
        display_name: name,
        detail:       "person".to_owned(),
      });
    }

    let mut stmt = self.conn().prepare(
      "SELECT system, entity_type, entity_key, normalized, present
         FROM entity
        WHERE lower(entity_key) LIKE ?1
           OR lower(coalesce(json_extract(normalized, '$.display_name'), ''))
              LIKE ?1
        ORDER BY present DESC, system, entity_key LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![&needle, limit], |r| {
      Ok((
        EntityRef::new(
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, String>(2)?,
        ),
        r.get::<_, Option<String>>(3)?,
        r.get::<_, i64>(4)? != 0,
      ))
    })?;
    for row in rows {
      let (entity, normalized, present) = row?;
      let name = normalized
        .map(|n| serde_json::from_str::<NormalizedRecord>(&n))
        .transpose()?
        .and_then(|n| n.display_name);
      let detail = if present {
        entity.system.to_string()
      } else {
        format!("{} (absent)", entity.system)
      };
      out.push(SubjectHit {
        subject: SubjectRef::Entity(entity),
        display_name: name,
        detail,
      });
    }

    Ok(out)
  }
}

// --- checks -----------------------------------------------------------

/// One saved revision of a check, for the editor's history panel.
#[derive(Debug, Clone)]
pub struct CheckRevisionRow {
  pub revision:   Revision,
  pub draft:      CheckDraft,
  pub at:         Timestamp,
  pub actor:      Actor,
  /// Whether this revision has a dry-run, which is what gates enabling
  /// it (SPEC.md section 7).
  pub dryrun:     Option<DryRunRow>,
  pub is_current: bool,
  /// The revision the check is enabled at, if it is enabled. A check can
  /// be enabled at an older revision than its current one.
  pub is_enabled: bool,
}

/// A recorded dry-run.
#[derive(Debug, Clone)]
pub struct DryRunRow {
  pub at:          Timestamp,
  pub match_count: i64,
  pub samples:     Vec<DryrunSample>,
}

impl Reader<'_> {
  /// Every revision of one check, newest first.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn check_revisions(
    &self,
    check: &CheckId,
  ) -> Result<Vec<CheckRevisionRow>> {
    let head: Option<(u32, i64, Option<u32>)> = self
      .conn()
      .query_row(
        "SELECT revision, enabled, enabled_rev FROM check_head
          WHERE check_id = ?1",
        [check.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
      )
      .optional()?;
    let (current, enabled, enabled_rev) =
      head.map_or((None, false, None), |(c, e, er)| (Some(c), e != 0, er));

    let mut stmt = self.conn().prepare(
      "SELECT revision, draft, at, actor FROM check_revision
        WHERE check_id = ?1 ORDER BY revision DESC",
    )?;
    let rows = stmt.query_map([check.as_str()], |r| {
      Ok((
        r.get::<_, u32>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
        r.get::<_, String>(3)?,
      ))
    })?;

    let mut out = Vec::new();
    for row in rows {
      let (revision, draft, at, actor) = row?;
      out.push(CheckRevisionRow {
        revision:   Revision(revision),
        draft:      serde_json::from_str(&draft)?,
        at:         at.parse()?,
        actor:      Actor::new(actor),
        dryrun:     self.dryrun(check, Revision(revision))?,
        is_current: current == Some(revision),
        is_enabled: enabled && enabled_rev == Some(revision),
      });
    }
    Ok(out)
  }

  /// The dry-run recorded for one `(check, revision)`, if there is one.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn dryrun(
    &self,
    check: &CheckId,
    revision: Revision,
  ) -> Result<Option<DryRunRow>> {
    let row: Option<(String, i64, String)> = self
      .conn()
      .query_row(
        "SELECT at, match_count, samples FROM check_dryrun
          WHERE check_id = ?1 AND revision = ?2",
        params![check.as_str(), revision.0],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
      )
      .optional()?;
    let Some((at, match_count, samples)) = row else {
      return Ok(None);
    };
    Ok(Some(DryRunRow {
      at: at.parse()?,
      match_count,
      samples: serde_json::from_str(&samples)?,
    }))
  }

  /// Normalization rulesets as the Settings screen lists them (SPEC.md
  /// section 11: normalization is versioned and operator-visible).
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn normalization_rulesets(
    &self,
  ) -> Result<Vec<(String, String, SystemKind)>> {
    let mut stmt = self.conn().prepare(
      "SELECT ruleset_id, version, system_kind FROM normalization_ruleset
        ORDER BY ruleset_id, version",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
      ))
    })?;
    let mut out = Vec::new();
    for row in rows {
      let (id, version, kind) = row?;
      out.push((id, version, kind.parse()?));
    }
    Ok(out)
  }
}
