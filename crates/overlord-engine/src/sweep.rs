//! Running a sweep (SPEC.md section 10).
//!
//! A sweep is an explicit operator action. It records `started_at` —
//! the single definition of "now" for the entire run — pins the check
//! revisions and normalization versions it will use, runs each
//! connector's read-only `observe`, appends facts, and evaluates.

use std::{collections::BTreeSet, sync::Arc};

use overlord_connect::{
  Connector, ObserveCtx, Progress, ProgressEvent, Registry, Ruleset,
};
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

/// A step in a sweep, reported as it happens.
///
/// A sweep can run longer than an operator's patience, so the engine
/// narrates its phases rather than only returning at the end. A caller
/// that does not care passes a no-op sink via [`run_sweep`]; the web
/// runner folds these into the Sweeps screen.
#[derive(Debug, Clone)]
pub enum SweepProgress {
  /// The sweep row is open, so the run has an identity to report under.
  Opened {
    sweep:   overlord_core::SweepId,
    systems: usize,
  },
  /// Collection of one system is beginning. `index` is one-based.
  SystemStarted {
    system: SystemId,
    index:  usize,
    total:  usize,
  },
  /// One system finished and its coverage row is recorded.
  SystemFinished {
    system: SystemId,
    index:  usize,
    total:  usize,
  },
  /// A step reported from inside one connector's read — a page fetched,
  /// a group walked. `done`/`total` are present when the connector can
  /// count the work.
  Detail {
    system: SystemId,
    note:   String,
    done:   Option<u64>,
    total:  Option<u64>,
  },
  /// Every system is in; evaluation is running.
  Evaluating { systems: usize },
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
  run_sweep_with_progress(db, registry, plan, |_| {}).await
}

/// Run a sweep, reporting each phase to `progress` as it happens.
///
/// Each system's coverage row is written the moment that system
/// finishes, rather than once the whole run does, so a screen that polls
/// the store sees the table fill in. The callback is for what the store
/// cannot yet know: which system is in flight, and that evaluation has
/// begun.
///
/// # Errors
/// As [`run_sweep`].
pub async fn run_sweep_with_progress<F>(
  db: &Db,
  registry: &Registry,
  plan: &SweepPlan,
  progress: F,
) -> Result<SweepOutcome>
where
  F: Fn(SweepProgress) + Send + Sync + 'static,
{
  let started_at = plan.started_at;
  // Shared so each system can be handed a `Progress` that forwards to
  // it; the sink inside `Progress` must be `'static` and `Sync`, which
  // is why the bound is tightened here.
  let progress = Arc::new(progress);

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
      progress: system_progress(&progress, &sys.id),
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
  progress(SweepProgress::Opened {
    sweep,
    systems: prepared.len(),
  });

  let total = prepared.len();
  let mut outcomes = Vec::new();
  let mut warnings = Vec::new();

  for (index, (sys, ctx, ruleset)) in prepared.iter().enumerate() {
    progress(SweepProgress::SystemStarted {
      system: sys.id.clone(),
      index: index + 1,
      total,
    });
    let connector = registry
      .get(&sys.connector)
      .ok_or_else(|| EngineError::UnknownConnector(sys.connector.clone()))?;
    let (outcome, mut system_warnings) =
      sweep_one(db, connector, ruleset, sys, ctx, sweep, plan).await?;
    // Record this system before moving to the next, so the coverage
    // table is a live record of the run rather than a postmortem.
    db.write(|w| w.record_system(sweep, &outcome))?;
    progress(SweepProgress::SystemFinished {
      system: outcome.system.clone(),
      index: index + 1,
      total,
    });
    warnings.append(&mut system_warnings);
    outcomes.push(outcome);
  }

  progress(SweepProgress::Evaluating {
    systems: outcomes.len(),
  });
  let status = overall_status(&outcomes);
  let evaluation = db.write(|w| -> Result<_> {
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

/// A per-system [`Progress`] that forwards each connector report to the
/// run's sink, tagged with the system it came from.
fn system_progress<F>(progress: &Arc<F>, system: &SystemId) -> Progress
where
  F: Fn(SweepProgress) + Send + Sync + 'static,
{
  let progress = Arc::clone(progress);
  let system = system.clone();
  Progress::new(Arc::new(move |event: ProgressEvent| {
    progress(SweepProgress::Detail {
      system: system.clone(),
      note:   event.note,
      done:   event.done,
      total:  event.total,
    });
  }))
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
    connector:      connector.name().to_owned(),
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
      connector: connector.name().to_owned(),
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

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use super::*;

  #[test]
  fn a_connector_report_arrives_tagged_with_its_system() {
    let seen: Arc<Mutex<Vec<SweepProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let run = Arc::new(move |p: SweepProgress| sink.lock().unwrap().push(p));

    let progress = system_progress(&run, &SystemId::new("access-hq"));
    progress.counted("group 2 of 9", 2, Some(9));

    let seen = seen.lock().unwrap();
    match &seen[0] {
      SweepProgress::Detail {
        system,
        note,
        done,
        total,
      } => {
        assert_eq!(system.as_str(), "access-hq");
        assert_eq!(note, "group 2 of 9");
        assert_eq!(*done, Some(2));
        assert_eq!(*total, Some(9));
      }
      other => panic!("expected a detail event, got {other:?}"),
    }
  }

  #[test]
  fn an_inert_progress_sends_nothing() {
    let seen: Arc<Mutex<Vec<SweepProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let run = Arc::new(move |p: SweepProgress| sink.lock().unwrap().push(p));

    // The default `Progress` has no sink; reporting through it must not
    // reach the run.
    Progress::default().say("nobody is listening");
    assert!(seen.lock().unwrap().is_empty());
    // And the run's own sink still works, proving the closure is live.
    run(SweepProgress::Evaluating { systems: 0 });
    assert_eq!(seen.lock().unwrap().len(), 1);
  }
}
