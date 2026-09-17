use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use overlord_engine::SystemConfig;
use serde::{Deserialize, Serialize};

/// The configuration file.
///
/// SPEC.md section 14: connector credentials and the OIDC client secret
/// come from configuration and environment, never the streams. Nothing
/// here affects evaluation — checks and normalization live in the
/// command stream precisely because they do.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
  #[serde(default)]
  pub store: Store,
  #[serde(default)]
  pub sweep: Sweep,
  #[serde(default)]
  pub systems: Vec<SystemConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Store {
  pub path: PathBuf,
}

impl Default for Store {
  fn default() -> Self {
    Self {
      path: PathBuf::from("overlord.db"),
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sweep {
  /// A snapshot that would tombstone more than this share of a system's
  /// entities is refused (SPEC.md section 10).
  pub absence_guard_pct: u32,
}

impl Default for Sweep {
  fn default() -> Self {
    Self {
      absence_guard_pct: 10,
    }
  }
}

impl Config {
  /// Load from a TOML file, or fall back to defaults if the default
  /// path is simply absent.
  ///
  /// # Errors
  /// If an explicitly named file is missing or malformed.
  pub fn load(path: &Path, explicit: bool) -> Result<Self> {
    match std::fs::read_to_string(path) {
      Ok(text) => toml::from_str(&text)
        .with_context(|| format!("reading {}", path.display())),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
        Ok(Self::default())
      }
      Err(e) => Err(
        anyhow::Error::new(e).context(format!("reading {}", path.display())),
      ),
    }
  }
}
