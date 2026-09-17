//! Getting an access token, without ever holding a writable one.
//!
//! Google offers two credential shapes, and overlord reads both exactly
//! as Google writes them — a service account key, and the
//! `authorized_user` file `gcloud auth application-default login`
//! leaves behind. Nothing is invented: an operator pastes the file they
//! already have.
//!
//! **A service account is the intended shape.** Workspace admin APIs
//! answer for a *user*, so a service account reads a tenant by
//! domain-wide delegation: the operator grants the service account's
//! client id a fixed list of scopes in the Admin console, and the
//! connector impersonates one administrator. The scopes granted there
//! are the whole of what overlord can do to that tenant, which is the
//! property worth having — see the crate docs for the list.
//!
//! The `authorized_user` shape is supported because it is what a person
//! debugging a tenant already has on their laptop. It ties the read to
//! that person's own account, so it is the wrong thing to run a
//! scheduled sweep as, and the crate docs say so.
//!
//! SPEC.md section 14: credentials come from the environment, never from
//! the configuration file and never from the streams. Nothing in this
//! module is ever written to a fact, a command, a log line, or a
//! `Debug` rendering.

use overlord_connect::{ConnectorError, RestrictedHttp};
use overlord_core::Timestamp;
use serde::{Deserialize, Serialize};

/// Google's token endpoint. Declared in the connector's allowlist like
/// every other endpoint it can reach.
pub const TOKEN_BASE: &str = "https://oauth2.googleapis.com/";
pub const TOKEN_PATH: &str = "/token";

/// The assertion grant a self-signed JWT is exchanged under.
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// How long a minted assertion is good for. Google caps this at an
/// hour; a sweep that outlives it will have finished reading long
/// before, because the token is fetched once at the start of the run.
const ASSERTION_SECS: i64 = 3600;

/// A Google credential file, in either shape Google writes.
///
/// `Deserialize` only: this type is never serialized, so there is no
/// path by which a private key reaches a log or a stream.
#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub enum Credential {
  /// A service account key. Reads a tenant by domain-wide delegation,
  /// impersonating one administrator.
  #[serde(rename = "service_account")]
  ServiceAccount {
    client_email: String,
    private_key:  String,
    #[serde(default)]
    token_uri:    Option<String>,
  },
  /// The file `gcloud auth application-default login` writes. Reads as
  /// the person who consented.
  #[serde(rename = "authorized_user")]
  AuthorizedUser {
    client_id:     String,
    client_secret: String,
    refresh_token: String,
  },
}

/// Deliberately says nothing. A credential's fields are secrets, and
/// this type is reachable from a connector that gets logged.
impl std::fmt::Debug for Credential {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Self::ServiceAccount { .. } => "Credential::ServiceAccount(redacted)",
      Self::AuthorizedUser { .. } => "Credential::AuthorizedUser(redacted)",
    })
  }
}

impl Credential {
  /// Read a credential from an environment variable holding either the
  /// JSON itself or a path to the file Google wrote.
  ///
  /// Both are accepted because both are how this is actually carried: a
  /// container gets the JSON in the environment, a host gets the file on
  /// disk. Neither may come from the configuration file (SPEC.md section
  /// 14), which is why this reads the environment and takes no argument
  /// but the variable's name.
  ///
  /// # Errors
  /// If the variable is unset, the file unreadable, or the JSON is
  /// neither shape Google writes. The message never quotes the value.
  pub fn from_env(var: &str) -> Result<Self, ConnectorError> {
    let value = std::env::var(var).map_err(|_| {
      ConnectorError::Config(format!(
        "{var} is not set; Google Workspace credentials come from the \
         environment, never from the configuration file"
      ))
    })?;
    let text = if value.trim_start().starts_with('{') {
      value
    } else {
      std::fs::read_to_string(value.trim()).map_err(|e| {
        // The path may itself be sensitive; report the variable, not it.
        ConnectorError::Config(format!(
          "{var} names a file that could not be read: {}",
          e.kind()
        ))
      })?
    };
    serde_json::from_str(&text).map_err(|e| {
      ConnectorError::Config(format!(
        "{var} is not a Google credential file ({}); expected \"type\": \
         \"service_account\" or \"authorized_user\"",
        e.classify_line_column()
      ))
    })
  }

  /// The subject a service account must impersonate, if this is one.
  #[must_use]
  pub fn needs_impersonation(&self) -> bool {
    matches!(self, Self::ServiceAccount { .. })
  }
}

/// Anything that can classify a serde error without echoing the input.
trait Classify {
  fn classify_line_column(&self) -> String;
}

impl Classify for serde_json::Error {
  /// The location and category only. A parse error's `Display` can
  /// contain the offending text, and the offending text here is a
  /// private key.
  fn classify_line_column(&self) -> String {
    format!(
      "{:?} at line {}, column {}",
      self.classify(),
      self.line(),
      self.column()
    )
  }
}

/// What Google's token endpoint returns. `expires_in` is read and
/// discarded: the token is fetched once per sweep and the sweep is over
/// long before it lapses, so nothing here caches across runs.
#[derive(Debug, Deserialize)]
struct TokenResponse {
  access_token: String,
}

#[derive(Serialize)]
struct Claims<'a> {
  iss:   &'a str,
  scope: String,
  aud:   &'a str,
  exp:   i64,
  iat:   i64,
  #[serde(skip_serializing_if = "Option::is_none")]
  sub:   Option<&'a str>,
}

/// Exchange a credential for an access token good for `scopes`.
///
/// `now` is the sweep's `started_at`, not a clock read: PLAN.md section
/// 5 allows the run exactly one, taken at the edge, and an assertion's
/// `iat`/`exp` are as happy with it as anything else.
///
/// # Errors
/// If the key will not parse, the assertion will not sign, or the token
/// endpoint refuses the grant.
pub async fn access_token(
  http: &RestrictedHttp,
  credential: &Credential,
  impersonate: Option<&str>,
  scopes: &[&str],
  now: Timestamp,
) -> Result<String, ConnectorError> {
  let form = match credential {
    Credential::ServiceAccount {
      client_email,
      private_key,
      token_uri,
    } => {
      let assertion = assertion(
        client_email,
        private_key,
        token_uri
          .as_deref()
          .unwrap_or("https://oauth2.googleapis.com/token"),
        impersonate,
        scopes,
        now,
      )?;
      vec![
        ("grant_type", JWT_BEARER.to_owned()),
        ("assertion", assertion),
      ]
    }
    Credential::AuthorizedUser {
      client_id,
      client_secret,
      refresh_token,
    } => vec![
      ("grant_type", "refresh_token".to_owned()),
      ("client_id", client_id.clone()),
      ("client_secret", client_secret.clone()),
      ("refresh_token", refresh_token.clone()),
    ],
  };

  let body = http.post_form(TOKEN_PATH, &form).await.map_err(|e| {
    // A 400 here is nearly always one of three things, and saying which
    // saves an operator an afternoon.
    match e {
      ConnectorError::Status { status: 400, .. } => ConnectorError::Config(
        "the token endpoint refused the grant: check that the service account \
         has domain-wide delegation for exactly the scopes below, that \
         `impersonate` names a real administrator, and that the credential \
         has not been revoked"
          .to_owned(),
      ),
      other => other,
    }
  })?;

  let token: TokenResponse = serde_json::from_value(body).map_err(|_| {
    ConnectorError::Decode("the token endpoint returned no access_token".into())
  })?;
  Ok(token.access_token)
}

/// Mint the self-signed assertion a service account presents.
fn assertion(
  client_email: &str,
  private_key: &str,
  audience: &str,
  impersonate: Option<&str>,
  scopes: &[&str],
  now: Timestamp,
) -> Result<String, ConnectorError> {
  let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes())
    .map_err(|_| {
      // Deliberately without the underlying message: it can quote the
      // key material it failed on.
      ConnectorError::Config(
        "the service account's private_key is not a PEM RSA key; paste the \
         credential file exactly as Google wrote it, escaped newlines and all"
          .to_owned(),
      )
    })?;

  let iat = now.as_jiff().as_second();
  let claims = Claims {
    iss: client_email,
    scope: scopes.join(" "),
    aud: audience,
    exp: iat + ASSERTION_SECS,
    iat,
    sub: impersonate,
  };

  jsonwebtoken::encode(
    &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
    &claims,
    &key,
  )
  .map_err(|e| ConnectorError::Config(format!("signing the assertion: {e}")))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A throwaway 2048-bit key, generated for this test and used nowhere
  /// else. It exists so the signing path is exercised for real rather
  /// than mocked.
  const TEST_KEY: &str = include_str!("../tests/testing-key.pem");

  fn now() -> Timestamp { "2026-01-15T00:00:00Z".parse().unwrap() }

  #[test]
  fn a_service_account_file_reads_as_google_writes_it() {
    let c: Credential = serde_json::from_str(
      r#"{"type":"service_account","project_id":"p",
          "client_email":"svc@p.iam.gserviceaccount.com",
          "private_key":"-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----\n",
          "token_uri":"https://oauth2.googleapis.com/token"}"#,
    )
    .unwrap();
    assert!(c.needs_impersonation());
  }

  #[test]
  fn an_authorized_user_file_reads_as_gcloud_writes_it() {
    let c: Credential = serde_json::from_str(
      r#"{"type":"authorized_user","client_id":"id.apps.googleusercontent.com",
          "client_secret":"s","refresh_token":"r"}"#,
    )
    .unwrap();
    assert!(!c.needs_impersonation());
  }

  #[test]
  fn a_credential_never_renders_its_secrets() {
    let c: Credential = serde_json::from_str(
      r#"{"type":"authorized_user","client_id":"id","client_secret":"hunter2",
          "refresh_token":"also-secret"}"#,
    )
    .unwrap();
    let rendered = format!("{c:?}");
    assert!(!rendered.contains("hunter2"), "{rendered}");
    assert!(!rendered.contains("also-secret"), "{rendered}");
  }

  #[test]
  fn an_assertion_carries_the_impersonated_subject_and_the_sweeps_clock() {
    let jwt = assertion(
      "svc@p.iam.gserviceaccount.com",
      TEST_KEY,
      "https://oauth2.googleapis.com/token",
      Some("admin@example.com"),
      &["https://www.googleapis.com/auth/admin.directory.user.readonly"],
      now(),
    )
    .unwrap();

    let claims = decode_claims(&jwt);
    assert_eq!(claims["sub"], "admin@example.com");
    assert_eq!(claims["iss"], "svc@p.iam.gserviceaccount.com");
    // Bound to the sweep's definition of "now", not to a clock read.
    assert_eq!(claims["iat"], 1_768_435_200_i64);
    assert_eq!(claims["exp"], 1_768_435_200_i64 + ASSERTION_SECS);
    assert!(
      claims["scope"].as_str().unwrap().contains("directory.user"),
      "{claims}"
    );
  }

  #[test]
  fn an_unimpersonated_assertion_omits_the_subject_rather_than_emptying_it() {
    // Google rejects `sub: ""`; the field has to be absent.
    let jwt = assertion(
      "svc@p.iam.gserviceaccount.com",
      TEST_KEY,
      "https://oauth2.googleapis.com/token",
      None,
      &["scope"],
      now(),
    )
    .unwrap();
    assert!(decode_claims(&jwt).get("sub").is_none());
  }

  #[test]
  fn a_key_that_is_not_a_key_says_so_without_quoting_it() {
    let err = assertion(
      "svc@p.iam.gserviceaccount.com",
      "-----BEGIN PRIVATE KEY-----\nnot-base64\n-----END PRIVATE KEY-----\n",
      "aud",
      None,
      &[],
      now(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not a PEM RSA key"), "{msg}");
    assert!(!msg.contains("not-base64"), "{msg}");
  }

  #[test]
  fn an_unset_variable_names_itself_and_says_where_credentials_live() {
    let err =
      Credential::from_env("OVERLORD_GWS_NO_SUCH_VARIABLE").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("OVERLORD_GWS_NO_SUCH_VARIABLE"), "{msg}");
    assert!(msg.contains("environment"), "{msg}");
  }

  /// The payload of a JWT, without verifying it: these tests are about
  /// what overlord *claims*, and Google is the one that checks it.
  fn decode_claims(jwt: &str) -> serde_json::Value {
    let payload = jwt.split('.').nth(1).expect("a JWT has three parts");
    let bytes = base64url(payload);
    serde_json::from_slice(&bytes).expect("claims are JSON")
  }

  fn base64url(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] =
      b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
      let Some(v) = ALPHABET.iter().position(|&a| a == c) else {
        continue;
      };
      acc = (acc << 6) | u32::try_from(v).unwrap_or(0);
      bits += 6;
      if bits >= 8 {
        bits -= 8;
        out.push(u8::try_from((acc >> bits) & 0xff).unwrap_or(0));
      }
    }
    out
  }
}
