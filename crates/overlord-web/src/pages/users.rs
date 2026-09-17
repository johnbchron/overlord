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
  http::HeaderMap,
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::SubjectRef;
use overlord_store::{ScoreRow, SubjectFilter};
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::Result,
  layout::{self, Section},
  view,
};

/// How many subjects the roster shows at once. The cap is on the
/// filtered roster, not on the roster it was filtered from, and the
/// screen says so when it bites — a list silently cut at its limit is
/// the same thing as a list missing people.
const ROSTER_LIMIT: usize = 200;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsersQuery {
  #[serde(default)]
  pub q:    String,
  /// `confirmed` hides implicit singleton persons.
  #[serde(default)]
  pub kind: String,
}

impl UsersQuery {
  /// The roster this asks for. Unrecognised values mean everyone: a
  /// hand-edited query string should show more than it asked for, never
  /// less.
  fn filter(&self) -> SubjectFilter {
    match self.kind.as_str() {
      "confirmed" => SubjectFilter::Confirmed,
      "implicit" => SubjectFilter::Unlinked,
      _ => SubjectFilter::Everyone,
    }
  }
}

pub async fn list(
  identity: Identity,
  State(state): State<AppState>,
  headers: HeaderMap,
  Query(query): Query<UsersQuery>,
) -> Result<Response> {
  let body = body(&state, &query)?;

  // The filters push this URL into the address bar, so it has to answer
  // both questions: the results table when htmx is swapping it in, and
  // the whole screen when the browser loads it — which is what happens
  // on a reload, on a shared link, and on a back-button entry htmx's
  // cache no longer holds.
  if layout::is_htmx(&headers) {
    return Ok(Html(body.into_string()).into_response());
  }

  let content = html! {
    (layout::head(
      "Users",
      "Every person and unlinked account overlord has collected, ranked \
       by the sum of weights of active violations — a clean account \
       scores zero and sorts last. Confirming a link never changes a \
       total: it merges two scores rather than revealing a new one.",
      html! {},
    ))

    form class="filters"
         hx-get="/users"
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

  // Everyone, not only the subjects carrying risk: an account with a
  // clean record missing from this list is indistinguishable from an
  // account overlord never collected, and the first thing an operator
  // does after a sweep is look for somebody they know is there.
  //
  // The filter is the query's, not this function's: the two kinds are
  // interleaved by score, so keeping the confirmed rows out of the worst
  // 200 subjects would answer a different question — and answer it
  // short, dropping every confirmed person ranked below the cut. One
  // extra row is asked for so the screen can tell a full page from a
  // truncated one.
  let mut rows: Vec<ScoreRow> = state
    .db
    .read(|r| r.all_subjects(query.filter(), ROSTER_LIMIT + 1))?;
  let truncated = rows.len() > ROSTER_LIMIT;
  rows.truncate(ROSTER_LIMIT);

  Ok(html! {
    div class="panel" {
      @if rows.is_empty() {
        (layout::empty(
          "Nobody here yet. Run a sweep: this list is the accounts \
           overlord has collected, whether or not anything is wrong \
           with them.",
        ))
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
                td class="shrink num" {
                  // A zero is a real answer here — nothing is open
                  // against them — but it is not a number to read
                  // first, so it is not set in bold like the rest.
                  @if r.score > 0 {
                    b { (r.score) }
                  } @else {
                    span class="muted" { "0" }
                  }
                }
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
        @if truncated {
          p class="hint" {
            "The first " (ROSTER_LIMIT) ", worst first. Search by name, \
             email or key to reach somebody further down."
          }
        }
      }
    }
  })
}
