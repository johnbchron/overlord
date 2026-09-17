//! Domain vocabulary shared by every other overlord crate.
//!
//! This crate depends on no sibling crate. It defines the nouns of
//! SPEC.md sections 4 and 6 — systems, entities, persons, checks,
//! violations, commands — plus the value model that normalization
//! produces and the expression language consumes.

pub mod check;
pub mod command;
pub mod error;
pub mod ids;
pub mod normalized;
pub mod severity;
pub mod time;
pub mod value;
pub mod violation;

pub use check::{CheckDraft, CheckRecord, DryrunSample, SystemSelector};
pub use command::{CommandKind, CommandRecord, NewCommand};
pub use error::{CoreError, ParseRefError};
pub use ids::{
  Actor, CheckId, EntityKey, EntityRef, EntityType, PersonUid, Revision, Seq,
  SubjectKind, SubjectRef, SweepId, SystemId, SystemKind,
};
pub use normalized::{EntityStatus, NormalizedRecord, RESERVED_FIELDS};
pub use severity::Severity;
pub use time::Timestamp;
pub use value::Value;
pub use violation::{
  Completeness, Evidence, EvidenceLeaf, ResolveReason, SuppressReason,
  ViolationEventKind, ViolationState,
};
