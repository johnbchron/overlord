//! The connector against recorded Grandstream UCM response shapes.
//!
//! The extension payloads use the documented API's envelope (`response`
//! / `status`) and `listAccount`'s documented field names; the values
//! are invented. The Zero Config payloads cannot be recorded from the
//! documentation, because Grandstream documents no such action — those
//! tests pin the connector's *defensiveness* instead: that it finds the
//! device list under an unknown key, survives field names it has never
//! seen, and refuses to invent an entity when it cannot.
//!
//! The password is handed to the connector directly rather than through
//! the environment: `std::env::set_var` is unsafe in this edition and
//! `unsafe_code` is forbidden across the workspace.

use std::sync::{Arc, Mutex};

use overlord_connect::{Connector, ConnectorError, ObserveCtx, Progress};
use overlord_connector_grandstream_ucm::{
  GrandstreamUcmConnector, device_envelope, normalize_mac, redact,
  yes_no_to_bool,
};
use overlord_core::{Completeness, SystemId, Timestamp, Value as CoreValue};
use serde_json::{Value, json};
use wiremock::{
  Mock, MockServer, Request, ResponseTemplate,
  matchers::{method, path},
};

const PASSWORD: &str = "s3cret";

fn connector() -> GrandstreamUcmConnector {
  GrandstreamUcmConnector::with_password(PASSWORD)
}

fn ctx(server: &MockServer, extra: Value) -> ObserveCtx {
  let mut config = json!({
    "base_url": server.uri(),
    "username": "cdrapi",
  });
  let obj = config.as_object_mut().unwrap();
  for (k, v) in extra.as_object().into_iter().flatten() {
    obj.insert(k.clone(), v.clone());
  }
  ObserveCtx {
    system: SystemId::new("ucm"),
    started_at: "2026-01-15T00:00:00Z".parse::<Timestamp>().unwrap(),
    config,
    progress: Progress::default(),
  }
}

fn ok(response: Value) -> ResponseTemplate {
  ResponseTemplate::new(200).set_body_json(json!({
    "response": response,
    "status": 0,
  }))
}

/// A UCM standing in for the appliance: it routes on the action in the
/// body, the way the real one does, and records every action it saw.
async fn ucm(handlers: Vec<(&'static str, Value)>) -> (MockServer, Actions) {
  let server = MockServer::start().await;
  let seen: Actions = Arc::new(Mutex::new(Vec::new()));
  let log = Arc::clone(&seen);

  Mock::given(method("POST"))
    .and(path("/api"))
    .respond_with(move |req: &Request| {
      let body: Value =
        serde_json::from_slice(&req.body).unwrap_or(Value::Null);
      let action = body
        .get("request")
        .and_then(|r| r.get("action"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
      log.lock().unwrap().push(action.clone());

      match action.as_str() {
        "challenge" => ok(json!({ "challenge": "0000001652831717" })),
        "login" => ok(json!({ "cookie": "sid1-2" })),
        other => handlers
          .iter()
          .find(|(name, _)| *name == other)
          .map_or_else(
            // The appliance's way of saying no: a non-zero status and
            // nothing else.
            || ResponseTemplate::new(200).set_body_json(json!({"status": -1})),
            |(_, body)| ok(body.clone()),
          ),
      }
    })
    .mount(&server)
    .await;

  (server, seen)
}

type Actions = Arc<Mutex<Vec<String>>>;

fn account(extension: &str, name: &str, extra: Value) -> Value {
  let mut a = json!({
    "extension": extension,
    "fullname": name,
    "account_type": "SIP",
    "status": "Idle",
    "addr": "10.0.0.31:5062",
  });
  let o = a.as_object_mut().unwrap();
  for (k, v) in extra.as_object().into_iter().flatten() {
    o.insert(k.clone(), v.clone());
  }
  a
}

fn list(accounts: Vec<Value>) -> Value {
  json!({
    "account": accounts,
    "page": 1,
    "total_item": 2,
    "total_page": 1,
  })
}

async fn observe(
  server: &MockServer,
  extra: Value,
) -> Result<overlord_connect::Snapshot, ConnectorError> {
  let c = connector();
  let ctx = ctx(server, extra);
  let http = c.http(&ctx).unwrap();
  c.observe(&http, &ctx).await
}

/// The normalized overlay for each observation, via the shipped ruleset.
fn normalized(
  snapshot: &overlord_connect::Snapshot,
  ctx: &ObserveCtx,
  c: &GrandstreamUcmConnector,
) -> Vec<overlord_core::NormalizedRecord> {
  let rs = c.default_ruleset(ctx);
  snapshot
    .observations
    .iter()
    .map(|o| rs.apply(&SystemId::new("ucm"), &o.raw).unwrap().record)
    .collect()
}

// --- extensions -------------------------------------------------------

#[tokio::test]
async fn extensions_are_read_and_normalized() {
  let (server, seen) = ucm(vec![
    (
      "listAccount",
      list(vec![
        account("1001", "Ada Lovelace", json!({})),
        account("1002", "Grace Hopper", json!({ "status": "Unavailable" })),
      ]),
    ),
    (
      "listUser",
      user_list(vec![user("1001", json!("ada@x.com"), json!({}))]),
    ),
  ])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  assert_eq!(snapshot.observations.len(), 2);
  assert!(snapshot.completeness.is_complete());

  // The handshake happened, in order, before either read.
  assert_eq!(seen.lock().unwrap().as_slice(), [
    "challenge",
    "login",
    "listAccount",
    "listUser"
  ]);

  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, json!({})), &c);
  assert_eq!(records[0].entity_type.as_str(), "phone-extension");
  assert_eq!(records[0].entity_key.as_str(), "1001");
  assert_eq!(records[0].display_name.as_deref(), Some("Ada Lovelace"));
  assert_eq!(
    records[0].get("email"),
    CoreValue::String("ada@x.com".to_owned())
  );
  // "Idle" is a registered extension, not an idle person.
  assert_eq!(records[0].status, overlord_core::EntityStatus::Active);
  // "Unavailable" is not a claim that the extension is deprovisioned.
  assert_eq!(records[1].status, overlord_core::EntityStatus::Unknown);
}

#[tokio::test]
async fn an_out_of_service_extension_is_suspended_whatever_its_status_says() {
  let (server, _) = ucm(vec![(
    "listAccount",
    list(vec![account(
      "1003",
      "Reception",
      json!({ "out_of_service": "1", "status": "Idle" }),
    )]),
  )])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, json!({})), &c);
  assert_eq!(records[0].status, overlord_core::EntityStatus::Suspended);
}

#[tokio::test]
async fn a_sip_password_never_reaches_the_fact_stream() {
  // The fact stream is a plaintext file kept forever. What a check
  // needs is that a secret exists and how long it is.
  let (server, _) = ucm(vec![
    (
      "listAccount",
      list(vec![account("1001", "Ada", json!({ "secret": "hunter2" }))]),
    ),
    (
      "getSIPAccount",
      json!({ "extension": {
        "extension": "1001",
        "secret": "hunter2",
        "vmsecret": "1234",
        "sip_password": "nested",
        "hasvoicemail": "yes",
      }}),
    ),
  ])
  .await;

  let snapshot = observe(&server, json!({ "detail": true })).await.unwrap();
  let raw = serde_json::to_string(&snapshot.observations[0].raw).unwrap();
  for secret in ["hunter2", "1234", "nested"] {
    assert!(
      !raw.contains(secret),
      "{secret} reached the observation: {raw}"
    );
  }

  let c = connector();
  let records =
    normalized(&snapshot, &ctx(&server, json!({ "detail": true })), &c);
  assert_eq!(records[0].get("secret_len"), CoreValue::Number(7.0));
  assert_eq!(
    records[0].get("voicemail_secret_len"),
    CoreValue::Number(4.0)
  );
  assert_eq!(records[0].get("has_secret"), CoreValue::Bool(true));
}

#[tokio::test]
async fn detail_is_not_fetched_unless_it_is_asked_for() {
  // One call per extension is the cost, and the list already carries
  // what most checks need.
  let (server, seen) = ucm(vec![(
    "listAccount",
    list(vec![account("1001", "Ada", json!({}))]),
  )])
  .await;

  observe(&server, json!({})).await.unwrap();
  assert!(!seen.lock().unwrap().iter().any(|a| a == "getSIPAccount"));
}

#[tokio::test]
async fn one_extensions_detail_failing_makes_the_snapshot_partial() {
  // Not a failure: the enumeration succeeded and one record is thinner
  // than the rest. Partial is what stops the gap tombstoning anything.
  let (server, _) = ucm(vec![(
    "listAccount",
    list(vec![account("1001", "Ada", json!({}))]),
  )])
  .await;

  let snapshot = observe(&server, json!({ "detail": true, "users": false }))
    .await
    .unwrap();
  assert_eq!(snapshot.observations.len(), 1);
  assert!(matches!(
    snapshot.completeness,
    Completeness::Partial { .. }
  ));
  assert!(
    snapshot.warnings[0].contains("1001"),
    "{:?}",
    snapshot.warnings
  );
}

#[tokio::test]
async fn a_ucm_that_reports_no_extensions_is_a_failure_not_an_empty_sweep() {
  // Otherwise the first sweep against a misconfigured API user would
  // tombstone every extension the last one found.
  let (server, _) = ucm(vec![("listAccount", list(vec![]))]).await;
  let e = observe(&server, json!({})).await.unwrap_err();
  assert!(e.to_string().contains("no extensions"), "{e}");
}

#[tokio::test]
async fn a_refused_login_is_reported_as_itself() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/api"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "status": -6
    })))
    .mount(&server)
    .await;

  let e = observe(&server, json!({})).await.unwrap_err().to_string();
  assert!(e.contains("status -6"), "{e}");
  // The documented meaning, when there is one.
  assert!(e.contains("cookie is missing"), "{e}");
  // A failure at the challenge is not a wrong password, and saying so
  // is most of the diagnosis: the challenge is unauthenticated.
  assert!(e.contains("challenge failed"), "{e}");
  assert!(e.contains("HTTPS API"), "{e}");
  assert!(!e.contains("password in the environment"), "{e}");
}

#[tokio::test]
async fn an_undocumented_status_is_reported_as_the_number_it_is() {
  // Grandstream publishes a handful of codes and no table covering the
  // rest. A wrong translation is worse than none when the number is the
  // only thing the operator has to go on.
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/api"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "status": -47
    })))
    .mount(&server)
    .await;

  let e = observe(&server, json!({})).await.unwrap_err().to_string();
  assert!(e.contains("status -47"), "{e}");
  // No invented meaning in the parentheses.
  assert!(!e.contains("(-47"), "{e}");
  assert!(e.contains("separate credential"), "{e}");
}

#[tokio::test]
async fn a_failure_after_the_challenge_points_at_the_password_instead() {
  let server = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/api"))
    .respond_with(move |req: &Request| {
      let body: Value =
        serde_json::from_slice(&req.body).unwrap_or(Value::Null);
      let action = body
        .get("request")
        .and_then(|r| r.get("action"))
        .and_then(Value::as_str)
        .unwrap_or("");
      if action == "challenge" {
        ok(json!({ "challenge": "0000001652831717" }))
      } else {
        ResponseTemplate::new(200).set_body_json(json!({ "status": -47 }))
      }
    })
    .mount(&server)
    .await;

  let e = observe(&server, json!({})).await.unwrap_err().to_string();
  assert!(e.contains("login failed"), "{e}");
  assert!(e.contains("credentials_env"), "{e}");
  assert!(!e.contains("HTTPS API is enabled"), "{e}");
}

#[tokio::test]
async fn the_request_matches_the_documented_shape() {
  // Both halves of this are how the connector first failed against a
  // real UCM6308A: an options list carrying two undocumented fields,
  // and paging values sent as JSON numbers. Either one makes the
  // firmware answer invalid-parameters for the whole call, which
  // presents as a partial sweep with no facts rather than as an error
  // pointing anywhere useful.
  let server = MockServer::start().await;
  let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
  let log = Arc::clone(&seen);

  Mock::given(method("POST"))
    .and(path("/api"))
    .respond_with(move |req: &Request| {
      let body: Value =
        serde_json::from_slice(&req.body).unwrap_or(Value::Null);
      let request = body.get("request").cloned().unwrap_or(Value::Null);
      let action = request
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
      log.lock().unwrap().push(request);
      match action.as_str() {
        "challenge" => ok(json!({ "challenge": "c" })),
        "login" => ok(json!({ "cookie": "sid1-2" })),
        _ => ok(list(vec![account("1001", "Ada", json!({}))])),
      }
    })
    .mount(&server)
    .await;

  observe(&server, json!({})).await.unwrap();

  let requests = seen.lock().unwrap().clone();
  let list_call = requests
    .iter()
    .find(|r| r["action"] == json!("listAccount"))
    .expect("listAccount was called");

  // Paging values are strings, as the vendor's examples spell them.
  assert_eq!(list_call["page"], json!("1"));
  assert_eq!(list_call["item_num"], json!("100"));

  // Every requested option is one Grandstream documents. A field the
  // firmware does not know is not ignored — it fails the whole call.
  const DOCUMENTED: [&str; 13] = [
    "extension",
    "account_type",
    "fullname",
    "out_of_service",
    "status",
    "addr",
    "urgemsg",
    "newmsg",
    "oldmsg",
    "presence_status",
    "presence_def_script",
    "user_name",
    "email_to_user",
  ];
  let options = list_call["options"].as_str().unwrap();
  for field in options.split(',') {
    assert!(
      DOCUMENTED.contains(&field),
      "{field:?} is not a documented listAccount option; asking for it        \
       fails the whole call"
    );
  }
  // And the password is never asked for in the first place.
  assert!(!options.contains("secret"), "{options}");
}

#[tokio::test]
async fn a_rejected_options_list_fails_the_system_and_says_what_it_asked_for() {
  // The failure mode that started this. Two things were wrong with how
  // it reported: the sweep said "partial" when nothing had been read at
  // all — which reads as "worked, found nothing", the one thing that
  // did not happen — and nothing named the cause.
  let (server, _) = ucm(vec![]).await;
  let e = observe(&server, json!({})).await.unwrap_err();

  assert!(e.is_partial(), "a failed read must not tombstone anything");
  let text = e.to_string();
  assert!(text.contains("no extensions were read"), "{text}");
  assert!(text.contains("listAccount failed"), "{text}");
  assert!(text.contains("fields requested"), "{text}");
  assert!(text.contains("email_to_user"), "{text}");
}

#[tokio::test]
async fn an_unanswered_zero_config_action_fails_the_system_too() {
  let (server, _) = ucm(vec![]).await;
  let e = observe(&server, json!({ "mode": "devices" }))
    .await
    .unwrap_err();

  assert!(e.is_partial());
  let text = e.to_string();
  assert!(text.contains("no Zero Config devices were read"), "{text}");
  assert!(text.contains("zero_config_action"), "{text}");
}

#[tokio::test]
async fn a_ucm_with_no_provisioned_handsets_is_a_real_answer() {
  // Unlike extensions: Zero Config legitimately holds nothing, and
  // saying so is not the same as failing to read it.
  let (server, _) =
    ucm(vec![("listZeroConfig", json!({ "zero_config": [] }))]).await;
  let snapshot = observe(&server, json!({ "mode": "devices" }))
    .await
    .unwrap();

  assert!(snapshot.observations.is_empty());
  assert!(snapshot.completeness.is_complete());
}

/// A `listUser` page, as the guide's worked example shapes it: the
/// array lives under `user_id`, and `user_name` is the extension.
fn user_list(users: Vec<Value>) -> Value {
  json!({ "user_id": users, "page": 1, "total_item": 1, "total_page": 1 })
}

fn user(extension: &str, email: Value, extra: Value) -> Value {
  let mut u = json!({
    "user_id": 2,
    "user_name": extension,
    "privilege": 3,
    "first_name": "Ada",
    "last_name": "Lovelace",
    "department": "engineering",
    "email": email,
    "email_to_user": "yes",
    "enable_multiple_extension": "no",
    "multiple_extension": null,
    "cookie": "sid523099813-1555662509",
    "login_time": "2019-04-19 16:49:05",
  });
  let o = u.as_object_mut().unwrap();
  for (k, v) in extra.as_object().into_iter().flatten() {
    o.insert(k.clone(), v.clone());
  }
  u
}

// --- voicemail forwarding addresses -----------------------------------

#[tokio::test]
async fn an_extensions_email_address_comes_from_its_user_record() {
  // `listAccount` carries `email_to_user`, a yes/no flag, and no
  // address at all. The address voicemail is emailed to is on the user
  // record, which is why the connector reads both.
  let (server, seen) = ucm(vec![
    (
      "listAccount",
      list(vec![account("1001", "Ada Lovelace", json!({}))]),
    ),
    (
      "listUser",
      user_list(vec![user("1001", json!("ada@example.com"), json!({}))]),
    ),
  ])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  assert!(seen.lock().unwrap().iter().any(|a| a == "listUser"));
  assert!(snapshot.completeness.is_complete());

  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, json!({})), &c);
  assert_eq!(
    records[0].get("email"),
    CoreValue::String("ada@example.com".to_owned())
  );
  assert_eq!(records[0].get("email_to_user"), CoreValue::Bool(true));
  assert_eq!(
    records[0].get("department"),
    CoreValue::String("engineering".to_owned())
  );
}

#[tokio::test]
async fn an_extension_with_no_user_record_has_no_address_rather_than_a_wrong_one()
 {
  // Null is not an empty address. An extension the appliance keeps no
  // user for — a paging slot, a conference room — simply has none.
  let (server, _) = ucm(vec![
    (
      "listAccount",
      list(vec![
        account("1001", "Ada", json!({})),
        account("7000", "Paging", json!({})),
      ]),
    ),
    (
      "listUser",
      user_list(vec![user("1001", json!("ada@example.com"), json!({}))]),
    ),
  ])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, json!({})), &c);
  let paging = records
    .iter()
    .find(|r| r.entity_key.as_str() == "7000")
    .unwrap();
  assert_eq!(paging.get("email"), CoreValue::Null);
  assert_eq!(paging.get("email_to_user"), CoreValue::Null);
}

#[tokio::test]
async fn a_user_whose_address_is_unset_is_null_not_the_flag() {
  // The bug this replaced mapped `email` to `email_to_user`, putting
  // the string "no" in the overlay's email field — which is what the
  // identity resolver reads to propose links between systems.
  let (server, _) = ucm(vec![
    ("listAccount", list(vec![account("1001", "Ada", json!({}))])),
    (
      "listUser",
      user_list(vec![user(
        "1001",
        Value::Null,
        json!({ "email_to_user": "no" }),
      )]),
    ),
  ])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, json!({})), &c);
  assert_eq!(records[0].get("email"), CoreValue::Null);
  assert_eq!(records[0].get("email_to_user"), CoreValue::Bool(false));
}

#[tokio::test]
async fn a_web_session_cookie_never_reaches_the_fact_stream() {
  // A user record carries the live session id of whoever is logged into
  // the web UI as that user. Unlike a password, even its length is
  // worth nothing.
  let (server, _) = ucm(vec![
    ("listAccount", list(vec![account("1001", "Ada", json!({}))])),
    (
      "listUser",
      user_list(vec![user("1001", json!("ada@example.com"), json!({}))]),
    ),
  ])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  let raw = serde_json::to_string(&snapshot.observations[0].raw).unwrap();
  assert!(!raw.contains("sid523099813"), "{raw}");
  assert!(!raw.contains("cookie"), "{raw}");
  // The rest of the record is still there.
  assert!(raw.contains("ada@example.com"));
}

#[tokio::test]
async fn a_failed_user_read_is_a_gap_not_a_failure() {
  // Extensions still collect; they just have no addresses this sweep.
  // Partial is what stops the gap tombstoning anything.
  let (server, _) = ucm(vec![(
    "listAccount",
    list(vec![account("1001", "Ada", json!({}))]),
  )])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  assert_eq!(snapshot.observations.len(), 1);
  assert!(matches!(
    snapshot.completeness,
    Completeness::Partial { .. }
  ));
  assert!(
    snapshot.warnings[0].contains("listUser"),
    "{:?}",
    snapshot.warnings
  );
}

#[tokio::test]
async fn users_can_be_turned_off() {
  let (server, seen) = ucm(vec![(
    "listAccount",
    list(vec![account("1001", "Ada", json!({}))]),
  )])
  .await;

  let snapshot = observe(&server, json!({ "users": false })).await.unwrap();
  assert!(!seen.lock().unwrap().iter().any(|a| a == "listUser"));
  // And with the read not attempted, there is no gap to report.
  assert!(snapshot.completeness.is_complete());
}

#[tokio::test]
async fn a_user_holding_several_extensions_attaches_to_each() {
  let (server, _) = ucm(vec![
    (
      "listAccount",
      list(vec![
        account("1001", "Ada", json!({})),
        account("1002", "Ada desk", json!({})),
      ]),
    ),
    (
      "listUser",
      user_list(vec![user(
        "1001",
        json!("ada@example.com"),
        json!({
          "enable_multiple_extension": "yes",
          "multiple_extension": "1002",
        }),
      )]),
    ),
  ])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, json!({})), &c);
  for r in &records {
    assert_eq!(
      r.get("email"),
      CoreValue::String("ada@example.com".to_owned()),
      "{} missed the address",
      r.entity_key
    );
  }
}

#[tokio::test]
async fn list_user_asks_for_nothing_it_does_not_need() {
  // A UCM6308A answers -26 to the sort parameters the UCM62xx guide's
  // example sends — it sorts by `extension`, which the user record does
  // not have. Nothing here needs an order, so nothing here asks for one.
  let server = MockServer::start().await;
  let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
  let log = Arc::clone(&seen);

  Mock::given(method("POST"))
    .and(path("/api"))
    .respond_with(move |req: &Request| {
      let body: Value =
        serde_json::from_slice(&req.body).unwrap_or(Value::Null);
      let request = body.get("request").cloned().unwrap_or(Value::Null);
      let action = request
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
      log.lock().unwrap().push(request);
      match action.as_str() {
        "challenge" => ok(json!({ "challenge": "c" })),
        "login" => ok(json!({ "cookie": "sid1-2" })),
        "listUser" => {
          ok(user_list(vec![user("1001", json!("ada@x.com"), json!({}))]))
        }
        _ => ok(list(vec![account("1001", "Ada", json!({}))])),
      }
    })
    .mount(&server)
    .await;

  observe(&server, json!({})).await.unwrap();

  let call = seen
    .lock()
    .unwrap()
    .iter()
    .find(|r| r["action"] == json!("listUser"))
    .cloned()
    .expect("listUser was called");
  assert!(call.get("sidx").is_none(), "{call}");
  assert!(call.get("sord").is_none(), "{call}");
  // The paging it does need still travels as strings.
  assert_eq!(call["page"], json!("1"));
  assert_eq!(call["item_num"], json!("100"));
}

#[tokio::test]
async fn list_user_retries_once_with_no_parameters_at_all() {
  // Firmwares disagree about this call's parameters and nothing
  // published says what any of them accepts. Asking for the whole
  // collection in one request is the fallback that depends on least.
  let server = MockServer::start().await;
  let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
  let seen = Arc::clone(&log);

  Mock::given(method("POST"))
    .and(path("/api"))
    .respond_with(move |req: &Request| {
      let body: Value =
        serde_json::from_slice(&req.body).unwrap_or(Value::Null);
      let request = body.get("request").cloned().unwrap_or(Value::Null);
      let action = request
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
      seen.lock().unwrap().push(request.clone());
      match action.as_str() {
        "challenge" => ok(json!({ "challenge": "c" })),
        "login" => ok(json!({ "cookie": "sid1-2" })),
        "listUser" => {
          // Rejects any paging, the way -26 did.
          if request.get("item_num").is_some() {
            ResponseTemplate::new(200).set_body_json(json!({ "status": -26 }))
          } else {
            ok(user_list(vec![user("1001", json!("ada@x.com"), json!({}))]))
          }
        }
        _ => ok(list(vec![account("1001", "Ada", json!({}))])),
      }
    })
    .mount(&server)
    .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  // The retry succeeded, so the sweep is whole and the address landed.
  assert!(snapshot.completeness.is_complete());
  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, json!({})), &c);
  assert_eq!(
    records[0].get("email"),
    CoreValue::String("ada@x.com".to_owned())
  );

  let calls = log.lock().unwrap();
  let user_calls: Vec<&Value> = calls
    .iter()
    .filter(|r| r["action"] == json!("listUser"))
    .collect();
  assert_eq!(user_calls.len(), 2, "one paged attempt, then one bare");
  assert!(user_calls[1].get("item_num").is_none());
}

#[tokio::test]
async fn both_list_user_attempts_failing_reports_both() {
  // The pair is the diagnosis: "it refused paging and it refused
  // nothing at all" says something different from either alone.
  let (server, _) = ucm(vec![(
    "listAccount",
    list(vec![account("1001", "Ada", json!({}))]),
  )])
  .await;

  let snapshot = observe(&server, json!({})).await.unwrap();
  assert_eq!(snapshot.observations.len(), 1, "extensions still collect");
  assert!(matches!(
    snapshot.completeness,
    Completeness::Partial { .. }
  ));
  let warning = &snapshot.warnings[0];
  assert!(warning.contains("item_num and page"), "{warning}");
  assert!(warning.contains("no parameters at all"), "{warning}");
}

// --- the appliance's dialect ------------------------------------------

#[tokio::test]
async fn the_appliances_yes_and_no_become_real_booleans() {
  // The shared normalizer accepts only "true"/"false", deliberately. The
  // UCM says "yes"/"no" for every boolean it has, so without the
  // rewrite a ruleset asking for a boolean gets null and a warning on
  // every sweep — `has_voicemail` was three-valued forever and no check
  // reading it could ever fire.
  let (server, _) = ucm(vec![
    ("listAccount", list(vec![account("1001", "Ada", json!({}))])),
    (
      "getSIPAccount",
      json!({ "extension": {
        "extension": "1001",
        "hasvoicemail": "yes",
        "dnd": "no",
        "nat": "yes",
        "permission": "internal",
        "auto_record": "off",
      }}),
    ),
  ])
  .await;

  let cfg = json!({ "detail": true, "users": false });
  let snapshot = observe(&server, cfg.clone()).await.unwrap();
  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, cfg), &c);
  assert_eq!(records[0].get("has_voicemail"), CoreValue::Bool(true));
  assert_eq!(records[0].get("dnd"), CoreValue::Bool(false));
  assert_eq!(records[0].get("nat"), CoreValue::Bool(true));
  // Values that merely look enum-ish are left alone.
  assert_eq!(
    records[0].get("permission"),
    CoreValue::String("internal".to_owned())
  );
}

#[tokio::test]
async fn an_out_of_service_extension_is_suspended_when_the_ucm_says_no() {
  // The appliance spells this `"no"`/`"yes"`, not `"1"`. The status rule
  // keyed only on "1"/"true", so a disabled extension read as active.
  let (server, _) = ucm(vec![(
    "listAccount",
    list(vec![account(
      "1003",
      "Reception",
      json!({ "out_of_service": "yes", "status": "Idle" }),
    )]),
  )])
  .await;

  let cfg = json!({ "users": false });
  let snapshot = observe(&server, cfg.clone()).await.unwrap();
  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, cfg), &c);
  assert_eq!(records[0].status, overlord_core::EntityStatus::Suspended);
}

#[test]
fn yes_and_no_convert_but_nothing_else_does() {
  let v = yes_no_to_bool(&json!({
    "a": "yes",
    "b": "no",
    "c": "Yes",
    "d": "off",
    "e": "internal",
    "nested": { "f": "no" },
    "list": [{ "g": "yes" }],
    "n": 1,
  }));
  assert_eq!(v["a"], json!(true));
  assert_eq!(v["b"], json!(false));
  // Only the exact lowercase spellings the appliance uses.
  assert_eq!(v["c"], json!("Yes"));
  assert_eq!(v["d"], json!("off"));
  assert_eq!(v["e"], json!("internal"));
  assert_eq!(v["nested"]["f"], json!(false));
  assert_eq!(v["list"][0]["g"], json!(true));
  assert_eq!(v["n"], json!(1));
}

// --- zero config devices ----------------------------------------------

#[tokio::test]
async fn devices_are_read_through_the_configured_action() {
  let (server, seen) = ucm(vec![(
    "listZeroConfig",
    json!({ "zero_config": [
      {
        "mac": "00:0B:82:AA:BB:CC",
        "model": "GRP2615",
        "vendor": "Grandstream",
        "version": "1.0.11.76",
        "ip": "10.0.0.31",
        "extension": "1001",
      }
    ]}),
  )])
  .await;

  let cfg = json!({ "mode": "devices" });
  let snapshot = observe(&server, cfg.clone()).await.unwrap();
  assert!(seen.lock().unwrap().iter().any(|a| a == "listZeroConfig"));

  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, cfg), &c);
  assert_eq!(records[0].entity_type.as_str(), "phone-device");
  // Punctuation and case are stripped: the key has to be identical
  // every sweep or a firmware upgrade re-provisions the whole fleet.
  assert_eq!(records[0].entity_key.as_str(), "000b82aabbcc");
  assert_eq!(
    records[0].get("firmware_version"),
    CoreValue::String("1.0.11.76".to_owned())
  );
  assert_eq!(
    records[0].get("extension"),
    CoreValue::String("1001".to_owned())
  );
}

#[tokio::test]
async fn the_action_name_and_list_key_are_both_configurable() {
  let (server, seen) = ucm(vec![(
    "listZCDevice",
    json!({
      "devices": [{ "macaddr": "000b82aabbcd", "device_model": "GRP2612" }],
      "notes": [{ "unrelated": true }],
    }),
  )])
  .await;

  let cfg = json!({
    "mode": "devices",
    "zero_config_action": "listZCDevice",
    "zero_config_list_key": "devices",
  });
  let snapshot = observe(&server, cfg.clone()).await.unwrap();
  assert!(seen.lock().unwrap().iter().any(|a| a == "listZCDevice"));
  assert_eq!(snapshot.observations.len(), 1);

  let c = connector();
  let records = normalized(&snapshot, &ctx(&server, cfg), &c);
  // Field names this connector has never seen still land, because the
  // envelope tries the spellings rather than trusting one.
  assert_eq!(records[0].entity_key.as_str(), "000b82aabbcd");
  assert_eq!(
    records[0].get("model"),
    CoreValue::String("GRP2612".to_owned())
  );
}

#[tokio::test]
async fn a_device_row_with_no_mac_is_skipped_rather_than_keyed_on_a_guess() {
  let (server, _) = ucm(vec![(
    "listZeroConfig",
    json!({ "zero_config": [
      { "model": "GRP2615", "ip": "10.0.0.32" },
      { "mac": "000b82aabbce", "model": "GRP2615" },
    ]}),
  )])
  .await;

  let snapshot = observe(&server, json!({ "mode": "devices" }))
    .await
    .unwrap();
  assert_eq!(snapshot.observations.len(), 1, "only the keyed row");
  assert!(matches!(
    snapshot.completeness,
    Completeness::Partial { .. }
  ));
  // The warning names the fields that were there, which is what makes
  // the next configuration change an informed one.
  assert!(
    snapshot.warnings[0].contains("model"),
    "{:?}",
    snapshot.warnings
  );
}

// --- read-only by construction ----------------------------------------

#[tokio::test]
async fn the_connector_cannot_reach_anything_but_the_api_path() {
  let server = MockServer::start().await;
  let c = connector();
  let http = c.http(&ctx(&server, json!({}))).unwrap();

  // The UCM's web UI and its mutating routes live off /api; none of
  // them is nameable.
  for p in ["/cgi", "/api/v1/admin", "/", "/maintenance"] {
    let e = http.post_json(p, &[], &json!({})).await.unwrap_err();
    assert!(
      matches!(e, ConnectorError::NotAllowed { .. }),
      "{p} should be refused before any call, got {e}"
    );
  }
}

#[tokio::test]
async fn base_url_is_required_rather_than_defaulted_to_somebodys_pbx() {
  let c = connector();
  let ctx = ObserveCtx {
    system:     SystemId::new("ucm"),
    started_at: "2026-01-15T00:00:00Z".parse::<Timestamp>().unwrap(),
    config:     json!({ "username": "cdrapi" }),
    progress:   Progress::default(),
  };
  // No base_url: the failure names the field and the value it wants,
  // rather than surfacing the URL parser's "relative URL without a
  // base" — and it happens before any socket is opened.
  let e = c.http(&ctx).unwrap_err();
  assert!(e.to_string().contains("base_url"), "{e}");
  assert!(e.to_string().contains("8089"), "{e}");
}

// --- units ------------------------------------------------------------

#[test]
fn macs_normalize_to_one_spelling_and_nonsense_is_rejected() {
  for spelling in [
    "00:0b:82:aa:bb:cc",
    "00-0B-82-AA-BB-CC",
    "000b82aabbcc",
    "000B82AABBCC",
    " 00 0b 82 aa bb cc ",
  ] {
    assert_eq!(
      normalize_mac(spelling).as_deref(),
      Some("000b82aabbcc"),
      "{spelling}"
    );
  }
  for bad in ["", "000b82aabbc", "000b82aabbccdd", "zz0b82aabbcc", "n/a"] {
    assert!(normalize_mac(bad).is_none(), "{bad} should not be a MAC");
  }
}

#[test]
fn redaction_follows_the_field_name_not_a_fixed_list() {
  // A firmware that adds a new secret must not put it in the stream
  // before anybody notices it exists.
  let v = redact(&json!({
    "secret": "abc",
    "vmsecret": "12",
    "admin_password": "xyz",
    "SIP_Secret": "QQ",
    "nested": { "vmsecret": "9999", "keep": 1 },
    "list": [{ "secret": "zzz" }],
    "fullname": "Ada",
  }));
  let text = serde_json::to_string(&v).unwrap();
  for secret in ["abc", "\"12\"", "xyz", "QQ", "9999", "zzz"] {
    assert!(!text.contains(secret), "{secret} survived: {text}");
  }
  assert_eq!(v["secret_len"], json!(3));
  assert_eq!(v["nested"]["vmsecret_len"], json!(4));
  assert_eq!(v["list"][0]["secret_len"], json!(3));
  assert_eq!(v["fullname"], json!("Ada"));
  assert_eq!(v["nested"]["keep"], json!(1));
}

#[test]
fn an_absent_device_field_stays_absent_rather_than_becoming_empty() {
  // Null is not "this handset has no firmware version".
  let e = device_envelope(&json!({ "mac": "000b82aabbcc" })).unwrap();
  assert!(e.get("firmware").is_none());
  assert!(e.get("model").is_none());
  assert_eq!(e["mac"], json!("000b82aabbcc"));
  // The row survives whole, so raw.device.<path> still reaches it.
  assert_eq!(e["device"]["mac"], json!("000b82aabbcc"));
}
