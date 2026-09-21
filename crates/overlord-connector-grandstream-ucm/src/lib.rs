//! The Grandstream UCM connector: read-only, the appliance's HTTPS API.
//!
//! Built against a UCM6308A. The HTTPS API is common to the UCM63xx
//! line, so other models in the series should work unchanged.
//!
//! # Two systems, not one connector reading two things
//!
//! A UCM holds two populations that have nothing to do with each other:
//! **extensions**, which belong to people, and **Zero Config devices**,
//! which are handsets. overlord normalizes one entity type per system
//! (a ruleset declares its `entity_type`), so this connector is
//! configured twice against the same appliance — once per `mode`:
//!
//! ```toml
//! [[systems]]
//! id = "ucm-extensions"
//! connector = "grandstream-ucm"
//! config = { base_url = "https://10.0.0.5:8089", mode = "extensions" }
//!
//! [[systems]]
//! id = "ucm-devices"
//! connector = "grandstream-ucm"
//! config = { base_url = "https://10.0.0.5:8089", mode = "devices" }
//! ```
//!
//! That reads as a workaround and is not one. The two populations are
//! swept at different rates, scoped by different checks, and fail
//! independently: Zero Config being unreachable on a firmware leaves
//! extensions collecting normally, which is precisely what one system
//! carrying both would not do.
//!
//! `mode` also picks the system kind, and the choice is load-bearing.
//! Extensions report [`SystemKind::Sso`] — an extension is an
//! assignment to a person within one application, which is what section
//! 11 groups under access and SSO. Reporting `Workspace` would make
//! `has_entity("workspace")` true for somebody who has only a desk
//! phone, and quietly break the shipped `workspace-without-idp` rule.
//! Devices report [`SystemKind::Mdm`], so `count_entities("mdm")` and a
//! device-scoped check mean what they say.
//!
//! # Zero Config is not in the documented API
//!
//! Grandstream's HTTPS API reference enumerates the actions the
//! appliance answers — extensions, trunks, routes, queues, call control
//! — and **none of them returns Zero Config's device inventory**. The
//! web UI reaches that list by a route outside the documented API.
//!
//! So `mode = "devices"` is built, but its action name is configuration
//! rather than a constant: `zero_config_action` defaults to
//! `listZeroConfig`, which follows the vendor's naming for every other
//! list, and is a guess and documented as one. If the firmware does not
//! answer it, the sweep records a **partial** snapshot naming the
//! action it tried and the status it got back — so nothing is
//! tombstoned and no device is invented — and the fix is one line of
//! TOML.
//!
//! The device payload's field names are unknown for the same reason, so
//! they are not trusted: [`device_envelope`] reduces whatever comes back
//! to a stable shape by trying the names each attribute is plausibly
//! spelled, and a device whose MAC address cannot be found is skipped
//! with a warning rather than given a key that would not survive the
//! next sweep. `raw.<path>` still reaches the payload exactly as the
//! appliance sent it.
//!
//! # The shape of an observation
//!
//! Extensions are the vendor's `listAccount` row, with `detail` folded
//! in when `detail = true`:
//!
//! ```json
//! {
//!   "account": { "extension": "1001", "fullname": "Ada Lovelace",
//!                "status": "Idle", "addr": "10.0.0.31:5062" },
//!   "detail":  { "...": "the getSIPAccount record, secrets removed" }
//! }
//! ```
//!
//! Devices are the envelope plus the payload:
//!
//! ```json
//! {
//!   "device": { "...": "the vendor row, verbatim" },
//!   "mac": "00:0b:82:aa:bb:cc",
//!   "model": "GRP2615", "vendor": "Grandstream",
//!   "firmware": "1.0.11.76", "ip": "10.0.0.31",
//!   "extension": "1001"
//! }
//! ```
//!
//! # Secrets are stripped, not stored
//!
//! An extension's record carries its SIP password (`secret`) and
//! voicemail PIN (`vmsecret`). The fact stream is a plaintext SQLite
//! file that section 2 keeps forever, so both are dropped before the
//! observation is built, along with anything else whose key ends in
//! `secret` or `password`. What a check needs — that a secret exists,
//! and how long it is — survives as `has_secret` and `secret_len`,
//! which is enough to write "this extension has a four-character SIP
//! password" without the file holding the password.
//!
//! # Credentials and TLS
//!
//! The API user is created on the appliance under **System Settings →
//! HTTPS API**; its password comes from the environment variable named
//! by `credentials_env` and nowhere else (SPEC.md section 14).
//!
//! A UCM serves `:8089` with a self-signed certificate, so the read
//! fails with `UnknownIssuer` until the operator either trusts the
//! appliance's certificate through `ca_cert` or accepts it unverified
//! with `tls_insecure` — the same two options, with the same trade, as
//! the UniFi Access connector.

pub mod api;

use async_trait::async_trait;
use overlord_connect::{
  Allow, Connector, ConnectorError, Observation, ObserveCtx, RestrictedHttp,
  Ruleset, Snapshot,
};
use overlord_core::{Completeness, SystemKind};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::api::{API_PATH, Session};

/// The action [`Config::zero_config_action`] defaults to.
///
/// A guess, and the module docs say why it has to be: Grandstream
/// documents no Zero Config action at all. It follows the naming of
/// every documented list action, which makes it the most likely spelling
/// and nothing more.
pub const DEFAULT_ZERO_CONFIG_ACTION: &str = "listZeroConfig";

/// Which population of the appliance a system collects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
  /// SIP extensions, as `phone-extension`.
  #[default]
  Extensions,
  /// Zero Config handsets, as `phone-device`.
  Devices,
}

impl Mode {
  #[must_use]
  pub fn entity_type(self) -> &'static str {
    match self {
      Self::Extensions => "phone-extension",
      Self::Devices => "phone-device",
    }
  }

  #[must_use]
  pub fn system_kind(self) -> SystemKind {
    match self {
      Self::Extensions => SystemKind::Sso,
      Self::Devices => SystemKind::Mdm,
    }
  }
}

/// Per-system configuration, from the `[systems.config]` table.
///
/// The API password is conspicuously absent: it comes from the
/// environment named by `credentials_env` and nowhere else.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  /// The appliance's address and HTTPS API port, e.g.
  /// `https://10.0.0.5:8089`. Required: a PBX has no factory address
  /// worth defaulting to.
  pub base_url:             Option<String>,
  /// The API user configured under System Settings → HTTPS API.
  pub username:             String,
  /// The environment variable holding that user's password.
  pub credentials_env:      String,
  /// Which population this system collects.
  pub mode:                 Mode,
  /// Fetch each extension's full record as well as its list row.
  ///
  /// Off by default because it is one call per extension: the list
  /// already carries status, registration and name, and the detail is
  /// for checks that reach past those. Ignored in `devices` mode.
  pub detail:               bool,
  /// What to call for the Zero Config device list.
  ///
  /// Configuration rather than a constant because Grandstream documents
  /// no such action; see the module docs. Ignored in `extensions` mode.
  pub zero_config_action:   String,
  /// The field of that action's response holding the device list.
  ///
  /// Absent means "infer it", which succeeds only when the response
  /// carries exactly one array of objects.
  pub zero_config_list_key: Option<String>,
  /// A PEM file holding the appliance's certificate or its CA, so the
  /// host can verify a UCM that signs its own.
  pub ca_cert:              Option<String>,
  /// Stop verifying the appliance's certificate. Off by default.
  pub tls_insecure:         bool,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      base_url:             None,
      username:             "cdrapi".to_owned(),
      credentials_env:      "OVERLORD_UCM_PASSWORD".to_owned(),
      mode:                 Mode::default(),
      detail:               false,
      zero_config_action:   DEFAULT_ZERO_CONFIG_ACTION.to_owned(),
      zero_config_list_key: None,
      ca_cert:              None,
      tls_insecure:         false,
    }
  }
}

impl Config {
  fn read(ctx: &ObserveCtx) -> Result<Self, ConnectorError> {
    if ctx.config.is_null() {
      return Ok(Self::default());
    }
    serde_json::from_value(ctx.config.clone())
      .map_err(|e| ConnectorError::Config(format!("grandstream-ucm: {e}")))
  }

  fn require_base(&self) -> Result<String, ConnectorError> {
    self.base_url.clone().ok_or_else(|| {
      ConnectorError::Config(
        "grandstream-ucm: base_url is required, e.g. \"https://10.0.0.5:8089\""
          .to_owned(),
      )
    })
  }
}

#[derive(Debug, Default)]
pub struct GrandstreamUcmConnector {
  /// Set only by [`Self::with_password`], for tests.
  password: Option<String>,
}

impl GrandstreamUcmConnector {
  #[must_use]
  pub fn new() -> Self { Self { password: None } }

  /// The connector with a password supplied directly, for tests.
  ///
  /// A test cannot set an environment variable: `std::env::set_var` is
  /// unsafe in this edition and `unsafe_code` is forbidden across the
  /// workspace. No configuration path reaches this — a `[[systems]]`
  /// entry names a connector and the binary's registry calls
  /// [`Self::new`] — so the environment stays the only way a password
  /// gets into a deployment.
  #[must_use]
  pub fn with_password(password: impl Into<String>) -> Self {
    Self {
      password: Some(password.into()),
    }
  }

  #[must_use]
  pub fn boxed() -> Box<dyn Connector> { Box::new(Self::new()) }

  fn password(&self, cfg: &Config) -> Result<String, ConnectorError> {
    if let Some(p) = &self.password {
      return Ok(p.clone());
    }
    let var = &cfg.credentials_env;
    let value = std::env::var(var).map_err(|_| {
      ConnectorError::Config(format!(
        "{var} is not set; the UCM API password comes from the environment, \
         never from the configuration file"
      ))
    })?;
    let value = value.trim().to_owned();
    if value.is_empty() {
      return Err(ConnectorError::Config(format!(
        "{var} is empty; create an API user on the appliance under System \
         Settings → HTTPS API"
      )));
    }
    Ok(value)
  }

  async fn observe_extensions(
    &self,
    http: &RestrictedHttp,
    ctx: &ObserveCtx,
    cfg: &Config,
    session: &Session,
  ) -> Result<Snapshot, ConnectorError> {
    let read = api::accounts(http, session, &ctx.progress).await;
    let mut incomplete: Vec<String> = read.incomplete.into_iter().collect();

    if read.items.is_empty() {
      // A read that produced nothing is a *failed* system, not a
      // partial one. The difference is what the operator is told: a
      // partial snapshot of zero extensions reads as "the sweep worked
      // and found nothing", which is the one thing that did not happen.
      if let Some(reason) = incomplete.first() {
        return Err(ConnectorError::Incomplete(format!(
          "no extensions were read: {reason}"
        )));
      }
      return Err(ConnectorError::Other(
        "the UCM reported no extensions at all; check that the API user has \
         permission to read them"
          .to_owned(),
      ));
    }

    let mut observations = Vec::with_capacity(read.items.len());
    for account in read.items {
      let extension = account
        .get("extension")
        .and_then(value_as_str)
        .unwrap_or_default();

      let detail = if cfg.detail && !extension.is_empty() {
        match api::sip_account(http, session, &extension).await {
          Ok(d) => Some(redact(&d)),
          Err(e) => {
            // One extension's detail failing is not the enumeration
            // failing, but it is a gap, and a gap makes the snapshot
            // partial so nothing is tombstoned on the strength of it.
            incomplete.push(format!("extension {extension}: {e}"));
            None
          }
        }
      } else {
        None
      };

      let mut raw = Map::new();
      raw.insert("account".to_owned(), redact(&account));
      if let Some(d) = detail {
        raw.insert("has_secret".to_owned(), json!(has_secret(&d)));
        raw.insert("detail".to_owned(), d);
      }
      observations.push(Observation::new(Value::Object(raw)));
    }

    Ok(finish(observations, incomplete))
  }

  async fn observe_devices(
    &self,
    http: &RestrictedHttp,
    ctx: &ObserveCtx,
    cfg: &Config,
    session: &Session,
  ) -> Result<Snapshot, ConnectorError> {
    let read = api::zero_config(
      http,
      session,
      &cfg.zero_config_action,
      cfg.zero_config_list_key.as_deref(),
      &ctx.progress,
    )
    .await;
    let mut incomplete: Vec<String> = read.incomplete.into_iter().collect();

    // As above: nothing read *and* something wrong is a failed system.
    // An empty list with nothing wrong is different, and is left alone
    // — a UCM with no provisioned handsets is a real answer, and the
    // absence guard is what stops it emptying a fleet by accident.
    if read.items.is_empty()
      && let Some(reason) = incomplete.first()
    {
      return Err(ConnectorError::Incomplete(format!(
        "no Zero Config devices were read: {reason}"
      )));
    }

    let mut observations = Vec::with_capacity(read.items.len());
    for device in read.items {
      match device_envelope(&device) {
        Some(envelope) => observations.push(Observation::new(envelope)),
        None => {
          // No MAC address means no key that survives to the next
          // sweep, so this row is not an entity. It is a gap, not a
          // deletion.
          let keys: Vec<&str> = device
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
          incomplete.push(format!(
            "a Zero Config row carried no MAC address; its fields were [{}]. \
             Nothing was recorded for it",
            keys.join(", ")
          ));
        }
      }
    }

    Ok(finish(observations, incomplete))
  }
}

/// Assemble the snapshot, partial exactly when something was missed.
fn finish(observations: Vec<Observation>, incomplete: Vec<String>) -> Snapshot {
  if incomplete.is_empty() {
    return Snapshot::complete(observations);
  }
  Snapshot {
    completeness: Completeness::Partial {
      reason: incomplete.join("; "),
    },
    observations,
    warnings: incomplete,
  }
}

/// A JSON scalar as a string. The UCM spells an extension number as a
/// string in one action and a number in another, and both are the same
/// extension.
fn value_as_str(v: &Value) -> Option<String> {
  match v {
    Value::String(s) => Some(s.clone()),
    Value::Number(n) => Some(n.to_string()),
    _ => None,
  }
}

/// Whether a redacted record had a SIP secret before it was redacted.
fn has_secret(redacted: &Value) -> bool {
  redacted
    .get("secret_len")
    .and_then(Value::as_u64)
    .is_some_and(|n| n > 0)
}

/// Drop every secret from a vendor record, keeping only its length.
///
/// Recursive and keyed on the field *name* rather than a fixed list:
/// the records carry `secret`, `vmsecret` and `sip_password` today, and
/// a firmware that adds another would otherwise put it in the fact
/// stream forever before anybody noticed. A name that ends in `secret`
/// or `password` is a secret, and the only thing kept about it is how
/// long it was — enough to check that one is set, or too short, without
/// storing it.
#[must_use]
pub fn redact(v: &Value) -> Value {
  match v {
    Value::Object(o) => {
      let mut out = Map::new();
      for (k, val) in o {
        let lower = k.to_ascii_lowercase();
        if lower.ends_with("secret") || lower.ends_with("password") {
          let len = val.as_str().map_or(0, str::len);
          out.insert(format!("{k}_len"), json!(len));
          continue;
        }
        out.insert(k.clone(), redact(val));
      }
      Value::Object(out)
    }
    Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
    other => other.clone(),
  }
}

/// Reduce an undocumented Zero Config row to a stable envelope.
///
/// Each attribute is looked for under the names it is plausibly spelled,
/// because the payload's field names are undocumented along with the
/// action that returns it. Returns `None` when there is no MAC address:
/// that is the device's identity, and a row without one cannot be
/// tracked across sweeps.
///
/// The original row is kept whole under `device`, so `raw.device.<path>`
/// reaches anything this did not name.
#[must_use]
pub fn device_envelope(device: &Value) -> Option<Value> {
  const MAC: [&str; 5] = ["mac", "macaddr", "mac_address", "macAddress", "MAC"];
  const MODEL: [&str; 4] = ["model", "device_model", "devtype", "device_type"];
  const VENDOR: [&str; 3] = ["vendor", "manufacturer", "brand"];
  const FIRMWARE: [&str; 5] = [
    "version",
    "firmware",
    "firmware_version",
    "fw_version",
    "sw_version",
  ];
  const IP: [&str; 4] = ["ip", "ip_address", "ipaddr", "ipv4"];
  const EXTENSION: [&str; 4] = ["extension", "ext", "account", "sip_account"];

  let mac = normalize_mac(&first_of(device, &MAC)?)?;
  let mut out = Map::new();
  out.insert("device".to_owned(), device.clone());
  out.insert("mac".to_owned(), json!(mac));
  for (name, candidates) in [
    ("model", &MODEL[..]),
    ("vendor", &VENDOR[..]),
    ("firmware", &FIRMWARE[..]),
    ("ip", &IP[..]),
    ("extension", &EXTENSION[..]),
  ] {
    // Absent stays absent: a field that was not collected is null, and
    // null is not "this device has no firmware version".
    if let Some(v) = first_of(device, candidates) {
      out.insert(name.to_owned(), json!(v));
    }
  }
  Some(Value::Object(out))
}

/// The first of `names` the row carries as a non-empty scalar.
fn first_of(device: &Value, names: &[&str]) -> Option<String> {
  names.iter().find_map(|n| {
    device
      .get(*n)
      .and_then(value_as_str)
      .map(|s| s.trim().to_owned())
      .filter(|s| !s.is_empty())
  })
}

/// A MAC address as lowercase hex with no separators.
///
/// The key has to be identical every sweep, and vendors are
/// inconsistent about case and separators — a firmware upgrade that
/// started sending `00:0B:82:…` where it used to send `000b82…` would
/// otherwise deprovision every handset and provision it again.
/// Returns `None` for anything that is not twelve hex digits.
#[must_use]
pub fn normalize_mac(raw: &str) -> Option<String> {
  let hex: String = raw
    .chars()
    .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
    .collect();
  if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
    return None;
  }
  Some(hex.to_ascii_lowercase())
}

#[async_trait]
impl Connector for GrandstreamUcmConnector {
  fn name(&self) -> &'static str { "grandstream-ucm" }

  /// The kind a system reports comes from its `mode`, through the
  /// ruleset. This is the connector-level answer, which the registry
  /// uses before any system is in hand.
  fn system_kind(&self) -> SystemKind { SystemKind::Sso }

  /// One entry, and the honest reading of it is in [`api`]: the whole
  /// UCM API is a single `POST /api` with the verb in the body, so the
  /// path allowlist cannot separate reading an extension from editing
  /// one. What does separate them is that [`api`] is the only place a
  /// request body is built and it names four actions, none of which
  /// mutates. `ReadMethod` still cannot spell a `PUT` or a `DELETE`.
  fn allowlist(&self) -> Vec<Allow> {
    vec![Allow::post(
      API_PATH,
      "the UCM's only endpoint: challenge, login, listAccount, getSIPAccount, \
       and the Zero Config device list",
    )]
  }

  fn base_url(&self, ctx: &ObserveCtx) -> String {
    Config::read(ctx)
      .ok()
      .and_then(|c| c.base_url)
      .unwrap_or_default()
  }

  /// The default, with the missing-`base_url` case caught first.
  ///
  /// [`Connector::base_url`] cannot fail, so an unconfigured address
  /// would otherwise reach the URL parser and come back as "relative
  /// URL without a base" — which names neither the field nor the file
  /// the operator has to edit.
  fn http(&self, ctx: &ObserveCtx) -> Result<RestrictedHttp, ConnectorError> {
    Config::read(ctx)?.require_base()?;
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

  /// The shipped ruleset for this system's `mode`.
  ///
  /// Per system rather than per connector, which is the seam that lets
  /// one appliance be two systems: the extension ruleset and the device
  /// ruleset declare different entity types and different system kinds,
  /// and a `[[systems]]` entry picks between them.
  fn default_ruleset(&self, ctx: &ObserveCtx) -> Ruleset {
    let mode = Config::read(ctx).map(|c| c.mode).unwrap_or_default();
    let src = match mode {
      Mode::Extensions => include_str!("ruleset-extension.json"),
      Mode::Devices => include_str!("ruleset-device.json"),
    };
    serde_json::from_str(src)
      .expect("the shipped Grandstream UCM rulesets must parse")
  }

  fn root_certificates(
    &self,
    ctx: &ObserveCtx,
  ) -> Result<Vec<Vec<u8>>, ConnectorError> {
    let Some(path) = Config::read(ctx)?.ca_cert else {
      return Ok(Vec::new());
    };
    std::fs::read(&path).map(|bytes| vec![bytes]).map_err(|e| {
      ConnectorError::Config(format!(
        "ca_cert {path} could not be read: {}",
        e.kind()
      ))
    })
  }

  fn accept_invalid_certificates(&self, ctx: &ObserveCtx) -> bool {
    Config::read(ctx).is_ok_and(|c| c.tls_insecure)
  }

  async fn observe(
    &self,
    http: &RestrictedHttp,
    ctx: &ObserveCtx,
  ) -> Result<Snapshot, ConnectorError> {
    let cfg = Config::read(ctx)?;
    cfg.require_base()?;
    if cfg.tls_insecure {
      warn!(
        system = %ctx.system,
        "TLS verification is disabled for this system (tls_insecure = true)"
      );
    }

    ctx.progress.say("logging in");
    let session =
      api::login(http, &cfg.username, &self.password(&cfg)?).await?;

    match cfg.mode {
      Mode::Extensions => {
        self.observe_extensions(http, ctx, &cfg, &session).await
      }
      Mode::Devices => self.observe_devices(http, ctx, &cfg, &session).await,
    }
  }
}
