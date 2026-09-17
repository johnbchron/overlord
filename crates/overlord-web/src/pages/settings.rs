//! Settings (SPEC.md section 5).
//!
//! Read-only in v1. Configuration and secrets come from a file and the
//! environment (SPEC.md section 14), so this screen exists to let an
//! operator confirm what the running process actually loaded rather than
//! to change it — editing here would put a third source of truth beside
//! the file and the streams.

use axum::{
  extract::State,
  response::{Html, IntoResponse, Response},
};
use maud::html;

use crate::{
  AppState,
  auth::{AuthMode, Identity},
  error::Result,
  layout::{self, Section},
};

pub async fn show(
  identity: Identity,
  State(state): State<AppState>,
) -> Result<Response> {
  let (rulesets, counts) = state.db.read(|r| -> Result<_> {
    Ok((r.normalization_rulesets()?, r.counts()?))
  })?;

  let content = html! {
    (layout::head(
      "Settings",
      "What this process loaded. Configuration lives in a file and the \
       environment; checks and normalization live in the command stream.",
      html! {},
    ))

    h2 { "Authentication" }
    div class="panel" {
      div class="panel-body" {
        @match &state.auth {
          AuthMode::Dev { actor } => {
            div class="banner banner-warn" {
              "Running with --dev-actor " code { (actor) }
              ". This authenticates nobody, and will not serve a \
               non-loopback address."
            }
          },
          AuthMode::Oidc(cfg) => {
            table {
              tbody {
                tr { td { "Issuer" } td class="ref" { (cfg.issuer) } }
                tr { td { "Client id" } td class="ref" { (cfg.client_id) } }
                tr {
                  td { "Redirect" }
                  td class="ref" { (cfg.redirect_url) }
                }
                tr {
                  td { "Allowed subjects" }
                  td {
                    @if cfg.allowed_subjects.is_empty() {
                      span class="muted" { "none" }
                    } @else {
                      (cfg.allowed_subjects.iter().cloned()
                         .collect::<Vec<_>>().join(", "))
                    }
                  }
                }
                tr {
                  td { "Required group" }
                  td {
                    @match &cfg.required_group {
                      Some(g) => span class="ref" { (g) },
                      None => span class="muted" { "none" },
                    }
                  }
                }
              }
            }
          },
        }
      }
    }

    h2 { "Store" }
    div class="panel" {
      div class="panel-body" {
        table {
          tbody {
            tr {
              td { "Configuration file" }
              td class="ref" { (state.config_path) }
            }
            tr { td { "Entities" } td { (counts.entities) } }
            tr {
              td { "Confirmed persons" }
              td {
                (counts.persons)
                " " span class="muted" {
                  "(unlinked accounts are implicit persons)"
                }
              }
            }
            tr { td { "Checks" } td { (counts.checks) } }
          }
        }
      }
    }

    h2 { "Connectors" }
    div class="panel" {
      @if state.sweeps.configured().is_empty() {
        (layout::empty("No systems are configured."))
      } @else {
        table {
          thead {
            tr { th { "System" } th { "Connector" } th { "Settings" } }
          }
          tbody {
            @for c in state.sweeps.configured() {
              tr {
                td class="ref" { (c.id.as_str()) }
                td { span class="tag" { (c.connector) } }
                td class="evidence" {
                  @if c.config.is_null() {
                    span class="muted" { "—" }
                  } @else {
                    (c.config.to_string())
                  }
                }
              }
            }
          }
        }
      }
    }

    h2 { "Normalization" }
    p class="lede" {
      "Every ruleset revision is a command. Each fact records the version \
       that produced its overlay, so a rebuild reproduces exactly what was \
       read at the time."
    }
    div class="panel" {
      @if rulesets.is_empty() {
        (layout::empty(
          "No ruleset revisions recorded; connectors are using their \
           shipped defaults.",
        ))
      } @else {
        table {
          thead {
            tr { th { "Ruleset" } th { "Version" } th { "System kind" } }
          }
          tbody {
            @for (id, version, kind) in &rulesets {
              tr {
                td class="ref" { (id) }
                td class="ref" { (version) }
                td { span class="tag" { (kind.as_str()) } }
              }
            }
          }
        }
      }
    }
  };

  Ok(
    Html(
      layout::page(&identity, "Settings", Section::Settings, content)
        .into_string(),
    )
    .into_response(),
  )
}
