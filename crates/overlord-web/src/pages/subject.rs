//! Person and entity detail (SPEC.md section 5).
//!
//! One page answers "what is this thing, what is wrong with it, and what
//! has it looked like over time": identities and links, every violation
//! with its history, and the fact timeline.
//!
//! Identity *actions* — confirm, link, unlink, merge, split — are M3.
//! Pending suggestions are shown here from M2 because the board already
//! marks subjects ambiguous, and an operator sent here by that flag
//! needs to see what overlord proposed even before they can act on it.

use axum::{
  extract::{Query, State},
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::{EntityRef, PersonUid, SubjectRef};
use overlord_store::{ViolationFilter, ViolationRow};
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::{Result, WebError},
  layout::{self, Section},
  view,
};

#[derive(Debug, Deserialize)]
pub struct PersonQuery {
  pub uid: String,
}

pub async fn person(
  identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<PersonQuery>,
) -> Result<Response> {
  let uid = PersonUid::new(query.uid.clone());

  let (detail, violations) = state.db.read(|r| -> Result<_> {
    let detail = r.person_detail(&uid)?;
    let subject = SubjectRef::Person(r.resolve_person(&uid)?);
    let violations = r.violations_where(&ViolationFilter {
      states: crate::pages::subject::ALL_STATES.to_vec(),
      subject: Some(subject),
      limit: 500,
      ..ViolationFilter::default()
    })?;
    Ok((detail, violations))
  })?;

  let Some(detail) = detail else {
    return Err(WebError::not_found(format!("person {uid}")));
  };

  // A person's own score already folds in their entities' violations
  // (SPEC.md section 8), but the *list* has to gather both: an
  // entity-scoped violation is recorded against the entity's ref, not
  // the person's, and it is still this person's problem.
  let mut entity_violations = Vec::new();
  for entity in &detail.entities {
    let rows = state.db.read(|r| {
      r.violations_where(&ViolationFilter {
        states: ALL_STATES.to_vec(),
        subject: Some(SubjectRef::Entity(entity.clone())),
        limit: 500,
        ..ViolationFilter::default()
      })
    })?;
    entity_violations.extend(rows);
  }

  let title = detail.display_name.clone().unwrap_or_else(|| {
    view::subject_label(&SubjectRef::Person(detail.person_uid.clone()), None)
  });

  let content = html! {
    (layout::head(&title, "", html! {
      a class="btn" href="/users" { "All users" }
    }))

    div class="stats" {
      (view::stat(detail.score, "risk score"))
      (view::stat(detail.violations, "active violations"))
      (view::stat(detail.entities.len(), "linked accounts"))
    }

    @if detail.implicit {
      div class="banner banner-warn" {
        "This is an unlinked account, evaluated as a person in its own \
         right so cross-system checks work before linking is complete. \
         Confirming a link will merge this score into the surviving \
         person rather than adding to the organization's total."
      }
    }

    h2 { "Accounts" }
    div class="panel" {
      @if detail.entities.is_empty() {
        (layout::empty("No linked accounts."))
      } @else {
        table {
          thead {
            tr {
              th { "Account" }
              th class="shrink" { "System" }
              th class="shrink" { "Primary for" }
            }
          }
          tbody {
            @for e in &detail.entities {
              tr {
                td {
                  a class="ref" href=(view::subject_href(
                    &SubjectRef::Entity(e.clone())
                  )) { (e.entity_key.as_str()) }
                }
                td class="shrink" { span class="tag" { (e.system.as_str()) } }
                td class="shrink" {
                  @for (kind, primary) in &detail.primaries {
                    @if primary == e {
                      span class="tag tag-ok" { (kind.as_str()) }
                    }
                  }
                }
              }
            }
          }
        }
      }
    }

    (violation_panel(
      "Violations",
      &violations,
      "Nothing is recorded against this person.",
    ))

    @if !entity_violations.is_empty() {
      (violation_panel(
        "Violations on their accounts",
        &entity_violations,
        "",
      ))
    }
  };

  Ok(
    Html(layout::page(&identity, &title, Section::None, content).into_string())
      .into_response(),
  )
}

#[derive(Debug, Deserialize)]
pub struct EntityQuery {
  #[serde(rename = "ref")]
  pub entity_ref: String,
}

pub async fn entity(
  identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<EntityQuery>,
) -> Result<Response> {
  let entity: EntityRef = query.entity_ref.parse()?;

  let (detail, facts, violations, suggestions) =
    state.db.read(|r| -> Result<_> {
      Ok((
        r.entity_detail(&entity)?,
        r.entity_facts(&entity, 50)?,
        r.violations_where(&ViolationFilter {
          states: ALL_STATES.to_vec(),
          subject: Some(SubjectRef::Entity(entity.clone())),
          limit: 500,
          ..ViolationFilter::default()
        })?,
        r.suggestions_for(&entity)?,
      ))
    })?;

  let Some(detail) = detail else {
    return Err(WebError::not_found(format!("entity {entity}")));
  };

  let title = detail
    .normalized
    .as_ref()
    .and_then(|n| n.display_name.clone())
    .unwrap_or_else(|| entity.entity_key.to_string());

  let content = html! {
    (layout::head(&title, "", html! {
      @if let Some(uid) = &detail.person {
        a class="btn" href={ "/person?uid=" (view::urlencode(uid.as_str())) } {
          "The person"
        }
      }
    }))

    @if !detail.present {
      div class="banner banner-warn" {
        "This account is absent: its latest fact is a tombstone, so it is \
         excluded from evaluation and its violations have resolved."
      }
    }

    div class="panel" {
      div class="panel-body" {
        div class="row" {
          div { div class="k muted" { "System" } (entity.system.as_str()) }
          div { div class="k muted" { "Type" } (entity.entity_type.as_str()) }
          div { div class="k muted" { "Key" }
                span class="ref" { (entity.entity_key.as_str()) } }
          div { div class="k muted" { "Status" }
                span class="tag" { (detail.status.as_str()) } }
          div { div class="k muted" { "First seen" }
                "sweep " (detail.first_seen) }
          div { div class="k muted" { "Last seen" }
                "sweep " (detail.last_seen) }
        }
      }
    }

    @if !suggestions.is_empty() {
      h2 { "Suggested links" }
      p class="lede" {
        "Machine-proposed and never applied automatically. Confirming them \
         arrives with identity work."
      }
      div class="panel" {
        table {
          thead {
            tr { th { "Person" } th class="shrink" { "Signal" } th { "Why" } }
          }
          tbody {
            @for (uid, signal, evidence) in &suggestions {
              tr {
                td {
                  a class="ref"
                    href={ "/person?uid=" (view::urlencode(uid.as_str())) } {
                    (uid.as_str())
                  }
                }
                td class="shrink" { span class="tag" { (signal) } }
                td class="evidence" { (evidence.to_string()) }
              }
            }
          }
        }
      }
    }

    (violation_panel(
      "Violations",
      &violations,
      "Nothing is open against this account.",
    ))

    h2 { "Normalized overlay" }
    p class="lede" {
      "What checks actually evaluate over. The stored overlay is \
       authoritative: changing normalization affects future sweeps only."
    }
    div class="panel" {
      @match &detail.normalized {
        Some(n) => table {
          thead { tr { th { "Field" } th { "Value" } } }
          tbody {
            @for name in n.field_names() {
              tr {
                td class="ref" { (name) }
                td { (n.get(name).to_string()) }
              }
            }
          }
        },
        None => (layout::empty(
          "No overlay: the latest fact for this entity is a tombstone.",
        )),
      }
    }

    h2 { "Raw payload" }
    details {
      summary { "Show the vendor payload as received" }
      pre style="margin-top:0.6rem" {
        (serde_json::to_string_pretty(&detail.raw)
          .unwrap_or_else(|_| "unreadable".to_owned()))
      }
    }

    h2 { "Fact timeline" }
    div class="panel" {
      @if facts.is_empty() {
        (layout::empty("No facts."))
      } @else {
        table {
          thead {
            tr {
              th class="shrink" { "Sweep" }
              th class="shrink" { "Observed" }
              th class="shrink" { "Present" }
              th class="shrink" { "Changed" }
              th { "Normalization" }
            }
          }
          tbody {
            @for f in &facts {
              tr {
                td class="shrink" {
                  a href={ "/sweep?id=" (f.sweep.0) } { (f.sweep) }
                }
                td class="shrink" { (view::when(f.observed_at)) }
                td class="shrink" {
                  @if f.present {
                    span class="tag tag-ok" { "present" }
                  } @else {
                    span class="tag tag-warn" { "tombstone" }
                  }
                }
                td class="shrink" {
                  @if f.changed {
                    span class="tag" { "changed" }
                  } @else {
                    span class="muted" { "—" }
                  }
                }
                td class="ref soft" { (f.norm_version) }
              }
            }
          }
        }
      }
    }
  };

  Ok(
    Html(layout::page(&identity, &title, Section::None, content).into_string())
      .into_response(),
  )
}

pub(crate) const ALL_STATES: [overlord_core::ViolationState; 5] = [
  overlord_core::ViolationState::Open,
  overlord_core::ViolationState::Acknowledged,
  overlord_core::ViolationState::Suppressed,
  overlord_core::ViolationState::FalsePositive,
  overlord_core::ViolationState::Resolved,
];

fn violation_panel(
  title: &str,
  rows: &[ViolationRow],
  empty_message: &str,
) -> Markup {
  html! {
    h2 { (title) }
    div class="panel" {
      @if rows.is_empty() {
        (layout::empty(empty_message))
      } @else {
        table {
          thead {
            tr {
              th class="shrink" { "Severity" }
              th { "Check" }
              th class="shrink" { "State" }
              th class="shrink" { "Opened" }
              th class="shrink" { "" }
            }
          }
          tbody {
            @for v in rows {
              tr {
                td class="shrink" { (view::severity(v.severity)) }
                td {
                  (v.check_name)
                  (view::evidence(&v.evidence))
                }
                td class="shrink" {
                  (view::state(v.state))
                  (view::flags(v.stale, v.ambiguous, v.overlay_stale))
                }
                td class="shrink" { (view::when(v.opened_at)) }
                td class="shrink" {
                  a href={ "/violation?check="
                           (view::urlencode(v.check_id.as_str()))
                           "&subject="
                           (view::urlencode(&v.subject.to_string())) } {
                    "History"
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
