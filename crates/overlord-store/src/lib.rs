//! SQLite storage: the two append-only streams and the projections
//! derived from them (SPEC.md section 6, PLAN.md section 3).
//!
//! This crate owns every SQL statement in overlord and no business
//! logic: it will happily record a violation, but it never decides that
//! one should open. That boundary is what lets [`rebuild`] drop every
//! projection and replay the streams through the same code the live path
//! uses.

pub mod db;
pub mod detail;
pub mod error;
pub mod identity;
pub mod project;
pub mod read;
pub mod rebuild;
pub mod streams;
pub mod violations;

pub use db::{Db, Reader, Writer};
pub use detail::{
  CheckRevisionRow, CoverageRow, DryRunRow, EntityDetail, EpisodeRow, EventRow,
  FactRowSummary, PersonDetail, SubjectHit, SweepRow, SystemRow,
};
pub use error::{Result, StoreError};
pub use identity::Suggestion;
pub use read::{
  Counts, EntityFilter, EntityRow, EntityState, ScoreRow, SubjectFilter,
  TOKEN_CHARS, ViolationFilter, ViolationRow, fts_query,
};
pub use streams::{
  FactRow, NewFact, SweepStart, SweepStatus, SweepSummary, SystemOutcome,
  SystemStatus,
};
pub use violations::{Episode, EpisodeFacts};
