//! The operator UI (SPEC.md section 5).
//!
//! Server-rendered HTML, built with maud, made interactive with htmx
//! fragment swaps. There is no JSON API and no client-side model: every
//! screen is a function from the projections to markup, and every
//! interaction is a form post or a fragment GET that returns the same
//! markup a full page load would have produced. That is why a filter, a
//! violation row and a dry-run panel can each be re-rendered in
//! isolation without a second implementation of how they look.
//!
//! The UI is the only place checks are authored (SPEC.md section 15),
//! and every command it appends carries the authenticated
//! [`auth::Identity`] as its actor.

pub mod actions;
pub mod assets;
pub mod auth;
pub mod error;
pub mod layout;
pub mod oidc;
pub mod pages;
pub mod sweeprun;
pub mod view;

use std::{net::SocketAddr, sync::Arc};

use axum::{
  Router,
  extract::FromRef,
  routing::{get, post},
};
use overlord_store::Db;

use crate::{
  auth::AuthMode, error::WebError, oidc::Oidc, sweeprun::SweepRunner,
};

/// Everything a handler can reach.
#[derive(Clone)]
pub struct AppState {
  pub db:          Arc<Db>,
  pub sweeps:      Arc<SweepRunner>,
  pub auth:        AuthMode,
  /// Present exactly when [`AuthMode::Oidc`] is in effect.
  pub oidc:        Option<Arc<Oidc>>,
  /// Whether to mark the session cookie `Secure`. Off for a loopback
  /// dev server, which is plain HTTP and would otherwise never receive
  /// its own cookie back.
  pub secure:      bool,
  /// Shown on the Settings screen so the operator can confirm what the
  /// running process actually loaded.
  pub config_path: String,
}

impl FromRef<AppState> for AuthMode {
  fn from_ref(state: &AppState) -> Self { state.auth.clone() }
}

/// Assemble the router.
///
/// Route shapes worth noting: subject refs contain `/` and `@`, so
/// entities and persons are addressed by query parameter rather than by
/// path segment (see [`view::subject_href`]). Every mutating route is a
/// POST carrying an idempotency key minted when the form was rendered
/// (SPEC.md section 6.2).
pub fn router(state: AppState) -> Router {
  Router::new()
    // --- violations (home) --------------------------------------------
    .route("/", get(pages::violations::board))
    .route("/violations", get(pages::violations::board))
    // The old fragment endpoint. It was what `hx-push-url` put in the
    // address bar, so it is in browser histories and bookmarks already;
    // pointing it at the page handler turns those into a screen rather
    // than a bare table.
    .route("/violations/rows", get(pages::violations::board))
    .route("/violation", get(pages::violations::detail))
    .route("/violations/act", post(actions::violation_action))
    // --- rules ---------------------------------------------------------
    .route("/rules", get(pages::rules::list))
    .route("/rules/new", get(pages::rules::new))
    .route("/rules/edit", get(pages::rules::edit))
    .route("/rules/validate", post(pages::rules::validate))
    .route("/rules/save", post(actions::check_save))
    .route("/rules/dry-run", post(actions::check_dry_run))
    .route("/rules/enable", post(actions::check_enable))
    .route("/rules/disable", post(actions::check_disable))
    // --- users and detail ----------------------------------------------
    .route("/users", get(pages::users::list))
    // As `/violations/rows` above: a URL earlier versions pushed.
    .route("/users/results", get(pages::users::list))
    .route("/entities", get(pages::entities::list))
    .route("/person", get(pages::subject::person))
    .route("/entity", get(pages::subject::entity))
    // --- identity (SPEC.md section 12) ---------------------------------
    .route("/identity", get(pages::identity::queue))
    .route("/identity/candidates", get(pages::identity::picker))
    .route("/identity/link", post(actions::identity_link))
    .route("/identity/unlink", post(actions::identity_unlink))
    .route("/identity/primary", post(actions::identity_primary))
    .route("/identity/merge", post(actions::identity_merge))
    .route("/identity/split", post(actions::identity_split))
    // --- sweeps, systems, settings -------------------------------------
    .route("/sweeps", get(pages::sweeps::list))
    .route("/sweeps/run", post(actions::sweep_run))
    .route("/sweeps/progress", get(pages::sweeps::progress))
    .route("/sweep", get(pages::sweeps::detail))
    .route("/systems", get(pages::systems::list))
    .route("/settings", get(pages::settings::show))
    // --- auth and assets -----------------------------------------------
    .route("/auth/login", get(pages::signin::login))
    .route("/auth/callback", get(pages::signin::callback))
    .route("/auth/logout", get(pages::signin::logout))
    .route("/assets/{name}", get(assets::serve))
    .with_state(state)
}

/// Bind and serve until the process is asked to stop.
///
/// # Errors
/// If the address cannot be bound, or the server exits with an error.
pub async fn serve(state: AppState, bind: SocketAddr) -> Result<(), WebError> {
  if matches!(state.auth, AuthMode::Dev { .. })
    && !auth::dev_actor_permits(bind)
  {
    return Err(WebError::Auth(format!(
      "--dev-actor authenticates nobody, so it will not serve {bind}; \
       configure [auth] for OIDC, or bind a loopback address"
    )));
  }
  if let AuthMode::Oidc(cfg) = &state.auth
    && !cfg.admits_anyone()
  {
    return Err(WebError::Auth(
      "[auth] has neither allowed_subjects nor required_group, so no account \
       could sign in; authentication alone is not sufficient (SPEC.md section \
       14)"
        .to_owned(),
    ));
  }

  let listener = tokio::net::TcpListener::bind(bind)
    .await
    .map_err(|e| WebError::internal(format!("binding {bind}: {e}")))?;
  tracing::info!(%bind, "overlord is listening");
  axum::serve(listener, router(state))
    .await
    .map_err(|e| WebError::internal(format!("server: {e}")))
}
