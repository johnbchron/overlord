//! The connector against recorded UniFi Access response shapes.
//!
//! PLAN.md section 7 asks for `wiremock` with recorded response shapes
//! and an allowlist-violation test per connector. The payloads below
//! use the developer API's field names and envelope (`code`, `msg`,
//! `data`, `pagination`); the values are invented.
//!
//! The token is handed to the connector directly rather than through
//! the environment: `std::env::set_var` is unsafe in this edition and
//! `unsafe_code` is forbidden across the workspace, so a test has no
//! way to set one. `UnifiAccessConnector::with_token` is the seam, and
//! no configuration path reaches it.

use std::sync::{Arc, Mutex};

use overlord_connect::{Connector, ConnectorError, ObserveCtx, Progress};
use overlord_connector_unifi_access::UnifiAccessConnector;
use overlord_core::{Completeness, SystemId, Timestamp, Value as CoreValue};
use serde_json::{Value, json};
use wiremock::{
  Mock, MockServer, ResponseTemplate,
  matchers::{method, path, query_param},
};

fn connector() -> UnifiAccessConnector {
  UnifiAccessConnector::with_token("test-token")
}

fn ctx(server: &MockServer) -> ObserveCtx {
  ObserveCtx {
    system:     SystemId::new("access-hq"),
    started_at: "2026-01-15T00:00:00Z".parse::<Timestamp>().unwrap(),
    config:     json!({ "base_url": server.uri(), "groups": true }),
    progress:   overlord_connect::Progress::default(),
  }
}

/// One developer-API envelope around a list and its pagination.
fn list_body(data: Value, page: u64, total: u64) -> Value {
  json!({
    "code": "SUCCESS",
    "msg": "ok",
    "data": data,
    "pagination": { "page_num": page, "page_size": 100, "total": total }
  })
}

fn user(id: &str, email: &str, extra: Value) -> Value {
  let mut u = json!({
    "id": id,
    "first_name": "A",
    "last_name": "B",
    "full_name": "A B",
    "user_email": email,
    "employee_number": "E-1",
    "status": "ACTIVE",
    "onboard_time": 1689304925,
    "access_policy_ids": ["p-1"],
    "access_policies": [
      { "id": "p-1", "name": "Lab Access",
        "resources": [{ "id": "door-1", "type": "door" }] }
    ]
  });
  if let (Value::Object(u), Value::Object(extra)) = (&mut u, extra) {
    u.extend(extra);
  }
  u
}

async fn two_pages_of_users(server: &MockServer) {
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/users"))
    .and(query_param("page_num", "2"))
    .respond_with(ResponseTemplate::new(200).set_body_json(list_body(
      json!([user(
        "u-1002",
        "grace@example.com",
        json!({
          "status": "DEACTIVATED"
        })
      )]),
      2,
      2,
    )))
    .mount(server)
    .await;
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/users"))
    .respond_with(ResponseTemplate::new(200).set_body_json(list_body(
      json!([user("u-1001", "ada@example.com", json!({
        "pin_code": { "token": "pin-secret" },
        "nfc_cards": [{ "id": "card-1", "token": "nfc-secret", "type": "ua_card" }]
      }))]),
      1,
      2,
    )))
    .mount(server)
    .await;
}

async fn doors(server: &MockServer) {
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/doors"))
    .respond_with(ResponseTemplate::new(200).set_body_json(list_body(
      json!([{ "id": "door-1", "name": "Front Door", "full_name": "HQ / Front Door" }]),
      1,
      1,
    )))
    .mount(server)
    .await;
}

async fn groups(server: &MockServer) {
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/user_groups"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "code": "SUCCESS",
      "msg": "ok",
      "data": [{ "id": "g-1", "name": "Engineering" }]
    })))
    .mount(server)
    .await;
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/user_groups/g-1/users/all"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "code": "SUCCESS",
      "msg": "ok",
      "data": [{ "id": "u-1001", "full_name": "A B" }]
    })))
    .mount(server)
    .await;
}

/// Run `observe` and normalize, as a sweep does.
async fn observe(
  server: &MockServer,
) -> Result<
  (
    overlord_connect::Snapshot,
    Vec<overlord_core::NormalizedRecord>,
  ),
  ConnectorError,
> {
  observe_as(ctx(server)).await
}

/// The same, for a console read without its groups — the reads that
/// have nothing to say about group membership.
async fn observe_without_groups(
  server: &MockServer,
) -> Result<
  (
    overlord_connect::Snapshot,
    Vec<overlord_core::NormalizedRecord>,
  ),
  ConnectorError,
> {
  observe_as(ctx_without_groups(server)).await
}

async fn observe_as(
  ctx: ObserveCtx,
) -> Result<
  (
    overlord_connect::Snapshot,
    Vec<overlord_core::NormalizedRecord>,
  ),
  ConnectorError,
> {
  let c = connector();
  let http = c.http(&ctx)?;
  let snapshot = c.observe(&http, &ctx).await?;
  let ruleset = c.default_ruleset(&ctx);
  let records = snapshot
    .observations
    .iter()
    .map(|o| ruleset.apply(&ctx.system, &o.raw).unwrap().record)
    .collect();
  Ok((snapshot, records))
}

#[tokio::test]
async fn a_console_is_read_across_pages_and_lands_on_the_guaranteed_overlay() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  groups(&server).await;

  let (snapshot, records) = observe(&server).await.unwrap();
  assert!(matches!(snapshot.completeness, Completeness::Complete));
  assert_eq!(records.len(), 2);

  let ada = records
    .iter()
    .find(|r| r.entity_key.as_str() == "u-1001")
    .unwrap();
  assert_eq!(ada.display_name.as_deref(), Some("A B"));
  assert_eq!(ada.status, overlord_core::EntityStatus::Active);
  assert_eq!(
    ada.get("email"),
    CoreValue::String("ada@example.com".to_owned())
  );
  // The policy's door resource is resolved against the door list.
  let doors = ada.get("doors");
  assert_eq!(doors.type_name(), "list");
  // Group membership is inverted onto the account.
  let groups = ada.get("groups");
  assert_eq!(groups.type_name(), "list");

  let grace = records
    .iter()
    .find(|r| r.entity_key.as_str() == "u-1002")
    .unwrap();
  assert_eq!(grace.status, overlord_core::EntityStatus::Deprovisioned);
}

#[tokio::test]
async fn credential_tokens_are_stripped_before_the_fact_stream() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  groups(&server).await;

  let (snapshot, _) = observe(&server).await.unwrap();
  let raw = snapshot
    .observations
    .iter()
    .map(|o| o.raw.to_string())
    .collect::<Vec<_>>()
    .join("\n");
  assert!(!raw.contains("pin-secret"), "{raw}");
  assert!(!raw.contains("nfc-secret"), "{raw}");
}

#[tokio::test]
async fn a_failed_group_read_degrades_rather_than_tombstoning() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  // Groups are refused outright.
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/user_groups"))
    .respond_with(ResponseTemplate::new(500))
    .mount(&server)
    .await;

  let (snapshot, records) = observe(&server).await.unwrap();
  assert!(matches!(
    snapshot.completeness,
    Completeness::Partial { .. }
  ));
  assert!(!snapshot.warnings.is_empty());
  // Absent, not empty: null is not "no groups".
  assert_eq!(records[0].get("groups"), CoreValue::Null);
}

/// A console with many accounts and many groups is the slow sweep this
/// feedback exists for, so the connector and its HTTP client must both
/// narrate what they are doing while the read is still running.
#[tokio::test]
async fn a_long_read_narrates_pages_groups_and_requests() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  groups(&server).await;

  let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
  let sink = Arc::clone(&seen);
  let mut ctx = ctx(&server);
  ctx.progress = Progress::new(Arc::new(move |e| {
    sink.lock().unwrap().push(e.note);
  }));

  let c = connector();
  let http = c.http(&ctx).unwrap();
  c.observe(&http, &ctx).await.unwrap();

  let seen = seen.lock().unwrap().clone();
  assert!(seen.iter().any(|n| n == "reading accounts"), "{seen:?}");
  assert!(seen.iter().any(|n| n == "accounts: page 2"), "{seen:?}");
  assert!(seen.iter().any(|n| n == "group 1 of 1"), "{seen:?}");
  // The HTTP client reports each round trip too, which is the only
  // signal available from a connector that does not report its own.
  assert!(
    seen
      .iter()
      .any(|n| n == "GET /api/v1/developer/users (page 2)"),
    "{seen:?}"
  );
}

#[tokio::test]
async fn a_first_page_that_fails_is_a_failed_system_not_a_partial_one() {
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/users"))
    .respond_with(ResponseTemplate::new(500))
    .mount(&server)
    .await;

  let err = observe(&server).await.unwrap_err();
  assert!(matches!(err, ConnectorError::Incomplete(_)), "{err:?}");
}

/// A body with no `pagination` sibling: the shape of an endpoint that
/// answers with its whole collection. Every fixture above invents a
/// `pagination` object, which is exactly what hid the paging bug — the
/// loop's only exit was always handed to it.
fn whole_body(data: Value) -> Value {
  json!({ "code": "SUCCESS", "msg": "ok", "data": data })
}

fn ctx_without_groups(server: &MockServer) -> ObserveCtx {
  ObserveCtx {
    config: json!({ "base_url": server.uri(), "groups": false }),
    ..ctx(server)
  }
}

async fn mount(server: &MockServer, at: &str, body: Value) {
  Mock::given(method("GET"))
    .and(path(at.to_owned()))
    .respond_with(ResponseTemplate::new(200).set_body_json(body))
    .mount(server)
    .await;
}

async fn hits(server: &MockServer, at: &str) -> usize {
  server
    .received_requests()
    .await
    .unwrap()
    .iter()
    .filter(|r| r.url.path() == at)
    .count()
}

/// An endpoint that answers with its whole collection sends no
/// `pagination` and repeats itself for every `page_num`, so a reader
/// counting its own pages never terminates: it collected the same rows
/// 10,000 times, made 10,000 requests at the console, and reported the
/// snapshot partial every sweep — which meant this system could never
/// tombstone.
#[tokio::test]
async fn an_endpoint_that_answers_whole_is_read_once_not_ten_thousand_times() {
  let server = MockServer::start().await;
  mount(
    &server,
    "/api/v1/developer/users",
    whole_body(json!([user("u-1001", "ada@example.com", json!({}))])),
  )
  .await;
  mount(
    &server,
    "/api/v1/developer/doors",
    whole_body(json!([{ "id": "door-1", "name": "Front Door" }])),
  )
  .await;

  let (snapshot, records) = observe_without_groups(&server).await.unwrap();
  assert_eq!(hits(&server, "/api/v1/developer/users").await, 1);
  assert_eq!(hits(&server, "/api/v1/developer/doors").await, 1);
  assert_eq!(snapshot.observations.len(), 1);
  assert_eq!(records.len(), 1);
  assert!(
    matches!(snapshot.completeness, Completeness::Complete),
    "{:?}",
    snapshot.completeness
  );
  assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
  // The whole body was still read, names and all.
  assert_eq!(records[0].get("doors").type_name(), "list");
}

/// A console that sends `pagination` but ignores `page_num` answers
/// every page with the same rows. Each one is a row already collected,
/// so the read stops and says so rather than multiplying one account
/// into a fact per page.
#[tokio::test]
async fn a_console_that_ignores_page_num_is_stopped_not_multiplied() {
  let server = MockServer::start().await;
  mount(
    &server,
    "/api/v1/developer/users",
    // "three accounts", and the same one every time.
    list_body(json!([user("u-1001", "ada@example.com", json!({}))]), 1, 3),
  )
  .await;
  mount(&server, "/api/v1/developer/doors", whole_body(json!([]))).await;

  let (snapshot, records) = observe_without_groups(&server).await.unwrap();
  // Page one, then one more that proved the console was not advancing.
  assert_eq!(hits(&server, "/api/v1/developer/users").await, 2);
  assert_eq!(records.len(), 1);
  let Completeness::Partial { reason } = &snapshot.completeness else {
    panic!("expected a partial snapshot: {:?}", snapshot.completeness);
  };
  assert!(reason.contains("repeated rows already read"), "{reason}");
}

/// `pagination` on a collection read in one body is the console saying
/// the collection is larger than what it sent. Reporting the first page
/// as all of it lost the rest of the groups *and* called the snapshot
/// complete, so every group-based violation resolved.
#[tokio::test]
async fn a_collection_that_pages_is_not_reported_as_all_of_it() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  mount(
    &server,
    "/api/v1/developer/user_groups",
    list_body(json!([{ "id": "g-1", "name": "Engineering" }]), 1, 5),
  )
  .await;
  mount(
    &server,
    "/api/v1/developer/user_groups/g-1/users/all",
    whole_body(json!([{ "id": "u-1001", "full_name": "A B" }])),
  )
  .await;

  let (snapshot, _) = observe(&server).await.unwrap();
  let Completeness::Partial { reason } = &snapshot.completeness else {
    panic!("expected a partial snapshot: {:?}", snapshot.completeness);
  };
  assert!(reason.contains("user_groups"), "{reason}");
  assert!(reason.contains("reported 5 rows and sent 1"), "{reason}");
}

/// A read that stops short of the total the console reported is a
/// truncated read, not a finished one.
#[tokio::test]
async fn a_read_that_stops_short_of_the_reported_total_is_partial() {
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/api/v1/developer/users"))
    .and(query_param("page_num", "2"))
    .respond_with(ResponseTemplate::new(200).set_body_json(list_body(
      json!([]),
      2,
      9,
    )))
    .mount(&server)
    .await;
  mount(
    &server,
    "/api/v1/developer/users",
    list_body(json!([user("u-1001", "ada@example.com", json!({}))]), 1, 9),
  )
  .await;
  mount(&server, "/api/v1/developer/doors", whole_body(json!([]))).await;

  let (snapshot, records) = observe_without_groups(&server).await.unwrap();
  assert_eq!(records.len(), 1);
  let Completeness::Partial { reason } = &snapshot.completeness else {
    panic!("expected a partial snapshot: {:?}", snapshot.completeness);
  };
  assert!(
    reason.contains("reported 9 rows and stopped after 1"),
    "{reason}"
  );
}

/// `expand[]=access_policy` is the whole of how entitlement is read. A
/// console that answers with policy ids and no expansion leaves it
/// unknown — which must not present as an account that can open no
/// door, because that would resolve every door violation at once.
#[tokio::test]
async fn an_unexpanded_policy_is_partial_rather_than_an_empty_grant() {
  let server = MockServer::start().await;
  mount(
    &server,
    "/api/v1/developer/users",
    whole_body(json!([{
      "id": "u-1001",
      "full_name": "A B",
      "user_email": "ada@example.com",
      "status": "ACTIVE",
      "access_policy_ids": ["p-1"]
    }])),
  )
  .await;
  mount(
    &server,
    "/api/v1/developer/doors",
    whole_body(json!([{ "id": "door-1", "name": "Front Door" }])),
  )
  .await;

  let (snapshot, records) = observe_without_groups(&server).await.unwrap();
  let Completeness::Partial { reason } = &snapshot.completeness else {
    panic!("expected a partial snapshot: {:?}", snapshot.completeness);
  };
  assert!(reason.contains("did not expand"), "{reason}");
  // Absent, not empty: null is not "no doors".
  assert_eq!(records[0].get("doors"), CoreValue::Null);
}
