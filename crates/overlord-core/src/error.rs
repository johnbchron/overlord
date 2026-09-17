use thiserror::Error;

/// Failure to parse a stringified reference back into a typed id.
///
/// References round-trip through `TEXT` columns (`subject_ref`,
/// `command.subject`), so every `Display` impl in [`crate::ids`] has a
/// matching `FromStr`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseRefError {
  #[error("empty reference")]
  Empty,
  #[error("expected {expected} segments separated by '/', found {found}")]
  Arity { expected: usize, found: usize },
  #[error("unknown reference kind {0:?}")]
  UnknownKind(String),
  #[error("unknown system kind {0:?}")]
  UnknownSystemKind(String),
  #[error("unknown severity {0:?}")]
  UnknownSeverity(String),
  #[error("unknown status {0:?}")]
  UnknownStatus(String),
  #[error("unknown violation state {0:?}")]
  UnknownViolationState(String),
  #[error("unknown suppression reason {0:?}")]
  UnknownSuppressReason(String),
  #[error("segment contains a reserved separator")]
  ReservedSeparator,
}

#[derive(Debug, Error)]
pub enum CoreError {
  #[error(transparent)]
  ParseRef(#[from] ParseRefError),
  #[error("invalid timestamp: {0}")]
  Timestamp(String),
}
