use thiserror::Error;

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Error)]
pub enum StoreError {
  #[error(transparent)]
  Sqlite(#[from] rusqlite::Error),

  #[error("migration failed: {0}")]
  Migration(#[from] rusqlite_migration::Error),

  #[error("stored JSON is not readable: {0}")]
  Json(#[from] serde_json::Error),

  #[error(transparent)]
  Core(#[from] overlord_core::CoreError),

  /// A stored reference no longer parses. Only reachable if the file
  /// was written by a different version or edited by hand.
  #[error("stored reference is not readable: {0}")]
  BadRef(#[from] overlord_core::ParseRefError),

  /// A command the store refuses to record, with the operator-facing
  /// reason. Validation lives above the store; this covers the
  /// invariants only the store can see, such as an unknown check id.
  #[error("{0}")]
  Rejected(String),

  #[error("{0} not found")]
  NotFound(String),
}

impl StoreError {
  pub fn rejected(msg: impl Into<String>) -> Self {
    Self::Rejected(msg.into())
  }

  pub fn not_found(what: impl Into<String>) -> Self {
    Self::NotFound(what.into())
  }
}
