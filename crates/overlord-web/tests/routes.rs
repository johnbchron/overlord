//! Route-level tests: every screen of SPEC.md section 5, driven through
//! the real router against a real store seeded by a real sweep.
//!
//! PLAN.md section 7 asks for route tests via `tower::ServiceExt::oneshot`,
//! which is what these are. They deliberately assert on rendered markup
//! rather than on handler return values: the contract this milestone
//! adds is what an operator sees, and a handler that returns the right
//! data into the wrong template is exactly the failure worth catching.

use std::sync::Arc;

use axum::{
  body::Body,
  http::{Request, StatusCode, header},
  response::Response,
};
use overlord_connect::Registry;
use overlord_connector_fixture::FixtureConnector;
use overlord_core::{
  Actor, CheckDraft, CheckId, Revision, Severity, SubjectKind, SystemId,
  Timestamp,
};
use overlord_engine::{SweepPlan, SystemConfig, checks, run_sweep};
use overlord_store::Db;
use overlord_web::{
  AppState, auth::AuthMode, oidc::Oidc, sweeprun::SweepRunner,
};
use tower::ServiceExt;

/// A fixed instant so `days_ago` windows are stable.
const NOW: &str = "2026-02-01T00:00:00Z";

fn ts(s: &str) -> Timestamp { s.parse().unwrap() }

fn fixture(name: &str) -> String {
  format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn system(id: &str, scenario: &str, stage: usize) -> SystemConfig {
  SystemConfig {
    id:        SystemId::new(id),
    connector: "fixture".to_owned(),
    config:    serde_json::json!({ "path": fixture(scenario), "stage": stage }),
  }
}

fn draft(
  id: &str,
  name: &str,
  severity: Severity,
  applies_to: SubjectKind,
  condition: &str,
) -> CheckDraft {
  CheckDraft {
    id: CheckId::new(id),
    name: name.to_owned(),
    description: None,
    rationale: None,
    remediation: Some("Do the thing".to_owned()),
    references: Vec::new(),
    severity,
    weight: None,
    applies_to,
    systems: Vec::new(),
    entity_types: Vec::new(),
    condition: condition.to_owned(),
    suppress_if_pending_links: false,
  }
}

/// A store with two systems swept once and four checks enabled, spanning
/// every severity band the board treats differently.
async fn seeded() -> AppState {
  let db = Arc::new(Db::open_memory().unwrap());
  let actor = Actor::new("cli:test");
  let now = ts(NOW);

  let drafts = [
    draft(
      "idp-mfa-missing",
      "MFA missing",
      Severity::Critical,
      SubjectKind::Entity,
      "status == \"active\" and not mfa_enrolled",
    ),
    draft(
      "admin-dormant",
      "Admin dormant",
      Severity::High,
      SubjectKind::Entity,
      "is_admin and last_login_at < days_ago(30)",
    ),
    draft(
      "workspace-without-idp",
      "Workspace account with no IdP account",
      Severity::Medium,
      SubjectKind::Person,
      "has_entity(\"workspace\") and not has_entity(\"idp\")",
    ),
    draft(
      "quiet-one",
      "Everything is a little bit wrong",
      Severity::Info,
      SubjectKind::Entity,
      "entity_type == \"user\"",
    ),
  ];
  for d in &drafts {
    checks::upsert(&db, &actor, d, now, None).unwrap();
    checks::dry_run(&db, &actor, &d.id, Revision::FIRST, now).unwrap();
    checks::enable(&db, &actor, &d.id, now).unwrap();
  }

  let systems = vec![
    system("gws-prod", "baseline.json", 0),
    system("okta-prod", "idp.json", 0),
  ];
  let registry = Arc::new(Registry::new().with(FixtureConnector::boxed()));
  let plan = SweepPlan::new(systems.clone()).at(now);
  run_sweep(&db, &registry, &plan).await.unwrap();

  state_for(db, registry, systems, AuthMode::Dev {
    actor: "tester".to_owned(),
  })
}

fn state_for(
  db: Arc<Db>,
  registry: Arc<Registry>,
  systems: Vec<SystemConfig>,
  auth: AuthMode,
) -> AppState {
  let oidc = match &auth {
    AuthMode::Oidc(cfg) => Some(Oidc::new((**cfg).clone()).unwrap()),
    AuthMode::Dev { .. } => None,
  };
  AppState {
    sweeps: SweepRunner::new(Arc::clone(&db), registry, systems, 10),
    db,
    auth,
    oidc,
    secure: false,
    config_path: "test.toml".to_owned(),
  }
}

async fn get(state: &AppState, uri: &str) -> Response {
  request(
    state,
    Request::builder().uri(uri).body(Body::empty()).unwrap(),
  )
  .await
}

async fn post(state: &AppState, uri: &str, form: &str) -> Response {
  request(
    state,
    Request::builder()
      .method("POST")
      .uri(uri)
      .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
      .body(Body::from(form.to_owned()))
      .unwrap(),
  )
  .await
}

async fn request(state: &AppState, request: Request<Body>) -> Response {
  overlord_web::router(state.clone())
    .oneshot(request)
    .await
    .unwrap()
}

async fn body(response: Response) -> String {
  let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
    .await
    .unwrap();
  String::from_utf8(bytes.to_vec()).unwrap()
}

async fn page(state: &AppState, uri: &str) -> String {
  let response = get(state, uri).await;
  assert_eq!(response.status(), StatusCode::OK, "GET {uri}");
  body(response).await
}

// --- the screens --------------------------------------------------------

#[tokio::test]
async fn every_screen_is_reachable() {
  let state = seeded().await;
  // SPEC.md section 5 lists seven screens. Each one is asserted by
  // something only that screen renders, so a route that silently falls
  // through to another page fails here.
  for (uri, marker) in [
    ("/", "New since the last sweep"),
    ("/rules", "Checks are the only detection mechanism"),
    ("/users", "Ranked by the sum of weights"),
    ("/sweeps", "the definition of"),
    ("/systems", "overlord never writes to any of them"),
    ("/settings", "What this process loaded"),
    ("/sweep?id=1", "Coverage"),
  ] {
    let html = page(&state, uri).await;
    assert!(html.contains(marker), "GET {uri} did not render {marker:?}");
  }
}

#[tokio::test]
async fn the_board_ranks_worst_first_and_folds_the_quiet_tiers() {
  let state = seeded().await;
  let html = page(&state, "/").await;

  let critical = html.find("sev-critical").expect("a critical row");
  let medium = html.find("sev-medium").expect("a medium row");
  assert!(critical < medium, "critical must lead the board");

  // SPEC.md section 5: `low` and `info` are collapsed by default. They
  // are still counted and still reachable — just not competing.
  assert!(
    html.contains("Low and info ("),
    "info tier must be folded away"
  );
  let details = html.find("<details>").expect("a fold");
  assert!(
    details > medium,
    "the fold must come after the loud tiers, not before"
  );
}

#[tokio::test]
async fn the_board_filters_narrow_the_query_not_the_page() {
  let state = seeded().await;

  let critical = page(&state, "/violations/rows?severity=critical").await;
  assert!(critical.contains("sev-critical"));
  assert!(
    !critical.contains("sev-medium"),
    "the severity facet must exclude other tiers"
  );

  // The fragment is the board and nothing else: no masthead, no nav.
  assert!(!critical.contains("<html"), "a fragment must not be a page");
  assert!(!critical.contains("masthead"));

  let searched = page(&state, "/violations/rows?q=svc-deploy").await;
  assert!(searched.contains("svc-deploy@example.com"));
  assert!(
    !searched.contains("ada@example.com"),
    "the subject search must actually narrow"
  );

  let nothing = page(&state, "/violations/rows?q=nobody-by-that-name").await;
  assert!(
    nothing.contains("Nothing matches those filters"),
    "an empty filtered board must not read as \"nothing is wrong\""
  );
}

#[tokio::test]
async fn a_person_scoped_violation_survives_the_system_facet() {
  let state = seeded().await;
  // A person spans systems, so "only show me okta-prod" cannot sensibly
  // exclude one — and excluding them would hide exactly the
  // cross-system findings the facet is being used to investigate.
  let html = page(&state, "/violations/rows?system=okta-prod").await;
  assert!(
    html.contains("Workspace account with no IdP account"),
    "person-scoped violations must survive a system filter"
  );
}

// --- actions ------------------------------------------------------------

#[tokio::test]
async fn acknowledging_records_a_command_and_re_renders_one_row() {
  let state = seeded().await;
  let subject = "entity/gws-prod/user/grace@example.com";

  let response = request(
    &state,
    Request::builder()
      .method("POST")
      .uri("/violations/act")
      .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
      .header("hx-request", "true")
      .body(Body::from(format!(
        "check=idp-mfa-missing&subject={}&episode=1&verb=acknowledge&\
         idempotency_key=k1",
        urlencoding(subject)
      )))
      .unwrap(),
  )
  .await;
  assert_eq!(response.status(), StatusCode::OK);

  let html = body(response).await;
  assert!(html.contains("acknowledged"), "{html}");
  assert!(html.starts_with("<tr"), "htmx asked for one row: {html}");
  assert!(!html.contains("<html"), "a row is not a page");

  // The overlay is in the stream, attributed, and the projection agrees.
  let rows = state
    .db
    .read(|r| r.violations(&[overlord_core::ViolationState::Acknowledged], 50))
    .unwrap();
  assert!(
    rows.iter().any(|v| v.subject.to_string() == subject
      && v.check_id.as_str() == "idp-mfa-missing"),
    "the acknowledgement did not reach the projection"
  );
}

#[tokio::test]
async fn a_replayed_submission_does_not_act_twice() {
  let state = seeded().await;
  let form = format!(
    "check=idp-mfa-missing&subject={}&episode=1&verb=acknowledge&\
     idempotency_key=same-key",
    urlencoding("entity/gws-prod/user/grace@example.com")
  );

  for _ in 0..3 {
    let response = post(&state, "/violations/act", &form).await;
    assert!(
      response.status().is_redirection() || response.status().is_success()
    );
  }

  let commands: i64 = state
    .db
    .read(|r| {
      Ok::<_, overlord_store::StoreError>(r.conn().query_row(
        "SELECT count(*) FROM command WHERE kind = 'violation.acknowledge'",
        [],
        |row| row.get(0),
      )?)
    })
    .unwrap();
  assert_eq!(commands, 1, "a retried submission must be a no-op");
}

#[tokio::test]
async fn suppression_carries_its_reason_expiry_and_note() {
  let state = seeded().await;
  let subject = "entity/gws-prod/user/grace@example.com";
  let form = format!(
    "check=admin-dormant&subject={}&episode=1&verb=suppress&\
     reason=accepted_risk&until=2026-12-31T00:00&note=asked+the+owner&\
     idempotency_key=s1",
    urlencoding(subject)
  );
  let response = post(&state, "/violations/act", &form).await;
  assert!(response.status().is_redirection());

  let html = page(
    &state,
    &format!(
      "/violation?check=admin-dormant&subject={}",
      urlencoding(subject)
    ),
  )
  .await;
  assert!(html.contains("accepted_risk"), "the reason must be visible");
  assert!(html.contains("2026-12-31"), "the expiry must be visible");
  assert!(html.contains("asked the owner"), "the note must be visible");
  assert!(
    html.contains("dev:tester"),
    "the operator must be attributed"
  );
}

// --- the check editor ---------------------------------------------------

#[tokio::test]
async fn a_bad_condition_is_underlined_where_it_is_wrong() {
  let state = seeded().await;
  let html = body(
    post(
      &state,
      "/rules/validate",
      "applies_to=entity&condition=status%20%3D%3D%20and",
    )
    .await,
  )
  .await;

  assert!(html.contains("expected a value"), "{html}");
  // PLAN.md section 5: spans, not just a message. The caret run is the
  // whole point of hand-rolling the parser.
  assert!(html.contains("caret"), "{html}");
  assert!(
    html.contains("^^^"),
    "the offending bytes must be underlined"
  );
}

#[tokio::test]
async fn validation_works_before_the_rest_of_the_form_is_filled_in() {
  let state = seeded().await;
  // A half-written new check must still get its condition checked;
  // demanding an id first would make the editor useless exactly when it
  // is most wanted.
  let html =
    body(post(&state, "/rules/validate", "condition=not%20mfa_enrolled").await)
      .await;
  assert!(html.contains("compiles"), "{html}");
}

#[tokio::test]
async fn a_scope_error_names_the_scope_that_would_fix_it() {
  let state = seeded().await;
  let html = body(
    post(
      &state,
      "/rules/validate",
      "applies_to=entity&condition=has_entity(%22idp%22)",
    )
    .await,
  )
  .await;
  assert!(html.contains("person-scoped"), "{html}");
}

#[tokio::test]
async fn saving_appends_a_revision_and_leaves_the_check_disabled() {
  let state = seeded().await;
  let response = post(
    &state,
    "/rules/save",
    "id=new-rule&name=New+rule&severity=low&applies_to=entity&\
     condition=status+%3D%3D+%22suspended%22&idempotency_key=n1",
  )
  .await;
  assert!(response.status().is_redirection());

  let record = state
    .db
    .read(|r| r.checks())
    .unwrap()
    .into_iter()
    .find(|c| c.draft.id.as_str() == "new-rule")
    .expect("the check was not saved");
  assert_eq!(record.revision, Revision::FIRST);
  assert!(
    !record.enabled,
    "an upsert must never enable a check (SPEC.md section 7)"
  );
}

#[tokio::test]
async fn enabling_is_refused_without_a_dry_run_for_that_revision() {
  let state = seeded().await;
  post(
    &state,
    "/rules/save",
    "id=ungated&name=Ungated&severity=low&applies_to=entity&condition=status+%\
     3D%3D+%22suspended%22&idempotency_key=u1",
  )
  .await;

  let response = post(&state, "/rules/enable", "id=ungated").await;
  assert_eq!(
    response.status(),
    StatusCode::CONFLICT,
    "enabling without a dry-run must be refused by the store, not just hidden \
     by the template"
  );

  let record = state
    .db
    .read(|r| r.checks())
    .unwrap()
    .into_iter()
    .find(|c| c.draft.id.as_str() == "ungated")
    .unwrap();
  assert!(!record.enabled);
}

#[tokio::test]
async fn the_editor_offers_enable_only_once_a_dry_run_exists() {
  let state = seeded().await;
  post(
    &state,
    "/rules/save",
    "id=gated&name=Gated&severity=low&applies_to=entity&condition=status+%3D%\
     3D+%22suspended%22&idempotency_key=g1",
  )
  .await;

  let before = page(&state, "/rules/edit?id=gated").await;
  assert!(
    before.contains("<button disabled"),
    "Enable must be disabled before a dry-run"
  );

  post(&state, "/rules/dry-run", "id=gated").await;

  let after = page(&state, "/rules/edit?id=gated").await;
  assert!(
    after.contains("/rules/enable"),
    "Enable must be offered once the revision has a dry-run"
  );
  assert!(
    after.contains("would match"),
    "the dry-run result must show"
  );
}

#[tokio::test]
async fn a_revised_check_needs_a_fresh_dry_run() {
  let state = seeded().await;
  // `idp-mfa-missing` is enabled at revision 1 with a dry-run. Revising
  // it must not carry that dry-run forward: a rule that was safe to run
  // yesterday is a different rule today.
  post(
    &state,
    "/rules/save",
    "id=idp-mfa-missing&name=MFA+missing&severity=critical&applies_to=entity&\
     condition=not+mfa_enrolled&idempotency_key=r2",
  )
  .await;

  let record = state
    .db
    .read(|r| r.checks())
    .unwrap()
    .into_iter()
    .find(|c| c.draft.id.as_str() == "idp-mfa-missing")
    .unwrap();
  assert_eq!(record.revision, Revision(2));

  let has = state
    .db
    .read(|r| r.has_dryrun(&CheckId::new("idp-mfa-missing"), Revision(2)))
    .unwrap();
  assert!(!has, "a new revision must not inherit a dry-run");
}

// --- detail screens -----------------------------------------------------

#[tokio::test]
async fn an_entity_page_shows_the_overlay_the_raw_payload_and_the_timeline() {
  let state = seeded().await;
  let html = page(
    &state,
    &format!(
      "/entity?ref={}",
      urlencoding("gws-prod/user/grace@example.com")
    ),
  )
  .await;

  assert!(html.contains("Normalized overlay"));
  assert!(html.contains("mfa_enrolled"), "a mapped field must show");
  assert!(html.contains("Raw payload"));
  assert!(html.contains("Fact timeline"));
  assert!(html.contains("MFA missing"), "its violations must show");
}

#[tokio::test]
async fn an_implicit_person_is_labelled_by_their_account() {
  let state = seeded().await;
  let html = page(
    &state,
    &format!(
      "/person?uid={}",
      urlencoding("implicit:gws-prod/user/svc-deploy@example.com")
    ),
  )
  .await;

  assert!(
    html.contains("unlinked account"),
    "an implicit person must say what they are"
  );
  assert!(html.contains("svc-deploy@example.com"));
}

#[tokio::test]
async fn the_coverage_view_shows_what_each_system_reported() {
  let state = seeded().await;
  let html = page(&state, "/sweep?id=1").await;
  assert!(html.contains("gws-prod"));
  assert!(html.contains("okta-prod"));
  assert!(
    html.contains("complete"),
    "whether a snapshot was a full enumeration decides whether absences may \
     become tombstones"
  );
}

// --- errors and auth ----------------------------------------------------

#[tokio::test]
async fn an_unknown_subject_is_a_not_found_page_not_a_blank_500() {
  let state = seeded().await;
  let response = get(&state, "/rules/edit?id=no-such-check").await;
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
  let html = body(response).await;
  assert!(html.contains("Not found"));
  assert!(
    html.contains("Back to the board"),
    "an error page is a page"
  );
}

#[tokio::test]
async fn a_malformed_reference_is_a_bad_request() {
  let state = seeded().await;
  let response = get(&state, "/entity?ref=not-an-entity-ref").await;
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn assets_are_content_hashed_and_cached_immutably() {
  let state = seeded().await;
  let page = page(&state, "/").await;
  let start = page.find("/assets/overlord.").unwrap();
  let end = page[start..].find(".css").unwrap() + start + 4;
  let path = &page[start..end];
  assert_ne!(path, "/assets/overlord.css", "the path must carry a hash");

  let response = get(&state, path).await;
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(
    response.headers().get(header::CACHE_CONTROL).unwrap(),
    "public, max-age=31536000, immutable"
  );

  // A stale path is a miss, not a stale body.
  assert_eq!(
    get(&state, "/assets/overlord.0000000000000000.css")
      .await
      .status(),
    StatusCode::NOT_FOUND
  );
}

#[tokio::test]
async fn without_oidc_no_command_can_reach_the_store() {
  let db = Arc::new(Db::open_memory().unwrap());
  let registry = Arc::new(Registry::new().with(FixtureConnector::boxed()));
  let state = state_for(
    db,
    registry,
    Vec::new(),
    AuthMode::Oidc(Box::new(overlord_web::auth::OidcConfig {
      issuer:           "https://id.example.com".to_owned(),
      client_id:        "overlord".to_owned(),
      client_secret:    None,
      redirect_url:     "https://overlord.example.com/auth/callback".to_owned(),
      allowed_subjects: ["ada@example.com".to_owned()].into_iter().collect(),
      required_group:   None,
    })),
  );

  // Every route that writes, and every route that reads.
  for (method, uri) in [
    ("GET", "/"),
    ("GET", "/rules"),
    ("GET", "/users"),
    ("POST", "/violations/act"),
    ("POST", "/rules/save"),
    ("POST", "/sweeps/run"),
  ] {
    let response = if method == "GET" {
      get(&state, uri).await
    } else {
      post(&state, uri, "").await
    };
    assert_eq!(
      response.status(),
      StatusCode::SEE_OTHER,
      "{method} {uri} must bounce an unauthenticated request"
    );
    assert_eq!(
      response.headers().get(header::LOCATION).unwrap(),
      "/auth/login"
    );
  }

  // Nothing was appended by any of that.
  let commands: i64 = state
    .db
    .read(|r| {
      Ok::<_, overlord_store::StoreError>(r.conn().query_row(
        "SELECT count(*) FROM command",
        [],
        |row| row.get(0),
      )?)
    })
    .unwrap();
  assert_eq!(commands, 0);
}

#[tokio::test]
async fn a_forged_session_does_not_authenticate() {
  let db = Arc::new(Db::open_memory().unwrap());
  let registry = Arc::new(Registry::new().with(FixtureConnector::boxed()));
  let state = state_for(
    db,
    registry,
    Vec::new(),
    AuthMode::Oidc(Box::new(overlord_web::auth::OidcConfig {
      issuer:           "https://id.example.com".to_owned(),
      client_id:        "overlord".to_owned(),
      client_secret:    None,
      redirect_url:     "https://overlord.example.com/auth/callback".to_owned(),
      allowed_subjects: ["ada@example.com".to_owned()].into_iter().collect(),
      required_group:   None,
    })),
  );

  let response = request(
    &state,
    Request::builder()
      .uri("/")
      .header(header::COOKIE, "overlord_session=abcdef.012345")
      .body(Body::empty())
      .unwrap(),
  )
  .await;
  assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn an_htmx_fragment_is_redirected_by_header_not_by_body() {
  let db = Arc::new(Db::open_memory().unwrap());
  let registry = Arc::new(Registry::new().with(FixtureConnector::boxed()));
  let state = state_for(
    db,
    registry,
    Vec::new(),
    AuthMode::Oidc(Box::new(overlord_web::auth::OidcConfig {
      issuer:           "https://id.example.com".to_owned(),
      client_id:        "overlord".to_owned(),
      client_secret:    None,
      redirect_url:     "https://overlord.example.com/auth/callback".to_owned(),
      allowed_subjects: ["ada@example.com".to_owned()].into_iter().collect(),
      required_group:   None,
    })),
  );

  // Swapping a sign-in page into the middle of a table would be worse
  // than useless, so an expired session navigates the whole window.
  let response = request(
    &state,
    Request::builder()
      .uri("/violations/rows")
      .header("hx-request", "true")
      .body(Body::empty())
      .unwrap(),
  )
  .await;
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
  assert_eq!(
    response.headers().get("hx-redirect").unwrap(),
    "/auth/login"
  );
}

/// Percent-encode a query value. The crate's own encoder is private to
/// the view layer, and a test that shares it could not catch it being
/// wrong.
fn urlencoding(s: &str) -> String {
  let mut out = String::new();
  for b in s.as_bytes() {
    match b {
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
        out.push(*b as char);
      }
      other => out.push_str(&format!("%{other:02X}")),
    }
  }
  out
}
