//! The only HTTP surface a connector is given.
//!
//! SPEC.md section 11: overlord reads systems and never writes to them,
//! and that is enforced "by construction, not by policy alone". Two
//! mechanisms do it here.
//!
//! First, [`ReadMethod`] has no mutating variant, so a connector cannot
//! *express* a `PUT`, `PATCH` or `DELETE` — there is no value to pass.
//! `POST` exists because several vendor read APIs require it (batch
//! reads, some search endpoints) and is documented as such.
//!
//! Second, every request is matched against the connector's allowlist of
//! method-and-path pairs before it is sent. Anything unlisted fails
//! closed, without a network call.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::ConnectorError;

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
  pub path: PathPattern,
  /// Why this endpoint is needed, shown on the Systems screen.
  pub reason: &'static str,
}

impl Allow {
  #[must_use]
  pub fn get(path: &str, reason: &'static str) -> Self {
    Self {
      method: ReadMethod::Get,
      path: PathPattern::new(path),
      reason,
    }
  }

  #[must_use]
  pub fn post(path: &str, reason: &'static str) -> Self {
    Self {
      method: ReadMethod::Post,
      path: PathPattern::new(path),
      reason,
    }
  }
}

impl fmt::Display for Allow {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{} {}", self.method, self.path)
  }
}

/// An HTTP client that can only reach a connector's allowlisted
/// endpoints.
pub struct RestrictedHttp {
  client: reqwest::Client,
  base: Url,
  allow: Vec<Allow>,
  /// Set for tests and dry runs: refuse every request rather than
  /// reaching the network at all.
  offline: bool,
}

impl RestrictedHttp {
  /// Build a client pinned to `base`, permitting only `allow`.
  ///
  /// # Errors
  /// If `base` is not a valid URL or the TLS stack cannot start.
  pub fn new(base: &str, allow: Vec<Allow>) -> Result<Self, ConnectorError> {
    let base = Url::parse(base)
      .map_err(|e| ConnectorError::Config(format!("base url: {e}")))?;
    let client = reqwest::Client::builder()
      .user_agent(concat!("overlord/", env!("CARGO_PKG_VERSION")))
      .timeout(std::time::Duration::from_secs(30))
      .build()
      .map_err(|e| ConnectorError::Config(e.to_string()))?;
    Ok(Self {
      client,
      base,
      allow,
      offline: false,
    })
  }

  /// A client that allowlists the same endpoints but never dials out.
  /// Used to assert what a connector *would* request.
  ///
  /// # Errors
  /// As [`Self::new`].
  pub fn offline(
    base: &str,
    allow: Vec<Allow>,
  ) -> Result<Self, ConnectorError> {
    let mut http = Self::new(base, allow)?;
    http.offline = true;
    Ok(http)
  }

  #[must_use]
  pub fn allowlist(&self) -> &[Allow] {
    &self.allow
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

    let mut url = self
      .base
      .join(path.trim_start_matches('/'))
      .map_err(|e| ConnectorError::Config(format!("path {path}: {e}")))?;
    for (k, v) in query {
      url.query_pairs_mut().append_pair(k, v);
    }

    let resp = self
      .client
      .request(method.to_reqwest(), url)
      .send()
      .await
      .map_err(|e| ConnectorError::Transport(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
      return Err(ConnectorError::Status {
        status: status.as_u16(),
        path: path.to_owned(),
      });
    }
    resp
      .json()
      .await
      .map_err(|e| ConnectorError::Decode(e.to_string()))
  }
}

impl fmt::Debug for RestrictedHttp {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("RestrictedHttp")
      .field("base", &self.base.as_str())
      .field("allow", &self.allow.len())
      .field("offline", &self.offline)
      .finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn http() -> RestrictedHttp {
    RestrictedHttp::offline(
      "https://example.test/admin/",
      vec![
        Allow::get("/directory/v1/users", "enumerate accounts"),
        Allow::get("/directory/v1/users/*/aliases", "account aliases"),
        Allow::get("/reports/v1/**", "login activity"),
      ],
    )
    .unwrap()
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
}
