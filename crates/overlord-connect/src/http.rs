//! The only HTTP surface a connector is given.
//!
//! SPEC.md section 11: overlord reads systems and never writes to them,
//! and that is enforced "by construction, not by policy alone". Three
//! mechanisms do it here.
//!
//! First, [`ReadMethod`] has no mutating variant, so a connector cannot
//! *express* a `PUT`, `PATCH` or `DELETE` — there is no value to pass.
//! `POST` exists because several vendor read APIs require it (batch
//! reads, some search endpoints, every OAuth token grant) and is
//! documented as such.
//!
//! Second, every request is matched against the connector's allowlist of
//! method-and-path pairs before it is sent. Anything unlisted fails
//! closed, without a network call.
//!
//! Third, the allowlist covers *origins* as well as paths. A connector
//! that needs a second host — a token endpoint, a sibling API on its own
//! domain — declares it in the same list, so one screen still shows
//! everything the connector can reach. A client is built for exactly one
//! origin and carries only that origin's entries, so a path allowed on
//! the vendor's API is not thereby allowed on its token endpoint.

use std::{fmt, sync::RwLock};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
  error::ConnectorError,
  progress::{Progress, ProgressEvent},
};

/// The methods overlord can issue. There is deliberately no way to name
/// a mutating one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ReadMethod {
  Get,
  Head,
  /// Only for vendor endpoints that require it to *read*. A connector
  /// using this must say why in its allowlist comment.
  Post,
}

impl ReadMethod {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Get => "GET",
      Self::Head => "HEAD",
      Self::Post => "POST",
    }
  }

  fn to_reqwest(self) -> reqwest::Method {
    match self {
      Self::Get => reqwest::Method::GET,
      Self::Head => reqwest::Method::HEAD,
      Self::Post => reqwest::Method::POST,
    }
  }
}

impl fmt::Display for ReadMethod {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

/// A path pattern. `*` matches one segment, `**` matches the rest.
///
/// Patterns are deliberately weak: a connector should be able to say
/// "users and their aliases" and nothing else, and a reviewer should be
/// able to read the allowlist and know what the connector can reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern(Vec<String>);

impl PathPattern {
  #[must_use]
  pub fn new(pattern: &str) -> Self {
    Self(
      pattern
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect(),
    )
  }

  #[must_use]
  pub fn matches(&self, path: &str) -> bool {
    let segments: Vec<&str> = path
      .trim_matches('/')
      .split('/')
      .filter(|s| !s.is_empty())
      .collect();

    let mut p = 0;
    let mut s = 0;
    while p < self.0.len() {
      if self.0[p] == "**" {
        return true;
      }
      if s >= segments.len() {
        return false;
      }
      if self.0[p] != "*" && self.0[p] != segments[s] {
        return false;
      }
      p += 1;
      s += 1;
    }
    s == segments.len()
  }
}

impl fmt::Display for PathPattern {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "/{}", self.0.join("/"))
  }
}

/// One entry in a connector's allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allow {
  pub method: ReadMethod,
  pub path:   PathPattern,
  /// Why this endpoint is needed, shown on the Systems screen.
  pub reason: &'static str,
  /// Which origin this entry applies to. `None` — the usual case —
  /// means the connector's own [`Connector::base_url`], which is
  /// per-system and so cannot be named by a constant here.
  ///
  /// [`Connector::base_url`]: crate::Connector::base_url
  pub base:   Option<&'static str>,
}

impl Allow {
  #[must_use]
  pub fn get(path: &str, reason: &'static str) -> Self {
    Self {
      method: ReadMethod::Get,
      path: PathPattern::new(path),
      reason,
      base: None,
    }
  }

  #[must_use]
  pub fn post(path: &str, reason: &'static str) -> Self {
    Self {
      method: ReadMethod::Post,
      path: PathPattern::new(path),
      reason,
      base: None,
    }
  }

  /// Point this entry at a secondary origin rather than the connector's
  /// own base URL.
  #[must_use]
  pub fn at(mut self, base: &'static str) -> Self {
    self.base = Some(base);
    self
  }
}

impl fmt::Display for Allow {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.base {
      Some(base) => {
        write!(
          f,
          "{} {}{}",
          self.method,
          base.trim_end_matches('/'),
          self.path
        )
      }
      None => write!(f, "{} {}", self.method, self.path),
    }
  }
}

/// An HTTP client that can only reach a connector's allowlisted
/// endpoints, on one origin.
pub struct RestrictedHttp {
  client:         reqwest::Client,
  base:           Url,
  allow:          Vec<Allow>,
  /// A bearer token, once the connector has obtained one.
  ///
  /// Behind a lock rather than fixed at construction because
  /// `observe` receives `&RestrictedHttp`: the token is acquired
  /// during the run, and a long run may have to renew it. It is never
  /// logged and never leaves this struct.
  bearer:         RwLock<Option<String>>,
  /// Set for tests and dry runs: refuse every request rather than
  /// reaching the network at all.
  offline:        bool,
  /// Extra roots, kept so later builder calls can rebuild the client
  /// without losing them.
  roots:          Vec<Vec<u8>>,
  /// Refuse to verify the server's certificate. Off unless a connector
  /// explicitly asks; see [`Self::insecure`].
  accept_invalid: bool,
  /// Where each request is narrated. Inert unless a sweep is watching.
  progress:       Progress,
}

impl RestrictedHttp {
  /// Build a client for the connector's own base URL. It carries the
  /// allowlist entries that name no other origin.
  ///
  /// # Errors
  /// If `base` is not a valid URL or the TLS stack cannot start.
  pub fn new(base: &str, allow: Vec<Allow>) -> Result<Self, ConnectorError> {
    Self::build(
      base,
      allow.into_iter().filter(|a| a.base.is_none()).collect(),
    )
  }

  /// Build a client for one of the connector's secondary origins. It
  /// carries only the entries that name that origin, so a path allowed
  /// on the vendor's API is not thereby allowed here.
  ///
  /// # Errors
  /// As [`Self::new`].
  pub fn at(
    base: &'static str,
    allow: Vec<Allow>,
  ) -> Result<Self, ConnectorError> {
    Self::at_via(base, base, allow)
  }

  /// As [`Self::at`], but sending the requests somewhere other than the
  /// origin they were declared against.
  ///
  /// For a deployment whose outbound traffic goes through an egress
  /// proxy, and for tests that stand a double in front of a vendor. It
  /// relaxes nothing: the entries carried are still exactly the ones
  /// written down for `declared`, so the paths and methods reachable
  /// through `via` are the paths and methods a reviewer approved.
  ///
  /// # Errors
  /// As [`Self::new`].
  pub fn at_via(
    declared: &str,
    via: &str,
    allow: Vec<Allow>,
  ) -> Result<Self, ConnectorError> {
    Self::build(
      via,
      allow
        .into_iter()
        .filter(|a| a.base == Some(declared))
        .map(|mut a| {
          a.base = None;
          a
        })
        .collect(),
    )
  }

  fn build(base: &str, allow: Vec<Allow>) -> Result<Self, ConnectorError> {
    let base = Url::parse(base)
      .map_err(|e| ConnectorError::Config(format!("base url: {e}")))?;
    let client = client(&[], false)?;
    Ok(Self {
      client,
      base,
      allow,
      bearer: RwLock::new(None),
      offline: false,
      roots: Vec::new(),
      accept_invalid: false,
      progress: Progress::default(),
    })
  }

  /// Narrate each request to `progress`, so a screen watching a sweep
  /// sees the connector working rather than waiting in silence.
  ///
  /// The default [`crate::Connector::http`] attaches the context's own
  /// handle; this is the seam for a caller that builds a client some
  /// other way.
  #[must_use]
  pub fn with_progress(mut self, progress: Progress) -> Self {
    self.progress = progress;
    self
  }

  /// Trust one additional root certificate, given as PEM.
  ///
  /// A self-hosted system — an appliance console with a certificate
  /// signed by its own CA — is not reachable through the public roots
  /// `reqwest` ships. This is the narrow way to add exactly that CA and
  /// nothing else: verification stays on, a host the CA did not sign is
  /// still refused, and there is deliberately no "skip verification"
  /// counterpart.
  ///
  /// # Errors
  /// If the PEM does not parse or the TLS stack cannot restart.
  pub fn trusted(mut self, pem: &[u8]) -> Result<Self, ConnectorError> {
    if pem.is_empty() {
      return Ok(self);
    }
    // `reqwest` accepts an empty parse quietly, so a file that is not a
    // certificate would otherwise become "trust added, nothing
    // verified" — the same opaque failure the option exists to fix.
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    if !pem.windows(BEGIN.len()).any(|w| w == BEGIN) {
      return Err(ConnectorError::Config(
        "the trusted certificate is not a PEM certificate: no BEGIN \
         CERTIFICATE block"
          .to_owned(),
      ));
    }
    self.roots.push(pem.to_vec());
    self.client = client(&self.roots, self.accept_invalid)?;
    Ok(self)
  }

  /// Stop verifying the server's certificate.
  ///
  /// A last resort, and never the default. Some appliances — a console
  /// with a self-signed leaf it will not let you replace, and no CA to
  /// pin — are unreachable any other way, and the alternative for an
  /// operator is not "more secure", it is "no observability at all".
  /// It can still reach exactly the allowlisted paths and still cannot
  /// name a mutating method, so the read-only guarantee is untouched;
  /// what is given up is knowing which host answered.
  ///
  /// Prefer [`Self::trusted`]: it trusts one certificate and keeps the
  /// check. Reach for this only when there is no certificate to trust.
  ///
  /// # Errors
  /// If the TLS stack cannot restart.
  pub fn insecure(mut self) -> Result<Self, ConnectorError> {
    self.accept_invalid = true;
    self.client = client(&self.roots, true)?;
    Ok(self)
  }

  /// The same client, allowlisting the same endpoints, but refusing to
  /// dial out. Used to assert what a connector *would* request.
  #[must_use]
  pub fn offline(mut self) -> Self {
    self.offline = true;
    self
  }

  #[must_use]
  pub fn allowlist(&self) -> &[Allow] { &self.allow }

  /// Present this token on every subsequent request.
  pub fn set_bearer(&self, token: impl Into<String>) {
    // A poisoned lock here would mean a panic mid-request; replacing
    // the token is still the right thing to do, and refusing to would
    // only turn one failure into every failure.
    let mut slot = self.bearer.write().unwrap_or_else(|e| e.into_inner());
    *slot = Some(token.into());
  }

  /// Whether a request would be permitted. The check every request goes
  /// through, exposed so a test can assert the boundary directly.
  #[must_use]
  pub fn permits(&self, method: ReadMethod, path: &str) -> bool {
    self
      .allow
      .iter()
      .any(|a| a.method == method && a.path.matches(path))
  }

  /// Issue a request and parse the response as JSON.
  ///
  /// # Errors
  /// [`ConnectorError::NotAllowed`] before any network call if the
  /// method-and-path pair is unlisted; otherwise a transport or status
  /// error.
  pub async fn json(
    &self,
    method: ReadMethod,
    path: &str,
    query: &[(&str, String)],
  ) -> Result<serde_json::Value, ConnectorError> {
    let url = self.check(method, path)?;
    let mut url = url;
    for (k, v) in query {
      url.query_pairs_mut().append_pair(k, v);
    }
    // Report before the round trip, so the screen names what is in
    // flight rather than what just finished.
    self
      .progress
      .report(ProgressEvent::note(request_note(method, path, query)));
    self
      .send(self.client.request(method.to_reqwest(), url), path)
      .await
  }

  /// POST a form-encoded body and parse the response as JSON.
  ///
  /// This exists for OAuth token grants, which are POSTs that read: they
  /// exchange a credential for a token and change nothing in the system
  /// being observed. A connector using it still has to allowlist the
  /// endpoint, and [`ReadMethod`] still cannot name a mutating method.
  ///
  /// # Errors
  /// As [`Self::json`].
  pub async fn post_form(
    &self,
    path: &str,
    form: &[(&str, String)],
  ) -> Result<serde_json::Value, ConnectorError> {
    let url = self.check(ReadMethod::Post, path)?;
    self.progress.report(ProgressEvent::note(request_note(
      ReadMethod::Post,
      path,
      form,
    )));
    self.send(self.client.post(url).form(form), path).await
  }

  /// Allowlist check and URL resolution, before anything is sent.
  fn check(
    &self,
    method: ReadMethod,
    path: &str,
  ) -> Result<Url, ConnectorError> {
    if !self.permits(method, path) {
      return Err(ConnectorError::NotAllowed {
        method,
        path: path.to_owned(),
      });
    }
    if self.offline {
      return Err(ConnectorError::Offline {
        method,
        path: path.to_owned(),
      });
    }
    self
      .base
      .join(path.trim_start_matches('/'))
      .map_err(|e| ConnectorError::Config(format!("path {path}: {e}")))
  }

  async fn send(
    &self,
    req: reqwest::RequestBuilder,
    path: &str,
  ) -> Result<serde_json::Value, ConnectorError> {
    let token = self
      .bearer
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .clone();
    let req = match token {
      Some(t) => req.bearer_auth(t),
      None => req,
    };

    let resp = req
      .send()
      .await
      .map_err(|e| ConnectorError::Transport(causes(&e)))?;

    let status = resp.status();
    if !status.is_success() {
      return Err(ConnectorError::Status {
        status: status.as_u16(),
        path:   path.to_owned(),
      });
    }
    resp
      .json()
      .await
      .map_err(|e| ConnectorError::Decode(e.to_string()))
  }
}

/// One line naming a request, with the page number when the caller
/// paged. Paging one endpoint repeats the same path, so the page is the
/// part that says the read is moving.
fn request_note(
  method: ReadMethod,
  path: &str,
  query: &[(&str, String)],
) -> String {
  match query.iter().find(|(k, _)| *k == "page_num") {
    Some((_, page)) => format!("{method} {path} (page {page})"),
    None => format!("{method} {path}"),
  }
}

impl fmt::Debug for RestrictedHttp {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // The bearer token is deliberately absent: this type is logged.
    f.debug_struct("RestrictedHttp")
      .field("base", &self.base.as_str())
      .field("allow", &self.allow.len())
      .field("offline", &self.offline)
      .field("verify_tls", &!self.accept_invalid)
      .finish()
  }
}

/// Build a client honouring the given extra root certificates.
///
/// Each `roots` entry is a PEM bundle and *every* certificate in it is
/// added. An appliance that presents a leaf plus its own intermediate
/// CA (UniFi OS does) is the reason: a bundle parsed as one certificate
/// would trust only whichever happened to be written first — usually
/// the leaf — and verification then fails with the same `UnknownIssuer`
/// the option exists to prevent.
fn client(
  roots: &[Vec<u8>],
  accept_invalid: bool,
) -> Result<reqwest::Client, ConnectorError> {
  let mut builder = reqwest::Client::builder()
    .user_agent(concat!("overlord/", env!("CARGO_PKG_VERSION")))
    .timeout(std::time::Duration::from_secs(30))
    .danger_accept_invalid_certs(accept_invalid);
  for pem in roots {
    for cert in parse_bundle(pem)? {
      builder = builder.add_root_certificate(cert);
    }
  }
  builder
    .build()
    .map_err(|e| ConnectorError::Config(e.to_string()))
}

/// Parse every certificate in a PEM bundle.
///
/// A single-certificate parse would trust only the first block, and a
/// chain is printed leaf-first — so trusting "the certificate" would
/// trust the one certificate that is not an issuer, and verification
/// would fail with the same `UnknownIssuer` the option exists to fix.
fn parse_bundle(
  pem: &[u8],
) -> Result<Vec<reqwest::Certificate>, ConnectorError> {
  let certs = reqwest::Certificate::from_pem_bundle(pem)
    .map_err(|e| ConnectorError::Config(format!("root certificate: {e}")))?;
  if certs.is_empty() {
    return Err(ConnectorError::Config(
      "root certificate: the PEM contains no CERTIFICATE block".to_owned(),
    ));
  }
  Ok(certs)
}

/// The full cause chain of a transport failure.
///
/// `reqwest::Error`'s own `Display` is only the outermost sentence —
/// "error sending request for url (…)" — while the reason a connector
/// actually failed (an untrusted certificate, a refused connection, a
/// DNS failure) lives in its `source`. Walking the chain is what turns
/// a coverage-view error from "it did not work" into something an
/// operator can act on.
fn causes(err: &dyn std::error::Error) -> String {
  let mut out = err.to_string();
  let mut source = std::error::Error::source(err);
  while let Some(cause) = source {
    out.push_str(": ");
    out.push_str(&cause.to_string());
    source = std::error::Error::source(cause);
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  const TOKEN: &str = "https://oauth2.example.test/";

  fn allowlist() -> Vec<Allow> {
    vec![
      Allow::get("/directory/v1/users", "enumerate accounts"),
      Allow::get("/directory/v1/users/*/aliases", "account aliases"),
      Allow::get("/reports/v1/**", "login activity"),
      Allow::post("/token", "exchange a credential for a read token").at(TOKEN),
    ]
  }

  fn http() -> RestrictedHttp {
    RestrictedHttp::new("https://example.test/admin/", allowlist())
      .unwrap()
      .offline()
  }

  #[test]
  fn listed_pairs_are_permitted() {
    let h = http();
    assert!(h.permits(ReadMethod::Get, "/directory/v1/users"));
    assert!(h.permits(ReadMethod::Get, "/directory/v1/users/ada/aliases"));
    assert!(h.permits(ReadMethod::Get, "/reports/v1/activity/users/all"));
  }

  #[test]
  fn anything_unlisted_fails_closed() {
    let h = http();
    // A different path.
    assert!(!h.permits(ReadMethod::Get, "/directory/v1/groups"));
    // A deeper path than the pattern allows.
    assert!(!h.permits(ReadMethod::Get, "/directory/v1/users/ada"));
    // The right path with a method the connector did not ask for.
    assert!(!h.permits(ReadMethod::Post, "/directory/v1/users"));
  }

  #[test]
  fn a_request_note_names_the_page_when_there_is_one() {
    assert_eq!(
      request_note(ReadMethod::Get, "/directory/v1/users", &[]),
      "GET /directory/v1/users"
    );
    let query = [
      ("expand[]", "access_policy".to_owned()),
      ("page_num", "2".to_owned()),
    ];
    assert_eq!(
      request_note(ReadMethod::Get, "/directory/v1/users", &query),
      "GET /directory/v1/users (page 2)"
    );
  }

  #[test]
  fn a_client_without_progress_reports_nothing() {
    // The default carries no sink, so wiring one in is optional and a
    // client used outside a watched sweep stays silent.
    let h = http().with_progress(Progress::default());
    assert!(!h.progress.is_active());
  }

  #[tokio::test]
  async fn an_unlisted_request_never_reaches_the_network() {
    let h = http();
    // The offline client errors on *any* dial, so an `Offline` error
    // would mean the allowlist let it through. `NotAllowed` proves the
    // check ran first.
    let err = h
      .json(ReadMethod::Get, "/directory/v1/groups", &[])
      .await
      .unwrap_err();
    assert!(
      matches!(err, ConnectorError::NotAllowed { .. }),
      "expected the allowlist to refuse it, got {err:?}"
    );
  }

  #[test]
  fn an_entry_for_another_origin_is_not_carried_by_the_primary_client() {
    // Otherwise declaring a token endpoint would quietly widen what the
    // vendor's own API accepts.
    let primary = http();
    assert!(!primary.permits(ReadMethod::Post, "/token"));
    assert_eq!(primary.allowlist().len(), 3);
  }

  #[test]
  fn a_secondary_client_carries_only_its_own_origins_entries() {
    let token = RestrictedHttp::at(TOKEN, allowlist()).unwrap().offline();
    assert!(token.permits(ReadMethod::Post, "/token"));
    assert!(!token.permits(ReadMethod::Get, "/directory/v1/users"));
    assert_eq!(token.allowlist().len(), 1);
  }

  #[tokio::test]
  async fn a_form_post_is_allowlisted_like_any_other_request() {
    let token = RestrictedHttp::at(TOKEN, allowlist()).unwrap().offline();
    let err = token
      .post_form("/revoke", &[("token", "x".to_owned())])
      .await
      .unwrap_err();
    assert!(
      matches!(err, ConnectorError::NotAllowed { .. }),
      "expected the allowlist to refuse it, got {err:?}"
    );
  }

  #[test]
  fn an_allow_renders_the_origin_it_applies_to() {
    let entries = allowlist();
    assert_eq!(entries[0].to_string(), "GET /directory/v1/users");
    assert_eq!(
      entries[3].to_string(),
      "POST https://oauth2.example.test/token"
    );
  }

  #[test]
  fn a_single_star_does_not_span_segments() {
    let p = PathPattern::new("/a/*/c");
    assert!(p.matches("/a/b/c"));
    assert!(!p.matches("/a/b/x/c"));
    assert!(!p.matches("/a/c"));
  }

  #[test]
  fn a_double_star_matches_the_rest() {
    let p = PathPattern::new("/a/**");
    assert!(p.matches("/a/b"));
    assert!(p.matches("/a/b/c/d"));
    assert!(!p.matches("/b/a"));
  }

  #[test]
  fn trailing_slashes_do_not_change_a_match() {
    let p = PathPattern::new("directory/v1/users");
    assert!(p.matches("/directory/v1/users"));
    assert!(p.matches("directory/v1/users/"));
  }

  #[test]
  fn a_bogus_root_certificate_is_refused_rather_than_ignored() {
    let err = RestrictedHttp::new("https://example.test/", allowlist())
      .unwrap()
      .trusted(b"not a certificate")
      .unwrap_err();
    assert!(matches!(err, ConnectorError::Config(_)), "{err:?}");
  }

  /// A real certificate, so the bundle test exercises the parser rather
  /// than an error path. Contents are immaterial; only that it parses.
  const CERT: &str = "\
-----BEGIN CERTIFICATE-----
MIIDwzCCAqugAwIBAgIBATANBgkqhkiG9w0BAQsFADCBgjELMAkGA1UEBhMCREUx
KzApBgNVBAoMIlQtU3lzdGVtcyBFbnRlcnByaXNlIFNlcnZpY2VzIEdtYkgxHzAd
BgNVBAsMFlQtU3lzdGVtcyBUcnVzdCBDZW50ZXIxJTAjBgNVBAMMHFQtVGVsZVNl
YyBHbG9iYWxSb290IENsYXNzIDIwHhcNMDgxMDAxMTA0MDE0WhcNMzMxMDAxMjM1
OTU5WjCBgjELMAkGA1UEBhMCREUxKzApBgNVBAoMIlQtU3lzdGVtcyBFbnRlcnBy
aXNlIFNlcnZpY2VzIEdtYkgxHzAdBgNVBAsMFlQtU3lzdGVtcyBUcnVzdCBDZW50
ZXIxJTAjBgNVBAMMHFQtVGVsZVNlYyBHbG9iYWxSb290IENsYXNzIDIwggEiMA0G
CSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCqX9obX+hzkeXaXPSi5kfl82hVYAUd
AqSzm1nzHoqvNK38DcLZSBnuaY/JIPwhqgcZ7bBcrGXHX+0CfHt8LRvWurmAwhiC
FoT6ZrAIxlQjgeTNuUk/9k9uN0goOA/FvudocP05l03Sx5iRUKrERLMjfTlH6VJi
1hKTXrcxlkIF+3anHqP1wvzpesVsqXFP6st4vGCvx9702cu+fjOlbpSD8DT6Iavq
jnKgP6TeMFvvhk1qlVtDRKgQFRzlAVfFmPHmBiiRqiDFt1MmUUOyCxGVWOHAD3bZ
wI18gfNycJ5v/hqO2V81xrJvNHy+SE/iWjnX2J14np+GPgNeGYtEotXHAgMBAAGj
QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgEGMB0GA1UdDgQWBBS/
WSA2AHmgoCJrjNXyYdK4LMuCSjANBgkqhkiG9w0BAQsFAAOCAQEAMQOiYQsfdOhy
NsZt+U2e+iKo4YFWz827n+qrkRk4r6p8FU3ztqONpfSO9kSpp+ghla0+AGIWiPAC
uvxhI+YzmzB6azZie60EI4RYZeLbK4rnJVM3YlNfvNoBYimipidx5joifsFvHZVw
IEoHNN/q/xWA5brXethbdXwFeilHfkCoMRN3zUA7tFFHei4R40cR3p1m0IvVVGb6
g1XqfMIpiRvpb7PO4gWEyS8+eIVibslfwXhjdFjASBgMmTnrpMwatXlajRWc2BQN
9noHV8cigwUtPJslJj0Ys6lDfMjIq2SPDqO/nBudMNva0Bkuqjzx+zOAduTNrRlP
BSeOE6Fuwg==
-----END CERTIFICATE-----
";

  #[test]
  fn every_certificate_in_a_bundle_is_trusted_not_just_the_first() {
    // `s_client -showcerts` prints leaf first, so trusting one
    // certificate trusts the only one that is not an issuer. UniFi OS
    // sends a chain; so does anything with its own CA.
    let one = parse_bundle(CERT.as_bytes()).unwrap();
    assert_eq!(one.len(), 1);
    let two = parse_bundle(format!("{CERT}{CERT}").as_bytes()).unwrap();
    assert_eq!(two.len(), 2);
  }

  #[test]
  fn an_insecure_client_says_so_in_its_debug() {
    let h = RestrictedHttp::new("https://example.test/", allowlist())
      .unwrap()
      .insecure()
      .unwrap();
    let debug = format!("{h:?}");
    assert!(debug.contains("verify_tls: false"), "{debug}");
  }

  #[test]
  fn a_transport_failure_reports_its_whole_cause_chain() {
    // `reqwest`'s own Display stops at "error sending request", which
    // is exactly the sentence that made a self-signed console
    // undiagnosable. The reason lives one level down.
    #[derive(Debug)]
    struct Outer;
    #[derive(Debug)]
    struct Inner;

    impl fmt::Display for Outer {
      fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("error sending request")
      }
    }
    impl fmt::Display for Inner {
      fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid peer certificate: UnknownIssuer")
      }
    }
    impl std::error::Error for Outer {
      fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&Inner)
      }
    }
    impl std::error::Error for Inner {}

    assert_eq!(
      causes(&Outer),
      "error sending request: invalid peer certificate: UnknownIssuer"
    );
  }
}
