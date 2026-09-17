//! What a connector narrates while it reads.
//!
//! A sweep can spend a long time inside one connector — paging a vendor
//! API, then making one call per group to invert its membership. A
//! single "system started" event is not enough feedback for that, so a
//! connector is handed a [`Progress`] and reports each step it takes.
//! The engine forwards those reports to whoever is watching the sweep
//! (PLAN.md section 4, item 17).
//!
//! [`Progress`] is inert until someone attaches a sink, so a connector
//! reports unconditionally: in a unit test or a CLI run the reports cost
//! nothing and go nowhere.

use std::sync::Arc;

/// One step a connector reports during an `observe`.
#[derive(Debug, Clone)]
pub struct ProgressEvent {
  /// What is happening, in words an operator can read.
  pub note:  String,
  /// Work finished so far, when the connector can count it.
  pub done:  Option<u64>,
  /// Work expected, when the connector knows the size.
  pub total: Option<u64>,
}

impl ProgressEvent {
  /// A plain note, with no counts.
  #[must_use]
  pub fn note(note: impl Into<String>) -> Self {
    Self {
      note:  note.into(),
      done:  None,
      total: None,
    }
  }

  /// A note with progress against a known or unknown total.
  #[must_use]
  pub fn counted(
    note: impl Into<String>,
    done: u64,
    total: Option<u64>,
  ) -> Self {
    Self {
      note: note.into(),
      done: Some(done),
      total,
    }
  }
}

/// Where a connector narrates its work.
///
/// Cheap to clone and `Send + Sync`. The default carries no sink, so
/// [`Self::report`] is a no-op and a connector never has to branch on
/// whether anyone is watching.
#[derive(Clone, Default)]
pub struct Progress {
  sink: Option<Arc<dyn Fn(ProgressEvent) + Send + Sync>>,
}

impl Progress {
  /// A progress handle that forwards to `sink`.
  #[must_use]
  pub fn new(sink: Arc<dyn Fn(ProgressEvent) + Send + Sync>) -> Self {
    Self { sink: Some(sink) }
  }

  /// Whether anyone is listening. A connector may use this to skip
  /// composing a note it would otherwise discard, though the reports
  /// are cheap enough that most never need to.
  #[must_use]
  pub fn is_active(&self) -> bool { self.sink.is_some() }

  /// Report a step.
  pub fn report(&self, event: ProgressEvent) {
    if let Some(sink) = &self.sink {
      sink(event);
    }
  }

  /// Report a plain note.
  pub fn say(&self, note: impl Into<String>) {
    self.report(ProgressEvent::note(note));
  }

  /// Report counted progress.
  pub fn counted(
    &self,
    note: impl Into<String>,
    done: u64,
    total: Option<u64>,
  ) {
    self.report(ProgressEvent::counted(note, done, total));
  }
}

impl std::fmt::Debug for Progress {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // The sink is a closure with nothing useful to print; whether one is
    // attached is the whole of what a caller can act on.
    f.debug_struct("Progress")
      .field("active", &self.is_active())
      .finish()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use super::*;

  #[test]
  fn a_default_progress_is_inert() {
    let progress = Progress::default();
    assert!(!progress.is_active());
    // Reporting into the void is fine, not a panic.
    progress.say("nobody is listening");
  }

  #[test]
  fn reports_reach_the_sink_with_their_counts() {
    let seen: Arc<Mutex<Vec<ProgressEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let progress =
      Progress::new(Arc::new(move |e| sink.lock().unwrap().push(e)));

    progress.say("reading accounts");
    progress.counted("accounts: page 2", 200, Some(450));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].note, "reading accounts");
    assert_eq!(seen[0].done, None);
    assert_eq!(seen[1].note, "accounts: page 2");
    assert_eq!(seen[1].done, Some(200));
    assert_eq!(seen[1].total, Some(450));
  }
}
