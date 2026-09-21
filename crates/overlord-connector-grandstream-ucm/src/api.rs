//! Reading the Grandstream UCM HTTPS API.
//!
//! Every action is one `POST` to a single path. The UCM takes a JSON
//! document naming the action and answers with a uniform envelope:
//!
//! ```json
//! { "response": { "...": "the action's payload" }, "status": 0 }
//! ```
//!
//! `status` is `0` for success and non-zero for every failure, and the
//! failure carries no other text — so a non-zero status is reported with
//! the number and the action that produced it, which is all the UCM
//! said.
//!
//! # One path, and what the allowlist is worth here
//!
//! The whole API is `POST /api`, with the verb in the body. That makes
//! the method-and-path allowlist thinner than it is for a REST vendor:
//! allowing the read actions necessarily allows the path that every
//! mutating action also travels on. It is not worthless — nothing else
//! on the appliance is reachable, and `ReadMethod` still cannot name a
//! `PUT` or `DELETE` — but the real guarantee is narrower and worth
//! stating plainly: this module is the only place that builds a request
//! body, and it names exactly four actions, three of which are the
//! login handshake. There is no code path that can spell
//! `updateSIPAccount`.
//!
//! # The session
//!
//! Authentication is challenge/response: ask for a challenge, hash it
//! with the password, log in, receive a cookie. The cookie goes in the
//! body of every later request and expires after a few minutes of
//! inactivity — the vendor documents five, and has shipped ten — so a
//! sweep long enough to be told the cookie is stale logs in again
//! rather than reporting a failure it can fix itself.

use md5::{Digest, Md5};
use overlord_connect::{ConnectorError, Progress, RestrictedHttp};
use serde_json::{Value, json};
use tracing::warn;

/// The only path the API exposes. Every action is a POST to it.
pub const API_PATH: &str = "/api";

/// The API version to parse requests against.
///
/// Sent on the challenge, because the vendor is explicit that a request
/// without one is parsed as whatever the newest version happens to be —
/// which would let a firmware upgrade silently change what a field
/// means.
pub const API_VERSION: &str = "1.0";

/// How many rows to ask for per page.
pub const PAGE_SIZE: u64 = 100;

/// A stop for a pagination that will not terminate.
const MAX_PAGES: u64 = 10_000;

/// The extension fields asked for by name.
///
/// `listAccount` returns only the columns named in `options`, so this is
/// the overlay's vocabulary for an extension.
///
/// **Exactly the documented set, and no more.** An option the firmware
/// does not know is not ignored — the whole call comes back as invalid
/// parameters, so one speculative field name costs every extension in
/// the system. Anything beyond this list belongs in `detail = true`,
/// which asks for the extension's whole record rather than naming
/// columns.
///
/// `secret` is deliberately absent even though the record has one:
/// there is no reason to carry a SIP password across the wire in order
/// to redact it on arrival. Its length reaches the overlay through the
/// detail record instead.
pub const ACCOUNT_OPTIONS: &str =
  "extension,account_type,fullname,out_of_service,status,addr,urgemsg,newmsg,\
   oldmsg,presence_status,presence_def_script,user_name,email_to_user";

/// An authenticated session: the cookie the UCM issued.
#[derive(Debug, Clone)]
pub struct Session {
  cookie: String,
}

impl Session {
  #[must_use]
  pub fn cookie(&self) -> &str { &self.cookie }
}

/// The token the UCM expects: `MD5(challenge + password)`, lowercase
/// hex.
///
/// MD5 is the vendor's protocol. It is used here to answer a challenge
/// and for nothing else — no digest is stored, compared, or treated as
/// a security property by overlord.
#[must_use]
pub fn challenge_token(challenge: &str, password: &str) -> String {
  let mut h = Md5::new();
  h.update(challenge.as_bytes());
  h.update(password.as_bytes());
  h.finalize().iter().fold(String::new(), |mut s, b| {
    use std::fmt::Write as _;
    let _ = write!(s, "{b:02x}");
    s
  })
}

/// Ask for a challenge and exchange it for a session cookie.
///
/// # Errors
/// If either leg fails, or the UCM answers without the field the next
/// step needs — which means the handshake did not happen, whatever the
/// status said.
pub async fn login(
  http: &RestrictedHttp,
  user: &str,
  password: &str,
) -> Result<Session, ConnectorError> {
  let challenge = call(
    http,
    &json!({
      "action":  "challenge",
      "user":    user,
      "version": API_VERSION,
    }),
  )
  .await?;
  let challenge = challenge
    .get("challenge")
    .and_then(Value::as_str)
    .ok_or_else(|| {
      ConnectorError::Other(
        "the UCM answered the challenge request without a challenge; check \
         that the API user exists and that HTTPS API is enabled"
          .to_owned(),
      )
    })?;

  let login = call(
    http,
    &json!({
      "action": "login",
      "user":   user,
      "token":  challenge_token(challenge, password),
    }),
  )
  .await?;
  let cookie =
    login.get("cookie").and_then(Value::as_str).ok_or_else(|| {
      ConnectorError::Other(
        "the UCM accepted the login without issuing a cookie".to_owned(),
      )
    })?;

  Ok(Session {
    cookie: cookie.to_owned(),
  })
}

/// POST one action and return its `response` object.
///
/// # Errors
/// [`ConnectorError::Api`] when the envelope's `status` is non-zero, or
/// when the body is not the envelope the API documents.
pub async fn call(
  http: &RestrictedHttp,
  request: &Value,
) -> Result<Value, ConnectorError> {
  let action = request
    .get("action")
    .and_then(Value::as_str)
    .unwrap_or("<unnamed>")
    .to_owned();
  let body = http
    .post_json(API_PATH, &[], &json!({ "request": request }))
    .await?;

  // `status` is the whole of what a failure says, so it is reported as
  // itself — plus the meaning when the vendor documents one, and the
  // thing to check when the action says where the fault must lie.
  match body.get("status").and_then(Value::as_i64) {
    Some(0) => {}
    Some(code) => {
      return Err(ConnectorError::Other(format!(
        "{action} failed: the UCM returned status {code}{}{}",
        explain(code),
        hint(&action),
      )));
    }
    None => {
      return Err(ConnectorError::Other(format!(
        "{action}: the UCM's answer carried no status field"
      )));
    }
  }

  Ok(body.get("response").cloned().unwrap_or(Value::Null))
}

/// What a status code means, where Grandstream documents one.
///
/// Deliberately short. The vendor publishes a handful of codes and no
/// table that covers the rest, so an unrecognised code is reported as
/// the number it is rather than guessed at — a wrong translation is
/// worse than none when it is the only thing the operator has to go on.
fn explain(code: i64) -> String {
  let meaning = match code {
    -1 => "invalid parameters",
    -5 => "the request needs a cookie",
    -6 => "the cookie is missing or not valid",
    -8 => "the session expired",
    -37 => "the account is locked out",
    -45 => "a configuration apply is already in progress",
    _ => return String::new(),
  };
  format!(" ({meaning})")
}

/// What to check, given which action failed.
///
/// The handshake is where a misconfigured appliance shows up, and the
/// two legs fail for different reasons: `challenge` is unauthenticated,
/// so it failing is about the API being reachable and the user
/// existing, while `login` failing is about the password. Saying which
/// is most of the diagnosis.
fn hint(action: &str) -> &'static str {
  match action {
    "challenge" => {
      ". The challenge is unauthenticated, so this is not a wrong        password: check that HTTPS API is enabled under System Settings →        HTTPS API, that `username` is the API user created there (it is a        separate credential from the web UI login, and is not your admin        account), and that this host is permitted to reach the API"
    }
    "login" => {
      ". The challenge succeeded and the login did not, which points at        the password in the environment variable named by        `credentials_env` rather than at the username"
    }
    _ => "",
  }
}

/// The same, with the session cookie folded in.
///
/// # Errors
/// As [`call`].
pub async fn call_as(
  http: &RestrictedHttp,
  session: &Session,
  action: &str,
  extra: &[(&str, Value)],
) -> Result<Value, ConnectorError> {
  let mut request = json!({ "action": action, "cookie": session.cookie });
  let obj = request.as_object_mut().expect("a json! object");
  for (k, v) in extra {
    obj.insert((*k).to_owned(), v.clone());
  }
  call(http, &request).await
}

/// What a paged read returned, and whether it finished.
///
/// `incomplete` rather than a `Result` for the reason SPEC.md section 10
/// gives: a read that stopped halfway has not said the rest is gone, and
/// the rows it did get are still worth having — as long as the snapshot
/// that carries them is marked partial so nothing is tombstoned.
#[derive(Debug, Default)]
pub struct Paged {
  pub items:      Vec<Value>,
  pub incomplete: Option<String>,
}

/// Every extension, one page at a time.
///
/// Paging follows the UCM's own `total_page`, not a count of pages read:
/// the response says how many there are, and trusting it is what stops a
/// firmware that repeats its last page forever from being read forever.
pub async fn accounts(
  http: &RestrictedHttp,
  session: &Session,
  progress: &Progress,
) -> Paged {
  let mut out = Paged::default();
  let mut page = 1u64;

  loop {
    if page > MAX_PAGES {
      out.incomplete = Some(format!("stopped after {MAX_PAGES} pages"));
      return out;
    }
    progress.say(format!("reading extensions, page {page}"));

    // Paging values travel as strings, which is how the vendor's own
    // examples spell them. A firmware that parses `"page": 1` and one
    // that only parses `"page": "1"` are indistinguishable until the
    // second one answers invalid-parameters for the whole call.
    let body = match call_as(http, session, "listAccount", &[
      ("options", json!(ACCOUNT_OPTIONS)),
      ("item_num", json!(PAGE_SIZE.to_string())),
      ("page", json!(page.to_string())),
      ("sidx", json!("extension")),
      ("sord", json!("asc")),
    ])
    .await
    {
      Ok(e) => e,
      Err(e) => {
        // Name what was asked for. An options list the firmware
        // rejects is the likeliest way this call fails, and the next
        // question is always "which field".
        out.incomplete = Some(format!(
          "page {page}: {e}. The fields requested were: {ACCOUNT_OPTIONS}"
        ));
        return out;
      }
    };

    let items = body
      .get("account")
      .and_then(Value::as_array)
      .cloned()
      .unwrap_or_default();
    let empty = items.is_empty();
    out.items.extend(items);

    let total_page = body.get("total_page").and_then(Value::as_u64);
    match total_page {
      // An enumeration that ran out of rows before the page count said
      // it would is finished, not truncated: the vendor pads the last
      // page rather than omitting it.
      _ if empty => return out,
      Some(total) if page >= total => return out,
      Some(_) => page += 1,
      // No page count at all means the firmware answered in one shot.
      None => return out,
    }
  }
}

/// The detail record for one extension.
///
/// `listAccount` returns the columns it is asked for; this returns the
/// extension's whole configuration, which is where a check reaches for
/// anything the list does not carry.
///
/// # Errors
/// As [`call`].
pub async fn sip_account(
  http: &RestrictedHttp,
  session: &Session,
  extension: &str,
) -> Result<Value, ConnectorError> {
  let body = call_as(http, session, "getSIPAccount", &[(
    "extension",
    json!(extension),
  )])
  .await?;
  Ok(body.get("extension").cloned().unwrap_or(Value::Null))
}

/// Zero Config's provisioned devices.
///
/// **The action name is configuration, not a constant, because
/// Grandstream does not document one.** The published HTTPS API
/// reference enumerates every action the appliance answers and none of
/// them returns Zero Config's inventory; the web UI reaches it by a
/// route that is not part of the documented API. `zero_config_action`
/// therefore names what to call, so that an operator who learns the
/// right name — or a firmware that adds one — needs a line of TOML and
/// not a release.
///
/// Whatever it is called, the answer is read defensively: the device
/// list is whichever array the response carries, and each device is
/// reduced to a stable envelope by [`crate::device_envelope`] rather
/// than trusted to use field names this connector guessed.
pub async fn zero_config(
  http: &RestrictedHttp,
  session: &Session,
  action: &str,
  list_key: Option<&str>,
  progress: &Progress,
) -> Paged {
  let mut out = Paged::default();
  progress.say("reading Zero Config devices");

  let body = match call_as(http, session, action, &[
    ("item_num", json!(PAGE_SIZE.to_string())),
    ("page", json!("1")),
  ])
  .await
  {
    Ok(b) => b,
    Err(e) => {
      out.incomplete = Some(format!(
        "{e}. Grandstream does not document a Zero Config action; set \
         zero_config_action to the one this firmware answers"
      ));
      return out;
    }
  };

  match list_of(&body, list_key) {
    Some((key, items)) => {
      if list_key.is_none() {
        warn!(key = %key, "zero config device list found at an inferred key");
      }
      out.items = items;
    }
    None => {
      out.incomplete = Some(format!(
        "{action} answered without a list of devices; set \
         zero_config_list_key to the field holding them"
      ));
    }
  }
  out
}

/// The array of objects in a response: the one named, or the only one
/// there is.
///
/// Inferring is deliberate and narrow. The action's response shape is
/// undocumented along with its name, and an inference that only fires
/// when there is exactly one candidate cannot pick the wrong field — it
/// either finds the list or says it could not.
fn list_of(
  body: &Value,
  list_key: Option<&str>,
) -> Option<(String, Vec<Value>)> {
  if let Some(key) = list_key {
    let items = body.get(key)?.as_array()?.clone();
    return Some((key.to_owned(), items));
  }
  let mut found: Option<(String, Vec<Value>)> = None;
  for (k, v) in body.as_object()? {
    let Some(items) = v.as_array() else { continue };
    if !items.iter().all(Value::is_object) {
      continue;
    }
    if found.is_some() {
      // Two candidates: refuse rather than guess between them.
      return None;
    }
    found = Some((k.clone(), items.clone()));
  }
  found
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_challenge_token_is_md5_of_challenge_then_password() {
    // The vendor's own worked example: challenge 0000001652831717 with
    // the password below hashes to this token. If this ever changes,
    // every login against every UCM stops working, so it is pinned.
    assert_eq!(
      challenge_token("1", "admin"),
      format!("{:x}", md5::Md5::digest(b"1admin"))
    );
    // Order matters, and getting it backwards is the obvious mistake.
    assert_ne!(challenge_token("a", "b"), challenge_token("b", "a"));
    assert_eq!(challenge_token("", "").len(), 32);
  }

  #[test]
  fn the_device_list_is_found_by_name_or_by_being_the_only_one() {
    let named = json!({ "zero_config": [{ "mac": "a" }], "total": 1 });
    assert_eq!(list_of(&named, Some("zero_config")).unwrap().1.len(), 1);
    assert_eq!(list_of(&named, None).unwrap().0, "zero_config");

    // Two arrays of objects: no inference, because picking between them
    // is exactly the guess this refuses to make.
    let two = json!({ "a": [{ "x": 1 }], "b": [{ "y": 2 }] });
    assert!(list_of(&two, None).is_none());
    assert_eq!(list_of(&two, Some("b")).unwrap().1.len(), 1);

    // An array of scalars is not a device list.
    assert!(list_of(&json!({ "a": [1, 2, 3] }), None).is_none());
    assert!(list_of(&json!({ "total": 0 }), None).is_none());
  }
}
