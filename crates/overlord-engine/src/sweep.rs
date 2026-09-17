//! Running a sweep (SPEC.md section 10).
//!
//! A sweep is an explicit operator action. It records `started_at` —
//! the single definition of "now" for the entire run — pins the check
//! revisions and normalization versions it will use, runs each
//! connector's read-only `observe`, appends facts, and evaluates.

use std::collections::BTreeSet;

use overlord_connect::{Connector, ObserveCtx, Registry, Ruleset};
use overlord_core::{
  Actor, Completeness, EntityKey, EntityRef, EntityType, SystemId, SystemKind,
  Timestamp,
};
use overlord_store::{
  Db, NewFact, SweepStart, SweepStatus, SystemOutcome, SystemStatus,
};
use tracing::{info, warn};

use crate::{
  error::{EngineError, Result},
  evaluate::{EvalReport, evaluate_sweep},
};

/// One configured system.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SystemConfig {
  pub id:        SystemId,
  /// Which connector implementation to use.
  pub connector: String,
  /// Connector-specific settings. Credentials never live here; they
  /// come from the environment (SPEC.md section 14).
  #[serde(default)]
  pub config:    serde_json::Value,
}

/// What to sweep and how.
#[derive(Debug, Clone)]
pub struct SweepPlan {
  pub systems:           Vec<SystemConfig>,
  /// The single definition of "now" for the run (SPEC.md section 10).
  ///
  /// Taken here, at the edge, rather than read inside the sweep: it is
  /// the one clock read the whole run is allowed, so making it an input
  /// is what lets a test pin it and a replay reproduce it.
  pub started_at:        Timestamp,
  /// A snapshot that would tombstone more than this share of a system's
  /// entities is refused (SPEC.md section 10).
  pub absence_guard_pct: u32,
  pub actor:             Actor,
}

impl SweepPlan {
  #[must_use]
  pub fn new(systems: Vec<SystemConfig>) -> Self {
    Self {
      systems,
      started_at: Timestamp::now(),
      absence_guard_pct: 10,
      actor: Actor::new("cli"),
    }
  }

  /// Pin the run to a specific instant.
  #[must_use]
  pub fn at(mut self, started_at: Timestamp) -> Self {
    self.started_at = started_at;
    self
  }

  /// Restrict to a subset of configured systems. A partial sweep is a
  /// first-class thing, not a degraded one.
  #[must_use]
  pub fn only(mut self, ids: &[SystemId]) -> Self {
    if !ids.is_empty() {
      self.systems.retain(|s| ids.contains(&s.id));
    }
    self
  }
}

/// The result of one run.
#[derive(Debug, Clone)]
pub struct SweepOutcome {
  pub sweep:      overlord_core::SweepId,
  pub status:     SweepStatus,
  pub systems:    Vec<SystemOutcome>,
  pub evaluation: EvalReport,
  /// Normalization problems worth showing on the coverage view.
  pub warnings:   Vec<String>,
}

/// Run a sweep end to end.
///
/// # Errors
/// On a store failure, or if a configured system names a connector this
/// binary does not have. A connector that *fails* is recorded as a
/// failed system, not raised: SPEC.md section 10 requires a sweep to be
/// transactional per system, so one unreachable vendor must not lose
/// another vendor's results.
pub async fn run_sweep(
  db: &Db,
  registry: &Registry,
  plan: &SweepPlan,
) -> Result<SweepOutcome> {
  let started_at = plan.started_at;

  // Pin what this run will use, before anything is read. An edit made
  // while the sweep runs lands in the stream but does not reach this
  // run — which is what makes the run replayable.
  let pinned_checks = db.read(|r| -> Result<_> {
    Ok(
      r.enabled_checks()?
        .into_iter()
        .map(|c| c.pinned())
        .collect::<Vec<_>>(),
    )
  })?;

  // The observation context is built once per system and reused, so a
  // connector's ruleset and its read see exactly the same inputs.
  let mut prepared = Vec::new();
  for sys in &plan.systems {
    let connector = registry
      .get(&sys.connector)
      .ok_or_else(|| EngineError::UnknownConnector(sys.connector.clone()))?;
    let ctx = ObserveCtx {
      system: sys.id.clone(),
      started_at,
      config: sys.config.clone(),
    };
    let ruleset = connector.default_ruleset(&ctx);
    prepared.push((sys, ctx, ruleset));
  }
  let pinned_norm: Vec<(String, String)> =
    prepared.iter().map(|(_, _, rs)| rs.pin()).collect();

  let sweep = db.write(|w| {
    w.open_sweep(&SweepStart {
      started_at,
      requested: plan.systems.iter().map(|s| s.id.clone()).collect(),
      pinned_checks,
      pinned_norm,
      absence_guard_pct: plan.absence_guard_pct,
    })
  })?;
  info!(
    sweep = sweep.0,
    systems = plan.systems.len(),
    "sweep opened"
  );

  let mut outcomes = Vec::new();
  let mut warnings = Vec::new();

  for (sys, ctx, ruleset) in &prepared {
    let connector = registry
      .get(&sys.connector)
      .ok_or_else(|| EngineError::UnknownConnector(sys.connector.clone()))?;
    let (outcome, mut system_warnings) =
      sweep_one(db, connector, ruleset, sys, ctx, sweep, plan).await?;
    warnings.append(&mut system_warnings);
    outcomes.push(outcome);
  }

  let status = overall_status(&outcomes);
  let evaluation = db.write(|w| -> Result<_> {
    for o in &outcomes {
      w.record_system(sweep, o)?;
    }
    w.commit_sweep(sweep, status, Timestamp::now())?;
    evaluate_sweep(w, sweep)
  })?;

  Ok(SweepOutcome {
    sweep,
    status,
    systems: outcomes,
    evaluation,
    warnings,
  })
}

fn overall_status(outcomes: &[SystemOutcome]) -> SweepStatus {
  if outcomes.is_empty() {
    return SweepStatus::Ok;
  }
  if outcomes.iter().all(|o| o.status == SystemStatus::Failed) {
    return SweepStatus::Failed;
  }
  if outcomes
    .iter()
    .any(|o| o.status != SystemStatus::Ok || o.guard_tripped)
  {
    return SweepStatus::Partial;
  }
  SweepStatus::Ok
}

/// Collect one system. Transactional on its own: a failure here records
/// a failed system and leaves every other system's facts intact.
async fn sweep_one(
  db: &Db,
  connector: &dyn Connector,
  ruleset: &Ruleset,
  sys: &SystemConfig,
  ctx: &ObserveCtx,
  sweep: overlord_core::SweepId,
  plan: &SweepPlan,
) -> Result<(SystemOutcome, Vec<String>)> {
  let began = Timestamp::now();
  let started_at = ctx.started_at;

  let previous_count = db.read(|r| r.present_entity_count(&sys.id))?;
  let elapsed = |t: Timestamp| {
    u64::try_from((t.as_jiff() - began.as_jiff()).get_milliseconds().max(0))
      .unwrap_or(0)
  };

  let failed = |err: &dyn std::fmt::Display, at: Timestamp| SystemOutcome {
    system:         sys.id.clone(),
    system_kind:    ruleset.system_kind,
    status:         SystemStatus::Failed,
    completeness:   Completeness::Partial {
      reason: err.to_string(),
    },
    observed_count: 0,
    tombstoned:     0,
    previous_count: Some(previous_count),
    guard_tripped:  false,
    duration_ms:    elapsed(at),
    error:          Some(err.to_string()),
  };

  let http = match connector.http(ctx) {
    Ok(h) => h,
    Err(e) => {
      warn!(system = %sys.id, error = %e, "connector could not start");
      return Ok((failed(&e, Timestamp::now()), Vec::new()));
    }
  };

  let snapshot = match connector.observe(&http, ctx).await {
    Ok(s) => s,
    Err(e) => {
      warn!(system = %sys.id, error = %e, "connector failed");
      return Ok((failed(&e, Timestamp::now()), Vec::new()));
    }
  };

  let mut warnings = snapshot.warnings.clone();
  let mut facts = Vec::with_capacity(snapshot.observations.len());
  let mut observed: BTreeSet<EntityRef> = BTreeSet::new();

  for obs in &snapshot.observations {
    match ruleset.apply(&sys.id, &obs.raw) {
      Ok(normalized) => {
        for w in normalized.warnings {
          warnings.push(format!("{}: {w}", sys.id));
        }
        let record = normalized.record;
        observed.insert(record.entity_ref());
        facts.push(NewFact {
          system:       sys.id.clone(),
          entity_type:  record.entity_type.clone(),
          entity_key:   record.entity_key.clone(),
          observed_at:  started_at,
          raw:          Some(obs.raw.clone()),
          normalized:   Some(record),
          norm_version: ruleset.version.clone(),
        });
      }
      Err(e) => warnings.push(format!("{}: {e}", sys.id)),
    }
  }

  // Tombstones, and the guard that governs them.
  let known: Vec<EntityRef> = db.read(|r| r.present_entities(&sys.id))?;
  let missing: Vec<EntityRef> = known
    .into_iter()
    .filter(|e| !observed.contains(e))
    .collect();

  let mut guard_tripped = false;
  let mut tombstones: Vec<EntityRef> = Vec::new();

  if snapshot.completeness.is_complete() {
    if previous_count > 0 && !missing.is_empty() {
      let share = (missing.len() * 100) / previous_count;
      if share > plan.absence_guard_pct as usize {
        // SPEC.md section 10: a truncated snapshot must never present
        // as a mass deprovisioning. Nothing is tombstoned, the anomaly
        // is recorded, and the system is flagged for confirmation.
        guard_tripped = true;
        warnings.push(format!(
          "{}: {} of {previous_count} entities were absent ({share}%, guard \
           is {}%); no tombstones written, confirm the snapshot",
          sys.id,
          missing.len(),
          plan.absence_guard_pct
        ));
      }
    }
    if !guard_tripped {
      tombstones = missing;
    }
  } else if !missing.is_empty() {
    // A partial snapshot may never produce tombstones, whatever it
    // omits — the omission is the connector's, not the vendor's.
    warnings.push(format!(
      "{}: snapshot is partial, so {} absent entities were left alone",
      sys.id,
      missing.len()
    ));
  }

  let tombstoned = tombstones.len();
  for entity in tombstones {
    facts.push(NewFact::tombstone(&entity, started_at));
  }

  let observed_count = observed.len();
  db.write(|w| w.append_facts(sweep, &facts))?;

  let status = if guard_tripped || !snapshot.completeness.is_complete() {
    SystemStatus::Partial
  } else {
    SystemStatus::Ok
  };

  Ok((
    SystemOutcome {
      system: sys.id.clone(),
      system_kind: ruleset.system_kind,
      status,
      completeness: snapshot.completeness,
      observed_count,
      tombstoned,
      previous_count: Some(previous_count),
      guard_tripped,
      duration_ms: elapsed(Timestamp::now()),
      error: None,
    },
    warnings,
  ))
}

/// Unused today, but the shape the Systems screen will want: a
/// connector's declared scopes, rendered.
#[must_use]
pub fn describe_allowlist(connector: &dyn Connector) -> Vec<String> {
  connector
    .allowlist()
    .iter()
    .map(|a| format!("{a} — {}", a.reason))
    .collect()
}

/// Re-exported so callers need not depend on the connect crate directly
/// to spell an entity.
pub type Ref = (SystemId, EntityType, EntityKey, SystemKind);
