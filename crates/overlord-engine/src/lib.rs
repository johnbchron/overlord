//! Sweeps, evaluation, and the violation lifecycle.
//!
//! This is where SPEC.md's rules actually live. The store records what
//! it is told; the expression crate answers yes, no or null about one
//! subject. Deciding *which* subjects, *which* check revisions, what a
//! sweep is allowed to conclude from a snapshot, and how a violation
//! moves between states — all of that is here.

pub mod checks;
pub mod error;
pub mod evaluate;
pub mod identity;
pub mod rebuild;
pub mod sweep;
pub mod world;

pub use error::{EngineError, Result};
pub use evaluate::{EvalReport, evaluate_sweep};
pub use identity::recompute_suggestions;
pub use rebuild::rebuild;
pub use sweep::{SweepOutcome, SweepPlan, SystemConfig, run_sweep};
pub use world::World;
