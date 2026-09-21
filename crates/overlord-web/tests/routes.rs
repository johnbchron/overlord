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
  Actor, CheckDraft, CheckId, CommandKind, EntityKey, EntityRef, EntityStatus,
  EntityType, NewCommand, NormalizedRecord, PersonUid, Revision, Severity,
  SubjectKind, SystemId, Timestamp,
};
use overlord_engine::{SweepPlan, SystemConfig, checks, run_sweep};
use overlord_store::{Db, NewFact, SweepStart, SweepStatus};
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

/// A GET as htmx makes one. The `HX-Request` header is the whole of what
/// tells a screen to answer with its fragment rather than its page, so a
/// test that swaps fragments has to send it.
async fn fragment(state: &AppState, uri: &str) -> String {
  let response = request(
    state,
    Request::builder()
      .uri(uri)
      .header("hx-request", "true")
      .body(Body::empty())
      .unwrap(),
  )
  .await;
  assert_eq!(response.status(), StatusCode::OK, "GET {uri} (htmx)");
  body(response).await
}

// --- the screens --------------------------------------------------------

#[tokio::test]
async fn every_screen_is_reachable() {
  let state = seeded().await;
  // SPEC.md section 5 lists seven screens, plus the identity queue
  // section 12 needs. Each one is asserted by
  // something only that screen renders, so a route that silently falls
  // through to another page fails here.
  for (uri, marker) in [
    ("/", "New since the last sweep"),
    ("/rules", "Checks are the only detection mechanism"),
    ("/users", "Every person and unlinked account"),
    ("/identity", "Proposed links, computed fresh each sweep"),
    ("/sweeps", "the definition of"),
    ("/systems", "overlord never writes to any of them"),
    ("/settings", "What this process loaded"),
    ("/entities", "Every object overlord has collected"),
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

  let critical = fragment(&state, "/violations?severity=critical").await;
  assert!(critical.contains("sev-critical"));
  assert!(
    !critical.contains("sev-medium"),
    "the severity facet must exclude other tiers"
  );

  // The fragment is the board and nothing else: no masthead, no nav.
  assert!(!critical.contains("<html"), "a fragment must not be a page");
  assert!(!critical.contains("masthead"));

  let searched = fragment(&state, "/violations?q=svc-deploy").await;
  assert!(searched.contains("svc-deploy@example.com"));
  assert!(
    !searched.contains("ada@example.com"),
    "the subject search must actually narrow"
  );

  let nothing = fragment(&state, "/violations?q=nobody-by-that-name").await;
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

// --- identity (SPEC.md section 12) --------------------------------------

/// Ada holds an account in both fixtures under the same address, so the
/// queue proposes exactly one link for her in each direction.
const ADA_WS: &str = "gws-prod/user/ada@example.com";
const ADA_IDP: &str = "okta-prod/user/ada@example.com";

#[tokio::test]
async fn the_queue_shows_a_proposal_with_the_signal_that_found_it() {
  let state = seeded().await;
  let html = page(&state, "/identity").await;
  assert!(html.contains("exact-email"), "{html}");
  assert!(html.contains("ada@example.com"), "{html}");
  assert!(
    html.contains("unlinked; confirming creates a person"),
    "a proposal naming an unlinked account must say what confirming does"
  );
}

#[tokio::test]
async fn confirming_from_the_queue_creates_the_person_and_links_both() {
  let state = seeded().await;

  let response = post(
    &state,
    "/identity/link",
    &format!(
      "entity={}&person={}&signal=exact-email&back=%2Fidentity&\
       idempotency_key=k1",
      urlencoding(ADA_WS),
      urlencoding(&format!("implicit:{ADA_IDP}")),
    ),
  )
  .await;
  assert!(
    response.status().is_redirection(),
    "{:?}",
    response.status()
  );

  // One person, holding both accounts. The operator clicked once.
  let links = state.db.read(|r| r.links()).unwrap();
  assert_eq!(links.len(), 2, "{links:?}");
  let uid = links[0].1.clone();
  assert!(links.iter().all(|(_, p)| *p == uid), "{links:?}");

  let html = page(
    &state,
    &format!("/person?uid={}", urlencoding(uid.as_str())),
  )
  .await;
  assert!(html.contains("gws-prod"), "{html}");
  assert!(html.contains("okta-prod"), "{html}");

  // And the proposal is gone: a linked account has had the operator's
  // attention already.
  let queue = page(&state, "/identity").await;
  assert!(!queue.contains("ada@example.com"), "{queue}");
}

#[tokio::test]
async fn a_promoted_person_still_shows_the_violation_they_arrived_with() {
  let state = seeded().await;

  // `workspace-without-idp` is person-scoped, so before linking it is
  // open against Ada's *workspace* account as an implicit person.
  let implicit = format!("person/implicit:{ADA_WS}");
  let opened = state
    .db
    .read(|r| r.violations(&[overlord_core::ViolationState::Open], 500))
    .unwrap();
  assert!(
    opened.iter().any(|v| v.subject.to_string() == implicit),
    "{opened:?}"
  );

  post(
    &state,
    "/identity/link",
    &format!(
      "entity={}&person={}&idempotency_key=k1",
      urlencoding(ADA_WS),
      urlencoding(&format!("implicit:{ADA_IDP}")),
    ),
  )
  .await;

  let uid = state.db.read(|r| r.links()).unwrap()[0].1.clone();
  let html = page(
    &state,
    &format!("/person?uid={}", urlencoding(uid.as_str())),
  )
  .await;
  assert!(
    html.contains("Workspace account with no IdP account"),
    "an episode filed against the implicit uid is this person's now: {html}"
  );
}

#[tokio::test]
async fn the_picker_offers_an_unlinked_account_as_somebody_to_link_to() {
  let state = seeded().await;
  let html = page(
    &state,
    &format!(
      "/identity/candidates?mode=link&q=ada&anchor={}",
      urlencoding(ADA_WS)
    ),
  )
  .await;
  assert!(html.contains("Link"), "{html}");
  assert!(html.contains(&urlencoding(ADA_WS)), "{html}");
}

#[tokio::test]
async fn the_merge_picker_leaves_out_unlinked_accounts() {
  let state = seeded().await;
  // Nothing is linked, so every hit is an unlinked account — and a
  // merge joins two confirmed persons (SPEC.md section 12). The list is
  // empty rather than full of buttons that would be refused.
  let html = page(
    &state,
    "/identity/candidates?mode=merge&q=ada&anchor=whoever",
  )
  .await;
  assert!(!html.contains("Merge into this person"), "{html}");
}

#[tokio::test]
async fn unlinking_returns_the_account_to_being_its_own_person() {
  let state = seeded().await;
  post(
    &state,
    "/identity/link",
    &format!("entity={}&idempotency_key=k1", urlencoding(ADA_WS)),
  )
  .await;
  let uid = state.db.read(|r| r.links()).unwrap()[0].1.clone();

  let response = post(
    &state,
    "/identity/unlink",
    &format!(
      "person={}&entity={}&idempotency_key=k2",
      urlencoding(uid.as_str()),
      urlencoding(ADA_WS),
    ),
  )
  .await;
  assert!(response.status().is_redirection());
  assert!(state.db.read(|r| r.links()).unwrap().is_empty());
}

#[tokio::test]
async fn a_designation_is_refused_for_an_account_the_person_does_not_hold() {
  let state = seeded().await;
  post(
    &state,
    "/identity/link",
    &format!("entity={}&idempotency_key=k1", urlencoding(ADA_WS)),
  )
  .await;
  let uid = state.db.read(|r| r.links()).unwrap()[0].1.clone();

  // The store refuses it, and the refusal arrives as a 409 with the
  // reason rather than a blank 500 — the same contract the check editor
  // relies on.
  let response = post(
    &state,
    "/identity/primary",
    &format!(
      "person={}&system_kind=idp&entity={}&idempotency_key=k2",
      urlencoding(uid.as_str()),
      urlencoding(ADA_IDP),
    ),
  )
  .await;
  assert_eq!(response.status(), StatusCode::CONFLICT);
  assert!(body(response).await.contains("not linked"));
}

#[tokio::test]
async fn an_off_site_return_address_is_not_followed() {
  let state = seeded().await;
  let response = post(
    &state,
    "/identity/link",
    &format!(
      "entity={}&back=https%3A%2F%2Felsewhere.invalid%2F&idempotency_key=k1",
      urlencoding(ADA_WS)
    ),
  )
  .await;
  let location = response
    .headers()
    .get(header::LOCATION)
    .map(|v| v.to_str().unwrap().to_owned())
    .unwrap_or_default();
  assert!(location.starts_with("/person?uid="), "{location}");
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

#[tokio::test]
async fn an_account_with_nothing_against_it_is_still_on_the_users_screen() {
  // No checks enabled, so no subject has a score at all. The screen
  // still has to show the four accounts the sweep collected: an
  // operator who has just swept looks for somebody they know is there,
  // and an empty list reads as a broken connector.
  let db = Arc::new(Db::open_memory().unwrap());
  let systems = vec![system("gws-prod", "baseline.json", 0)];
  let registry = Arc::new(Registry::new().with(FixtureConnector::boxed()));
  run_sweep(&db, &registry, &SweepPlan::new(systems.clone()).at(ts(NOW)))
    .await
    .unwrap();
  let state = state_for(db, registry, systems, AuthMode::Dev {
    actor: "tester".to_owned(),
  });

  let html = page(&state, "/users").await;
  for name in ["Ada Lovelace", "Grace Hopper", "Deploy Robot"] {
    assert!(html.contains(name), "{name} is missing from /users");
  }
  // Listed as themselves, not as their uid (SPEC.md section 6.4).
  assert!(!html.contains("implicit:gws-prod"), "{html}");
}

#[tokio::test]
async fn the_users_screen_ranks_risk_first_and_clean_accounts_last() {
  let state = seeded().await;
  let html = page(&state, "/users").await;

  let grace = html.find("Grace Hopper").expect("grace");
  let ada = html.find("Ada Lovelace").expect("ada");
  assert!(grace < ada, "the worst subject must lead the list");

  // The filter is a filter, not a different list: asking for confirmed
  // persons only on a store where nobody is linked yields nobody.
  let confirmed = page(&state, "/users/results?kind=confirmed").await;
  assert!(!confirmed.contains("Ada Lovelace"), "{confirmed}");
  assert!(
    page(&state, "/users/results?kind=implicit")
      .await
      .contains("Ada Lovelace")
  );
}

/// A roster big enough for its own limit to bite. Every account is
/// unlinked except one confirmed person, who sorts last: the row an
/// operator asking for confirmed persons is looking for, and the row a
/// filter applied after the limit would never reach.
async fn crowded() -> AppState {
  let db = Arc::new(Db::open_memory().unwrap());
  let now = ts(NOW);
  let start = db
    .write(|w| {
      w.open_sweep(&SweepStart {
        started_at:        now,
        requested:         vec![SystemId::new("gws-prod")],
        pinned_checks:     vec![],
        pinned_norm:       vec![],
        absence_guard_pct: 10,
      })
    })
    .unwrap();

  let mut facts: Vec<NewFact> = (0..260)
    .map(|i| account(&format!("aaa-{i:03}@x.com"), now))
    .collect();
  facts.push(account("zzz-linked@x.com", now));
  db.write(|w| w.append_facts(start, &facts)).unwrap();
  db.write(|w| w.commit_sweep(start, SweepStatus::Ok, now))
    .unwrap();

  db.write(|w| {
    w.append_command(&NewCommand::new(
      Actor::new("cli:test"),
      CommandKind::PersonLink {
        person_uid:      PersonUid::new("P-zoe"),
        entity:          EntityRef::new("gws-prod", "user", "zzz-linked@x.com"),
        from_suggestion: None,
      },
      now,
    ))
  })
  .unwrap();

  let registry = Arc::new(Registry::new().with(FixtureConnector::boxed()));
  state_for(db, registry, Vec::new(), AuthMode::Dev {
    actor: "tester".to_owned(),
  })
}

fn account(key: &str, at: Timestamp) -> NewFact {
  let mut n = NormalizedRecord::new(
    "gws-prod",
    overlord_core::SystemKind::Workspace,
    "user",
    key,
    EntityStatus::Active,
  );
  n.display_name = Some(key.to_owned());
  NewFact {
    system:       SystemId::new("gws-prod"),
    entity_type:  EntityType::new("user"),
    entity_key:   EntityKey::new(key),
    observed_at:  at,
    raw:          None,
    normalized:   Some(n),
    norm_version: "fixture/1".to_owned(),
  }
}

#[tokio::test]
async fn the_confirmed_filter_reaches_a_person_the_roster_limit_cuts_off() {
  let state = crowded().await;

  // Unfiltered, the person is past the cut and the screen says the list
  // was cut rather than presenting 200 rows as everybody.
  let everyone = page(&state, "/users/results").await;
  assert!(!everyone.contains("P-zoe"), "{everyone}");
  assert!(everyone.contains("The first 200"), "{everyone}");

  // Filtered, there is one confirmed person and the limit has nothing
  // to cut. Before, the filter ran over the worst 200 subjects and this
  // screen was empty.
  let confirmed = page(&state, "/users/results?kind=confirmed").await;
  // A linked person carries no display name of its own until one is
  // given, so it is listed by uid.
  assert!(confirmed.contains("P-zoe"), "{confirmed}");
  assert!(!confirmed.contains("Nobody here yet"), "{confirmed}");
  assert!(!confirmed.contains("The first 200"), "{confirmed}");
  assert!(!confirmed.contains("aaa-000@x.com"), "{confirmed}");

  // The other half is still capped, because there really are more.
  let unlinked = page(&state, "/users/results?kind=implicit").await;
  assert!(unlinked.contains("The first 200"), "{unlinked}");
  assert!(!unlinked.contains("P-zoe"), "{unlinked}");
  assert!(!unlinked.contains("zzz-linked@x.com"), "{unlinked}");
}

/// `hx-push-url` puts the URL htmx *fetched* into the address bar, so a
/// screen whose filters push must answer that same URL with a whole page
/// when the browser asks for it directly. When the filters fetched a
/// fragment-only endpoint, the address bar ended up holding one — and
/// the next reload, shared link, or back-button entry htmx's history
/// cache had dropped rendered the results table as the entire document.
#[tokio::test]
async fn a_pushed_filter_url_loads_as_a_page_and_swaps_as_a_fragment() {
  let state = seeded().await;

  for uri in [
    "/users?kind=confirmed",
    "/violations?severity=critical",
    // The endpoints earlier versions pushed are still routed, because
    // they are in browser histories already.
    "/users/results?kind=confirmed",
    "/violations/rows?severity=critical",
  ] {
    let loaded = page(&state, uri).await;
    assert!(
      loaded.contains("<html"),
      "GET {uri} must be a page:\n{loaded}"
    );
    assert!(loaded.contains("masthead"), "GET {uri} lost its nav");
  }

  // The same URLs the filters fetch, with htmx's header: the fragment
  // alone, which is what `hx-target` expects to swap in.
  for uri in ["/users?kind=confirmed", "/violations?severity=critical"] {
    let swapped = fragment(&state, uri).await;
    assert!(
      !swapped.contains("<html"),
      "htmx GET {uri} must be a fragment:\n{swapped}"
    );
    assert!(
      !swapped.contains("masthead"),
      "htmx GET {uri} carried the nav"
    );
  }

  // And the form fetches the page URL, not a fragment endpoint — which
  // is what makes the pushed URL the page's own.
  let users = page(&state, "/users").await;
  assert!(users.contains(r#"hx-get="/users""#), "{users}");
  assert!(!users.contains("/users/results"), "{users}");

  let board = page(&state, "/violations").await;
  assert!(board.contains(r#"hx-get="/violations""#), "{board}");
  assert!(!board.contains("/violations/rows"), "{board}");
}

// --- the entity browser -----------------------------------------------

/// The rows alone. The filter controls list every system and connector
/// in the store by name, so a whole-page assertion about a system being
/// absent is really an assertion about the dropdown.
fn rows_of(html: &str) -> String {
  let Some(start) = html.find("<tbody") else {
    return String::new();
  };
  let end = html[start..]
    .find("</tbody>")
    .map_or(html.len(), |e| start + e);
  html[start..end].to_owned()
}

#[tokio::test]
async fn the_entity_browser_lists_what_the_users_roster_leaves_out() {
  // The reason this screen exists. The roster is person-shaped and the
  // identity policy keeps non-person types off it; those entities still
  // have to be reachable somewhere.
  let state = seeded().await;
  state
    .db
    .write(|w| {
      w.append_command(&NewCommand::new(
        Actor::new("cli:test"),
        CommandKind::identity_policy([EntityType::new("user")]),
        ts(NOW),
      ))
    })
    .unwrap();

  let roster = page(&state, "/users").await;
  assert!(!roster.contains("Ada Lovelace"), "policy not in effect");

  let entities = page(&state, "/entities").await;
  assert!(
    entities.contains("Ada Lovelace"),
    "an entity the roster excludes must still be listed here"
  );
}

#[tokio::test]
async fn the_entity_browser_filters_on_each_facet() {
  let state = seeded().await;

  let all = page(&state, "/entities").await;
  assert!(all.contains("Ada Lovelace"));
  // Ada is in both the workspace and the IdP, and the unfiltered list
  // shows each as its own entity.
  let all_rows = rows_of(&all);
  assert!(all_rows.contains("okta-prod") && all_rows.contains("gws-prod"));

  // By system: one of the two Adas.
  let one_system = rows_of(&page(&state, "/entities?system=okta-prod").await);
  assert!(one_system.contains("Ada Lovelace"));
  assert!(!one_system.contains("gws-prod"));

  // By kind, which is the other way to make the same cut.
  let by_kind = rows_of(&page(&state, "/entities?kind=idp").await);
  assert!(by_kind.contains("okta-prod"));
  assert!(!by_kind.contains("gws-prod"));

  // The connector every seeded system is read through, and one that
  // nothing was read through.
  let by_connector = page(&state, "/entities?connector=fixture").await;
  assert!(by_connector.contains("Ada Lovelace"));
  let wrong_connector = page(&state, "/entities?connector=okta").await;
  assert!(!wrong_connector.contains("Ada Lovelace"));
  assert!(wrong_connector.contains("Nothing matches these filters"));

  // An entity type that exists, and one that does not.
  let by_type = page(&state, "/entities?entity_type=user").await;
  assert!(by_type.contains("Ada Lovelace"));
  let no_type = page(&state, "/entities?entity_type=phone-device").await;
  assert!(!no_type.contains("Ada Lovelace"));

  // A kind that does not parse is refused, as the violations board
  // refuses a bad severity. Ignoring it would widen a filter the
  // operator set, and defaulting it would answer a different question.
  let bogus = get(&state, "/entities?kind=not-a-kind").await;
  assert_eq!(bogus.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_entity_browser_searches_the_latest_facts_content() {
  let state = seeded().await;

  let hit = page(&state, "/entities?q=ada").await;
  assert!(hit.contains("Ada Lovelace"));
  assert!(!hit.contains("Grace Hopper"), "the search narrows");

  // A value the overlay carries but the display name does not.
  let by_department = page(&state, "/entities?q=engineering").await;
  assert!(by_department.contains("Ada Lovelace"));

  let miss = page(&state, "/entities?q=nobodyhasthisstring").await;
  assert!(!miss.contains("Ada Lovelace"));
  assert!(miss.contains("Nothing matches these filters"));

  // Punctuation is not an FTS5 syntax error, and an address finds its
  // account rather than 500-ing.
  for q in ["ada%40example.com", "%22oops", "a%20AND%20b", "*", "%5E"] {
    let res = get(&state, &format!("/entities?q={q}")).await;
    assert_eq!(res.status(), StatusCode::OK, "GET /entities?q={q}");
  }
  assert!(
    page(&state, "/entities?q=ada%40example.com")
      .await
      .contains("Ada Lovelace")
  );
}

#[tokio::test]
async fn the_entity_browser_answers_htmx_with_the_table_alone() {
  // As the other filtered screens: the same URL has to serve the swap
  // and a fresh browser load of it, or a reload of a filtered view
  // renders a bare fragment.
  let state = seeded().await;
  let swapped = fragment(&state, "/entities?q=ada").await;
  assert!(swapped.contains("Ada Lovelace"));
  assert!(
    !swapped.contains("<nav"),
    "the fragment must not carry the nav"
  );

  let whole = page(&state, "/entities?q=ada").await;
  assert!(whole.contains("<nav"), "a fresh load is the whole screen");
  assert!(whole.contains("Ada Lovelace"));
}

#[tokio::test]
async fn an_entity_with_a_blank_name_still_renders_a_clickable_link() {
  // End to end, against a fact already in the store carrying
  // `display_name: ""` — which is what a store written before
  // normalization dropped blanks holds, and what the view guard exists
  // for. The row must identify itself and the link must be clickable.
  let state = seeded().await;
  let sweep_id = state
    .db
    .write(|w| {
      w.open_sweep(&SweepStart {
        started_at:        ts(NOW),
        requested:         vec![SystemId::new("ucm-extensions")],
        pinned_checks:     vec![],
        pinned_norm:       vec![],
        absence_guard_pct: 10,
      })
    })
    .unwrap();

  let mut record = NormalizedRecord::new(
    "ucm-extensions",
    overlord_core::SystemKind::Sso,
    "phone-extension",
    "1001",
    EntityStatus::Active,
  );
  record.display_name = Some(String::new());

  state
    .db
    .write(|w| {
      w.append_facts(sweep_id, &[NewFact {
        system:       SystemId::new("ucm-extensions"),
        entity_type:  EntityType::new("phone-extension"),
        entity_key:   EntityKey::new("1001"),
        observed_at:  ts(NOW),
        raw:          Some(serde_json::json!({ "extension": "1001" })),
        normalized:   Some(record),
        norm_version: "grandstream-ucm-extension/1".to_owned(),
      }])?;
      w.commit_sweep(sweep_id, SweepStatus::Ok, ts(NOW))
    })
    .unwrap();

  let rows = rows_of(&page(&state, "/entities?system=ucm-extensions").await);
  assert!(
    rows.contains("ucm-extensions/1001"),
    "a blank name must fall back to the ref, not render an empty link: {rows}"
  );
  assert!(
    !rows.contains("\"ref\"></a>") && !rows.contains("\"ref\"> </a>"),
    "an empty anchor is invisible and unclickable: {rows}"
  );

  // The detail page it links to titles itself by the key for the same
  // reason, rather than opening with an empty heading.
  let detail = page(
    &state,
    "/entity?ref=ucm-extensions%2Fphone-extension%2F1001",
  )
  .await;
  assert!(detail.contains("1001"));
  assert!(!detail.contains("<h1></h1>"), "an empty title");
}
