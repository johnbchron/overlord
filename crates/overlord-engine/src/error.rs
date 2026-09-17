use thiserror::Error;

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Error)]
pub enum EngineError {
  #[error(transparent)]
  Store(#[from] overlord_store::StoreError),

  #[error(transparent)]
  Connector(#[from] overlord_connect::ConnectorError),

  #[error(transparent)]
  Core(#[from] overlord_core::CoreError),

  #[error(transparent)]
  BadRef(#[from] overlord_core::ParseRefError),

  #[error("{0}")]
  Json(#[from] serde_json::Error),

  /// A condition that will not compile. Carries every diagnostic so the
  /// check editor can underline all of them at once.
  #[error("the condition is not valid")]
  BadCondition(Vec<overlord_expr::Diagnostic>),

  #[error("no connector named {0:?}")]
  UnknownConnector(String),

  #[error("{0}")]
  Config(String),
}

impl EngineError {
  /// Render a bad condition against its source, for the CLI.
  #[must_use]
  pub fn render(&self, src: &str) -> String {
    match self {
      Self::BadCondition(ds) => ds
        .iter()
        .map(|d| d.render(src))
        .collect::<Vec<_>>()
        .join("\n\n"),
      other => other.to_string(),
    }
  }
}
