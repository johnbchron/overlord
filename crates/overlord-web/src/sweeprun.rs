//! Running a sweep from the UI (PLAN.md section 4, item 17).
//!
//! A sweep can take as long as the slowest vendor API, so the request
//! that starts one returns immediately and the work continues on a tokio
//! task. The **sweep row is the progress record** — it is written the
//! moment the run opens and updated per system — so the page polls the
//! store rather than any in-process channel, and a browser refresh or a
//! second operator sees the same thing.
//!
//! The only state held here is what the store cannot answer: whether a
//! task is in flight before it has opened its sweep row, and why the
//! last one died if it died before recording anything.

use std::sync::{Arc, Mutex};

use overlord_connect::Registry;
use overlord_core::{Actor, SystemId, Timestamp};
use overlord_engine::{SweepPlan, SystemConfig, run_sweep};
use overlord_store::Db;

use crate::error::WebError;

/// What the Sweeps screen is told about the run in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
  Idle,
  /// A task is running. The sweep row carries the detail.
  Running,
  /// The last run failed before, or instead of, recording itself.
  Failed(String),
}

/// Starts sweeps and remembers whether one is in flight.
pub struct SweepRunner {
  db:                Arc<Db>,
  registry:          Arc<Registry>,
  systems:           Vec<SystemConfig>,
  absence_guard_pct: u32,
  state:             Mutex<RunState>,
}

impl SweepRunner {
  #[must_use]
  pub fn new(
    db: Arc<Db>,
    registry: Arc<Registry>,
    systems: Vec<SystemConfig>,
    absence_guard_pct: u32,
  ) -> Arc<Self> {
    Arc::new(Self {
      db,
      registry,
      systems,
      absence_guard_pct,
      state: Mutex::new(RunState::Idle),
    })
  }

  #[must_use]
  pub fn state(&self) -> RunState { self.lock().clone() }

  #[must_use]
  pub fn configured(&self) -> &[SystemConfig] { &self.systems }

  /// Start a sweep, returning once it has been handed to a task.
  ///
  /// # Errors
  /// [`WebError::Refused`] if a sweep is already running, or if nothing
  /// is configured to sweep. Two concurrent sweeps would interleave
  /// their facts under two different definitions of "now" (SPEC.md
  /// section 10), so the second is refused rather than queued — the
  /// operator can see the first finish and decide.
  pub fn start(
    self: &Arc<Self>,
    actor: &Actor,
    only: &[SystemId],
  ) -> Result<(), WebError> {
    if self.systems.is_empty() {
      return Err(WebError::refused(
        "no systems are configured; add a [[systems]] entry to the \
         configuration file",
      ));
    }

    let plan = {
      let mut state = self.lock();
      if *state == RunState::Running {
        return Err(WebError::refused("a sweep is already running"));
      }

      let mut plan = SweepPlan::new(self.systems.clone()).only(only);
      if plan.systems.is_empty() {
        return Err(WebError::refused(
          "none of the named systems are configured",
        ));
      }
      plan.absence_guard_pct = self.absence_guard_pct;
      plan.actor = actor.clone();
      // `SweepPlan::new` already stamped the clock. Restamping here
      // keeps "now" as close as possible to the work actually starting.
      plan.started_at = Timestamp::now();

      *state = RunState::Running;
      plan
    };

    let this = Arc::clone(self);
    tokio::spawn(async move {
      let outcome = run_sweep(&this.db, &this.registry, &plan).await;
      let mut state = this.lock();
      *state = match outcome {
        Ok(o) => {
          tracing::info!(
            sweep = %o.sweep,
            status = o.status.as_str(),
            "sweep finished"
          );
          RunState::Idle
        }
        Err(e) => {
          // A connector that fails is recorded on its own row and does
          // not reach here (SPEC.md section 10). Reaching here means the
          // run itself could not proceed — an unknown connector, or the
          // store refusing a write — which the sweep row may never show.
          tracing::error!(error = %e, "sweep failed");
          RunState::Failed(e.to_string())
        }
      };
    });

    Ok(())
  }

  /// Clear a recorded failure once the operator has seen it.
  pub fn acknowledge_failure(&self) {
    let mut state = self.lock();
    if matches!(*state, RunState::Failed(_)) {
      *state = RunState::Idle;
    }
  }

  /// A poisoned lock means a previous holder panicked while holding it.
  /// The value is a three-variant enum with no invariant spanning the
  /// lock, so recovering it is strictly better than refusing to serve
  /// the Sweeps page for the life of the process.
  fn lock(&self) -> std::sync::MutexGuard<'_, RunState> {
    self.state.lock().unwrap_or_else(|e| e.into_inner())
  }
}
