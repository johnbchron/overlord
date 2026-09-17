use async_trait::async_trait;
use overlord_core::{Completeness, SystemId, SystemKind, Timestamp};

use crate::{
  error::ConnectorError,
  http::{Allow, RestrictedHttp},
  normalize::Ruleset,
};

/// One observed object, before normalization.
#[derive(Debug, Clone)]
pub struct Observation {
  /// The vendor payload exactly as received. Stored verbatim: SPEC.md
  /// section 2 keeps everything, and `raw.<path>` is the check
  /// language's escape hatch into it.
  pub raw: serde_json::Value,
}

impl Observation {
  #[must_use]
  pub fn new(raw: serde_json::Value) -> Self {
    Self { raw }
  }
}

/// What a connector returned for one system in one sweep.
#[derive(Debug, Clone)]
pub struct Snapshot {
  /// Whether this is a full enumeration. Only a complete snapshot may
  /// produce tombstones (SPEC.md section 11) — a paged-out or
  /// permission-denied read must never present as a mass
  /// deprovisioning.
  pub completeness: Completeness,
  pub observations: Vec<Observation>,
  /// Non-fatal problems worth surfacing on the coverage view.
  pub warnings: Vec<String>,
}

impl Snapshot {
  #[must_use]
  pub fn complete(observations: Vec<Observation>) -> Self {
    Self {
      completeness: Completeness::Complete,
      observations,
      warnings: Vec::new(),
    }
  }

  #[must_use]
  pub fn partial(
    observations: Vec<Observation>,
    reason: impl Into<String>,
  ) -> Self {
    Self {
      completeness: Completeness::Partial {
        reason: reason.into(),
      },
      observations,
      warnings: Vec::new(),
    }
  }
}

/// Everything a connector is told about the run it is part of.
#[derive(Debug, Clone)]
pub struct ObserveCtx {
  pub system: SystemId,
  /// The sweep's start time: the single definition of "now" for the run.
  pub started_at: Timestamp,
  /// This system's entry from the configuration file. Credentials come
  /// from the environment, never from here and never from the streams.
  pub config: serde_json::Value,
}

/// A read-only adapter for one system kind (SPEC.md section 11).
///
/// A connector is handed a [`RestrictedHttp`] and nothing else: no store
/// handle, no writable client, no way to name a mutating HTTP method. It
/// can therefore only do one thing, which is the point.
#[async_trait]
pub trait Connector: Send + Sync {
  /// Stable identifier, as named in configuration.
  fn name(&self) -> &'static str;

  fn system_kind(&self) -> SystemKind;

  /// The endpoints this connector may reach, each with the reason it is
  /// needed. Reviewable in one screen, which is the intent.
  fn allowlist(&self) -> Vec<Allow>;

  /// The base URL requests are resolved against.
  fn base_url(&self, ctx: &ObserveCtx) -> String;

  /// The normalization ruleset this connector ships for one system.
  ///
  /// Takes the context because a ruleset is per system, not per
  /// connector: the same adapter can front two tenants whose payload
  /// shapes differ, and the fixture connector fronts several system
  /// kinds at once. Operators may revise it; each revision is a
  /// `normalization.upsert` command.
  fn default_ruleset(&self, ctx: &ObserveCtx) -> Ruleset;

  /// Read current state. Must not write anything, anywhere.
  ///
  /// # Errors
  /// [`ConnectorError::Incomplete`] when the enumeration could not be
  /// finished — the sweep then records a partial snapshot rather than
  /// treating the gap as deletions.
  async fn observe(
    &self,
    http: &RestrictedHttp,
    ctx: &ObserveCtx,
  ) -> Result<Snapshot, ConnectorError>;

  /// Build the client this connector is allowed to use.
  ///
  /// Provided, not overridable in practice: the allowlist passed to the
  /// client is always the connector's own.
  ///
  /// # Errors
  /// If the base URL is invalid or the TLS stack cannot start.
  fn http(&self, ctx: &ObserveCtx) -> Result<RestrictedHttp, ConnectorError> {
    RestrictedHttp::new(&self.base_url(ctx), self.allowlist())
  }
}

/// The connectors a binary knows about.
#[derive(Default)]
pub struct Registry {
  connectors: Vec<Box<dyn Connector>>,
}

impl Registry {
  #[must_use]
  pub fn new() -> Self {
    Self::default()
  }

  #[must_use]
  pub fn with(mut self, c: Box<dyn Connector>) -> Self {
    self.connectors.push(c);
    self
  }

  #[must_use]
  pub fn get(&self, name: &str) -> Option<&dyn Connector> {
    self
      .connectors
      .iter()
      .find(|c| c.name() == name)
      .map(AsRef::as_ref)
  }

  #[must_use]
  pub fn names(&self) -> Vec<&'static str> {
    self.connectors.iter().map(|c| c.name()).collect()
  }
}

impl std::fmt::Debug for Registry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Registry")
      .field("connectors", &self.names())
      .finish()
  }
}
