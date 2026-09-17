//! The OIDC authorization-code flow with PKCE (SPEC.md section 14).
//!
//! Provider metadata is discovered once, on the first sign-in, and
//! cached: discovery is a network call, and doing it at startup would
//! make overlord refuse to boot because someone else's service is down.
//!
//! The per-login secrets — the PKCE verifier, the CSRF state and the
//! nonce — are held in memory between the redirect out and the callback
//! back. overlord is single-node by design (SPEC.md section 13), so
//! there is no second process to share them with; a restart mid-sign-in
//! costs the operator one click.

use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
};

use openidconnect::{
  AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
  PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
  core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use overlord_core::Timestamp;
use tokio::sync::OnceCell;

use crate::{auth::OidcConfig, error::WebError};

/// How long a half-finished sign-in stays valid. Long enough for a
/// password and a second factor, short enough that abandoned attempts do
/// not accumulate.
const PENDING_SECONDS: i64 = 600;

/// One sign-in in flight.
struct Pending {
  verifier: PkceCodeVerifier,
  nonce:    Nonce,
  started:  i64,
  /// Where the operator was going when they were bounced to sign in.
  next:     String,
}

/// The provider client and the sign-ins currently in flight.
pub struct Oidc {
  config:  OidcConfig,
  client: OnceCell<
    CoreClient<
      openidconnect::EndpointSet,
      openidconnect::EndpointNotSet,
      openidconnect::EndpointNotSet,
      openidconnect::EndpointNotSet,
      openidconnect::EndpointMaybeSet,
      openidconnect::EndpointMaybeSet,
    >,
  >,
  http:    reqwest::Client,
  pending: Mutex<HashMap<String, Pending>>,
}

/// What a verified sign-in established.
pub struct Verified {
  pub subject: String,
  pub label:   String,
  pub next:    String,
}

impl Oidc {
  /// # Errors
  /// If the HTTP client cannot be built.
  pub fn new(config: OidcConfig) -> Result<Arc<Self>, WebError> {
    // Redirects are refused rather than followed: an OIDC token
    // endpoint that answers with a redirect is a misconfiguration or an
    // attack, never something to chase.
    let http = reqwest::ClientBuilder::new()
      .redirect(reqwest::redirect::Policy::none())
      .build()
      .map_err(|e| WebError::internal(format!("http client: {e}")))?;
    Ok(Arc::new(Self {
      config,
      client: OnceCell::new(),
      http,
      pending: Mutex::new(HashMap::new()),
    }))
  }

  #[must_use]
  pub fn config(&self) -> &OidcConfig { &self.config }

  async fn client(
    &self,
  ) -> Result<
    &CoreClient<
      openidconnect::EndpointSet,
      openidconnect::EndpointNotSet,
      openidconnect::EndpointNotSet,
      openidconnect::EndpointNotSet,
      openidconnect::EndpointMaybeSet,
      openidconnect::EndpointMaybeSet,
    >,
    WebError,
  > {
    self
      .client
      .get_or_try_init(|| async {
        let issuer = IssuerUrl::new(self.config.issuer.clone())
          .map_err(|e| WebError::Auth(format!("issuer url: {e}")))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &self.http)
          .await
          .map_err(|e| WebError::Auth(format!("OIDC discovery failed: {e}")))?;
        let redirect = RedirectUrl::new(self.config.redirect_url.clone())
          .map_err(|e| WebError::Auth(format!("redirect url: {e}")))?;
        Ok(
          CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.config.client_id.clone()),
            self.config.client_secret.clone().map(ClientSecret::new),
          )
          .set_redirect_uri(redirect),
        )
      })
      .await
  }

  /// Begin a sign-in: the URL to send the operator to.
  ///
  /// # Errors
  /// If discovery fails or the provider metadata is unusable.
  pub async fn start(
    &self,
    next: &str,
    now: Timestamp,
  ) -> Result<String, WebError> {
    let client = self.client().await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf, nonce) = client
      .authorize_url(
        CoreAuthenticationFlow::AuthorizationCode,
        CsrfToken::new_random,
        Nonce::new_random,
      )
      .add_scope(Scope::new("email".to_owned()))
      .add_scope(Scope::new("profile".to_owned()))
      .set_pkce_challenge(challenge)
      .url();

    let mut pending = self.lock_pending();
    pending
      .retain(|_, p| now.as_jiff().as_second() - p.started < PENDING_SECONDS);
    pending.insert(csrf.secret().clone(), Pending {
      verifier,
      nonce,
      started: now.as_jiff().as_second(),
      next: next.to_owned(),
    });
    Ok(url.to_string())
  }

  /// Finish a sign-in: exchange the code, verify the id token, and
  /// check the operator is allowed in.
  ///
  /// # Errors
  /// [`WebError::Auth`] if the state is unknown or expired, the exchange
  /// fails, the token does not verify, or the claims are not admitted by
  /// the allowlist.
  pub async fn finish(
    &self,
    code: &str,
    state: &str,
    now: Timestamp,
  ) -> Result<Verified, WebError> {
    let pending = self
      .lock_pending()
      .remove(state)
      .ok_or_else(|| WebError::Auth("this sign-in has expired".to_owned()))?;
    if now.as_jiff().as_second() - pending.started >= PENDING_SECONDS {
      return Err(WebError::Auth("this sign-in has expired".to_owned()));
    }

    let client = self.client().await?;
    let response = client
      .exchange_code(AuthorizationCode::new(code.to_owned()))
      .map_err(|e| WebError::Auth(format!("token exchange: {e}")))?
      .set_pkce_verifier(pending.verifier)
      .request_async(&self.http)
      .await
      .map_err(|e| WebError::Auth(format!("token exchange failed: {e}")))?;

    let id_token = response
      .id_token()
      .ok_or_else(|| WebError::Auth("no id token in response".to_owned()))?;
    let claims = id_token
      .claims(&client.id_token_verifier(), &pending.nonce)
      .map_err(|e| WebError::Auth(format!("id token: {e}")))?;

    let subject = claims.subject().to_string();
    let email = claims.email().map(|e| e.as_str().to_owned());
    let label = claims
      .name()
      .and_then(|n| n.get(None))
      .map(|n| n.as_str().to_owned())
      .or_else(|| email.clone())
      .unwrap_or_else(|| subject.clone());

    // The signature covers the whole payload, and `claims` above is what
    // verified it. Re-reading that same payload for a claim the library
    // has no typed accessor for adds no trust: the bytes are already
    // authenticated, and a `groups` claim is provider-specific rather
    // than part of the OIDC core.
    let groups = groups_claim(&id_token.to_string());

    if !self.config.admits(&subject, email.as_deref(), &groups) {
      tracing::warn!(
        subject = %subject,
        "sign-in refused: not in the allowlist and not in the required group"
      );
      return Err(WebError::Auth(
        "that account is not permitted to use this overlord".to_owned(),
      ));
    }

    Ok(Verified {
      subject,
      label,
      next: pending.next,
    })
  }

  /// A poisoned lock means a previous holder panicked mid-update. The
  /// map is only a cache of in-flight sign-ins, so recovering it costs
  /// nothing worse than the sign-ins already lost to that panic.
  fn lock_pending(
    &self,
  ) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
    self.pending.lock().unwrap_or_else(|e| e.into_inner())
  }
}

/// Read a `groups` claim out of an already-verified JWT.
fn groups_claim(jwt: &str) -> Vec<String> {
  let Some(payload) = jwt.split('.').nth(1) else {
    return Vec::new();
  };
  let Some(bytes) = base64url(payload) else {
    return Vec::new();
  };
  let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
    return Vec::new();
  };
  match value.get("groups") {
    Some(serde_json::Value::Array(items)) => items
      .iter()
      .filter_map(|i| i.as_str().map(ToOwned::to_owned))
      .collect(),
    // Some providers send a single group as a bare string.
    Some(serde_json::Value::String(s)) => vec![s.clone()],
    _ => Vec::new(),
  }
}

/// Decode unpadded base64url, which is how a JWT's segments are encoded.
fn base64url(s: &str) -> Option<Vec<u8>> {
  const ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  let mut out = Vec::with_capacity(s.len() * 3 / 4);
  let mut acc: u32 = 0;
  let mut bits = 0u32;
  for b in s.bytes() {
    if b == b'=' {
      break;
    }
    let value = ALPHABET.iter().position(|a| *a == b)? as u32;
    acc = (acc << 6) | value;
    bits += 6;
    if bits >= 8 {
      bits -= 8;
      out.push(u8::try_from((acc >> bits) & 0xFF).ok()?);
    }
  }
  Some(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn base64url_decodes_unpadded_input() {
    // `{"groups":["secops"]}` as unpadded base64url.
    let encoded = "eyJncm91cHMiOlsic2Vjb3BzIl19";
    let decoded = base64url(encoded).unwrap();
    assert_eq!(
      String::from_utf8(decoded).unwrap(),
      r#"{"groups":["secops"]}"#
    );
  }

  #[test]
  fn groups_are_read_from_the_payload_segment() {
    let jwt = "header.eyJncm91cHMiOlsic2Vjb3BzIl19.signature";
    assert_eq!(groups_claim(jwt), ["secops"]);
  }

  #[test]
  fn a_token_without_groups_yields_none() {
    // `{"sub":"abc"}`
    let jwt = "header.eyJzdWIiOiJhYmMifQ.signature";
    assert!(groups_claim(jwt).is_empty());
  }

  #[test]
  fn a_malformed_token_does_not_panic() {
    assert!(groups_claim("not-a-jwt").is_empty());
    assert!(groups_claim("a.!!!!.c").is_empty());
  }
}
