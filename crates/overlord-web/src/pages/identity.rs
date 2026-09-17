//! Identity: the queue of proposed links, and the picker every manual
//! link uses (SPEC.md section 12).
//!
//! The screen is deliberately a *queue* rather than a graph. Each row is
//! one account, one proposed person, and the one signal that connects
//! them, because that is the unit an operator decides on — and because a
//! proposal that cannot be stated in one line is not conservative enough
//! to be shown at all.
//!
//! Nothing here applies anything. Every button is a form post that
//! appends a command, exactly like the violation verbs.

use axum::{
  extract::{Query, State},
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::{EntityRef, PersonUid, SubjectRef};
use overlord_store::Suggestion;
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::Result,
  layout::{self, Section},
  pages::violations::new_key,
  view,
};

pub async fn queue(
  identity: Identity,
  State(state): State<AppState>,
) -> Result<Response> {
  let (suggestions, unlinked, people) = state.db.read(|r| -> Result<_> {
    Ok((
      r.pending_suggestions(500)?,
      r.pending_suggestion_count()?,
      r.counts()?,
    ))
  })?;

  let content = html! {
    (layout::head(
      "Identity",
      "Proposed links, computed fresh each sweep from conservative \
       signals. They are never applied automatically: confirming one is \
       an operator command like any other.",
      html! {},
    ))

    div class="stats" {
      (view::stat(unlinked, "accounts with a proposal"))
      (view::stat(suggestions.len(), "proposals"))
      (view::stat(people.persons, "confirmed persons"))
    }

    h2 { "Proposed links" }
    div class="panel" {
      @if suggestions.is_empty() {
        (layout::empty(
          "Nothing to review. Either every account overlord can match is \
           linked, or no two systems agree about anybody — run a sweep \
           to recompute.",
        ))
      } @else {
        table {
          thead {
            tr {
              th { "Account" }
              th { "Proposed person" }
              th class="shrink" { "Signal" }
              th { "Why" }
              th class="shrink right" { "" }
            }
          }
          tbody {
            @for s in &suggestions { (row(s)) }
          }
        }
      }
    }
  };

  Ok(
    Html(
      layout::page(&identity, "Identity", Section::Identity, content)
        .into_string(),
    )
    .into_response(),
  )
}

fn row(s: &Suggestion) -> Markup {
  let person = SubjectRef::Person(s.person_uid.clone());
  html! {
    tr {
      td {
        (view::subject(&SubjectRef::Entity(s.entity.clone()), None))
        div class="muted soft" { (s.entity.system.as_str()) }
      }
      td {
        (view::subject(&person, None))
        // An implicit person is another unlinked account, so confirming
        // creates the person rather than joining one. Saying so here is
        // the difference between a button an operator trusts and one
        // they click carefully.
        @if s.person_uid.is_implicit() {
          div class="muted soft" { "unlinked; confirming creates a person" }
        }
      }
      td class="shrink" { span class="tag" { (s.signal) } }
      td class="evidence" { (explain(s)) }
      td class="shrink right" {
        form method="post" action="/identity/link" class="inline-form" {
          input type="hidden" name="entity" value=(s.entity.to_string());
          input type="hidden" name="person" value=(s.person_uid.as_str());
          input type="hidden" name="signal" value=(s.signal);
          input type="hidden" name="back" value="/identity";
          input type="hidden" name="idempotency_key" value=(new_key());
          button class="btn" { "Confirm" }
        }
      }
    }
  }
}

/// The evidence, in the words an operator would use to check it.
#[must_use]
pub fn explain(s: &Suggestion) -> String {
  let field = s.evidence.get("field").and_then(|v| v.as_str());
  let value = s.evidence.get("value").and_then(|v| v.as_str());
  let matched = s.evidence.get("matched_field").and_then(|v| v.as_str());
  match (field, value) {
    (Some(field), Some(value)) => {
      let where_ = match matched {
        Some(other) if other != field => format!("{field} / {other}"),
        _ => field.to_owned(),
      };
      format!("{where_} = {value}")
    }
    _ => s.evidence.to_string(),
  }
}

// --- the picker --------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PickerQuery {
  #[serde(default)]
  pub q:      String,
  /// `link` offers each hit as somebody to attach `subject` to; `merge`
  /// offers each as a uid to retire into `subject`.
  #[serde(default)]
  pub mode:   String,
  /// The entity being linked, or the person being merged into.
  #[serde(default)]
  pub anchor: String,
}

/// The candidate list behind every manual link and every merge.
///
/// A person uid is a ULID, which nobody is going to type, so choosing a
/// person is always a search. One fragment serves both verbs because
/// the only thing that differs is the button.
pub async fn picker(
  _identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<PickerQuery>,
) -> Result<Response> {
  if query.q.trim().is_empty() {
    return Ok(
      Html(
        layout::empty("Type a name, an email, or an account key.")
          .into_string(),
      )
      .into_response(),
    );
  }

  let hits = state.db.read(|r| r.search_subjects(query.q.trim(), 20))?;
  let merging = query.mode == "merge";

  let markup = html! {
    div class="panel" {
      @if hits.is_empty() {
        (layout::empty("Nothing matches."))
      } @else {
        table {
          tbody {
            @for hit in &hits {
              @if let Some(uid) = candidate_uid(&hit.subject, merging) {
                tr {
                  td {
                    (view::subject(&hit.subject, hit.display_name.as_deref()))
                    div class="muted soft" { (hit.detail) }
                  }
                  td class="shrink right" {
                    (button(&query, &uid, merging))
                  }
                }
              }
            }
          }
        }
      }
    }
  };
  Ok(Html(markup.into_string()).into_response())
}

/// The uid a hit offers, or `None` if it cannot play this role.
///
/// A merge joins two *confirmed* persons: an unlinked account is
/// promoted by linking it, not by merging it (SPEC.md section 12), so it
/// is left out of that list rather than offered and then refused.
fn candidate_uid(subject: &SubjectRef, merging: bool) -> Option<PersonUid> {
  match subject {
    SubjectRef::Person(uid) if uid.is_implicit() && merging => None,
    SubjectRef::Person(uid) => Some(uid.clone()),
    // An account stands for its own implicit person, which is what
    // linking to "that account over there" means — but an unlinked
    // account is exactly what a merge may not take.
    SubjectRef::Entity(_) if merging => None,
    SubjectRef::Entity(e) => Some(PersonUid::implicit(e)),
  }
}

fn button(query: &PickerQuery, uid: &PersonUid, merging: bool) -> Markup {
  html! {
    form method="post"
         action=(if merging { "/identity/merge" } else { "/identity/link" })
         class="inline-form" {
      input type="hidden" name="idempotency_key" value=(new_key());
      @if merging {
        input type="hidden" name="surviving" value=(query.anchor);
        input type="hidden" name="retired" value=(uid.as_str());
        input type="hidden" name="back"
              value={ "/person?uid=" (view::urlencode(&query.anchor)) };
        button class="btn" { "Merge into this person" }
      } @else {
        input type="hidden" name="entity" value=(query.anchor);
        input type="hidden" name="person" value=(uid.as_str());
        input type="hidden" name="back"
              value={ "/entity?ref=" (view::urlencode(&query.anchor)) };
        button class="btn" { "Link" }
      }
    }
  }
}

/// The search box that drives [`picker`], and the region it swaps into.
#[must_use]
pub fn picker_form(mode: &str, anchor: &str, label: &str) -> Markup {
  let target = format!("picker-{mode}");
  html! {
    form class="filters"
         hx-get="/identity/candidates"
         hx-target={ "#" (target) }
         hx-trigger="change, search, keyup changed delay:300ms from:find input[name='q']" {
      input type="hidden" name="mode" value=(mode);
      input type="hidden" name="anchor" value=(anchor);
      label class="field" style="flex:2" {
        span { (label) }
        input type="search" name="q" placeholder="name, email or key";
      }
      noscript { button type="submit" { "Search" } }
    }
    div id=(target) {}
  }
}

/// A one-click form: create a person for this account on its own.
#[must_use]
pub fn new_person_form(entity: &EntityRef) -> Markup {
  html! {
    form method="post" action="/identity/link" class="row" {
      input type="hidden" name="entity" value=(entity.to_string());
      input type="hidden" name="person" value="";
      input type="hidden" name="back"
            value={ "/entity?ref=" (view::urlencode(&entity.to_string())) };
      input type="hidden" name="idempotency_key" value=(new_key());
      label class="field" style="flex:2" {
        span { "Or create a person for this account" }
        input type="text" name="display_name" placeholder="Their name";
      }
      div class="field" { span { "\u{00a0}" } button class="btn" { "Create" } }
    }
  }
}
