//! Running a sweep from the UI (PLAN.md section 4, item 17).
//!
//! A sweep can take as long as the slowest vendor API, so the request
//! that starts one returns immediately and the work continues on a tokio
//! task. The **sweep row is the progress record** — it is written the
//! moment the run opens and updated per system as each one finishes — so
//! the page polls the store rather than any in-process channel, and a
//! browser refresh or a second operator sees the same thing.
//!
//! What the store cannot answer is held here: which system is in flight
//! before its coverage row exists, how many are done, and whether
//! evaluation has begun. Those come from the engine's progress sink and
//! are shared across connections, so every viewer watches the same run.

use std::sync::{Arc, Mutex};

use overlord_connect::Registry;
use overlord_core::{Actor, SystemId, Timestamp};
use overlord_engine::{
  SweepPlan, SweepProgress, SystemConfig, run_sweep_with_progress,
};
use overlord_store::Db;

use crate::error::WebError;

/// Where a run has got to, as far as the engine has told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunProgress {
  /// The sweep row this run is writing to, once it has opened one.
  pub sweep:      Option<i64>,
  /// How many systems the run intends to collect.
  pub total:      usize,
  /// How many have been recorded.
  pub finished:   usize,
  /// The system being collected now, if one is in flight.
  pub current:    Option<String>,
  /// Every system is in and evaluation is running.
  pub evaluating: bool,
  /// The most recent notes from inside the system being read, oldest
  /// first. Bounded: a screen wants the tail, not the transcript.
  pub notes:      Vec<String>,
}

/// How many detail lines to keep for the screen.
const NOTE_BACKLOG: usize = 12;

impl RunProgress {
  fn new(total: usize) -> Self {
    Self {
      sweep: None,
      total,
      finished: 0,
      current: None,
      evaluating: false,
      notes: Vec::new(),
    }
  }

  /// Collecting progress as a whole percentage, for the bar.
  #[must_use]
  pub fn percent(&self) -> usize {
    (self.finished * 100)
      .checked_div(self.total)
      .unwrap_or(0)
      .min(100)
  }
}

/// What the Sweeps screen is told about the run in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
  Idle,
  /// A task is running. Its detail is in `RunProgress`.
  Running(RunProgress),
  /// The last run failed before, or instead of, recording itself.
  Failed(String),
}

impl RunState {
  /// Whether a task is still working, which is what the page polls on.
  #[must_use]
  pub fn running(&self) -> bool { matches!(self, Self::Running(_)) }

  /// The in-flight detail, when there is a run to describe.
  #[must_use]
  pub fn progress(&self) -> Option<&RunProgress> {
    match self {
      Self::Running(p) => Some(p),
      _ => None,
    }
  }
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

  /// The connectors this binary knows about, so a screen can show what
  /// each configured system is permitted to reach (SPEC.md section 11
  /// asks for an allowlist reviewable in one place).
  #[must_use]
  pub fn registry(&self) -> &Registry { &self.registry }

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
      if state.running() {
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

      *state = RunState::Running(RunProgress::new(plan.systems.len()));
      plan
    };

    let this = Arc::clone(self);
    tokio::spawn(async move {
      let observer = Arc::clone(&this);
      let outcome = run_sweep_with_progress(
        &this.db,
        &this.registry,
        &plan,
        move |progress| observer.observe(progress),
      )
      .await;
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

  /// Fold one engine progress event into the in-flight state.
  ///
  /// A late event after the run has ended (or a second run has started)
  /// is ignored: the state it would describe is no longer being shown.
  fn observe(&self, progress: SweepProgress) {
    let mut state = self.lock();
    let RunState::Running(run) = &mut *state else {
      return;
    };
    match progress {
      SweepProgress::Opened { sweep, systems } => {
        run.sweep = Some(sweep.0);
        run.total = systems;
      }
      SweepProgress::SystemStarted {
        system,
        index,
        total,
      } => {
        run.total = total;
        run.finished = index.saturating_sub(1);
        run.current = Some(system.to_string());
        run.notes.clear();
      }
      SweepProgress::SystemFinished { index, total, .. } => {
        run.total = total;
        run.finished = index;
        run.current = None;
        run.notes.clear();
      }
      SweepProgress::Detail {
        note, done, total, ..
      } => {
        run.notes.push(detail_line(&note, done, total));
        if run.notes.len() > NOTE_BACKLOG {
          run.notes.remove(0);
        }
      }
      SweepProgress::Evaluating { systems } => {
        run.total = systems;
        run.finished = systems;
        run.current = None;
        run.evaluating = true;
        run.notes.clear();
      }
    }
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

/// One detail line, with its counts when the connector gave any.
fn detail_line(note: &str, done: Option<u64>, total: Option<u64>) -> String {
  match (done, total) {
    (Some(done), Some(total)) => format!("{note} ({done}/{total})"),
    (Some(done), None) => format!("{note} ({done})"),
    _ => note.to_owned(),
  }
}

#[cfg(test)]
mod tests {
  use overlord_core::SweepId;

  use super::*;

  fn runner() -> Arc<SweepRunner> {
    SweepRunner::new(
      Arc::new(Db::open_memory().unwrap()),
      Arc::new(Registry::new()),
      Vec::new(),
      10,
    )
  }

  fn running(runner: &SweepRunner, total: usize) {
    *runner.lock() = RunState::Running(RunProgress::new(total));
  }

  #[test]
  fn progress_tracks_the_system_in_flight_and_the_bar() {
    let runner = runner();
    running(&runner, 2);

    runner.observe(SweepProgress::Opened {
      sweep:   SweepId(7),
      systems: 2,
    });
    let run = runner.state();
    let p = run.progress().unwrap();
    assert_eq!(p.sweep, Some(7));
    assert_eq!(p.percent(), 0, "nothing recorded yet");

    runner.observe(SweepProgress::SystemStarted {
      system: SystemId::new("ws"),
      index:  1,
      total:  2,
    });
    let run = runner.state();
    let p = run.progress().unwrap();
    assert_eq!(p.current.as_deref(), Some("ws"));
    assert_eq!(p.finished, 0);
    assert_eq!(p.percent(), 0);

    runner.observe(SweepProgress::SystemFinished {
      system: SystemId::new("ws"),
      index:  1,
      total:  2,
    });
    let run = runner.state();
    let p = run.progress().unwrap();
    assert!(
      p.current.is_none(),
      "the finished system is no longer in flight"
    );
    assert_eq!(p.percent(), 50);

    runner.observe(SweepProgress::Evaluating { systems: 2 });
    let run = runner.state();
    let p = run.progress().unwrap();
    assert!(p.evaluating);
    assert_eq!(p.percent(), 100);
  }

  #[test]
  fn detail_notes_are_kept_for_the_screen_and_bounded() {
    let runner = runner();
    running(&runner, 1);
    let detail = |note: &str, done, total| SweepProgress::Detail {
      system: SystemId::new("access-hq"),
      note: note.to_owned(),
      done,
      total,
    };

    runner.observe(SweepProgress::SystemStarted {
      system: SystemId::new("access-hq"),
      index:  1,
      total:  1,
    });
    runner.observe(detail("accounts: page 2", Some(200), Some(450)));
    runner.observe(detail("reading doors", None, None));

    let run = runner.state();
    let notes = &run.progress().unwrap().notes;
    assert_eq!(notes, &[
      "accounts: page 2 (200/450)".to_owned(),
      "reading doors".to_owned(),
    ]);

    // The backlog is bounded: a long read must not grow the state
    // without limit, and the newest line is always present.
    for i in 0..100 {
      runner.observe(detail(&format!("step {i}"), None, None));
    }
    let run = runner.state();
    let notes = &run.progress().unwrap().notes;
    assert_eq!(notes.len(), NOTE_BACKLOG);
    assert_eq!(notes.last().unwrap(), "step 99");
    assert_eq!(notes.first().unwrap(), "step 88");
  }

  #[test]
  fn a_new_system_starts_with_a_clean_feed() {
    let runner = runner();
    running(&runner, 2);
    runner.observe(SweepProgress::SystemStarted {
      system: SystemId::new("ws"),
      index:  1,
      total:  2,
    });
    runner.observe(SweepProgress::Detail {
      system: SystemId::new("ws"),
      note:   "accounts: page 1".to_owned(),
      done:   None,
      total:  None,
    });
    runner.observe(SweepProgress::SystemFinished {
      system: SystemId::new("ws"),
      index:  1,
      total:  2,
    });

    let run = runner.state();
    assert!(
      run.progress().unwrap().notes.is_empty(),
      "the previous system's notes do not survive its finish"
    );
  }

  #[test]
  fn progress_events_after_the_run_ends_are_ignored() {
    let runner = runner();
    assert!(!runner.state().running());
    runner.observe(SweepProgress::SystemStarted {
      system: SystemId::new("ws"),
      index:  1,
      total:  1,
    });
    assert!(!runner.state().running(), "an idle runner stays idle");
  }
}
