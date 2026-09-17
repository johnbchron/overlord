//! A deterministic connector that reads scenarios from disk.
//!
//! This is the test bed the sweep engine, the absence guard, evaluation
//! and the replay test are all built against (PLAN.md, M1). It exists
//! because the interesting cases are the ones a real tenant will not
//! produce on demand: a snapshot that paged out halfway, a connector
//! that failed mid-sweep, a directory that appears to have lost 90% of
//! its accounts. Here they are three lines of JSON.
//!
//! It is not scaffolding to be deleted once Google Workspace lands. It
//! stays, because those cases stay worth testing.

use std::path::PathBuf;

use async_trait::async_trait;
use overlord_connect::{
  Allow, Connector, ConnectorError, Observation, ObserveCtx, RestrictedHttp,
  Ruleset, Snapshot,
};
use overlord_core::{Completeness, SystemKind};
use serde::{Deserialize, Serialize};

/// A scenario file: a sequence of stages, one per sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
  #[serde(default = "default_kind")]
  pub system_kind: SystemKind,
  #[serde(default = "default_entity_type")]
  pub entity_type: String,
  pub stages:      Vec<Stage>,
}

fn default_kind() -> SystemKind { SystemKind::Workspace }

fn default_entity_type() -> String { "user".to_owned() }

/// What the connector reports on one sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
  #[serde(default)]
  pub note:    Option<String>,
  /// Simulate a connector failure. The sweep records a failed system
  /// without corrupting other systems' results.
  #[serde(default)]
  pub fail:    Option<String>,
  /// Set to record a partial snapshot, which may never produce
  /// tombstones.
  #[serde(default)]
  pub partial: Option<String>,
  #[serde(default)]
  pub records: Vec<serde_json::Value>,
}

/// Per-system configuration for this connector.
#[derive(Debug, Clone, Deserialize)]
struct Config {
  path:  PathBuf,
  /// Which stage to serve. Stages past the end repeat the last one, so
  /// a two-stage scenario can be swept any number of times.
  #[serde(default)]
  stage: usize,
}

#[derive(Debug, Default)]
pub struct FixtureConnector;

impl FixtureConnector {
  #[must_use]
  pub fn new() -> Self { Self }

  /// Read the scenario named by a system's configuration.
  ///
  /// Returns `None` rather than failing: a misconfigured path is
  /// reported by `observe`, where the error has somewhere to go.
  fn scenario(&self, ctx: &ObserveCtx) -> Option<Scenario> {
    let cfg: Config = serde_json::from_value(ctx.config.clone()).ok()?;
    let text = std::fs::read_to_string(&cfg.path).ok()?;
    serde_json::from_str(&text).ok()
  }

  #[must_use]
  pub fn boxed() -> Box<dyn Connector> { Box::new(Self) }
}

#[async_trait]
impl Connector for FixtureConnector {
  fn name(&self) -> &'static str { "fixture" }

  fn system_kind(&self) -> SystemKind { SystemKind::Workspace }

  /// Empty, and meaningfully so: a connector with no allowlisted
  /// endpoints can reach nothing at all.
  fn allowlist(&self) -> Vec<Allow> { Vec::new() }

  fn base_url(&self, _: &ObserveCtx) -> String {
    "https://fixture.invalid/".to_owned()
  }

  /// The shipped ruleset, with the system kind and entity type taken
  /// from the scenario — so one fixture connector can stand in for a
  /// workspace and an identity provider in the same sweep, which is
  /// what cross-system checks need to be testable at all.
  fn default_ruleset(&self, ctx: &ObserveCtx) -> Ruleset {
    let mut rs: Ruleset = serde_json::from_str(include_str!("ruleset.json"))
      .expect("the shipped fixture ruleset must parse");
    if let Some(scenario) = self.scenario(ctx) {
      rs.system_kind = scenario.system_kind;
      rs.entity_type.clone_from(&scenario.entity_type);
      rs.version = format!("{}-{}", rs.version, scenario.system_kind);
    }
    rs
  }

  async fn observe(
    &self,
    _http: &RestrictedHttp,
    ctx: &ObserveCtx,
  ) -> Result<Snapshot, ConnectorError> {
    let cfg: Config = serde_json::from_value(ctx.config.clone())
      .map_err(|e| ConnectorError::Config(format!("fixture: {e}")))?;

    let text = std::fs::read_to_string(&cfg.path).map_err(|e| {
      ConnectorError::Config(format!("{}: {e}", cfg.path.display()))
    })?;
    let scenario: Scenario = serde_json::from_str(&text).map_err(|e| {
      ConnectorError::Config(format!("{}: {e}", cfg.path.display()))
    })?;

    if scenario.stages.is_empty() {
      return Err(ConnectorError::Config(format!(
        "{} has no stages",
        cfg.path.display()
      )));
    }
    let idx = cfg.stage.min(scenario.stages.len() - 1);
    let stage = &scenario.stages[idx];

    if let Some(msg) = &stage.fail {
      return Err(ConnectorError::Other(msg.clone()));
    }

    let observations = stage
      .records
      .iter()
      .cloned()
      .map(Observation::new)
      .collect();

    let mut snapshot = Snapshot {
      completeness: match &stage.partial {
        Some(reason) => Completeness::Partial {
          reason: reason.clone(),
        },
        None => Completeness::Complete,
      },
      observations,
      warnings: Vec::new(),
    };
    if let Some(note) = &stage.note {
      snapshot.warnings.push(note.clone());
    }
    Ok(snapshot)
  }
}

#[cfg(test)]
mod tests {
  use overlord_core::{SystemId, Timestamp, Value};

  use super::*;

  fn ctx(path: &str, stage: usize) -> ObserveCtx {
    ObserveCtx {
      system:     SystemId::new("fix"),
      started_at: "2026-01-15T00:00:00Z".parse::<Timestamp>().unwrap(),
      config:     serde_json::json!({ "path": path, "stage": stage }),
    }
  }

  fn scenario_path(name: &str) -> String {
    format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
  }

  async fn observe(name: &str, stage: usize) -> Snapshot {
    let c = FixtureConnector::new();
    let ctx = ctx(&scenario_path(name), stage);
    let http = c.http(&ctx).unwrap();
    c.observe(&http, &ctx).await.unwrap()
  }

  #[tokio::test]
  async fn serves_the_requested_stage() {
    let s = observe("baseline.json", 0).await;
    assert!(s.completeness.is_complete());
    assert_eq!(s.observations.len(), 4);
  }

  #[tokio::test]
  async fn a_partial_stage_is_marked_partial() {
    let s = observe("truncated.json", 1).await;
    assert!(!s.completeness.is_complete());
    assert!(s.completeness.reason().unwrap().contains("rate"));
  }

  #[tokio::test]
  async fn a_failing_stage_reports_an_error() {
    let c = FixtureConnector::new();
    let ctx = ctx(&scenario_path("unreachable.json"), 1);
    let http = c.http(&ctx).unwrap();
    assert!(c.observe(&http, &ctx).await.is_err());
  }

  #[tokio::test]
  async fn stages_past_the_end_repeat_the_last_one() {
    // So a two-stage scenario can be swept any number of times and
    // settle, rather than running out of data.
    let last = observe("baseline.json", 1).await;
    let beyond = observe("baseline.json", 99).await;
    assert_eq!(beyond.observations.len(), last.observations.len());
    assert_ne!(
      observe("baseline.json", 0).await.observations.len(),
      last.observations.len(),
      "the stages should actually differ, or this proves nothing"
    );
  }

  #[test]
  fn the_shipped_ruleset_normalizes_a_fixture_record() {
    let rs = FixtureConnector::new().default_ruleset(&ctx("x", 0));
    let n = rs
      .apply(
        &SystemId::new("fix"),
        &serde_json::json!({
          "key": "ada@example.com",
          "name": "Ada Lovelace",
          "state": "active",
          "mfa": false,
          "admin": true,
          "last_login": "2025-06-01T00:00:00Z",
          "groups": [{ "name": "eng", "external": false }]
        }),
      )
      .unwrap();
    assert!(n.warnings.is_empty(), "{:?}", n.warnings);
    assert_eq!(n.record.get("is_admin"), Value::Bool(true));
    assert_eq!(n.record.get("last_login_at").type_name(), "timestamp");
  }

  #[test]
  fn the_connector_can_reach_nothing() {
    let c = FixtureConnector::new();
    assert!(c.allowlist().is_empty());
    let http = c.http(&ctx("x", 0)).unwrap();
    assert!(!http.permits(overlord_connect::ReadMethod::Get, "/anything"));
  }
}
