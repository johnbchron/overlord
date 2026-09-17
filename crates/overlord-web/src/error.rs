//! What a handler returns when it cannot render the page it was asked
//! for.
//!
//! Every variant becomes a full HTML page rather than a bare status
//! line: the operator is in a browser, and an unstyled 500 in the middle
//! of a workflow tells them nothing about what to do next. htmx swaps
//! only 2xx responses into the page by default, so a failed fragment
//! request leaves the existing content alone and logs instead.

use axum::{
  http::StatusCode,
  response::{Html, IntoResponse, Response},
};
use maud::html;
use overlord_engine::EngineError;
use overlord_store::StoreError;

pub type Result<T> = std::result::Result<T, WebError>;

#[derive(Debug, thiserror::Error)]
pub enum WebError {
  #[error(transparent)]
  Store(#[from] StoreError),

  #[error(transparent)]
  Engine(#[from] EngineError),

  #[error(transparent)]
  BadRef(#[from] overlord_core::ParseRefError),

  /// The request named something that does not exist — a check id, a
  /// person uid, an entity that was never observed.
  #[error("{0} not found")]
  NotFound(String),

  /// The form was malformed in a way the operator can fix.
  #[error("{0}")]
  BadRequest(String),

  /// The action is understood but refused, such as enabling a check
  /// revision that has no dry-run (SPEC.md section 7).
  #[error("{0}")]
  Refused(String),

  #[error("{0}")]
  Auth(String),

  #[error("{0}")]
  Internal(String),
}

impl WebError {
  pub fn not_found(what: impl Into<String>) -> Self {
    Self::NotFound(what.into())
  }

  pub fn bad_request(why: impl Into<String>) -> Self {
    Self::BadRequest(why.into())
  }

  pub fn refused(why: impl Into<String>) -> Self { Self::Refused(why.into()) }

  pub fn internal(why: impl Into<String>) -> Self { Self::Internal(why.into()) }

  #[must_use]
  pub fn status(&self) -> StatusCode {
    match self {
      Self::NotFound(_) => StatusCode::NOT_FOUND,
      Self::BadRequest(_) | Self::BadRef(_) => StatusCode::BAD_REQUEST,
      Self::Refused(_) => StatusCode::CONFLICT,
      Self::Auth(_) => StatusCode::UNAUTHORIZED,
      // A condition that will not compile is the operator's input, not
      // overlord's fault. The editor renders the spans; a direct post
      // gets the status that says so.
      Self::Engine(EngineError::BadCondition(_)) => StatusCode::BAD_REQUEST,
      // Most refusals reach a handler through the engine rather than
      // straight from the store — `checks::enable` is the one that
      // matters, since SPEC.md section 7 has the store, not the
      // template, enforce the dry-run gate. Matching only the direct
      // variant turned every one of those into a blank 500.
      Self::Store(e) | Self::Engine(EngineError::Store(e)) => match e {
        StoreError::NotFound(_) => StatusCode::NOT_FOUND,
        StoreError::Rejected(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
      },
      _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
  }

  /// The headline shown above the message.
  #[must_use]
  fn title(&self) -> &'static str {
    match self.status() {
      StatusCode::NOT_FOUND => "Not found",
      StatusCode::BAD_REQUEST => "That request did not make sense",
      StatusCode::CONFLICT => "Refused",
      StatusCode::UNAUTHORIZED => "Not signed in",
      _ => "Something went wrong",
    }
  }
}

impl IntoResponse for WebError {
  fn into_response(self) -> Response {
    let status = self.status();
    if status.is_server_error() {
      tracing::error!(error = %self, "request failed");
    } else {
      tracing::debug!(error = %self, "request refused");
    }

    let body = crate::layout::bare(self.title(), html! {
      div class="panel" {
        div class="panel-body stack" {
          h1 { (self.title()) }
          p class="soft" { (self.to_string()) }
          p { a href="/" { "Back to the board" } }
        }
      }
    });
    (status, Html(body.into_string())).into_response()
  }
}
