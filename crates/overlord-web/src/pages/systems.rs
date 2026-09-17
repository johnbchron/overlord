//! Systems (SPEC.md section 5).
//!
//! Deliberately lightweight: connected systems, whether their last sweep
//! worked, and how many entities they hold. Explicitly not an asset
//! inventory (SPEC.md section 15).

use axum::{
  extract::State,
  response::{Html, IntoResponse, Response},
};
use maud::html;

use crate::{
  AppState,
  auth::Identity,
  error::Result,
  layout::{self, Section},
  view,
};

/// One configured system, its connector, and every endpoint that
/// connector may reach with the reason it asked for it.
type Reach = (String, String, Vec<(String, &'static str)>);

pub async fn list(
  identity: Identity,
  State(state): State<AppState>,
) -> Result<Response> {
  let rows = state.db.read(|r| r.systems())?;
  let configured = state.sweeps.configured();

  // A configured system that has never been swept has no row in the
  // store at all, and that is exactly the case worth surfacing: a
  // connector added to the configuration and never run looks identical
  // to no connector.
  let never_swept: Vec<&overlord_engine::SystemConfig> = configured
    .iter()
    .filter(|c| !rows.iter().any(|r| r.system == c.id))
    .collect();

  // What each configured system's connector is permitted to reach. The
  // point of showing it is the decision it informs: an operator about to
  // grant a vendor scope should be able to see, without reading Rust,
  // that the grant is wider than the use.
  let registry = state.sweeps.registry();
  let reach: Vec<Reach> = configured
    .iter()
    .map(|c| {
      let allowed = registry.get(&c.connector).map_or_else(Vec::new, |k| {
        k.allowlist()
          .into_iter()
          .map(|a| (a.to_string(), a.reason))
          .collect()
      });
      (c.id.to_string(), c.connector.clone(), allowed)
    })
    .collect();

  let content = html! {
    (layout::head(
      "Systems",
      "Connected, read-only sources. overlord never writes to any of them.",
      html! { a class="btn primary" href="/sweeps" { "Run a sweep" } },
    ))

    @if !never_swept.is_empty() {
      div class="banner banner-warn" {
        "Configured but never swept: "
        (never_swept.iter().map(|c| c.id.to_string())
           .collect::<Vec<_>>().join(", "))
        ". Nothing is known about them yet."
      }
    }

    div class="panel" {
      @if rows.is_empty() {
        (layout::empty(
          "No system has been swept yet.",
        ))
      } @else {
        table {
          thead {
            tr {
              th { "System" }
              th class="shrink" { "Kind" }
              th class="shrink num" { "Entities" }
              th class="shrink" { "Last sweep" }
              th class="shrink" { "Last status" }
              th class="shrink" { "Last success" }
              th { "Notes" }
            }
          }
          tbody {
            @for s in &rows {
              tr {
                td { b { (s.system.as_str()) } }
                td class="shrink" {
                  span class="tag" { (s.system_kind.as_str()) }
                }
                td class="shrink num" { (s.entities) }
                td class="shrink" {
                  @match s.last_sweep {
                    Some(id) => a href={ "/sweep?id=" (id.0) } { (id) },
                    None => span class="muted" { "never" },
                  }
                }
                td class="shrink" {
                  @match s.last_status {
                    Some(overlord_store::SystemStatus::Ok) =>
                      span class="tag tag-ok" { "ok" },
                    Some(other) =>
                      span class="tag tag-warn" { (other.as_str()) },
                    None => span class="muted" { "—" },
                  }
                }
                td class="shrink" { (view::when_opt(s.last_ok_at)) }
                td {
                  @if s.guard_recent {
                    div class="tag tag-warn" {
                      "absence guard tripped on the last run"
                    }
                  }
                  @if let Some(e) = &s.last_error {
                    div class="soft" { (e) }
                  }
                  @if s.last_ok_at.is_none() && s.last_sweep.is_some() {
                    div class="tag tag-warn" {
                      "this connector has never succeeded"
                    }
                  }
                }
              }
            }
          }
        }
      }
    }

    h2 { "What each connector can reach" }
    p class="lede" {
      "Every request a connector makes is matched against this list \
       before it is sent, and anything unlisted fails closed without a \
       network call. There is no way to name a mutating HTTP method."
    }

    @for (system, connector, allowed) in &reach {
      div class="panel" {
        div class="panel-body" {
          div class="row" {
            span class="k" { "System" }
            b { (system) }
            span class="k" { "Connector" }
            span class="tag" { (connector) }
          }
        }
        @if allowed.is_empty() {
          (layout::empty("This connector reaches nothing at all."))
        } @else {
          table {
            thead {
              tr { th { "Endpoint" } th { "Why" } }
            }
            tbody {
              @for (endpoint, reason) in allowed {
                tr {
                  td { code { (endpoint) } }
                  td class="soft" { (reason) }
                }
              }
            }
          }
        }
      }
    }

    h2 { "Credentials" }
    p class="lede" {
      "Connector credentials come from the environment, never from the \
       configuration file and never from the streams. overlord does not \
       display them."
    }
  };

  Ok(
    Html(
      layout::page(&identity, "Systems", Section::Systems, content)
        .into_string(),
    )
    .into_response(),
  )
}
