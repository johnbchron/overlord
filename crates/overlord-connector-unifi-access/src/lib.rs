//! The UniFi Access connector: read-only, Ubiquiti's Access developer
//! API.
//!
//! SPEC.md section 11 groups "access and SSO applications" as one
//! connector category, so this reports [`SystemKind::Sso`]: what it
//! observes is the entitlement that opens a door, which is the same
//! question an SSO connector answers about an application.
//!
//! # What it reads
//!
//! Per sweep, for one UniFi Access console:
//!
//! - **users** — every account, with its status, employee number, credentials
//!   and access policies folded in (`expand[]=access_policy`);
//! - **doors** — so a policy's resources can be named rather than left as
//!   opaque ids;
//! - **user groups and their members** — inverted into per-account membership,
//!   so `count(groups where …)` is a question the overlay can answer;
//! - **the door-opening log** over a bounded window, reduced to the one date an
//!   overlay asks for: when each account last opened a door.
//!
//! # Last unlock, and why it is not just a date
//!
//! A badge that still opens the building is the access equivalent of a
//! login, so `last_unlock_at` is what makes a dormancy check possible
//! on a console — `is_admin and last_login_at < days_ago(90)` has no
//! meaning here, but "has not opened a door in 90 days" does.
//!
//! The trap is that a null date has two meanings, and they are
//! opposites. "This account opened no door in the window" is a finding;
//! "the log was not read" is an absence of evidence, and a dormancy
//! check that could not tell them apart would open a violation against
//! every account in the console the first time the log endpoint
//! returned a 500. So the envelope always carries
//! `unlock_activity_known`, and a check that cares writes
//! `unlock_activity_known and (last_unlock_at is null or last_unlock_at
//! < days_ago(90))`. `unlock_window_days` says how far back the null
//! reaches, because "never" and "not since Tuesday" are different
//! claims.
//!
//! For the same reason the log is all-or-nothing: a read that stopped
//! halfway withholds the whole view rather than reporting the accounts
//! it happened to reach. A partial log dates the openings it read, but
//! it cannot distinguish an account that opened no door from one whose
//! openings were in the pages that never arrived — and that difference
//! is the entire content of the field.
//!
//! # Credentials and the seam
//!
//! The API is authenticated with a single bearer token an operator
//! generates on the console (Access → Settings → General → Advanced).
//! It comes from the environment variable named by `credentials_env`
//! and nowhere else (SPEC.md section 14). It is never written to a
//! fact, a command, a log line, or a `Debug` rendering.
//!
//! # Credential material is stripped, not stored
//!
//! A user's payload carries the *door* credentials assigned to them:
//! `pin_code.token` and each `nfc_cards[].token`. Those are secrets an
//! operator issues, and the fact stream is a plaintext SQLite file that
//! §2 keeps forever. The envelope therefore drops every credential
//! token before it is observed, keeping only what a check needs: the
//! card's id and type, and a `has_pin` boolean for the PIN. Nothing
//! else in the payload is altered — `raw.user.<anything>` still reaches
//! the vendor object exactly as the console sent it.
//!
//! # The shape of an observation
//!
//! ```json
//! {
//!   "user":   { "...": "the developer-API user resource, credentials removed" },
//!   "has_pin": false,
//!   "doors":  [ { "id": "…", "name": "Front Door", "type": "door" } ],
//!   "groups": [ { "id": "…", "name": "Engineering" } ],
//!   "unlock_activity_known": true,
//!   "unlock_window_days": 90,
//!   "last_unlock": {
//!     "at":   "2026-01-09T08:14:02Z",
//!     "door": { "id": "…", "name": "Front Door" },
//!     "via":  "NFC",
//!     "result": "ACCESS_GRANTED"
//!   }
//! }
//! ```
//!
//! `doors` is derived from the user's access policies, resolving each
//! policy resource's id against the console's door list for a name.
//!
//! Both derived lists follow the same rule, because null is not "no":
//! each is **absent** when it could not be collected and an empty list
//! when it was collected and there was nothing in it. `groups` is
//! absent when groups were not collected at all; `doors` is absent for
//! an account whose policy ids the console declined to expand, and the
//! snapshot is `Partial` when any account is in that state. A door list
//! that failed does not make `doors` absent — entitlement is still
//! known from the policy resources, and only the names are lost.
//!
//! `last_unlock` follows it too, one step further: it is absent both
//! when the log was not read and when it was read and this account
//! opened nothing, and `unlock_activity_known` is the boolean that
//! separates the two.
//!
//! # TLS
//!
//! A console serves the API on `:12445` with a certificate no public
//! root vouches for, so the read fails with `UnknownIssuer` until the
//! operator either trusts the console's CA in `ca_cert` or accepts the
//! certificate unverified with `tls_insecure`.
//!
//! **`ca_cert` is the one to reach for.** It takes a PEM bundle and
//! trusts every certificate in it — `openssl s_client -showcerts` prints
//! leaf first, and a single-certificate parse would trust the one
//! certificate that is not an issuer. Verification stays on.
//!
//! **`tls_insecure` is for the console that will not cooperate**: some
//! present a self-signed leaf `rustls` will not accept as a trust anchor
//! and send no CA to pin, so there is nothing to name in `ca_cert`. It
//! turns verification off, logs a warning each sweep, and is off by
//! default. It gives up knowing which host answered; it does not give up
//! read-only, which the allowlist and `ReadMethod` enforce independently.
//!
//! A console placed behind a properly issued certificate needs neither.

pub mod api;

use std::collections::BTreeMap;

use async_trait::async_trait;
use overlord_connect::{
  Allow, Connector, ConnectorError, Observation, ObserveCtx, RestrictedHttp,
  Ruleset, Snapshot,
};
use overlord_core::{Completeness, SystemKind, Timestamp};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::api::{
  DEFAULT_BASE, DOORS_PATH, SYSTEM_LOGS_PATH, USER_GROUPS_PATH, USERS_PATH,
};

/// How far back the door-opening log is read when the operator names no
/// window. A quarter is the horizon the dormancy checks in the shipped
/// examples are written against, and it is short enough that the log of
/// a busy building is still a bounded read.
pub const DEFAULT_UNLOCK_WINDOW_DAYS: u32 = 90;

/// Per-system configuration, from the `[systems.config]` table.
///
/// Credentials are conspicuously absent: the API token comes from the
/// environment named by `credentials_env` and nowhere else (SPEC.md
/// section 14).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  /// The environment variable holding the console's API token.
  pub credentials_env:    String,
  /// The console's address and the Access API port. The default is the
  /// address a fresh console ships with; a deployment should name its
  /// own.
  pub base_url:           Option<String>,
  /// Collect user groups and their membership. On by default — it is
  /// one call per group, and the alternative is an overlay that cannot
  /// answer a group question at all.
  pub groups:             bool,
  /// Read the console's door-opening log, so each account carries the
  /// date it last opened a door. On by default, for the same reason as
  /// `groups`: without it the overlay cannot answer a dormancy question
  /// on this system at all.
  ///
  /// It is the one read whose cost follows door traffic rather than
  /// headcount — a busy building is many pages — so a console that is
  /// swept often and read hard is the one to turn this off on.
  pub unlock_activity:    bool,
  /// How far back the door-opening log is read, in days. The window is
  /// also what a null `last_unlock_at` means: not "never", but "not in
  /// this many days", which is why it reaches the overlay beside it.
  pub unlock_window_days: u32,
  /// A PEM file holding the console's CA certificate chain, so the host
  /// can trust a console signed by a private CA. Absent means the public
  /// roots only, which is what a console behind a properly issued
  /// certificate needs.
  pub ca_cert:            Option<String>,
  /// Stop verifying the console's certificate. Off by default.
  ///
  /// A last resort for a console that presents a self-signed leaf
  /// `rustls` will not accept as a trust anchor and offers no CA to pin.
  /// `ca_cert` is strictly better when a certificate exists to trust;
  /// this only gives up knowing which host answered, not read-only.
  pub tls_insecure:       bool,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      credentials_env:    "OVERLORD_UNIFI_ACCESS_TOKEN".to_owned(),
      base_url:           None,
      groups:             true,
      unlock_activity:    true,
      unlock_window_days: DEFAULT_UNLOCK_WINDOW_DAYS,
      ca_cert:            None,
      tls_insecure:       false,
    }
  }
}

impl Config {
  fn read(ctx: &ObserveCtx) -> Result<Self, ConnectorError> {
    if ctx.config.is_null() {
      return Ok(Self::default());
    }
    serde_json::from_value(ctx.config.clone())
      .map_err(|e| ConnectorError::Config(format!("unifi-access: {e}")))
  }
}

#[derive(Debug, Default)]
pub struct UnifiAccessConnector {
  /// Set only by [`Self::with_token`]; see that constructor.
  token: Option<String>,
}

impl UnifiAccessConnector {
  /// The connector as the binary registers it: the API token comes
  /// from the environment variable the system's configuration names,
  /// and from nowhere else (SPEC.md section 14).
  #[must_use]
  pub fn new() -> Self { Self { token: None } }

  /// The connector with a token supplied directly, for tests.
  ///
  /// It exists because a test cannot set an environment variable at
  /// all: `std::env::set_var` is unsafe in this edition and
  /// `unsafe_code` is forbidden across the workspace. No configuration
  /// path reaches this — a `[[systems]]` entry names a connector, and
  /// the binary's registry calls [`Self::new`] — so the environment
  /// remains the only way a token gets into a real deployment.
  #[must_use]
  pub fn with_token(token: impl Into<String>) -> Self {
    Self {
      token: Some(token.into()),
    }
  }

  #[must_use]
  pub fn boxed() -> Box<dyn Connector> { Box::new(Self::new()) }

  /// The console's API token, from the environment unless a test put
  /// one here.
  fn token(&self, cfg: &Config) -> Result<String, ConnectorError> {
    if let Some(token) = &self.token {
      return Ok(token.clone());
    }
    let var = &cfg.credentials_env;
    let value = std::env::var(var).map_err(|_| {
      ConnectorError::Config(format!(
        "{var} is not set; the UniFi Access API token comes from the \
         environment, never from the configuration file"
      ))
    })?;
    let value = value.trim().to_owned();
    if value.is_empty() {
      return Err(ConnectorError::Config(format!(
        "{var} is empty; generate an API token on the console under Access → \
         Settings → General → Advanced"
      )));
    }
    Ok(value)
  }
}

#[async_trait]
impl Connector for UnifiAccessConnector {
  fn name(&self) -> &'static str { "unifi-access" }

  fn system_kind(&self) -> SystemKind { SystemKind::Sso }

  /// Five read endpoints, each with the reason it is needed. This is
  /// the whole of what the connector can reach; every mutating endpoint
  /// the developer API offers — unlock, assign, delete — fails closed
  /// because no allowlist entry names it.
  ///
  /// The system log is the one `POST`, and it is a read: the topic and
  /// the time window are a filter document the vendor takes in the
  /// body. `ReadMethod` still has no mutating variant, so the `PUT`
  /// that opens a door remains unnameable.
  fn allowlist(&self) -> Vec<Allow> {
    vec![
      Allow::get(USERS_PATH, "enumerate accounts and their access"),
      Allow::get(DOORS_PATH, "name the doors a policy grants"),
      Allow::get(USER_GROUPS_PATH, "enumerate user groups"),
      Allow::get(
        &format!("{USER_GROUPS_PATH}/*/users/all"),
        "group membership, for role checks",
      ),
      Allow::post(
        SYSTEM_LOGS_PATH,
        "door openings, to date each account's last unlock",
      ),
    ]
  }

  fn base_url(&self, ctx: &ObserveCtx) -> String {
    Config::read(ctx)
      .ok()
      .and_then(|c| c.base_url)
      .unwrap_or_else(|| DEFAULT_BASE.to_owned())
  }

  fn default_ruleset(&self, _: &ObserveCtx) -> Ruleset {
    serde_json::from_str(include_str!("ruleset.json"))
      .expect("the shipped UniFi Access ruleset must parse")
  }

  /// Trust the console's own CA when the operator names one.
  ///
  /// A console serves the API with a certificate signed by its own CA,
  /// which no public root vouches for. Naming that CA is the narrow fix;
  /// there is no option to skip verification.
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
    if cfg.tls_insecure {
      warn!(
        system = %ctx.system,
        "TLS verification is disabled for this system (tls_insecure = true)"
      );
    }
    http.set_bearer(self.token(&cfg)?);

    let mut incomplete: Vec<String> = Vec::new();

    // Accounts. An enumeration that produced nothing at all is a
    // failed system rather than a console that lost everybody.
    ctx.progress.say("reading accounts");
    let users = api::users(http, &ctx.progress).await;
    api::require_something(&users, "accounts")?;
    if let Some(reason) = &users.incomplete {
      incomplete.push(reason.clone());
    }
    info!(
      system = %ctx.system,
      accounts = users.items.len(),
      "access directory read"
    );

    // Doors name the resources a policy grants. A failure here degrades
    // the snapshot rather than failing it: entitlement is still known
    // from the policy resources, only the names are lost.
    ctx.progress.say("reading doors");
    let doors = api::doors(http, &ctx.progress).await;
    if let Some(reason) = &doors.incomplete {
      incomplete.push(format!(
        "{reason}; granted doors are reported by id without names"
      ));
    }
    let door_names = door_names(&doors);

    // An account holding policy ids the console did not expand has
    // entitlements this read cannot see. Its `doors` is omitted rather
    // than reported empty, and the snapshot says so: an overlay that
    // believed every account could open nothing would resolve every
    // door violation in the console at once.
    let unexpanded = users
      .items
      .iter()
      .filter(|u| policies_unreadable(u))
      .count();
    if unexpanded > 0 {
      warn!(
        system = %ctx.system,
        accounts = unexpanded,
        "the console returned access policy ids it did not expand"
      );
      incomplete.push(format!(
        "{unexpanded} of {} accounts carry access policy ids the console did \
         not expand (expand[]=access_policy); their granted doors are \
         unknown, not empty",
        users.items.len()
      ));
    }

    let (memberships, group_problems) = if cfg.groups {
      collect_groups(http, &ctx.progress).await
    } else {
      (None, Vec::new())
    };
    incomplete.extend(group_problems);

    // The door-opening log. Unlike the reads above it is withheld
    // whole when anything goes wrong, because a half-read log looks
    // exactly like a console where half the badges stopped being used.
    let (unlocks, unlock_problems) = if cfg.unlock_activity {
      collect_unlocks(http, &cfg, ctx, &door_names).await
    } else {
      (None, Vec::new())
    };
    incomplete.extend(unlock_problems);
    if let Some(unlocks) = &unlocks {
      info!(
        system = %ctx.system,
        accounts = unlocks.last.len(),
        window_days = unlocks.window_days,
        "door openings read"
      );
    }

    let observations = users
      .items
      .iter()
      .map(|user| {
        Observation::new(envelope(
          user,
          memberships.as_ref(),
          &door_names,
          unlocks.as_ref(),
        ))
      })
      .collect();

    Ok(Snapshot {
      completeness: if incomplete.is_empty() {
        Completeness::Complete
      } else {
        // SPEC.md section 10: a partial snapshot never tombstones, and
        // the violations it touches are marked stale. A half-read group
        // or door list must not resolve every entitlement violation in
        // the console.
        Completeness::Partial {
          reason: incomplete.join("; "),
        }
      },
      observations,
      warnings: incomplete,
    })
  }
}

/// Build the raw payload for one account.
///
/// The vendor object is nested under `user` after its credential tokens
/// are stripped; the derived views a ruleset can name sit beside it.
fn envelope(
  user: &Value,
  memberships: Option<&BTreeMap<String, Vec<Value>>>,
  door_names: &BTreeMap<String, String>,
  unlocks: Option<&Unlocks>,
) -> Value {
  let mut out = serde_json::Map::new();
  out.insert("user".to_owned(), redacted_user(user));
  out.insert("has_pin".to_owned(), Value::Bool(has_pin(user)));
  // Present when the policies were readable, and then an empty list
  // means "no doors granted" — the resources are known from the
  // policies even when the door list could not be read, so a missing
  // name never costs an entitlement. Absent when the console sent
  // policy ids it did not expand: that account's doors are unknown, and
  // `[]` would say it can open nothing.
  if let Some(doors) = user_doors(user, door_names) {
    out.insert("doors".to_owned(), Value::Array(doors));
  }

  // Absent when not collected, empty when collected and empty: null is
  // not "no groups".
  if let Some(by_user) = memberships {
    let id = user.get("id").and_then(Value::as_str).unwrap_or_default();
    out.insert(
      "groups".to_owned(),
      Value::Array(by_user.get(id).cloned().unwrap_or_default()),
    );
  }

  // A date has no absent form an overlay can read — a field that was
  // never collected and one that was collected and found nothing both
  // arrive as null — so the knownness is carried as its own boolean
  // rather than inferred from the date. It is always present, so a
  // check can lean on it.
  out.insert(
    "unlock_activity_known".to_owned(),
    Value::Bool(unlocks.is_some()),
  );
  if let Some(unlocks) = unlocks {
    // What the null means, for a check that has to say how stale is
    // stale. Beyond this horizon the log was not asked about.
    out.insert("unlock_window_days".to_owned(), json!(unlocks.window_days));
    let id = user.get("id").and_then(Value::as_str).unwrap_or_default();
    if let Some(last) = unlocks.last.get(id) {
      out.insert("last_unlock".to_owned(), last.clone());
    }
  }

  Value::Object(out)
}

/// The vendor user object with every issued credential's secret removed.
///
/// `pin_code.token` and `nfc_cards[].token` are door credentials, not
/// console API credentials, but they are still secrets and the fact
/// stream keeps everything in plaintext forever (SPEC.md section 2).
/// The PIN's *presence* survives as `has_pin`; a card keeps its id and
/// type so a check can count or name it.
fn redacted_user(user: &Value) -> Value {
  let Some(obj) = user.as_object() else {
    return user.clone();
  };
  let mut out = serde_json::Map::new();
  for (key, value) in obj {
    match key.as_str() {
      // Replaced wholesale by the `has_pin` boolean beside `user`.
      "pin_code" => {}
      "nfc_cards" => {
        out.insert(key.clone(), strip_card_tokens(value));
      }
      _ => {
        out.insert(key.clone(), value.clone());
      }
    }
  }
  Value::Object(out)
}

/// Drop `token` from each card in an NFC card list, keeping the rest.
fn strip_card_tokens(cards: &Value) -> Value {
  let Some(list) = cards.as_array() else {
    return cards.clone();
  };
  Value::Array(
    list
      .iter()
      .map(|card| match card.as_object() {
        Some(obj) => {
          let mut card = obj.clone();
          card.remove("token");
          Value::Object(card)
        }
        None => card.clone(),
      })
      .collect(),
  )
}

/// Whether a PIN is assigned, without keeping the PIN's token.
///
/// `pin_code` is the one field whose shape the console varies: some
/// send `{"token": "…"}` and some send the token as a bare string.
/// Reading only the object shape made `has_pin` silently `false` on the
/// other one — and since [`redacted_user`] drops the field whatever its
/// shape, that boolean is the only record of the PIN there is. Anything
/// present and not empty counts.
fn has_pin(user: &Value) -> bool {
  match user.get("pin_code") {
    None | Some(Value::Null) => false,
    Some(Value::String(token)) => !token.trim().is_empty(),
    Some(Value::Object(pin)) => pin
      .get("token")
      .and_then(Value::as_str)
      .is_some_and(|token| !token.trim().is_empty()),
    Some(_) => true,
  }
}

/// Whether this account's entitlement cannot be answered from what the
/// console sent.
///
/// `expand[]=access_policy` is what folds a policy's `resources` onto
/// the user. A console that ignores it — or one whose expansion failed
/// — still sends `access_policy_ids`, so an account holding policy ids
/// with nothing expanded beside them has entitlements this read cannot
/// see. That is the one case where reporting no doors would be a lie
/// rather than a fact.
fn policies_unreadable(user: &Value) -> bool {
  let holds_ids = user
    .get("access_policy_ids")
    .and_then(Value::as_array)
    .is_some_and(|ids| !ids.is_empty());
  let expanded = user
    .get("access_policies")
    .and_then(Value::as_array)
    .is_some_and(|policies| !policies.is_empty());
  holds_ids && !expanded
}

/// The doors a user can reach, resolved from the resources on their
/// access policies and named from the console's door list where one was
/// read.
///
/// `None` when the account's policies could not be read at all; see
/// [`policies_unreadable`].
fn user_doors(
  user: &Value,
  door_names: &BTreeMap<String, String>,
) -> Option<Vec<Value>> {
  if policies_unreadable(user) {
    return None;
  }

  let mut by_id: BTreeMap<String, (Option<String>, Option<String>)> =
    BTreeMap::new();

  let Some(policies) = user.get("access_policies").and_then(Value::as_array)
  else {
    return Some(Vec::new());
  };
  for policy in policies {
    let Some(resources) = policy.get("resources").and_then(Value::as_array)
    else {
      continue;
    };
    for resource in resources {
      let Some(id) = resource.get("id").and_then(Value::as_str) else {
        continue;
      };
      let kind = resource
        .get("type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
      let name = door_names.get(id).cloned().or_else(|| {
        resource
          .get("name")
          .and_then(Value::as_str)
          .map(ToOwned::to_owned)
      });
      by_id.entry(id.to_owned()).or_insert((name, kind));
    }
  }

  Some(
    by_id
      .into_iter()
      .map(
        |(id, (name, type_))| json!({ "id": id, "name": name, "type": type_ }),
      )
      .collect(),
  )
}

fn door_names(doors: &api::Paged) -> BTreeMap<String, String> {
  doors
    .items
    .iter()
    .filter_map(|door| {
      let id = door.get("id").and_then(Value::as_str)?;
      let name = door
        .get("full_name")
        .or_else(|| door.get("name"))
        .and_then(Value::as_str)?;
      Some((id.to_owned(), name.to_owned()))
    })
    .collect()
}

/// The door-opening log, reduced to the one fact per account an overlay
/// asks for.
///
/// The log itself is not kept. It is a stream of building traffic —
/// every badge tap, every door, every hour — and §2 keeps the fact
/// stream forever in plaintext; recording it per sweep would turn
/// overlord into a movement log of the people who work there. What
/// survives is the most recent opening per account, which is what a
/// dormancy question needs and the least that answers it.
#[derive(Debug)]
struct Unlocks {
  /// How far back the log was read. A missing entry below means "no
  /// opening in this many days", not "never".
  window_days: u32,
  /// Account id → its most recent opening within the window.
  last:        BTreeMap<String, Value>,
}

/// Read the door-opening log and reduce it to a last opening per
/// account.
///
/// Returns `None` for the view whenever the log is anything less than
/// whole, and a reason beside it. This is stricter than the group and
/// door reads, deliberately: those degrade a name or a list, and an
/// overlay missing a group name still knows the account exists. A
/// partial log degrades the *meaning* of every account it did not
/// reach, because "not in these pages" and "did not open a door" are
/// indistinguishable once the pages are gone — and the check the field
/// exists for fires on the null.
///
/// The window ends at the sweep's `started_at` rather than at a wall
/// clock: SPEC.md section 13 gives a sweep one clock, and a connector
/// that read its own would make two sweeps of the same console
/// disagree about what "90 days" was.
async fn collect_unlocks(
  http: &RestrictedHttp,
  cfg: &Config,
  ctx: &ObserveCtx,
  door_names: &BTreeMap<String, String>,
) -> (Option<Unlocks>, Vec<String>) {
  let until = ctx.started_at;
  let since = until.minus_days(i64::from(cfg.unlock_window_days));
  ctx.progress.say("reading door openings");
  let log = api::door_openings(
    http,
    since.as_jiff().as_second(),
    until.as_jiff().as_second(),
    &ctx.progress,
  )
  .await;

  if let Some(reason) = &log.incomplete {
    warn!(
      system = %ctx.system,
      reason,
      "the door-opening log was not read whole; last-unlock dates are withheld"
    );
    return (None, vec![format!(
      "{reason}; every account's last unlock is withheld, because a partial \
       log cannot tell an account that opened no door from one whose openings \
       were in the pages that did not arrive"
    )]);
  }

  let mut last: BTreeMap<String, Value> = BTreeMap::new();
  let mut at_by_actor: BTreeMap<String, Timestamp> = BTreeMap::new();
  let mut undated = 0_usize;

  for hit in &log.items {
    // The log wraps each entry in the search index's `_source`; a
    // console that sends the entry bare is read the same way.
    let event = hit.get("_source").unwrap_or(hit);
    if !opened(event) {
      continue;
    }
    let Some(actor) = event
      .get("actor")
      .and_then(|a| a.get("id"))
      .and_then(Value::as_str)
      .filter(|id| !id.is_empty())
    else {
      continue;
    };
    let Some(at) = opened_at(event) else {
      undated += 1;
      continue;
    };
    // Newest wins, by comparison rather than by position. A console
    // that answers newest-first would make the first entry per actor
    // the right one, but that ordering is the vendor's to change and
    // nothing announces it when it does — and the way that failure
    // would show up is as a wrong date nobody has reason to doubt.
    if at_by_actor.get(actor).is_some_and(|seen| *seen >= at) {
      continue;
    }
    at_by_actor.insert(actor.to_owned(), at);
    last.insert(
      actor.to_owned(),
      json!({
        "at":     at.to_string(),
        "door":   opened_door(event, door_names),
        "via":    event.pointer("/authentication/credential_provider"),
        "result": event.pointer("/event/result"),
      }),
    );
  }

  let mut problems = Vec::new();
  if undated > 0 {
    // A dated log is the whole point; entries without a usable time
    // are dropped rather than guessed at, and the sweep says how many.
    warn!(
      system = %ctx.system,
      entries = undated,
      "door openings carried no readable time"
    );
    problems.push(format!(
      "{undated} of {} log entries carried no readable time and were not \
       counted towards a last unlock",
      log.items.len()
    ));
  }

  (
    Some(Unlocks {
      window_days: cfg.unlock_window_days,
      last,
    }),
    problems,
  )
}

/// Whether this log entry is a door that opened, rather than one that
/// refused to.
///
/// The topic records both: a refused badge is an access event too. Only
/// an opening is activity — a deactivated card tapped daily at a door
/// that never opens is not a sign of use.
///
/// The test is for a refusal rather than for a grant, because the
/// wording of a success is the vendor's to change and the cost of the
/// two mistakes is not symmetric. Counting an unknown refusal as an
/// opening makes one account look active; failing to recognize a
/// renamed success would make every account in the building look
/// dormant at once.
fn opened(event: &Value) -> bool {
  let result = event
    .pointer("/event/result")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_ascii_uppercase();
  !["DENIED", "DENY", "BLOCK", "REJECT", "FAIL"]
    .iter()
    .any(|refusal| result.contains(refusal))
}

/// When the opening happened.
///
/// `@timestamp` is preferred because it is already ISO-8601 and needs
/// no unit guessed. `event.published` is the fallback and is an epoch
/// integer: consoles send it in milliseconds, but the field is bare and
/// a build that sends seconds would otherwise date every opening to
/// 1970. The two are ten orders of magnitude apart, so the magnitude
/// settles it — anything that would land before 1973 read as
/// milliseconds is seconds.
fn opened_at(event: &Value) -> Option<Timestamp> {
  if let Some(at) = event
    .get("@timestamp")
    .and_then(Value::as_str)
    .and_then(|s| s.parse::<Timestamp>().ok())
  {
    return Some(at);
  }
  /// 1973-03-03 as milliseconds; below it, the number is seconds.
  const SECONDS_BELOW: i64 = 100_000_000_000;
  let published = event.pointer("/event/published").and_then(Value::as_i64)?;
  let millis = if published.abs() < SECONDS_BELOW {
    published.checked_mul(1000)?
  } else {
    published
  };
  Timestamp::from_unix_millis(millis)
}

/// Which door opened, named from the console's door list where one was
/// read and from the log entry's own label otherwise.
///
/// The entry's targets are the door, the reader, and sometimes the
/// device; only the door is an entitlement, so only the door is kept.
fn opened_door(event: &Value, door_names: &BTreeMap<String, String>) -> Value {
  let Some(door) =
    event
      .get("target")
      .and_then(Value::as_array)
      .and_then(|targets| {
        targets
          .iter()
          .find(|t| t.get("type").and_then(Value::as_str) == Some("door"))
      })
  else {
    return Value::Null;
  };
  let id = door.get("id").and_then(Value::as_str);
  let name = id.and_then(|id| door_names.get(id).cloned()).or_else(|| {
    door
      .get("display_name")
      .and_then(Value::as_str)
      .map(ToOwned::to_owned)
  });
  json!({ "id": id, "name": name })
}

/// Enumerate groups and invert their membership onto accounts.
///
/// Returns `None` for the membership map when groups could not be read
/// at all, so [`envelope`] can tell "not collected" from "collected and
/// empty".
async fn collect_groups(
  http: &RestrictedHttp,
  progress: &overlord_connect::Progress,
) -> (Option<BTreeMap<String, Vec<Value>>>, Vec<String>) {
  let mut problems = Vec::new();
  progress.say("reading groups");
  let groups = api::user_groups(http, progress).await;
  if let Some(reason) = &groups.incomplete {
    problems.push(reason.clone());
    if groups.is_empty() {
      // Nothing was read, so there is no membership to report. Saying
      // "no groups" here would resolve every group-based violation at
      // once.
      return (None, problems);
    }
  }

  let mut by_user: BTreeMap<String, Vec<Value>> = BTreeMap::new();
  let total = groups.items.len() as u64;
  for (index, group) in groups.items.iter().enumerate() {
    let Some(id) = group.get("id").and_then(Value::as_str) else {
      continue;
    };
    // One call per group, so this is the read that dominates a console
    // with many groups; each is reported before it is made.
    progress.counted(
      format!("group {} of {total}", index + 1),
      (index + 1) as u64,
      Some(total),
    );
    let members = api::group_members(http, id, progress).await;
    if let Some(reason) = &members.incomplete {
      problems.push(reason.clone());
    }
    let group_id = group.get("id");
    let group_name = group.get("name");
    let group_full_name = group.get("full_name");
    for member in &members.items {
      let Some(user_id) = member.get("id").and_then(Value::as_str) else {
        continue;
      };
      by_user.entry(user_id.to_owned()).or_default().push(json!({
        "id":        group_id,
        "name":      group_name,
        "full_name": group_full_name,
      }));
    }
  }

  (Some(by_user), problems)
}

#[cfg(test)]
mod tests {
  use overlord_connect::ReadMethod;
  use overlord_core::{SystemId, Timestamp, Value as CoreValue};

  use super::*;

  fn ctx(config: Value) -> ObserveCtx {
    ObserveCtx {
      system: SystemId::new("access-hq"),
      started_at: "2026-01-15T00:00:00Z".parse::<Timestamp>().unwrap(),
      config,
      progress: overlord_connect::Progress::default(),
    }
  }

  fn sample_user() -> Value {
    json!({
      "id": "u-1001",
      "first_name": "Ada",
      "last_name": "Lovelace",
      "full_name": "Ada Lovelace",
      "user_email": "ada@example.com",
      "employee_number": "E-4471",
      "status": "ACTIVE",
      "onboard_time": 1689304925,
      "pin_code": { "token": "pin-secret" },
      "nfc_cards": [
        { "id": "card-1", "token": "nfc-secret", "type": "ua_card" }
      ],
      "access_policy_ids": ["p-1"],
      "access_policies": [
        { "id": "p-1", "name": "Lab Access",
          "resources": [{ "id": "door-1", "type": "door" }] }
      ]
    })
  }

  #[test]
  fn the_shipped_ruleset_parses_and_names_every_identity_signal() {
    // Identity runs off the overlay (PROGRESS.md, M4 handoff), so a
    // connector that maps none of these is only ever matched by its
    // key, and its key is a console id nobody recognizes.
    let rs = UnifiAccessConnector::new().default_ruleset(&ctx(Value::Null));
    assert!(rs.fields.contains_key("email"));
    assert!(rs.fields.contains_key("username"));
    assert!(rs.fields.contains_key("employee_id"));
    assert_eq!(rs.system_kind, SystemKind::Sso);
  }

  #[test]
  fn credential_tokens_never_reach_the_fact_stream() {
    let raw = envelope(&sample_user(), None, &BTreeMap::new(), None);
    let text = raw.to_string();
    assert!(!text.contains("pin-secret"), "{text}");
    assert!(!text.contains("nfc-secret"), "{text}");
    // The card itself survives, minus its secret.
    assert_eq!(raw["user"]["nfc_cards"][0]["id"], json!("card-1"));
    assert_eq!(raw["has_pin"], json!(true));
  }

  #[test]
  fn a_pin_is_found_whatever_shape_the_console_sends() {
    // The object shape was the only one read, so a console sending the
    // token as a bare string reported every account as having no PIN —
    // and `redacted_user` drops the field either way, so the boolean is
    // the only record there is.
    let cases = [
      (json!({ "token": "123456" }), true),
      (json!("123456"), true),
      (json!({ "token": "" }), false),
      (json!(""), false),
      (json!("   "), false),
      (Value::Null, false),
    ];
    for (pin, expected) in cases {
      let user = json!({ "id": "u-1", "pin_code": pin.clone() });
      assert_eq!(has_pin(&user), expected, "{pin}");
      // Whatever the shape, the token itself never survives.
      assert!(
        envelope(&user, None, &BTreeMap::new(), None)["user"]["pin_code"]
          .is_null()
      );
    }
    assert!(!has_pin(&json!({ "id": "u-1" })));
  }

  #[test]
  fn an_unexpanded_policy_is_unknown_access_not_no_access() {
    // A console that ignores `expand[]=access_policy` still sends the
    // ids. Reporting `doors: []` there would say the account can open
    // nothing, which is the opposite of what the ids mean.
    let user = json!({
      "id": "u-1",
      "status": "ACTIVE",
      "access_policy_ids": ["p-1"]
    });
    assert!(policies_unreadable(&user));
    let raw = envelope(&user, None, &BTreeMap::new(), None);
    assert!(raw.get("doors").is_none(), "{raw}");

    // An account with no policies at all is genuinely granted nothing,
    // and that is an empty list rather than an absence.
    let none = json!({ "id": "u-2", "access_policy_ids": [] });
    assert!(!policies_unreadable(&none));
    assert_eq!(
      envelope(&none, None, &BTreeMap::new(), None)["doors"],
      json!([])
    );
  }

  #[test]
  fn a_granted_door_is_named_from_the_door_list() {
    let names: BTreeMap<String, String> =
      [("door-1".to_owned(), "Front Door".to_owned())]
        .into_iter()
        .collect();
    let raw = envelope(&sample_user(), None, &names, None);
    assert_eq!(raw["doors"][0]["id"], json!("door-1"));
    assert_eq!(raw["doors"][0]["name"], json!("Front Door"));
    assert_eq!(raw["doors"][0]["type"], json!("door"));
  }

  #[test]
  fn the_allowlist_is_the_whole_of_what_the_connector_can_reach() {
    let c = UnifiAccessConnector::new();
    let http = c.http(&ctx(Value::Null)).unwrap();

    assert!(http.permits(ReadMethod::Get, USERS_PATH));
    assert!(http.permits(ReadMethod::Get, DOORS_PATH));
    assert!(http.permits(ReadMethod::Get, USER_GROUPS_PATH));
    assert!(http.permits(
      ReadMethod::Get,
      "/api/v1/developer/user_groups/g-1/users/all"
    ));
    // The log is a read that travels as a POST; it is allowed as one,
    // and only as one.
    assert!(http.permits(ReadMethod::Post, SYSTEM_LOGS_PATH));
    assert!(!http.permits(ReadMethod::Get, SYSTEM_LOGS_PATH));

    // Everything the developer API offers that overlord did not ask
    // for. Unlocking a door is a PUT, which `ReadMethod` cannot even
    // name; these are the GETs that would also be refused.
    assert!(!http.permits(ReadMethod::Get, "/api/v1/developer/users/u-1001"));
    assert!(!http.permits(
      ReadMethod::Get,
      "/api/v1/developer/credentials/nfc_cards/tokens"
    ));
    assert!(
      !http.permits(ReadMethod::Get, "/api/v1/developer/access_policies")
    );
    assert!(!http.permits(ReadMethod::Post, USERS_PATH));
  }

  #[test]
  fn the_ruleset_maps_a_console_user_onto_the_overlay() {
    let rs = UnifiAccessConnector::new().default_ruleset(&ctx(Value::Null));
    let n = rs
      .apply(
        &SystemId::new("access-hq"),
        &envelope(&sample_user(), None, &BTreeMap::new(), None),
      )
      .unwrap();
    assert!(n.warnings.is_empty(), "{:?}", n.warnings);
    let r = &n.record;
    assert_eq!(r.entity_key.as_str(), "u-1001");
    assert_eq!(r.display_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(r.status, overlord_core::EntityStatus::Active);
    assert_eq!(
      r.get("email"),
      CoreValue::String("ada@example.com".to_owned())
    );
    assert_eq!(r.get("username"), CoreValue::String("ada".to_owned()));
    assert_eq!(r.get("employee_id"), CoreValue::String("E-4471".to_owned()));
    assert_eq!(r.get("has_pin"), CoreValue::Bool(true));
    assert_eq!(r.get("doors").type_name(), "list");
    // Not collected is null, and null is not "no groups".
    assert_eq!(r.get("groups"), CoreValue::Null);
    // Nor "no unlock": the boolean is what says the log was not read.
    assert_eq!(r.get("unlock_activity_known"), CoreValue::Bool(false));
    assert_eq!(r.get("last_unlock_at"), CoreValue::Null);
  }

  #[test]
  fn an_account_with_no_opening_in_the_window_is_known_and_dateless() {
    // The distinction the boolean exists for: the log *was* read, and
    // this account is not in it. That is a finding, not a gap.
    let unlocks = Unlocks {
      window_days: 90,
      last:        BTreeMap::new(),
    };
    let raw = envelope(&sample_user(), None, &BTreeMap::new(), Some(&unlocks));
    assert_eq!(raw["unlock_activity_known"], json!(true));
    assert!(raw.get("last_unlock").is_none(), "{raw}");

    let rs = UnifiAccessConnector::new().default_ruleset(&ctx(Value::Null));
    let r = rs.apply(&SystemId::new("access-hq"), &raw).unwrap().record;
    assert_eq!(r.get("unlock_activity_known"), CoreValue::Bool(true));
    assert_eq!(r.get("last_unlock_at"), CoreValue::Null);
    assert_eq!(r.get("unlock_window_days").type_name(), "number");
  }

  #[test]
  fn a_refused_badge_is_not_an_opening() {
    // A deactivated card tapped daily at a door that never opens is
    // not a sign of use.
    for result in [
      "ACCESS_DENIED",
      "access_denied",
      "BLOCKED",
      "REJECTED",
      "AUTH_FAILED",
    ] {
      assert!(
        !opened(&json!({ "event": { "result": result } })),
        "{result}"
      );
    }
    // And a success — including a wording the vendor has not invented
    // yet — is. Failing open here costs one account that looks active;
    // failing closed would make the whole building look dormant.
    for result in ["ACCESS_GRANTED", "UNLOCKED", "SOMETHING_NEW", ""] {
      assert!(
        opened(&json!({ "event": { "result": result } })),
        "{result}"
      );
    }
    assert!(opened(&json!({ "actor": { "id": "u-1" } })));
  }

  #[test]
  fn an_opening_is_dated_from_the_string_or_from_the_epoch_beside_it() {
    let want: Timestamp = "2026-01-09T23:06:40Z".parse().unwrap();

    // The ISO string wins when it is there.
    assert_eq!(
      opened_at(&json!({
        "@timestamp": "2026-01-09T23:06:40Z",
        "event": { "published": 1_768_000_000_000_i64 }
      })),
      Some(want)
    );
    // Milliseconds, the shape consoles send.
    assert_eq!(
      opened_at(&json!({ "event": { "published": 1_768_000_000_000_i64 } })),
      Some(want)
    );
    // Seconds, from a build that sends the field bare. Reading this as
    // milliseconds would date every opening to 1970 and call the whole
    // console dormant.
    assert_eq!(
      opened_at(&json!({ "event": { "published": 1_768_000_000_i64 } })),
      Some(want)
    );
    // An entry with no time at all is dropped rather than guessed at.
    assert_eq!(opened_at(&json!({ "actor": { "id": "u-1" } })), None);
    assert_eq!(opened_at(&json!({ "@timestamp": "not a time" })), None);
  }

  #[test]
  fn an_opening_names_its_door_and_ignores_the_reader_beside_it() {
    let names: BTreeMap<String, String> =
      [("door-1".to_owned(), "Front Door".to_owned())]
        .into_iter()
        .collect();
    let event = json!({
      "target": [
        { "id": "dev-9", "type": "device", "display_name": "Reader 9" },
        { "id": "door-1", "type": "door", "display_name": "stale name" }
      ]
    });
    // The door list is the naming authority; the log's own label is the
    // fallback for a door the list did not carry.
    assert_eq!(
      opened_door(&event, &names),
      json!({ "id": "door-1", "name": "Front Door" })
    );
    assert_eq!(
      opened_door(&event, &BTreeMap::new()),
      json!({ "id": "door-1", "name": "stale name" })
    );
    assert_eq!(opened_door(&json!({ "target": [] }), &names), Value::Null);
  }

  #[test]
  fn the_three_console_states_map_onto_the_five_overlord_knows() {
    let rs = UnifiAccessConnector::new().default_ruleset(&ctx(Value::Null));
    let cases = [
      ("ACTIVE", overlord_core::EntityStatus::Active),
      ("PENDING", overlord_core::EntityStatus::Invited),
      ("DEACTIVATED", overlord_core::EntityStatus::Deprovisioned),
      ("WHATEVER", overlord_core::EntityStatus::Unknown),
    ];
    for (raw, expected) in cases {
      let user = json!({ "id": "u-1", "status": raw });
      let n = rs
        .apply(
          &SystemId::new("access-hq"),
          &envelope(&user, None, &BTreeMap::new(), None),
        )
        .unwrap();
      assert_eq!(n.record.status, expected, "{raw}");
    }
  }

  #[test]
  fn no_ca_certificate_means_no_extra_roots() {
    let c = UnifiAccessConnector::new();
    assert!(c.root_certificates(&ctx(Value::Null)).unwrap().is_empty());
    assert!(
      c.root_certificates(&ctx(json!({ "groups": false })))
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn a_named_ca_that_cannot_be_read_is_an_error_not_an_empty_trust() {
    // Silently trusting nothing would turn a typo into the same
    // undiagnosable transport failure the option exists to fix.
    let c = UnifiAccessConnector::new();
    let err = c
      .root_certificates(&ctx(json!({ "ca_cert": "/no/such/ca.pem" })))
      .unwrap_err();
    assert!(matches!(err, ConnectorError::Config(_)), "{err:?}");
  }

  #[test]
  fn verification_is_on_unless_explicitly_turned_off() {
    let c = UnifiAccessConnector::new();
    assert!(!c.accept_invalid_certificates(&ctx(Value::Null)));
    assert!(
      !c.accept_invalid_certificates(&ctx(json!({ "tls_insecure": false })))
    );
    assert!(
      c.accept_invalid_certificates(&ctx(json!({ "tls_insecure": true })))
    );
  }
}
