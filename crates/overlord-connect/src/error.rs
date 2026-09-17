use thiserror::Error;

use crate::http::ReadMethod;

#[derive(Debug, Error)]
pub enum ConnectorError {
  /// The connector tried to reach an endpoint outside its allowlist.
  /// Raised before any network call (SPEC.md section 11).
  #[error("{method} {path} is not in this connector's allowlist")]
  NotAllowed { method: ReadMethod, path: String },

  /// An offline client refused to dial. Only reachable in tests and
  /// dry runs.
  #[error("{method} {path} was refused: this client is offline")]
  Offline { method: ReadMethod, path: String },

  #[error("configuration: {0}")]
  Config(String),

  #[error("transport: {0}")]
  Transport(String),

  #[error("{path} returned HTTP {status}")]
  Status { status: u16, path: String },

  #[error("could not read the response: {0}")]
  Decode(String),

  /// The vendor refused part of the enumeration. The sweep records a
  /// partial snapshot rather than treating the gap as deletions.
  #[error("{0}")]
  Incomplete(String),

  #[error("{0}")]
  Other(String),
}

impl ConnectorError {
  /// Whether this failure means the snapshot is partial rather than
  /// absent. A partial snapshot never produces tombstones.
  #[must_use]
  pub fn is_partial(&self) -> bool {
    matches!(self, Self::Incomplete(_))
  }
}
