use std::{
  net::{Ipv4Addr, SocketAddr},
  path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use overlord_core::EntityType;
use overlord_engine::SystemConfig;
use overlord_web::auth::OidcConfig;
use serde::{Deserialize, Serialize};

/// The configuration file.
///
/// SPEC.md section 14: connector credentials and the OIDC client secret
/// come from configuration and environment, never the streams.
///
/// Nothing here is read *by* evaluation. [`Identity`] is the one
/// setting that shapes what evaluation sees, and it is deliberately not
/// an exception: the file is its authoring surface, and a change to it
/// appends an `identity.policy` command, which is what evaluation
/// actually reads. Checks and normalization work the same way, and
/// `replay(streams) == live` stays total.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
  #[serde(default)]
  pub store:    Store,
  #[serde(default)]
  pub sweep:    Sweep,
  #[serde(default)]
  pub systems:  Vec<SystemConfig>,
  #[serde(default)]
  pub server:   Server,
  #[serde(default)]
  pub identity: Identity,
  /// Absent means no OIDC is configured, which confines the server to
  /// `--dev-actor` on a loopback address.
  #[serde(default)]
  pub auth:     Option<Auth>,
}

/// How entities become people (SPEC.md section 6.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
  /// Entity types that are not people.
  ///
  /// Every unlinked entity is otherwise evaluated as an implicit
  /// singleton person, which is what makes orphan-account checks
  /// possible. A device is not an orphan account: listing its type here
  /// keeps a fleet of them off the Users roster and out of person-scoped
  /// checks, while leaving entity-scoped checks over them untouched.
  ///
  /// A denylist rather than an allowlist so that empty — the default —
  /// is exactly the behaviour every existing store already has.
  #[serde(default)]
  pub non_person_entity_types: Vec<EntityType>,
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

/// Where the web server listens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
  pub bind: SocketAddr,
}

impl Default for Server {
  /// Loopback, because the default configuration has no authentication
  /// and a default that listened to the network would be a trap.
  fn default() -> Self {
    Self {
      bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
    }
  }
}

/// OIDC, as the file spells it (SPEC.md section 14).
///
/// The client secret is deliberately absent: it comes from
/// `OVERLORD_OIDC_CLIENT_SECRET` and nowhere else, so a configuration
/// file can be committed without leaking one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
  pub issuer:           String,
  pub client_id:        String,
  /// Must match what is registered with the provider exactly.
  pub redirect_url:     String,
  #[serde(default)]
  pub allowed_subjects: Vec<String>,
  #[serde(default)]
  pub required_group:   Option<String>,
}

impl Auth {
  /// Build the web crate's view of this configuration, pulling the
  /// secret from the environment.
  #[must_use]
  pub fn to_oidc(&self) -> OidcConfig {
    OidcConfig {
      issuer:           self.issuer.clone(),
      client_id:        self.client_id.clone(),
      client_secret:    std::env::var("OVERLORD_OIDC_CLIENT_SECRET").ok(),
      redirect_url:     self.redirect_url.clone(),
      allowed_subjects: self.allowed_subjects.iter().cloned().collect(),
      required_group:   self.required_group.clone(),
    }
  }
}
