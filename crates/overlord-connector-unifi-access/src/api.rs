//! Reading the UniFi Access developer API.
//!
//! Everything here reads. All but one endpoint is a `GET`; the system
//! log is a `POST`, because its query — a topic and a time window — is
//! a document the vendor takes in the body, and reading a log changes
//! nothing.
//!
//! The developer API answers with a uniform envelope — `{"code":
//! "SUCCESS", "msg": …, "data": …}` — and a list endpoint that pages
//! carries a `pagination` sibling. That sibling is the only evidence
//! that an endpoint pages at all, so both readers are built on it:
//! [`paged`] stops at the first page without one, and [`list`] reports
//! a truncated read when one appears. An endpoint that answers with its
//! whole collection repeats that collection for every `page_num` and
//! never sends an empty page, so a reader that counted its own pages
//! would collect the same rows until it gave up.
//!
//! Like the Workspace connector, a page that failed mid-enumeration is
//! a truncated read rather than an empty one: a vendor that rate-limits
//! a console halfway through has not said the remaining accounts are
//! gone (SPEC.md sections 10 and 11). [`Paged`] therefore carries
//! `incomplete` beside the items instead of being a `Result`, because a
//! partial answer is still worth having.
//!
//! A page is only paged while the envelope says `SUCCESS`. A non-success
//! `code` on a single-object or unlisted endpoint is a real failure and
//! is surfaced as one, not swallowed.

use std::collections::BTreeSet;

use overlord_connect::{ConnectorError, Progress, ReadMethod, RestrictedHttp};
use serde_json::{Value, json};
use tracing::warn;

/// UniFi's default console address and the port the Access developer
/// API listens on. A deployment overrides this with `base_url`; the
/// default is only ever the one a fresh console ships.
pub const DEFAULT_BASE: &str = "https://192.168.1.1:12445/";

pub const USERS_PATH: &str = "/api/v1/developer/users";
pub const DOORS_PATH: &str = "/api/v1/developer/doors";
pub const USER_GROUPS_PATH: &str = "/api/v1/developer/user_groups";
pub const SYSTEM_LOGS_PATH: &str = "/api/v1/developer/system/logs";

/// The system-log topic that records a door being opened — who opened
/// it, which door, and with which credential. It is the only topic this
/// connector asks for: the others carry device health, admin actions
/// and visitor traffic, none of which is an account's entitlement.
pub const DOOR_OPENINGS_TOPIC: &str = "door_openings";

/// How many rows to ask for at once. UniFi defaults to 25 and accepts
/// more; a value this size keeps a large tenant to a handful of round
/// trips without assuming a ceiling the vendor has not published.
pub const PAGE_SIZE: &str = "100";

/// A stop for a pagination that will not terminate — a `total` that
/// never comes, or a server that repeats a page forever. A sweep that
/// hangs is worse than one that reports a partial read.
const MAX_PAGES: u64 = 10_000;

/// The result of a read: what came back, and why it might not be
/// everything.
#[derive(Debug, Default)]
pub struct Paged {
  pub items:      Vec<Value>,
  /// `Some` if any page failed. The items collected before that point
  /// are still returned.
  pub incomplete: Option<String>,
}

impl Paged {
  /// Whether nothing at all came back. The caller treats that
  /// differently from a truncated read: an enumeration whose *first*
  /// page failed is a failed system, not a partial one.
  #[must_use]
  pub fn is_empty(&self) -> bool { self.items.is_empty() }
}

/// Unwrap one developer-API envelope into its `data`.
///
/// # Errors
/// A transport or status error from the request, or a body whose `code`
/// is present and not `SUCCESS` — the vendor's own refusal, reported
/// with the message it gave rather than guessed at.
async fn page(
  http: &RestrictedHttp,
  path: &str,
  query: &[(&str, String)],
  post: Option<&Value>,
) -> Result<(Vec<Value>, Option<u64>), ConnectorError> {
  let body = match post {
    None => http.json(ReadMethod::Get, path, query).await?,
    Some(filter) => http.post_json(path, query, filter).await?,
  };

  if let Some(code) = body.get("code").and_then(Value::as_str)
    && code != "SUCCESS"
  {
    let msg = body.get("msg").and_then(Value::as_str).unwrap_or("");
    return Err(ConnectorError::Other(format!("{path}: {code} {msg}")));
  }

  // `data` is the collection itself on the directory endpoints. The
  // system log is the exception: it answers with an object whose `hits`
  // holds the rows, and it carries its own `pagination` inside that
  // object rather than beside it. Both shapes are read here so no
  // caller has to know which one it asked for. Anything else is still
  // refused: a non-list `data` with no `hits` means this is not a list
  // endpoint, and the caller is told so rather than handed nothing.
  let data = body.get("data");
  let (items, nested) = match data {
    Some(Value::Array(items)) => (items.clone(), None),
    Some(Value::Object(obj)) if obj.contains_key("hits") => {
      let hits = match obj.get("hits") {
        Some(Value::Array(hits)) => hits.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(other) => {
          return Err(ConnectorError::Decode(format!(
            "{path}: expected hits to be a list, got {other}"
          )));
        }
      };
      (hits, obj.get("pagination"))
    }
    // A null or absent `data` is an empty page, not a failure.
    None | Some(Value::Null) => (Vec::new(), None),
    Some(other) => {
      return Err(ConnectorError::Decode(format!(
        "{path}: expected a list, got {other}"
      )));
    }
  };

  let total = nested
    .or_else(|| body.get("pagination"))
    .and_then(|p| p.get("total"))
    .and_then(Value::as_u64);
  Ok((items, total))
}

/// Page through a list endpoint.
///
/// Paging is driven by what the console sends back, never by the
/// connector's own counter alone. `pagination.total` is the evidence
/// that this endpoint pages at all: an endpoint that answers with the
/// whole collection carries no `pagination` sibling and returns the
/// same body for every `page_num`, so asking for a second page would
/// collect the same rows again and never see an empty page. The read
/// therefore stops at the first page that carries no total, and stops
/// with a reason at any page that adds no rows it has not already seen.
///
/// `label` names what is being read for the sweep's progress screen;
/// each page is reported as it lands, because a large console is many
/// round trips and silence for all of them is the wrong feedback.
///
/// `post` carries the filter document for an endpoint that reads
/// through a `POST`; `None` is the usual `GET`.
pub async fn paged(
  http: &RestrictedHttp,
  path: &str,
  query: &[(&str, String)],
  post: Option<&Value>,
  progress: &Progress,
  label: &str,
) -> Paged {
  let mut out = Paged::default();
  let mut seen: BTreeSet<String> = BTreeSet::new();
  let mut page_num: u64 = 1;

  while page_num <= MAX_PAGES {
    let mut q: Vec<(&str, String)> = query.to_vec();
    q.push(("page_num", page_num.to_string()));
    q.push(("page_size", PAGE_SIZE.to_owned()));

    let (items, total) = match page(http, path, &q, post).await {
      Ok(pair) => pair,
      Err(e) => {
        warn!(path, error = %e, "enumeration stopped short");
        out.incomplete = Some(format!("{path}: {e}"));
        return out;
      }
    };

    // A row already collected is a console repeating itself, not a new
    // account. Keeping the first copy is what makes a repeat visible
    // below rather than silently multiplying the fact stream.
    let got = items.len();
    let mut fresh = 0_usize;
    for item in items {
      if seen.insert(identity(&item)) {
        out.items.push(item);
        fresh += 1;
      }
    }
    progress.counted(
      format!("{label}: page {page_num}"),
      out.items.len() as u64,
      total,
    );

    let Some(total) = total else {
      // No `pagination` on the first page: this endpoint does not page,
      // and what arrived is the whole collection. Later, it means the
      // answer changed shape mid-enumeration, which is a read that
      // stopped short rather than one that finished.
      if page_num > 1 {
        out.incomplete = Some(format!(
          "{path}: page {page_num} carried no pagination; stopped with {} rows",
          out.items.len()
        ));
      }
      return out;
    };

    if out.items.len() as u64 >= total {
      return out;
    }
    if got == 0 {
      out.incomplete = Some(format!(
        "{path}: the console reported {total} rows and stopped after {}",
        out.items.len()
      ));
      return out;
    }
    if fresh == 0 {
      // Below `total` and every row was one already read: the console
      // is ignoring `page_num`, and asking again only repeats it.
      out.incomplete = Some(format!(
        "{path}: page {page_num} repeated rows already read; stopped at {} of \
         {total}",
        out.items.len()
      ));
      return out;
    }
    page_num += 1;
  }

  out.incomplete = Some(format!("{path}: stopped after {MAX_PAGES} pages"));
  out
}

/// What makes one row the same row as another across pages. The
/// developer API keys every resource by `id`; anything without one is
/// compared whole.
fn identity(item: &Value) -> String {
  // A log entry has no `id` of its own and carries the search index's
  // `_id` instead. Without it, two openings that happen to render
  // identically would collapse into one — and, worse, a page of them
  // would look like a console repeating itself.
  for key in ["id", "_id"] {
    if let Some(id) = item.get(key).and_then(Value::as_str) {
      return format!("{key}:{id}");
    }
  }
  format!("row:{item}")
}

/// Read a list endpoint in one body.
///
/// A collection small enough to arrive whole is the usual case, and
/// then `pagination` is absent and there is nothing to check. When the
/// console does send one, it is the console saying the collection is
/// larger than the body: the rows beyond the first page are missing and
/// the read says so rather than presenting the first page as all of it.
pub async fn list(
  http: &RestrictedHttp,
  path: &str,
  progress: &Progress,
  label: &str,
) -> Paged {
  match page(http, path, &[], None).await {
    Ok((items, total)) => {
      progress.counted(
        format!("{label}: {} read", items.len()),
        items.len() as u64,
        total,
      );
      let mut out = Paged {
        items,
        incomplete: None,
      };
      let got = out.items.len() as u64;
      if let Some(total) = total
        && got < total
      {
        warn!(path, total, got, "collection was truncated");
        out.incomplete = Some(format!(
          "{path}: the console reported {total} rows and sent {got}; this \
           collection pages"
        ));
      }
      out
    }
    Err(e) => {
      warn!(path, error = %e, "collection could not be read");
      Paged {
        items:      Vec::new(),
        incomplete: Some(format!("{path}: {e}")),
      }
    }
  }
}

/// Every account in the console, with each one's access policies folded
/// in — `expand[]=access_policy` is what puts a policy's `resources` on
/// the user, so entitlement is answered without a call per account.
pub async fn users(http: &RestrictedHttp, progress: &Progress) -> Paged {
  paged(
    http,
    USERS_PATH,
    &[("expand[]", "access_policy".to_owned())],
    None,
    progress,
    "accounts",
  )
  .await
}

/// Every door, for naming the resources a policy grants.
pub async fn doors(http: &RestrictedHttp, progress: &Progress) -> Paged {
  paged(http, DOORS_PATH, &[], None, progress, "doors").await
}

/// Every door opening the console logged between `since` and `until`,
/// as Unix seconds.
///
/// This is the one `POST`, and the one endpoint that is not a
/// directory: the filter — topic and window — is a document the vendor
/// takes in the body while the paging stays in the query string. It
/// reads the log and changes nothing, which is why [`ReadMethod::Post`]
/// exists.
///
/// The window is bounded because the log is the largest thing on a busy
/// console: it grows with door traffic rather than with headcount, so
/// an unbounded read would be the whole history of the building.
pub async fn door_openings(
  http: &RestrictedHttp,
  since: i64,
  until: i64,
  progress: &Progress,
) -> Paged {
  let filter = json!({
    "topic": DOOR_OPENINGS_TOPIC,
    "since": since,
    "until": until,
  });
  paged(
    http,
    SYSTEM_LOGS_PATH,
    &[],
    Some(&filter),
    progress,
    "door openings",
  )
  .await
}

/// Every user group.
pub async fn user_groups(http: &RestrictedHttp, progress: &Progress) -> Paged {
  list(http, USER_GROUPS_PATH, progress, "groups").await
}

/// Every account in one group. This is the read that names the members,
/// rather than guessing at the group's own `up_id` fields.
pub async fn group_members(
  http: &RestrictedHttp,
  group_id: &str,
  progress: &Progress,
) -> Paged {
  list(
    http,
    &format!("{USER_GROUPS_PATH}/{group_id}/users/all"),
    progress,
    "group members",
  )
  .await
}

/// Turn a read that produced nothing into an error, and one that
/// produced something incomplete into a reason.
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
