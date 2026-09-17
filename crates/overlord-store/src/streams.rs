use overlord_core::{
  CheckId, Completeness, EntityKey, EntityRef, EntityType, NormalizedRecord,
  Revision, SweepId, SystemId, SystemKind, Timestamp,
};
use rusqlite::{OptionalExtension, params};

use crate::{
  db::{Writer, put_payload},
  error::{Result, StoreError},
};

/// What a sweep pins when it starts (SPEC.md section 10).
///
/// Later edits do not affect a run in progress or its replay, which is
/// the whole reason these are recorded rather than looked up.
#[derive(Debug, Clone)]
pub struct SweepStart {
  /// The single definition of "now" for the entire run.
  pub started_at:        Timestamp,
  pub requested:         Vec<SystemId>,
  pub pinned_checks:     Vec<(CheckId, Revision)>,
  pub pinned_norm:       Vec<(String, String)>,
  pub absence_guard_pct: u32,
}

/// An observation about to be appended.
#[derive(Debug, Clone)]
pub struct NewFact {
  pub system:       SystemId,
  pub entity_type:  EntityType,
  pub entity_key:   EntityKey,
  pub observed_at:  Timestamp,
  /// The vendor payload as received. `None` for a tombstone.
  pub raw:          Option<serde_json::Value>,
  /// The normalization overlay. `None` for a tombstone.
  pub normalized:   Option<NormalizedRecord>,
  pub norm_version: String,
}

impl NewFact {
  #[must_use]
  pub fn is_tombstone(&self) -> bool { self.normalized.is_none() }

  /// Record that an entity was absent from a complete snapshot.
  #[must_use]
  pub fn tombstone(entity: &EntityRef, observed_at: Timestamp) -> Self {
    Self {
      system: entity.system.clone(),
      entity_type: entity.entity_type.clone(),
      entity_key: entity.entity_key.clone(),
      observed_at,
      raw: None,
      normalized: None,
      norm_version: String::new(),
    }
  }
}

/// A fact as stored.
#[derive(Debug, Clone)]
pub struct FactRow {
  pub id:           i64,
  pub seq:          i64,
  pub sweep_id:     SweepId,
  pub entity:       EntityRef,
  pub observed_at:  Timestamp,
  pub present:      bool,
  pub raw_hash:     Option<String>,
  pub norm_hash:    Option<String>,
  pub norm_version: String,
}

/// What one connector did during one sweep, for the coverage view
/// (SPEC.md section 10).
#[derive(Debug, Clone)]
pub struct SystemOutcome {
  pub system:         SystemId,
  pub system_kind:    SystemKind,
  pub status:         SystemStatus,
  pub completeness:   Completeness,
  pub observed_count: usize,
  pub tombstoned:     usize,
  /// Entity count at the end of the previous sweep that covered this
  /// system; `None` when there was none.
  pub previous_count: Option<usize>,
  /// The absence guard refused to write tombstones for this system.
  pub guard_tripped:  bool,
  pub duration_ms:    u64,
  pub error:          Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemStatus {
  Ok,
  Partial,
  Failed,
  Skipped,
}

impl SystemStatus {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Ok => "ok",
      Self::Partial => "partial",
      Self::Failed => "failed",
      Self::Skipped => "skipped",
    }
  }
}

/// The column stores `as_str`, so reading a row back needs the inverse.
/// A value outside the set means the file was written by a different
/// version or edited by hand, which replay must refuse rather than
/// silently coerce.
impl std::str::FromStr for SystemStatus {
  type Err = crate::error::StoreError;

  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
    match s {
      "ok" => Ok(Self::Ok),
      "partial" => Ok(Self::Partial),
      "failed" => Ok(Self::Failed),
      "skipped" => Ok(Self::Skipped),
      other => Err(crate::error::StoreError::not_found(format!(
        "system status {other}"
      ))),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepStatus {
  Running,
  Ok,
  Partial,
  Failed,
}

impl SweepStatus {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Running => "running",
      Self::Ok => "ok",
      Self::Partial => "partial",
      Self::Failed => "failed",
    }
  }
}

/// As [`SystemStatus`]: the inverse of what the column stores.
impl std::str::FromStr for SweepStatus {
  type Err = crate::error::StoreError;

  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
    match s {
      "running" => Ok(Self::Running),
      "ok" => Ok(Self::Ok),
      "partial" => Ok(Self::Partial),
      "failed" => Ok(Self::Failed),
      other => Err(crate::error::StoreError::not_found(format!(
        "sweep status {other}"
      ))),
    }
  }
}

/// A sweep's run metadata (SPEC.md section 6.3).
#[derive(Debug, Clone)]
pub struct SweepSummary {
  pub id:            SweepId,
  pub started_at:    Timestamp,
  pub finished_at:   Option<Timestamp>,
  pub status:        SweepStatus,
  pub requested:     Vec<SystemId>,
  pub pinned_checks: Vec<(CheckId, Revision)>,
  pub systems:       Vec<SystemOutcome>,
}

impl Writer<'_> {
  /// Open a sweep, taking its place in the stream sequence.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn open_sweep(&self, start: &SweepStart) -> Result<SweepId> {
    let seq = self.take_seq(1)?;
    let pinned_checks: Vec<(String, u32)> = start
      .pinned_checks
      .iter()
      .map(|(c, r)| (c.to_string(), r.0))
      .collect();

    self.conn().execute(
      "INSERT INTO sweep (
         opened_seq, started_at, status, requested, pinned_checks,
         pinned_norm, absence_guard_pct)
       VALUES (?1, ?2, 'running', ?3, ?4, ?5, ?6)",
      params![
        seq,
        start.started_at.to_string(),
        serde_json::to_string(&start.requested)?,
        serde_json::to_string(&pinned_checks)?,
        serde_json::to_string(&start.pinned_norm)?,
        start.absence_guard_pct,
      ],
    )?;
    Ok(SweepId(self.conn().last_insert_rowid()))
  }

  /// Append observations and advance the entity projection.
  ///
  /// Facts are never updated or deleted; "current state" is the latest
  /// fact by `(sweep_id, id)`, which is exactly what the projection
  /// caches.
  ///
  /// # Errors
  /// On a SQLite failure or unserializable payload.
  pub fn append_facts(
    &self,
    sweep: SweepId,
    facts: &[NewFact],
  ) -> Result<Vec<i64>> {
    if facts.is_empty() {
      return Ok(Vec::new());
    }
    // One contiguous block of sequence positions for the whole batch,
    // so a sweep of thousands of entities takes one allocation and the
    // facts land in the order the connector reported them.
    let first_seq = self.take_seq(facts.len())?;
    let mut ids = Vec::with_capacity(facts.len());

    for (seq, f) in (first_seq..).zip(facts) {
      let raw_hash = match &f.raw {
        Some(v) => Some(put_payload(self.conn(), &serde_json::to_string(v)?)?),
        None => None,
      };
      let norm_hash = match &f.normalized {
        Some(n) => Some(put_payload(self.conn(), &serde_json::to_string(n)?)?),
        None => None,
      };
      let present = i32::from(!f.is_tombstone());

      self.conn().execute(
        "INSERT INTO fact (
           seq, sweep_id, system, entity_type, entity_key, observed_at,
           present, raw_hash, norm_hash, norm_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
          seq,
          sweep.0,
          f.system.as_str(),
          f.entity_type.as_str(),
          f.entity_key.as_str(),
          f.observed_at.to_string(),
          present,
          raw_hash.as_deref(),
          norm_hash.as_deref(),
          &f.norm_version,
        ],
      )?;
      let id = self.conn().last_insert_rowid();
      ids.push(id);

      let normalized = match &f.normalized {
        Some(n) => Some(serde_json::to_string(n)?),
        None => None,
      };

      // A tombstone clears the cached overlay rather than keeping a
      // stale one: an absent entity is excluded from evaluation
      // (SPEC.md section 6.1), and its last known state stays readable
      // in the fact stream where it is unambiguously historical.
      self.conn().execute(
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
          f.system.as_str(),
          f.entity_type.as_str(),
          f.entity_key.as_str(),
          present,
          id,
          normalized,
          raw_hash.as_deref(),
          sweep.0,
        ],
      )?;
    }
    Ok(ids)
  }

  /// Record what one connector reported.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn record_system(
    &self,
    sweep: SweepId,
    outcome: &SystemOutcome,
  ) -> Result<()> {
    self.conn().execute(
      "INSERT INTO sweep_system (
         sweep_id, system, system_kind, status, complete, observed_count,
         tombstoned, previous_count, guard_tripped, duration_ms, error)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
       ON CONFLICT (sweep_id, system) DO UPDATE SET
         status = excluded.status,
         complete = excluded.complete,
         observed_count = excluded.observed_count,
         tombstoned = excluded.tombstoned,
         previous_count = excluded.previous_count,
         guard_tripped = excluded.guard_tripped,
         duration_ms = excluded.duration_ms,
         error = excluded.error",
      params![
        sweep.0,
        outcome.system.as_str(),
        outcome.system_kind.as_str(),
        outcome.status.as_str(),
        i32::from(outcome.completeness.is_complete()),
        i64::try_from(outcome.observed_count).unwrap_or(i64::MAX),
        i64::try_from(outcome.tombstoned).unwrap_or(i64::MAX),
        outcome
          .previous_count
          .map(|c| i64::try_from(c).unwrap_or(i64::MAX)),
        i32::from(outcome.guard_tripped),
        i64::try_from(outcome.duration_ms).unwrap_or(i64::MAX),
        outcome.error.as_deref(),
      ],
    )?;
    Ok(())
  }

  /// Close a sweep, taking its commit position on the sequence.
  ///
  /// Evaluation replays at this point, which is why the position is
  /// recorded rather than inferred.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn commit_sweep(
    &self,
    sweep: SweepId,
    status: SweepStatus,
    finished_at: Timestamp,
  ) -> Result<()> {
    let seq = self.take_seq(1)?;
    let n = self.conn().execute(
      "UPDATE sweep
         SET committed_seq = ?2, status = ?3, finished_at = ?4
       WHERE id = ?1 AND committed_seq IS NULL",
      params![sweep.0, seq, status.as_str(), finished_at.to_string()],
    )?;
    if n == 0 {
      return Err(StoreError::rejected(format!(
        "sweep {sweep} is not running"
      )));
    }
    Ok(())
  }
}

impl crate::db::Reader<'_> {
  /// The number of present entities in a system, for the coverage
  /// delta that makes a silent connector visible.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn present_entity_count(&self, system: &SystemId) -> Result<usize> {
    let n: i64 = self.conn().query_row(
      "SELECT count(*) FROM entity WHERE system = ?1 AND present = 1",
      [system.as_str()],
      |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
  }

  /// Every present entity in a system, as refs. The sweep engine needs
  /// these to work out what a complete snapshot left out.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn present_entities(&self, system: &SystemId) -> Result<Vec<EntityRef>> {
    let mut stmt = self.conn().prepare(
      "SELECT system, entity_type, entity_key FROM entity
       WHERE system = ?1 AND present = 1",
    )?;
    let rows = stmt.query_map([system.as_str()], |r| {
      Ok(EntityRef::new(
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
      ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
  }

  /// The most recent sweep that covered `system` and committed.
  ///
  /// "New since last sweep" compares per system, so restricting a sweep
  /// does not manufacture change (SPEC.md section 10).
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn last_sweep_covering(
    &self,
    system: &SystemId,
    before: SweepId,
  ) -> Result<Option<SweepId>> {
    let id: Option<i64> = self
      .conn()
      .query_row(
        "SELECT s.id FROM sweep s
           JOIN sweep_system ss ON ss.sweep_id = s.id
          WHERE ss.system = ?1 AND s.id < ?2
            AND s.committed_seq IS NOT NULL
            AND ss.status IN ('ok', 'partial')
          ORDER BY s.id DESC LIMIT 1",
        params![system.as_str(), before.0],
        |r| r.get(0),
      )
      .optional()?;
    Ok(id.map(SweepId))
  }
}
