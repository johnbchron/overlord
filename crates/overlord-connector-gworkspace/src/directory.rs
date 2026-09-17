//! Reading the Admin SDK Directory API, and the Licensing API beside it.
//!
//! Everything here pages, and everything here can stop early. A vendor
//! that rate-limits a tenant halfway through an enumeration has not told
//! overlord that the remaining accounts are gone, so a truncated read is
//! reported as truncated and the sweep declines to tombstone from it
//! (SPEC.md sections 10 and 11). That is the whole reason [`Paged`]
//! carries `incomplete` alongside the items rather than being a
//! `Result`: a partial answer is still worth having.

use overlord_connect::{ConnectorError, ReadMethod, RestrictedHttp};
use serde_json::Value;
use tracing::warn;

pub const DIRECTORY_BASE: &str = "https://admin.googleapis.com/";
pub const LICENSING_BASE: &str = "https://licensing.googleapis.com/";

pub const USERS_PATH: &str = "/admin/directory/v1/users";
pub const GROUPS_PATH: &str = "/admin/directory/v1/groups";

/// Google's own ceilings. Asking for more is an error, not a courtesy.
const USERS_PER_PAGE: &str = "500";
const GROUPS_PER_PAGE: &str = "200";
const MEMBERS_PER_PAGE: &str = "200";
const LICENSES_PER_PAGE: &str = "100";

/// A stop for an enumeration that will not terminate. A `nextPageToken`
/// that never changes is the shape a vendor bug takes, and a sweep that
/// hangs is worse than one that reports a partial read.
const MAX_PAGES: usize = 10_000;

/// The result of a paged read: what came back, and why it might not be
/// everything.
#[derive(Debug, Default)]
pub struct Paged {
  pub items:      Vec<Value>,
  /// `Some` if any page failed. The items collected before that point
  /// are still returned — they are observations, and discarding them
  /// would turn a slow tenant into a blind one.
  pub incomplete: Option<String>,
}

impl Paged {
  /// Whether nothing at all came back. The caller treats that
  /// differently from a truncated read: an enumeration whose *first*
  /// page failed is a failed system, not a partial one.
  #[must_use]
  pub fn is_empty(&self) -> bool { self.items.is_empty() }
}

/// Page through a Google list endpoint, collecting `items_key`.
///
/// `query` is the fixed part; `pageToken` is added per page.
pub async fn pages(
  http: &RestrictedHttp,
  path: &str,
  query: &[(&str, String)],
  items_key: &str,
) -> Paged {
  let mut out = Paged::default();
  let mut token: Option<String> = None;

  for _ in 0..MAX_PAGES {
    let mut q: Vec<(&str, String)> = query.to_vec();
    if let Some(t) = &token {
      q.push(("pageToken", t.clone()));
    }

    let body = match http.json(ReadMethod::Get, path, &q).await {
      Ok(b) => b,
      Err(e) => {
        warn!(path, error = %e, "enumeration stopped short");
        out.incomplete = Some(format!("{path}: {e}"));
        return out;
      }
    };

    if let Some(Value::Array(items)) = body.get(items_key) {
      out.items.extend(items.iter().cloned());
    }

    let next = body
      .get("nextPageToken")
      .and_then(Value::as_str)
      .map(ToOwned::to_owned);
    match next {
      // A token identical to the one that produced this page would
      // fetch it again, forever.
      Some(t) if Some(&t) != token.as_ref() && !t.is_empty() => {
        token = Some(t);
      }
      _ => return out,
    }
  }

  out.incomplete = Some(format!("{path}: stopped after {MAX_PAGES} pages"));
  out
}

/// Every user in the customer, newest page ordering pinned so two
/// sweeps of an unchanged tenant read the same way.
pub async fn users(http: &RestrictedHttp, customer: &str) -> Paged {
  pages(
    http,
    USERS_PATH,
    &[
      ("customer", customer.to_owned()),
      ("maxResults", USERS_PER_PAGE.to_owned()),
      // `full` is what carries `isEnrolledIn2Sv`, `externalIds`,
      // `organizations` and `aliases`. Without it the overlay loses
      // both the MFA signal and two of the three identity signals.
      ("projection", "full".to_owned()),
      ("orderBy", "email".to_owned()),
    ],
    "users",
  )
  .await
}

/// Every group in the customer.
pub async fn groups(http: &RestrictedHttp, customer: &str) -> Paged {
  pages(
    http,
    GROUPS_PATH,
    &[
      ("customer", customer.to_owned()),
      ("maxResults", GROUPS_PER_PAGE.to_owned()),
    ],
    "groups",
  )
  .await
}

/// One group's members. Not recursive: a nested group is reported as a
/// member of type `GROUP` and left at that, because expanding it would
/// attribute a membership to a person that the directory does not.
pub async fn members(http: &RestrictedHttp, group_key: &str) -> Paged {
  pages(
    http,
    &format!("{GROUPS_PATH}/{group_key}/members"),
    &[("maxResults", MEMBERS_PER_PAGE.to_owned())],
    "members",
  )
  .await
}

/// The customer's verified domains, which is how an external member is
/// told from an internal one.
pub async fn domains(http: &RestrictedHttp, customer: &str) -> Paged {
  pages(
    http,
    &format!("/admin/directory/v1/customer/{customer}/domains"),
    &[],
    "domains",
  )
  .await
}

/// Licence assignments for one SKU.
///
/// A separate API on a separate host, and the only one overlord reads
/// through a scope Google does not offer in a read-only form — see the
/// crate docs.
pub async fn licence_assignments(
  http: &RestrictedHttp,
  product: &str,
  sku: &str,
  customer_id: &str,
) -> Paged {
  pages(
    http,
    &format!("/apps/licensing/v1/product/{product}/sku/{sku}/users"),
    &[
      ("customerId", customer_id.to_owned()),
      ("maxResults", LICENSES_PER_PAGE.to_owned()),
    ],
    "items",
  )
  .await
}

/// Turn a directory read that produced nothing into an error, and one
/// that produced something incomplete into a reason.
///
/// # Errors
/// [`ConnectorError::Incomplete`] when the very first page failed:
/// there is no snapshot at all, which is a failed system rather than a
/// partial one.
pub fn require_something(
  paged: &Paged,
  what: &str,
) -> Result<(), ConnectorError> {
  match (&paged.incomplete, paged.is_empty()) {
    (Some(reason), true) => {
      Err(ConnectorError::Incomplete(format!("{what}: {reason}")))
    }
    _ => Ok(()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_enumeration_that_returned_nothing_at_all_is_an_error() {
    let paged = Paged {
      items:      Vec::new(),
      incomplete: Some("403".to_owned()),
    };
    assert!(require_something(&paged, "users").is_err());
  }

  #[test]
  fn a_truncated_enumeration_keeps_what_it_read() {
    let paged = Paged {
      items:      vec![serde_json::json!({"id": "1"})],
      incomplete: Some("429".to_owned()),
    };
    assert!(require_something(&paged, "users").is_ok());
  }

  #[test]
  fn an_empty_tenant_is_not_an_error() {
    assert!(require_something(&Paged::default(), "users").is_ok());
  }
}
