//! The connector against recorded Google response shapes.
//!
//! PLAN.md section 7 asks for `wiremock` with recorded response shapes
//! and an allowlist-violation test per connector. The payloads below are
//! trimmed Admin SDK responses — the field names and nesting are
//! Google's, the values are invented.
//!
//! Credentials are handed to the connector directly rather than through
//! the environment: `std::env::set_var` is unsafe in this edition and
//! `unsafe_code` is forbidden across the workspace, so a test has no way
//! to set one. `GoogleWorkspaceConnector::with_credential` is the seam,
//! and it is reachable only from Rust — no configuration path leads to
//! it. The `authorized_user` shape keeps the exchange a plain
//! refresh-token grant, so these tests are about the read; the RS256
//! path has its own tests in `src/auth.rs`.

use overlord_connect::{Connector, ObserveCtx, ReadMethod};
use overlord_connector_gworkspace::{
  GoogleWorkspaceConnector, auth::Credential,
};
use overlord_core::{SystemId, Timestamp, Value as CoreValue};
use serde_json::{Value, json};
use wiremock::{
  Mock, MockServer, ResponseTemplate,
  matchers::{method, path, query_param},
};

fn refresh_token_credential() -> Credential {
  serde_json::from_str(
    r#"{"type":"authorized_user","client_id":"id",
        "client_secret":"s","refresh_token":"r"}"#,
  )
  .expect("the gcloud credential shape")
}

fn service_account_credential() -> Credential {
  serde_json::from_str(
    r#"{"type":"service_account",
        "client_email":"s@p.iam.gserviceaccount.com",
        "private_key":"-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----\n"}"#,
  )
  .expect("the service account shape")
}

fn connector() -> GoogleWorkspaceConnector {
  GoogleWorkspaceConnector::with_credential(refresh_token_credential())
}

fn user(id: &str, email: &str, extra: Value) -> Value {
  let mut u = json!({
    "kind": "admin#directory#user",
    "id": id,
    "primaryEmail": email,
    "name": { "givenName": "A", "familyName": "B", "fullName": "A B" },
    "isAdmin": false,
    "isDelegatedAdmin": false,
    "lastLoginTime": "2026-01-02T03:04:05.000Z",
    "creationTime": "2020-01-01T00:00:00.000Z",
    "agreedToTerms": true,
    "suspended": false,
    "archived": false,
    "changePasswordAtNextLogin": false,
    "isMailboxSetup": true,
    "isEnrolledIn2Sv": true,
    "isEnforcedIn2Sv": true,
    "customerId": "C01abc234",
    "orgUnitPath": "/"
  });
  if let (Value::Object(u), Value::Object(extra)) = (&mut u, extra) {
    u.extend(extra);
  }
  u
}

async fn token_endpoint(server: &MockServer) {
  Mock::given(method("POST"))
    .and(path("/token"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "access_token": "ya29.test",
      "expires_in": 3599,
      "token_type": "Bearer"
    })))
    .mount(server)
    .await;
}

fn ctx(server: &MockServer, extra: Value) -> ObserveCtx {
  let mut config = json!({
    "base_url": server.uri(),
    "token_url": server.uri(),
    "licensing_url": server.uri(),
  });
  if let (Value::Object(c), Value::Object(extra)) = (&mut config, extra) {
    c.extend(extra);
  }
  ObserveCtx {
    system: SystemId::new("gws-prod"),
    started_at: "2026-01-15T00:00:00Z".parse::<Timestamp>().unwrap(),
    config,
  }
}

/// Run `observe` and normalize, as a sweep does.
async fn observe(
  ctx: &ObserveCtx,
) -> Result<
  (
    overlord_connect::Snapshot,
    Vec<overlord_core::NormalizedRecord>,
  ),
  overlord_connect::ConnectorError,
> {
  observe_as(&connector(), ctx).await
}

async fn observe_as(
  c: &GoogleWorkspaceConnector,
  ctx: &ObserveCtx,
) -> Result<
  (
    overlord_connect::Snapshot,
    Vec<overlord_core::NormalizedRecord>,
  ),
  overlord_connect::ConnectorError,
> {
  let http = c.http(ctx)?;
  let snapshot = c.observe(&http, ctx).await?;
  let ruleset = c.default_ruleset(ctx);
  let records = snapshot
    .observations
    .iter()
    .map(|o| ruleset.apply(&ctx.system, &o.raw).unwrap().record)
    .collect();
  Ok((snapshot, records))
}

#[tokio::test]
async fn a_tenant_is_read_across_pages_and_lands_on_the_guaranteed_overlay() {
  let server = MockServer::start().await;
  token_endpoint(&server).await;

  // Two pages of accounts, which is the case a single-page mock would
  // never catch.
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/users"))
    .and(query_param("pageToken", "page-2"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "kind": "admin#directory#users",
      "users": [user("1002", "grace@example.com", json!({
        "isAdmin": true,
        "lastLoginTime": "1970-01-01T00:00:00.000Z"
      }))]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/users"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "kind": "admin#directory#users",
      "users": [user("1001", "ada@example.com", json!({
        "externalIds": [{ "value": "E-4471", "type": "organization" }],
        "organizations": [{ "department": "Platform", "primary": true }]
      }))],
      "nextPageToken": "page-2"
    })))
    .mount(&server)
    .await;

  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/customer/my_customer/domains"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "domains": [{ "domainName": "example.com", "isPrimary": true,
                    "verified": true }]
    })))
    .mount(&server)
    .await;

  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/groups"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "groups": [
        { "id": "g1", "email": "eng@example.com", "name": "Engineering" },
        { "id": "g2", "email": "board@example.com", "name": "Board" }
      ]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/groups/g1/members"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "members": [
        { "id": "1001", "email": "ada@example.com", "role": "MEMBER",
          "type": "USER" }
      ]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/groups/g2/members"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "members": [
        { "id": "1001", "email": "ada@example.com", "role": "OWNER",
          "type": "USER" },
        { "id": "9", "email": "auditor@partner.example", "role": "MEMBER",
          "type": "USER" }
      ]
    })))
    .mount(&server)
    .await;

  let ctx = ctx(&server, json!({}));
  let (snapshot, records) = observe(&ctx).await.unwrap();

  assert!(
    snapshot.completeness.is_complete(),
    "{:?}",
    snapshot.completeness
  );
  assert_eq!(records.len(), 2, "both pages");

  let ada = &records[0];
  assert_eq!(ada.entity_key.as_str(), "1001");
  assert_eq!(ada.status, overlord_core::EntityStatus::Active);
  assert_eq!(ada.get("mfa_enrolled"), CoreValue::Bool(true));
  assert_eq!(
    ada.get("email"),
    CoreValue::String("ada@example.com".to_owned())
  );
  assert_eq!(ada.get("username"), CoreValue::String("ada".to_owned()));
  assert_eq!(
    ada.get("employee_id"),
    CoreValue::String("E-4471".to_owned())
  );
  assert_eq!(
    ada.get("department"),
    CoreValue::String("Platform".to_owned())
  );

  // The group with an outside member is the external one, and the
  // membership is attributed to the account that holds it.
  let groups = ada.get("groups");
  let groups = groups.as_list().expect("groups is a list");
  assert_eq!(groups.len(), 2);
  let external: Vec<bool> = groups
    .iter()
    .map(|g| g.get_path("external") == CoreValue::Bool(true))
    .collect();
  assert_eq!(external, vec![false, true], "eng internal, board external");

  // The never-signed-in admin: Google's epoch sentinel is absence.
  let grace = &records[1];
  assert_eq!(grace.get("is_admin"), CoreValue::Bool(true));
  assert_eq!(grace.get("last_login_at"), CoreValue::Null);
  // Collected and empty is a list, not null.
  assert_eq!(
    grace.get("groups").as_list().map(<[CoreValue]>::len),
    Some(0)
  );
}

#[tokio::test]
async fn a_rate_limited_page_truncates_the_snapshot_rather_than_losing_people()
{
  let server = MockServer::start().await;
  token_endpoint(&server).await;

  // The first page answers and promises a second; the second is
  // refused. SPEC.md section 10: the absent accounts are not deletions.
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/users"))
    .and(query_param("pageToken", "page-2"))
    .respond_with(ResponseTemplate::new(429))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/users"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "users": [user("1001", "ada@example.com", json!({}))],
      "nextPageToken": "page-2"
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/customer/my_customer/domains"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "domains": [{ "domainName": "example.com" }]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/groups"))
    .respond_with(
      ResponseTemplate::new(200).set_body_json(json!({ "groups": [] })),
    )
    .mount(&server)
    .await;

  let ctx = ctx(&server, json!({}));
  let (snapshot, records) = observe(&ctx).await.unwrap();

  assert_eq!(records.len(), 1, "what was read is kept");
  assert!(!snapshot.completeness.is_complete());
  assert!(
    snapshot.completeness.reason().unwrap().contains("429"),
    "{:?}",
    snapshot.completeness
  );
}

#[tokio::test]
async fn a_tenant_that_answers_nothing_is_a_failed_system_not_an_empty_one() {
  let server = MockServer::start().await;
  token_endpoint(&server).await;

  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/users"))
    .respond_with(ResponseTemplate::new(403))
    .mount(&server)
    .await;

  let ctx = ctx(&server, json!({}));
  let err = observe(&ctx).await.unwrap_err();
  // `Incomplete` is what the sweep reads as "partial, never tombstone".
  assert!(err.is_partial(), "{err:?}");
  assert!(err.to_string().contains("accounts"), "{err}");
}

#[tokio::test]
async fn a_group_read_that_fails_degrades_the_snapshot_rather_than_the_answer()
{
  // Otherwise every sharing violation in the tenant resolves at once,
  // because an account with no groups looks like an account in no
  // external group.
  let server = MockServer::start().await;
  token_endpoint(&server).await;

  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/users"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "users": [user("1001", "ada@example.com", json!({}))]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/customer/my_customer/domains"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "domains": [{ "domainName": "example.com" }]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/groups"))
    .respond_with(ResponseTemplate::new(503))
    .mount(&server)
    .await;

  let ctx = ctx(&server, json!({}));
  let (snapshot, records) = observe(&ctx).await.unwrap();

  assert!(!snapshot.completeness.is_complete());
  // Not collected, so null — and null is not "no groups".
  assert_eq!(records[0].get("groups"), CoreValue::Null);
}

#[tokio::test]
async fn licences_are_read_only_for_the_skus_an_operator_named() {
  let server = MockServer::start().await;
  token_endpoint(&server).await;

  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/users"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "users": [
        user("1001", "ada@example.com", json!({})),
        user("1002", "grace@example.com", json!({}))
      ]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/admin/directory/v1/customer/my_customer/domains"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "domains": [{ "domainName": "example.com" }]
    })))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path(
      "/apps/licensing/v1/product/Google-Apps/sku/1010020020/users",
    ))
    .and(query_param("customerId", "C01abc234"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
      "items": [{
        "userId": "Ada@example.com",
        "productId": "Google-Apps",
        "skuId": "1010020020",
        "skuName": "Google Workspace Business Standard"
      }]
    })))
    .mount(&server)
    .await;

  let ctx = ctx(
    &server,
    json!({
      "groups": false,
      "licenses": [{ "product": "Google-Apps", "sku": "1010020020" }]
    }),
  );
  let (snapshot, records) = observe(&ctx).await.unwrap();
  assert!(
    snapshot.completeness.is_complete(),
    "{:?}",
    snapshot.completeness
  );

  // The Licensing API spells `userId` as an address, in whatever case
  // it feels like; matching it to an account has to fold.
  assert_eq!(
    records[0].get("licenses").as_list().map(<[CoreValue]>::len),
    Some(1)
  );
  assert_eq!(
    records[1].get("licenses").as_list().map(<[CoreValue]>::len),
    Some(0)
  );
  // The customer id came from the accounts, which is where Google puts
  // it — no extra configuration needed.
  assert_eq!(
    records[0].get("customer_id"),
    CoreValue::String("C01abc234".to_owned())
  );
}

#[tokio::test]
async fn a_service_account_without_a_subject_is_refused_before_any_request() {
  let server = MockServer::start().await;
  // Nothing is mounted: reaching the network at all would fail the test.
  let c =
    GoogleWorkspaceConnector::with_credential(service_account_credential());

  let ctx = ctx(&server, json!({}));
  let err = observe_as(&c, &ctx).await.unwrap_err().to_string();
  assert!(err.contains("impersonate"), "{err}");
}

#[tokio::test]
async fn an_unlisted_endpoint_fails_closed_without_a_request() {
  // The allowlist test PLAN.md section 7 asks for per connector. The
  // mock server is running and would answer anything; the refusal has
  // to come from overlord.
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    .mount(&server)
    .await;

  let c = connector();
  let http = c.http(&ctx(&server, json!({}))).unwrap();

  for unlisted in [
    // A single account, which would let the connector read one person
    // without enumerating — outside what the allowlist says it does.
    "/admin/directory/v1/users/1001",
    // Login activity, a different API and a much wider scope.
    "/admin/reports/v1/activity/users/all/applications/login",
    // Drive, which is the grant the crate docs decline to ask for.
    "/drive/v3/files",
  ] {
    let err = http
      .json(ReadMethod::Get, unlisted, &[])
      .await
      .expect_err("should be refused");
    assert!(
      matches!(err, overlord_connect::ConnectorError::NotAllowed { .. }),
      "{unlisted} reached the server: {err:?}"
    );
  }

  assert!(
    server
      .received_requests()
      .await
      .unwrap_or_default()
      .is_empty(),
    "the allowlist let a request through"
  );
}
