//! Sweeps and coverage (SPEC.md section 5, section 10).
//!
//! The coverage table is the point of this screen: it shows what each
//! system *actually reported*, with entity-count deltas against that
//! system's previous sweep, so a connector that has silently gone empty
//! is visible as a number rather than as an absence of alarms.

use axum::{
  extract::{Query, State},
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::SweepId;
use overlord_store::{CoverageRow, SweepRow};
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::{Result, WebError},
  layout::{self, Section},
  sweeprun::RunState,
  view,
};

pub async fn list(
  identity: Identity,
  State(state): State<AppState>,
) -> Result<Response> {
  let sweeps = state.db.read(|r| r.sweeps(25))?;
  let run_state = state.sweeps.state();
  let configured: Vec<String> = state
    .sweeps
    .configured()
    .iter()
    .map(|s| s.id.to_string())
    .collect();

  let content = html! {
    (layout::head(
      "Sweeps",
      "A sweep is an explicit action. Its start time is the definition of \
       \"now\" for everything it produces.",
      html! {},
    ))

    div class="panel" {
      div class="panel-body" {
        form method="post" action="/sweeps/run" class="row" {
          label class="field" {
            span { "Systems" }
            select name="system" {
              option value="" { "Every configured system" }
              @for id in &configured {
                option value=(id) { (id) }
              }
            }
            div class="hint" {
              "Restricting a sweep does not manufacture change: \"new since \
               last sweep\" is compared per system."
            }
          }
          div class="shrink" {
            button class="primary" type="submit"
                   disabled[run_state == RunState::Running] {
              @if run_state == RunState::Running {
                "Running…"
              } @else {
                "Run a sweep"
              }
            }
          }
        }
      }
    }

    div id="progress"
        hx-get="/sweeps/progress"
        hx-trigger=(if run_state == RunState::Running {
          "load, every 2s"
        } else {
          "none"
        })
        hx-swap="innerHTML" {
      (progress_body(&run_state, sweeps.first()))
    }

    h2 { "Runs" }
    div class="panel" {
      @if sweeps.is_empty() {
        (layout::empty("No sweeps yet."))
      } @else {
        table {
          thead {
            tr {
              th class="shrink" { "Sweep" }
              th class="shrink" { "Started" }
              th class="shrink" { "Status" }
              th { "Systems" }
              th class="shrink num" { "Facts" }
              th class="shrink" { "" }
            }
          }
          tbody {
            @for s in &sweeps {
              tr {
                td class="shrink" { b { (s.id) } }
                td class="shrink" { (view::when(s.started_at)) }
                td class="shrink" { (status_tag(s)) }
                td { (coverage_summary(&s.coverage)) }
                td class="shrink num" { (s.facts) }
                td class="shrink" {
                  a href={ "/sweep?id=" (s.id.0) } { "Coverage" }
                }
              }
            }
          }
        }
      }
    }
  };

  Ok(
    Html(
      layout::page(&identity, "Sweeps", Section::Sweeps, content).into_string(),
    )
    .into_response(),
  )
}

/// The htmx-polled fragment. Once the run is over it returns markup with
/// no polling trigger, which is how the poll stops: htmx re-reads the
/// attributes of what it swapped in.
pub async fn progress(
  _identity: Identity,
  State(state): State<AppState>,
) -> Result<Response> {
  let run_state = state.sweeps.state();
  let latest = state.db.read(|r| r.sweeps(1))?;
  let body = progress_body(&run_state, latest.first());

  let mut response = Html(body.into_string()).into_response();
  if run_state != RunState::Running {
    // The run has finished, so the page behind the fragment is now
    // stale — the Runs table, the counts, the board. Reload once rather
    // than teaching every region to poll.
    response
      .headers_mut()
      .insert("hx-refresh", axum::http::HeaderValue::from_static("true"));
    state.sweeps.acknowledge_failure();
  }
  Ok(response)
}

fn progress_body(run_state: &RunState, latest: Option<&SweepRow>) -> Markup {
  html! {
    @match run_state {
      RunState::Running => {
        div class="banner banner-warn" {
          "A sweep is running."
          @if let Some(s) = latest {
            @if s.running() {
              " Sweep " (s.id) " started " (view::when(s.started_at)) "."
            }
          }
        }
        @if let Some(s) = latest {
          @if s.running() && !s.coverage.is_empty() {
            div class="panel" { (coverage_table(&s.coverage)) }
          }
        }
      },
      RunState::Failed(why) => {
        div class="banner banner-err" {
          "The last sweep could not run: " (why)
        }
      },
      RunState::Idle => {},
    }
  }
}

#[derive(Debug, Deserialize)]
pub struct DetailQuery {
  pub id: i64,
}

/// One run's coverage.
pub async fn detail(
  identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<DetailQuery>,
) -> Result<Response> {
  let id = SweepId(query.id);
  let sweep = state
    .db
    .read(|r| r.sweep(id))?
    .ok_or_else(|| WebError::not_found(format!("sweep {id}")))?;

  let content = html! {
    (layout::head(
      &format!("Sweep {id}"),
      "What each system actually reported.",
      html! { a class="btn" href="/sweeps" { "All sweeps" } },
    ))

    div class="stats" {
      (view::stat(sweep.status.as_str(), "status"))
      (view::stat(sweep.coverage.len(), "systems"))
      (view::stat(sweep.facts, "facts appended"))
      (view::stat(
        sweep.coverage.iter().map(|c| c.tombstoned).sum::<i64>(),
        "entities marked absent",
      ))
    }

    div class="panel" {
      div class="panel-body" {
        div class="row" {
          div { div class="k muted" { "Started" } (view::when(sweep.started_at)) }
          div {
            div class="k muted" { "Finished" }
            (view::when_opt(sweep.finished_at))
          }
          div {
            div class="k muted" { "Requested" }
            @if sweep.requested.is_empty() {
              "every configured system"
            } @else {
              (sweep.requested.iter().map(ToString::to_string)
                 .collect::<Vec<_>>().join(", "))
            }
          }
        }
      }
    }

    h2 { "Coverage" }
    div class="panel" { (coverage_table(&sweep.coverage)) }
  };

  Ok(
    Html(
      layout::page(&identity, &format!("Sweep {id}"), Section::Sweeps, content)
        .into_string(),
    )
    .into_response(),
  )
}

fn coverage_table(coverage: &[CoverageRow]) -> Markup {
  html! {
    @if coverage.is_empty() {
      (layout::empty("This sweep has not reported on any system yet."))
    } @else {
      table {
        thead {
          tr {
            th { "System" }
            th class="shrink" { "Status" }
            th class="shrink" { "Snapshot" }
            th class="shrink num" { "Entities" }
            th class="shrink num" { "Delta" }
            th class="shrink num" { "Absent" }
            th class="shrink num" { "Took" }
            th { "Notes" }
          }
        }
        tbody {
          @for c in coverage {
            tr {
              td {
                (c.system.as_str())
                " " span class="tag" { (c.system_kind.as_str()) }
              }
              td class="shrink" {
                @if c.status == overlord_store::SystemStatus::Ok {
                  span class="tag tag-ok" { (c.status.as_str()) }
                } @else {
                  span class="tag tag-warn" { (c.status.as_str()) }
                }
              }
              td class="shrink" {
                @if c.complete {
                  span class="tag" title="A full enumeration, so absences \
                                          may become tombstones" {
                    "complete"
                  }
                } @else {
                  span class="tag tag-warn"
                       title="Partial: no tombstones may be written from it" {
                    "partial"
                  }
                }
              }
              td class="shrink num" { (c.observed_count) }
              td class="shrink num" {
                @match c.delta() {
                  Some(d) if d != 0 => span class=(if d < 0 {
                    "tag tag-warn"
                  } else {
                    "tag"
                  }) { (format!("{d:+}")) },
                  Some(_) => span class="muted" { "0" },
                  None => span class="muted" { "new" },
                }
              }
              td class="shrink num" { (c.tombstoned) }
              td class="shrink num soft" { (c.duration_ms) "ms" }
              td {
                @if c.guard_tripped {
                  div class="tag tag-warn"
                      title="The snapshot would have tombstoned more than \
                             the configured share of this system's \
                             entities, so none were written" {
                    "absence guard tripped — confirm this system"
                  }
                }
                @if let Some(e) = &c.error {
                  div class="soft" { (e) }
                }
              }
            }
          }
        }
      }
    }
  }
}

fn coverage_summary(coverage: &[CoverageRow]) -> Markup {
  html! {
    @for c in coverage {
      span class=(if c.status == overlord_store::SystemStatus::Ok
                     && !c.guard_tripped {
        "tag"
      } else {
        "tag tag-warn"
      }) {
        (c.system.as_str()) " " (c.observed_count)
        @if let Some(d) = c.delta() {
          @if d != 0 { " (" (format!("{d:+}")) ")" }
        }
      }
    }
  }
}

fn status_tag(s: &SweepRow) -> Markup {
  html! {
    @if s.status == overlord_store::SweepStatus::Ok {
      span class="tag tag-ok" { (s.status.as_str()) }
    } @else {
      span class="tag tag-warn" { (s.status.as_str()) }
    }
  }
}
