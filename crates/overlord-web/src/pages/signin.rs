//! Signing in and out (SPEC.md section 14).
//!
//! In dev mode these routes do nothing useful and say so: there is no
//! session to establish when the principal is fixed, and pretending
//! otherwise would let an operator believe they had authenticated.

use axum::{
  extract::{Query, State},
  response::{Html, IntoResponse, Redirect, Response},
};
use maud::html;
use overlord_core::Timestamp;
use serde::Deserialize;

use crate::{
  AppState,
  auth::{self, AuthMode},
  error::{Result, WebError},
  layout,
};

#[derive(Debug, Default, Deserialize)]
pub struct LoginQuery {
  /// Where the operator was going before they were bounced here.
  #[serde(default)]
  pub next: String,
}

pub async fn login(
  State(state): State<AppState>,
  Query(query): Query<LoginQuery>,
) -> Result<Response> {
  match (&state.auth, &state.oidc) {
    (AuthMode::Dev { .. }, _) => Ok(Redirect::to("/").into_response()),
    (AuthMode::Oidc(_), Some(oidc)) => {
      // Only a local path is honoured, so a crafted link cannot turn the
      // sign-in into an open redirect to somewhere else.
      let next = if query.next.starts_with('/') && !query.next.starts_with("//")
      {
        query.next.clone()
      } else {
        "/".to_owned()
      };
      let url = oidc.start(&next, Timestamp::now()).await?;
      Ok(Redirect::to(&url).into_response())
    }
    (AuthMode::Oidc(_), None) => Err(WebError::internal(
      "OIDC is configured but the client was not built",
    )),
  }
}

#[derive(Debug, Default, Deserialize)]
pub struct CallbackQuery {
  #[serde(default)]
  pub code:              String,
  #[serde(default)]
  pub state:             String,
  #[serde(default)]
  pub error:             String,
  #[serde(default)]
  pub error_description: String,
}

pub async fn callback(
  State(app): State<AppState>,
  Query(query): Query<CallbackQuery>,
) -> Result<Response> {
  if !query.error.is_empty() {
    return Ok(refused(&if query.error_description.is_empty() {
      query.error.clone()
    } else {
      format!("{}: {}", query.error, query.error_description)
    }));
  }

  let Some(oidc) = &app.oidc else {
    return Err(WebError::internal("OIDC is not configured"));
  };
  if query.code.is_empty() || query.state.is_empty() {
    return Err(WebError::Auth("the provider sent no code".to_owned()));
  }

  let now = Timestamp::now();
  let verified = match oidc.finish(&query.code, &query.state, now).await {
    Ok(v) => v,
    Err(WebError::Auth(why)) => return Ok(refused(&why)),
    Err(other) => return Err(other),
  };

  let cookie = auth::issue(&verified.subject, &verified.label, now)?;
  tracing::info!(subject = %verified.subject, "operator signed in");

  Ok(
    (
      [(
        axum::http::header::SET_COOKIE,
        auth::set_cookie(&cookie, app.secure),
      )],
      Redirect::to(&verified.next),
    )
      .into_response(),
  )
}

pub async fn logout(State(state): State<AppState>) -> Response {
  if matches!(state.auth, AuthMode::Dev { .. }) {
    return Redirect::to("/").into_response();
  }
  (
    [(axum::http::header::SET_COOKIE, auth::clear_cookie())],
    Html(
      layout::bare("Signed out", html! {
        div class="signin" {
          h1 { "Signed out" }
          p class="soft" { "Your session on this browser has been cleared." }
          p { a class="btn primary" href="/auth/login" { "Sign in again" } }
        }
      })
      .into_string(),
    ),
  )
    .into_response()
}

/// A refusal is a page, not a raw 403: the operator needs to know
/// whether to try a different account or to ask for access.
fn refused(why: &str) -> Response {
  (
    axum::http::StatusCode::FORBIDDEN,
    Html(
      layout::bare("Not permitted", html! {
        div class="signin" {
          h1 { "Not permitted" }
          p class="soft" { (why) }
          p class="soft" {
            "Access needs a listed subject or the required group claim. \
             Authentication alone is not sufficient."
          }
          p { a class="btn" href="/auth/login" { "Try again" } }
        }
      })
      .into_string(),
    ),
  )
    .into_response()
}
