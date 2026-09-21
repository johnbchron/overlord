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

/// One door-opening log entry, in the shape the system log sends: the
/// event wrapped in the search index's `_source`.
fn opening(id: &str, actor: &str, at: &str, result: &str) -> Value {
  json!({
    "_id": id,
    "_source": {
      "@timestamp": at,
      "actor": { "id": actor, "display_name": "A B", "type": "user" },
      "authentication": { "credential_provider": "NFC" },
      "event": { "type": "access.door.unlock", "result": result },
      "target": [
        { "id": "reader-1", "type": "device", "display_name": "Reader 1" },
        { "id": "door-1", "type": "door", "display_name": "Front Door" }
      ]
    }
  })
}

/// The system log's own envelope: the rows live under `data.hits`, and
/// the `pagination` — when there is one — sits inside `data` rather
/// than beside it.
fn log_body(hits: Value, pagination: Option<Value>) -> Value {
  let mut data = json!({ "hits": hits });
  if let (Value::Object(data), Some(p)) = (&mut data, pagination) {
    data.insert("pagination".to_owned(), p);
  }
  json!({ "code": "SUCCESS", "msg": "ok", "data": data })
}

/// The console's door-opening log, answered whole.
async fn openings(server: &MockServer, hits: Value) {
  Mock::given(method("POST"))
    .and(path("/api/v1/developer/system/logs"))
    .respond_with(
      ResponseTemplate::new(200).set_body_json(log_body(hits, None)),
    )
    .mount(server)
    .await;
}

/// A log with one opening for Ada and nothing for anyone else.
async fn one_opening(server: &MockServer) {
  openings(
    server,
    json!([opening(
      "log-1",
      "u-1001",
      "2026-01-09T08:14:02Z",
      "ACCESS_GRANTED"
    )]),
  )
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
  one_opening(&server).await;

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
  one_opening(&server).await;

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
  one_opening(&server).await;
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
  one_opening(&server).await;

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
  assert!(
    seen.iter().any(|n| n == "reading door openings"),
    "{seen:?}"
  );
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

/// A console read for its directory alone — neither groups nor the
/// door-opening log.
fn ctx_directory_only(server: &MockServer) -> ObserveCtx {
  ObserveCtx {
    config: json!({
      "base_url": server.uri(),
      "groups": false,
      "unlock_activity": false
    }),
    ..ctx(server)
  }
}

async fn observe_directory_only(
  server: &MockServer,
) -> Result<
  (
    overlord_connect::Snapshot,
    Vec<overlord_core::NormalizedRecord>,
  ),
  ConnectorError,
> {
  observe_as(ctx_directory_only(server)).await
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

  let (snapshot, records) = observe_directory_only(&server).await.unwrap();
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

  let (snapshot, records) = observe_directory_only(&server).await.unwrap();
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
  one_opening(&server).await;
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

  let (snapshot, records) = observe_directory_only(&server).await.unwrap();
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

  let (snapshot, records) = observe_directory_only(&server).await.unwrap();
  let Completeness::Partial { reason } = &snapshot.completeness else {
    panic!("expected a partial snapshot: {:?}", snapshot.completeness);
  };
  assert!(reason.contains("did not expand"), "{reason}");
  // Absent, not empty: null is not "no doors".
  assert_eq!(records[0].get("doors"), CoreValue::Null);
}

/// The last unlock, end to end: a console's log becomes one date per
/// account on the guaranteed overlay, which is what makes a dormancy
/// question answerable about a building.
#[tokio::test]
async fn a_last_unlock_lands_on_the_overlay_dated_and_named() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  groups(&server).await;
  openings(
    &server,
    json!([
      opening("log-1", "u-1001", "2026-01-09T08:14:02Z", "ACCESS_GRANTED"),
      // Somebody else's opening, so the reduction has to key on the
      // actor rather than take the newest entry in the log.
      opening("log-2", "u-9999", "2026-01-14T18:02:11Z", "ACCESS_GRANTED")
    ]),
  )
  .await;

  let (snapshot, records) = observe(&server).await.unwrap();
  assert!(
    matches!(snapshot.completeness, Completeness::Complete),
    "{:?}",
    snapshot.completeness
  );

  let ada = records
    .iter()
    .find(|r| r.entity_key.as_str() == "u-1001")
    .unwrap();
  assert_eq!(ada.get("unlock_activity_known"), CoreValue::Bool(true));
  assert_eq!(ada.get("last_unlock_at").type_name(), "timestamp");
  assert_eq!(
    ada.get("last_unlock_at"),
    CoreValue::Timestamp("2026-01-09T08:14:02Z".parse::<Timestamp>().unwrap())
  );
  // The door list names the door; the reader beside it in the log is
  // not an entitlement and is not kept.
  assert_eq!(
    ada.get("last_unlock_door"),
    CoreValue::String("HQ / Front Door".to_owned())
  );
  assert_eq!(
    ada.get("last_unlock_via"),
    CoreValue::String("NFC".to_owned())
  );

  // Grace opened nothing. The log was read, so that is a fact about
  // Grace rather than a gap in the sweep — and the boolean is what
  // says so.
  let grace = records
    .iter()
    .find(|r| r.entity_key.as_str() == "u-1002")
    .unwrap();
  assert_eq!(grace.get("unlock_activity_known"), CoreValue::Bool(true));
  assert_eq!(grace.get("last_unlock_at"), CoreValue::Null);
  assert_eq!(grace.get("unlock_window_days").type_name(), "number");
}

/// The body the connector sends, and the clock it dates it with. A
/// connector that read a wall clock would give two sweeps of the same
/// console different windows; SPEC.md section 13 gives a sweep one
/// clock, and this is it.
#[tokio::test]
async fn the_window_is_the_topic_and_the_sweeps_own_clock() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  one_opening(&server).await;

  observe_as(ctx_without_groups(&server)).await.unwrap();

  let sent = server.received_requests().await.unwrap();
  let log = sent
    .iter()
    .find(|r| r.url.path() == "/api/v1/developer/system/logs")
    .expect("the log is read");
  assert_eq!(log.method, wiremock::http::Method::POST);
  let filter: Value = serde_json::from_slice(&log.body).unwrap();
  assert_eq!(filter["topic"], json!("door_openings"));
  // `started_at` is 2026-01-15T00:00:00Z, and the default window is 90
  // days, so the read asks for 2025-10-17T00:00:00Z onwards.
  assert_eq!(filter["until"], json!(1_768_435_200_i64));
  assert_eq!(filter["since"], json!(1_760_659_200_i64));
  // Paging stays in the query string.
  assert!(
    log
      .url
      .query_pairs()
      .any(|(k, v)| k == "page_num" && v == "1"),
    "{}",
    log.url
  );
}

/// A refused badge is recorded in the same topic as an opening. Reading
/// it as activity would make a revoked card look like a person still
/// coming to work.
#[tokio::test]
async fn a_refused_badge_is_not_a_last_unlock() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  openings(
    &server,
    json!([opening(
      "log-1",
      "u-1001",
      "2026-01-09T08:14:02Z",
      "ACCESS_DENIED"
    )]),
  )
  .await;

  let (_, records) = observe_as(ctx_without_groups(&server)).await.unwrap();
  let ada = records
    .iter()
    .find(|r| r.entity_key.as_str() == "u-1001")
    .unwrap();
  assert_eq!(ada.get("unlock_activity_known"), CoreValue::Bool(true));
  assert_eq!(ada.get("last_unlock_at"), CoreValue::Null);
}

/// The log pages, and the newest opening wins whichever page it lands
/// on. Taking the first one seen would date an account from whatever
/// order the console happened to answer in.
#[tokio::test]
async fn the_newest_opening_wins_across_pages() {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  let page = |hits: Value, total: u64| {
    ResponseTemplate::new(200).set_body_json(log_body(
      hits,
      Some(json!({ "page_num": 1, "page_size": 100, "total": total })),
    ))
  };
  Mock::given(method("POST"))
    .and(path("/api/v1/developer/system/logs"))
    .and(query_param("page_num", "2"))
    .respond_with(page(
      json!([opening(
        "log-2",
        "u-1001",
        "2026-01-09T08:14:02Z",
        "ACCESS_GRANTED"
      )]),
      2,
    ))
    .mount(&server)
    .await;
  Mock::given(method("POST"))
    .and(path("/api/v1/developer/system/logs"))
    .respond_with(page(
      json!([opening(
        "log-1",
        "u-1001",
        "2025-11-02T07:00:00Z",
        "ACCESS_GRANTED"
      )]),
      2,
    ))
    .mount(&server)
    .await;

  let (snapshot, records) =
    observe_as(ctx_without_groups(&server)).await.unwrap();
  assert!(
    matches!(snapshot.completeness, Completeness::Complete),
    "{:?}",
    snapshot.completeness
  );
  assert_eq!(hits(&server, "/api/v1/developer/system/logs").await, 2);
  let ada = records
    .iter()
    .find(|r| r.entity_key.as_str() == "u-1001")
    .unwrap();
  assert_eq!(
    ada.get("last_unlock_at"),
    CoreValue::Timestamp("2026-01-09T08:14:02Z".parse::<Timestamp>().unwrap())
  );
}

/// A log read that stopped short withholds every date rather than
/// reporting the accounts it happened to reach. The pages that did not
/// arrive are indistinguishable from accounts that opened no door, and
/// the check this field exists for fires on the null — so a truncated
/// read would open a dormancy violation against most of the building.
#[tokio::test]
async fn a_partial_log_withholds_every_last_unlock_rather_than_inventing_dormancy()
 {
  let server = MockServer::start().await;
  two_pages_of_users(&server).await;
  doors(&server).await;
  Mock::given(method("POST"))
    .and(path("/api/v1/developer/system/logs"))
    .and(query_param("page_num", "2"))
    .respond_with(ResponseTemplate::new(500))
    .mount(&server)
    .await;
  Mock::given(method("POST"))
    .and(path("/api/v1/developer/system/logs"))
    .respond_with(ResponseTemplate::new(200).set_body_json(log_body(
      json!([opening(
        "log-1",
        "u-1001",
        "2026-01-09T08:14:02Z",
        "ACCESS_GRANTED"
      )]),
      Some(json!({ "page_num": 1, "page_size": 100, "total": 9 })),
    )))
    .mount(&server)
    .await;

  let (snapshot, records) =
    observe_as(ctx_without_groups(&server)).await.unwrap();
  let Completeness::Partial { reason } = &snapshot.completeness else {
    panic!("expected a partial snapshot: {:?}", snapshot.completeness);
  };
  assert!(reason.contains("system/logs"), "{reason}");
  assert!(reason.contains("withheld"), "{reason}");

  // Ada's opening *was* read, and it is still withheld: a date for her
  // and a null for everyone else is the shape that reads as a building
  // gone quiet.
  let ada = records
    .iter()
    .find(|r| r.entity_key.as_str() == "u-1001")
    .unwrap();
  assert_eq!(ada.get("unlock_activity_known"), CoreValue::Bool(false));
  assert_eq!(ada.get("last_unlock_at"), CoreValue::Null);
  assert_eq!(ada.get("unlock_window_days"), CoreValue::Null);
  // The rest of the sweep is untouched: this degrades one field, not
  // the account list.
  assert_eq!(records.len(), 2);
  assert_eq!(ada.get("doors").type_name(), "list");
}

/// A console swept often enough that the log is the expensive part can
/// turn it off, and then the log is never asked for at all.
#[tokio::test]
async fn a_log_the_operator_turned_off_is_never_requested() {
  let server = MockServer::start().await;
  mount(
    &server,
    "/api/v1/developer/users",
    whole_body(json!([user("u-1001", "ada@example.com", json!({}))])),
  )
  .await;
  mount(&server, "/api/v1/developer/doors", whole_body(json!([]))).await;

  let (snapshot, records) = observe_directory_only(&server).await.unwrap();
  assert_eq!(hits(&server, "/api/v1/developer/system/logs").await, 0);
  // Off is not the same as empty: nothing is claimed about the door.
  assert!(
    matches!(snapshot.completeness, Completeness::Complete),
    "{:?}",
    snapshot.completeness
  );
  assert_eq!(
    records[0].get("unlock_activity_known"),
    CoreValue::Bool(false)
  );
  assert_eq!(records[0].get("last_unlock_at"), CoreValue::Null);
}
