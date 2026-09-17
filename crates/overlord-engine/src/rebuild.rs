//! The full rebuild: streams in, projections out, violations included.
//!
//! SPEC.md section 13 makes `replay(streams) == live` a contract rather
//! than an aspiration, and this is the code that has to honour it. The
//! store replays the commands and facts; the hook below re-runs
//! evaluation at each sweep's recorded commit position, against exactly
//! the projections the live path had at that moment and with the check
//! revisions that sweep pinned.

use overlord_store::{Db, rebuild::RebuildReport};

use crate::{error::Result, evaluate::evaluate_sweep};

/// Drop every projection and rebuild it, evaluation and all.
///
/// # Errors
/// If a stored command cannot be interpreted, or evaluation fails
/// against the replayed state.
pub fn rebuild(db: &Db) -> Result<RebuildReport> {
  let mut hook = |w: &overlord_store::Writer<'_>, sweep| {
    evaluate_sweep(w, sweep).map(|_| ()).map_err(|e| match e {
      crate::EngineError::Store(s) => s,
      other => overlord_store::StoreError::rejected(other.to_string()),
    })
  };
  Ok(overlord_store::rebuild::rebuild_with(db, &mut hook)?)
}
