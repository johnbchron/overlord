use async_trait::async_trait;
use overlord_core::{Completeness, SystemId, SystemKind, Timestamp};

use crate::{
  error::ConnectorError,
  http::{Allow, RestrictedHttp},
  normalize::Ruleset,
  progress::Progress,
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
  pub fn new(raw: serde_json::Value) -> Self { Self { raw } }
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
  pub warnings:     Vec<String>,
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
  pub system:     SystemId,
  /// The sweep's start time: the single definition of "now" for the run.
  pub started_at: Timestamp,
  /// This system's entry from the configuration file. Credentials come
  /// from the environment, never from here and never from the streams.
  pub config:     serde_json::Value,
  /// Where the connector narrates its progress. Inert unless a sweep is
  /// being watched; see [`crate::progress`].
  pub progress:   Progress,
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

  /// Additional root certificates (PEM) this connector needs to reach a
  /// system whose certificate is signed by a private CA.
  ///
  /// Empty by default, which is right for every vendor on the public
  /// web. A self-hosted appliance is the case this exists for: its
  /// console presents a certificate no public root vouches for, and the
  /// only safe answer is to trust that one CA rather than to turn
  /// verification off. Takes the context because the certificate is
  /// per-system configuration.
  ///
  /// # Errors
  /// If the configured certificate cannot be read.
  fn root_certificates(
    &self,
    _ctx: &ObserveCtx,
  ) -> Result<Vec<Vec<u8>>, ConnectorError> {
    Ok(Vec::new())
  }

  /// Build the client this connector is allowed to use.
  ///
  /// Provided, not overridable in practice: the allowlist passed to the
  /// client is always the connector's own.
  ///
  /// # Errors
  /// If the base URL is invalid or the TLS stack cannot start.
  fn http(&self, ctx: &ObserveCtx) -> Result<RestrictedHttp, ConnectorError> {
    let mut http = RestrictedHttp::new(&self.base_url(ctx), self.allowlist())?
      .with_progress(ctx.progress.clone());
    for pem in self.root_certificates(ctx)? {
      http = http.trusted(&pem)?;
    }
    if self.accept_invalid_certificates(ctx) {
      http = http.insecure()?;
    }
    Ok(http)
  }

  /// Whether to stop verifying the server's certificate for this
  /// system. `false` unless a connector explicitly opts in, and
  /// [`Self::root_certificates`] is always the better answer when there
  /// is a certificate to trust.
  ///
  /// It exists for the appliance that presents a self-signed leaf
  /// rustls will not accept as an anchor and offers no CA to pin. The
  /// read-only guarantee does not depend on it — the allowlist and
  /// `ReadMethod` still bound every request — but an operator who turns
  /// it on has given up knowing which host answered.
  fn accept_invalid_certificates(&self, _ctx: &ObserveCtx) -> bool { false }

  /// Build a client for one of the secondary origins this connector
  /// declared with [`Allow::at`] — a token endpoint, or a sibling API on
  /// another host.
  ///
  /// The returned client carries only that origin's entries, so a
  /// second host widens the allowlist by exactly what was written down
  /// for it and nothing else. `via` sends the requests elsewhere (an
  /// egress proxy, a test double) without changing which of them are
  /// permitted. Provided for the same reason as [`Self::http`]: the
  /// allowlist is always the connector's own.
  ///
  /// # Errors
  /// If the base URL is invalid or the TLS stack cannot start.
  fn http_for(
    &self,
    declared: &str,
    via: Option<&str>,
  ) -> Result<RestrictedHttp, ConnectorError> {
    RestrictedHttp::at_via(declared, via.unwrap_or(declared), self.allowlist())
  }
}

/// The connectors a binary knows about.
#[derive(Default)]
pub struct Registry {
  connectors: Vec<Box<dyn Connector>>,
}

impl Registry {
  #[must_use]
  pub fn new() -> Self { Self::default() }

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
