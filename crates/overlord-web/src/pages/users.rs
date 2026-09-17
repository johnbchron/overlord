//! Users: persons and unlinked entities ranked by risk (SPEC.md s5, s8).
//!
//! Implicit singleton persons are listed alongside confirmed ones, with
//! a filter toggle — PLAN.md section 8 takes that as the default answer
//! to SPEC.md section 17's open question, on the grounds that an
//! unlinked account carrying real risk is exactly what the operator
//! needs to see, and hiding it would make identity work a prerequisite
//! for seeing anything.

use axum::{
  extract::{Query, State},
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::SubjectRef;
use overlord_store::ScoreRow;
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::Result,
  layout::{self, Section},
  view,
};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsersQuery {
  #[serde(default)]
  pub q:    String,
  /// `confirmed` hides implicit singleton persons.
  #[serde(default)]
  pub kind: String,
}

pub async fn list(
  identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<UsersQuery>,
) -> Result<Response> {
  let body = body(&state, &query)?;
  let content = html! {
    (layout::head(
      "Users",
      "Ranked by the sum of weights of active violations. Confirming a \
       link never changes a total — it merges two scores rather than \
       revealing a new one.",
      html! {},
    ))

    form class="filters"
         hx-get="/users/results"
         hx-target="#results"
         hx-push-url="true"
         hx-trigger="change, search, keyup changed delay:300ms from:find input[name='q']" {
      label class="field" style="flex:2" {
        span { "Search" }
        input type="search" name="q" value=(query.q)
              placeholder="name, email or key";
      }
      label class="field" {
        span { "Show" }
        select name="kind" {
          option value="" selected[query.kind.is_empty()] { "Everyone" }
          option value="confirmed" selected[query.kind == "confirmed"] {
            "Confirmed persons only"
          }
          option value="implicit" selected[query.kind == "implicit"] {
            "Unlinked accounts only"
          }
        }
      }
      noscript { button type="submit" { "Search" } }
    }

    div id="results" { (body) }
  };

  Ok(
    Html(
      layout::page(&identity, "Users", Section::Users, content).into_string(),
    )
    .into_response(),
  )
}

/// The htmx fragment.
pub async fn results(
  _identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<UsersQuery>,
) -> Result<Response> {
  Ok(Html(body(&state, &query)?.into_string()).into_response())
}

fn body(state: &AppState, query: &UsersQuery) -> Result<Markup> {
  // A free-text search is a different question from "who is worst", so
  // it answers with matches rather than with a filtered ranking: an
  // operator looking up one person wants them found whether or not they
  // carry any risk at all.
  if !query.q.trim().is_empty() {
    let hits = state.db.read(|r| r.search_subjects(query.q.trim(), 100))?;
    return Ok(html! {
      div class="panel" {
        @if hits.is_empty() {
          (layout::empty("Nothing matches."))
        } @else {
          table {
            thead {
              tr { th { "Subject" } th { "Where" } th class="shrink" { "Kind" } }
            }
            tbody {
              @for hit in &hits {
                tr {
                  td {
                    (view::subject(&hit.subject, hit.display_name.as_deref()))
                  }
                  td class="soft" { (hit.detail) }
                  td class="shrink" {
                    span class="tag" { (hit.subject.kind().as_str()) }
                  }
                }
              }
            }
          }
        }
      }
    });
  }

  let rows: Vec<ScoreRow> = state
    .db
    .read(|r| r.top_subjects(200))?
    .into_iter()
    .filter(|r| match query.kind.as_str() {
      "confirmed" => !r.implicit,
      "implicit" => r.implicit,
      _ => true,
    })
    .collect();

  Ok(html! {
    div class="panel" {
      @if rows.is_empty() {
        (layout::empty("Nobody is carrying risk."))
      } @else {
        table {
          thead {
            tr {
              th class="shrink num" { "Score" }
              th { "Subject" }
              th class="shrink" { "Worst" }
              th class="shrink num" { "Violations" }
              th class="shrink" { "Identity" }
            }
          }
          tbody {
            @for r in &rows {
              @let subject = SubjectRef::Person(r.person_uid.clone());
              tr {
                td class="shrink num" { b { (r.score) } }
                td { (view::subject(&subject, r.display_name.as_deref())) }
                td class="shrink" {
                  @match r.worst_severity {
                    Some(s) => (view::severity(s)),
                    None => span class="muted" { "—" },
                  }
                }
                td class="shrink num" { (r.count) }
                td class="shrink" {
                  @if r.implicit {
                    span class="tag"
                         title="An unlinked account, evaluated as a person \
                                in its own right" {
                      "unlinked"
                    }
                  } @else {
                    span class="tag tag-ok" { "confirmed" }
                  }
                }
              }
            }
          }
        }
      }
    }
  })
}
