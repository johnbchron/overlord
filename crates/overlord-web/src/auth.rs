//! Who is asking, and whether they may (SPEC.md section 14).
//!
//! Every command records an authenticated `actor`, so a handler cannot
//! reach the store without one: [`Identity`] is an axum extractor, and a
//! request with no valid session never reaches a handler that takes it.
//! That is the whole enforcement mechanism — there is no "check the
//! session" call for a handler to forget.
//!
//! The session itself is a signed cookie. There is no server-side
//! session table because there is nothing in a session worth storing:
//! the subject, a display label, and an expiry, authenticated with a
//! keyed hash so the browser cannot edit any of them.

use std::{
  collections::BTreeSet,
  net::{IpAddr, SocketAddr},
  sync::LazyLock,
};

use axum::{
  extract::FromRequestParts,
  http::{StatusCode, header, request::Parts},
  response::{IntoResponse, Redirect, Response},
};
use overlord_core::{Actor, Timestamp};
use serde::{Deserialize, Serialize};

use crate::error::WebError;

/// The cookie the session travels in.
pub const COOKIE: &str = "overlord_session";

/// How long a session lasts before the operator signs in again.
const SESSION_HOURS: i64 = 12;

/// How the server decides who someone is.
#[derive(Debug, Clone)]
pub enum AuthMode {
  /// A fixed principal, for local work (`--dev-actor`). Refuses to bind
  /// a non-loopback address: this mode authenticates nobody, and the
  /// store holds personal data for the whole organization.
  Dev { actor: String },
  /// External OIDC, authorization-code with PKCE.
  Oidc(Box<OidcConfig>),
}

/// OIDC settings. The client secret comes from the environment, never
/// from the configuration file or the streams (SPEC.md section 14).
#[derive(Debug, Clone)]
pub struct OidcConfig {
  pub issuer:           String,
  pub client_id:        String,
  pub client_secret:    Option<String>,
  /// Where the provider sends the operator back. Must match what is
  /// registered with the provider exactly.
  pub redirect_url:     String,
  /// Authentication is not sufficient on its own (SPEC.md section 14):
  /// access needs a listed subject or the required group claim.
  pub allowed_subjects: BTreeSet<String>,
  pub required_group:   Option<String>,
}

impl OidcConfig {
  /// Whether a verified set of claims is allowed in.
  ///
  /// An empty allowlist with no required group would let the whole
  /// identity provider in, which for this store is never what anyone
  /// meant — so that configuration denies everyone and says so at
  /// startup rather than silently opening the door.
  #[must_use]
  pub fn admits(
    &self,
    subject: &str,
    email: Option<&str>,
    groups: &[String],
  ) -> bool {
    if self.allowed_subjects.contains(subject) {
      return true;
    }
    if let Some(email) = email
      && self.allowed_subjects.contains(email)
    {
      return true;
    }
    match &self.required_group {
      Some(g) => groups.iter().any(|have| have == g),
      None => false,
    }
  }

  /// Whether this configuration can admit anyone at all.
  #[must_use]
  pub fn admits_anyone(&self) -> bool {
    !self.allowed_subjects.is_empty() || self.required_group.is_some()
  }
}

/// The signed-in operator, as a handler sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
  /// What goes in the command stream's `actor` column.
  pub actor: Actor,
  /// What the masthead shows.
  pub label: String,
  /// A dev principal authenticated nobody, and the UI says so rather
  /// than implying a real sign-in.
  pub dev:   bool,
}

impl Identity {
  #[must_use]
  pub fn label(&self) -> String {
    if self.dev {
      format!("{} (dev)", self.label)
    } else {
      self.label.clone()
    }
  }

  /// Signing out means anything only where signing in did.
  #[must_use]
  pub fn can_sign_out(&self) -> bool { !self.dev }
}

/// The claims carried in the session cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
  sub:   String,
  label: String,
  /// Unix seconds. Checked on every request, so a stolen cookie stops
  /// working without any server-side state to expire.
  exp:   i64,
}

/// The key the session cookie is authenticated with.
///
/// Taken from `OVERLORD_SESSION_KEY` (64 hex characters) when set, so
/// sessions survive a restart and a rolling deployment. Without it a
/// fresh key is derived at startup and every existing session stops
/// validating — safe, and loud enough in the log to explain itself.
static SESSION_KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
  if let Ok(hex) = std::env::var("OVERLORD_SESSION_KEY")
    && let Some(key) = unhex(&hex)
    && key.len() == 32
  {
    let mut out = [0u8; 32];
    out.copy_from_slice(&key);
    return out;
  }
  tracing::warn!(
    "OVERLORD_SESSION_KEY is not set; deriving an ephemeral session key, so \
     sessions will not survive a restart"
  );
  let entropy = format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new());
  *blake3::hash(entropy.as_bytes()).as_bytes()
});

/// Build a cookie value for a session that expires `SESSION_HOURS` from
/// `now`.
///
/// # Errors
/// Only if the claims cannot be serialized, which the type makes
/// unreachable.
pub fn issue(
  sub: &str,
  label: &str,
  now: Timestamp,
) -> Result<String, WebError> {
  let session = Session {
    sub:   sub.to_owned(),
    label: label.to_owned(),
    exp:   unix_seconds(now) + SESSION_HOURS * 3600,
  };
  let body = serde_json::to_vec(&session)
    .map_err(|e| WebError::internal(format!("session: {e}")))?;
  let mac = blake3::keyed_hash(&SESSION_KEY, &body);
  Ok(format!("{}.{}", hex(&body), mac.to_hex()))
}

/// The `Set-Cookie` header value that installs a session.
#[must_use]
pub fn set_cookie(value: &str, secure: bool) -> String {
  // `SameSite=Lax` rather than `Strict`: the OIDC provider redirects the
  // operator back with a top-level GET, and `Strict` would drop the
  // cookie on exactly that navigation. `Lax` still withholds it from
  // cross-site POSTs, which is what protects the action handlers.
  let mut c = format!(
    "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
    SESSION_HOURS * 3600
  );
  if secure {
    c.push_str("; Secure");
  }
  c
}

/// The `Set-Cookie` header value that clears one.
#[must_use]
pub fn clear_cookie() -> String {
  format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Verify a cookie value and return its claims.
fn verify(value: &str, now: Timestamp) -> Option<Session> {
  let (body_hex, mac_hex) = value.split_once('.')?;
  let body = unhex(body_hex)?;
  let expected = blake3::keyed_hash(&SESSION_KEY, &body);
  let given = unhex(mac_hex)?;
  // `blake3::Hash` compares in constant time, which is the reason to
  // build one rather than comparing the bytes directly.
  if given.len() != 32 {
    return None;
  }
  let mut given_arr = [0u8; 32];
  given_arr.copy_from_slice(&given);
  if expected != blake3::Hash::from(given_arr) {
    return None;
  }
  let session: Session = serde_json::from_slice(&body).ok()?;
  (session.exp > unix_seconds(now)).then_some(session)
}

/// Extract the identity a request carries, without deciding what to do
/// about its absence.
#[must_use]
pub fn identity_of(
  parts: &Parts,
  mode: &AuthMode,
  now: Timestamp,
) -> Option<Identity> {
  match mode {
    AuthMode::Dev { actor } => Some(Identity {
      actor: Actor::new(format!("dev:{actor}")),
      label: actor.clone(),
      dev:   true,
    }),
    AuthMode::Oidc(_) => {
      let raw = cookie(parts, COOKIE)?;
      let session = verify(&raw, now)?;
      Some(Identity {
        actor: Actor::new(format!("oidc:{}", session.sub)),
        label: session.label,
        dev:   false,
      })
    }
  }
}

/// Read one cookie out of the request headers.
fn cookie(parts: &Parts, name: &str) -> Option<String> {
  parts
    .headers
    .get_all(header::COOKIE)
    .iter()
    .filter_map(|v| v.to_str().ok())
    .flat_map(|v| v.split(';'))
    .filter_map(|pair| pair.split_once('='))
    .find(|(k, _)| k.trim() == name)
    .map(|(_, v)| v.trim().to_owned())
}

/// A handler that takes an [`Identity`] cannot run without one.
impl<S> FromRequestParts<S> for Identity
where
  S: Send + Sync,
  AuthMode: axum::extract::FromRef<S>,
{
  type Rejection = Response;

  async fn from_request_parts(
    parts: &mut Parts,
    state: &S,
  ) -> Result<Self, Self::Rejection> {
    let mode = <AuthMode as axum::extract::FromRef<S>>::from_ref(state);
    identity_of(parts, &mode, Timestamp::now()).ok_or_else(|| {
      // An htmx fragment request that has lost its session must not swap
      // a login page into the middle of a table. `HX-Redirect` tells
      // htmx to navigate the whole window instead.
      if parts.headers.contains_key("hx-request") {
        (StatusCode::UNAUTHORIZED, [("hx-redirect", "/auth/login")])
          .into_response()
      } else {
        Redirect::to("/auth/login").into_response()
      }
    })
  }
}

/// Whether `--dev-actor` may serve this bind address.
///
/// PLAN.md section 4 item 18: a dev principal authenticates nobody, so
/// it is confined to loopback. Anything reachable from another machine
/// needs real OIDC.
#[must_use]
pub fn dev_actor_permits(bind: SocketAddr) -> bool {
  match bind.ip() {
    IpAddr::V4(v4) => v4.is_loopback(),
    IpAddr::V6(v6) => v6.is_loopback(),
  }
}

fn unix_seconds(t: Timestamp) -> i64 { t.as_jiff().as_second() }

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
  if !s.len().is_multiple_of(2) {
    return None;
  }
  (0..s.len())
    .step_by(2)
    .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
    .collect()
}

#[cfg(test)]
mod tests {
  use std::net::Ipv4Addr;

  use super::*;

  fn now() -> Timestamp { "2026-01-01T00:00:00Z".parse().unwrap() }

  #[test]
  fn a_session_round_trips() {
    let c = issue("sub-1", "Ada", now()).unwrap();
    let s = verify(&c, now()).unwrap();
    assert_eq!(s.sub, "sub-1");
    assert_eq!(s.label, "Ada");
  }

  #[test]
  fn an_edited_session_is_rejected() {
    let c = issue("sub-1", "Ada", now()).unwrap();
    let (body, mac) = c.split_once('.').unwrap();
    // Flip one nibble of the claims and keep the original signature.
    let mut edited = body.to_owned();
    let last = edited.pop().unwrap();
    edited.push(if last == 'a' { 'b' } else { 'a' });
    assert!(verify(&format!("{edited}.{mac}"), now()).is_none());
  }

  #[test]
  fn an_expired_session_is_rejected() {
    let c = issue("sub-1", "Ada", now()).unwrap();
    let later: Timestamp = "2026-01-02T00:00:00Z".parse().unwrap();
    assert!(verify(&c, later).is_none());
  }

  #[test]
  fn a_dev_actor_is_confined_to_loopback() {
    assert!(dev_actor_permits(SocketAddr::from((
      Ipv4Addr::LOCALHOST,
      8080
    ))));
    assert!(!dev_actor_permits(SocketAddr::from((
      Ipv4Addr::UNSPECIFIED,
      8080
    ))));
  }

  #[test]
  fn an_empty_allowlist_admits_nobody() {
    let cfg = OidcConfig {
      issuer:           "https://id.example.com".to_owned(),
      client_id:        "overlord".to_owned(),
      client_secret:    None,
      redirect_url:     "https://overlord.example.com/auth/callback".to_owned(),
      allowed_subjects: BTreeSet::new(),
      required_group:   None,
    };
    assert!(!cfg.admits_anyone());
    assert!(!cfg.admits("anyone", Some("a@example.com"), &[]));
  }

  #[test]
  fn a_required_group_admits_its_members_only() {
    let cfg = OidcConfig {
      issuer:           "https://id.example.com".to_owned(),
      client_id:        "overlord".to_owned(),
      client_secret:    None,
      redirect_url:     "https://overlord.example.com/auth/callback".to_owned(),
      allowed_subjects: BTreeSet::new(),
      required_group:   Some("secops".to_owned()),
    };
    assert!(cfg.admits("s", None, &["secops".to_owned()]));
    assert!(!cfg.admits("s", None, &["everyone".to_owned()]));
  }
}
