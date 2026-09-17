//! The Violations board: the default view (SPEC.md section 5).
//!
//! Ranked worst first, then longest-ignored — the store's `ORDER BY`
//! already encodes SPEC.md section 8, so nothing here re-sorts. What
//! this module decides is presentation: which rows lead, which are
//! folded away, and what an operator can do to a row without leaving the
//! page.

use axum::{
  extract::{Query, State},
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::{CheckId, Severity, SubjectRef, SystemId, ViolationState};
use overlord_store::{ViolationFilter, ViolationRow};
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::{Result, WebError},
  layout::{self, Section},
  view,
};

/// The facets the board is narrowed by, as they travel in the URL.
///
/// One value per facet rather than a multi-select: repeated query keys
/// do not round-trip through `serde_urlencoded`, and more importantly a
/// single choice per facet is what an operator working a queue actually
/// reaches for. An absent or empty value means "all".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BoardQuery {
  #[serde(default)]
  pub severity: String,
  #[serde(default)]
  pub system:   String,
  #[serde(default)]
  pub check:    String,
  #[serde(default)]
  pub state:    String,
  /// Free text matched against the subject ref.
  #[serde(default)]
  pub q:        String,
}

impl BoardQuery {
  fn to_filter(&self) -> Result<ViolationFilter> {
    let states = match self.state.as_str() {
      "" => vec![ViolationState::Open, ViolationState::Acknowledged],
      // "Everything" has to include `resolved`, which is how an
      // operator confirms something really did clear.
      "all" => vec![
        ViolationState::Open,
        ViolationState::Acknowledged,
        ViolationState::Suppressed,
        ViolationState::FalsePositive,
        ViolationState::Resolved,
      ],
      one => vec![one.parse::<ViolationState>()?],
    };

    Ok(ViolationFilter {
      states,
      severities: if self.severity.is_empty() {
        Vec::new()
      } else {
        vec![self.severity.parse::<Severity>()?]
      },
      systems: if self.system.is_empty() {
        Vec::new()
      } else {
        vec![SystemId::new(self.system.clone())]
      },
      checks: if self.check.is_empty() {
        Vec::new()
      } else {
        vec![CheckId::new(self.check.clone())]
      },
      subject: None,
      subject_like: (!self.q.trim().is_empty())
        .then(|| self.q.trim().to_owned()),
      limit: 500,
    })
  }

  /// Whether the operator has narrowed anything, which decides whether
  /// an empty board reads as "nothing is wrong" or "nothing matched".
  fn is_narrowed(&self) -> bool {
    !(self.severity.is_empty()
      && self.system.is_empty()
      && self.check.is_empty()
      && self.state.is_empty()
      && self.q.trim().is_empty())
  }

  fn query_string(&self) -> String {
    let mut parts = Vec::new();
    for (k, v) in [
      ("severity", &self.severity),
      ("system", &self.system),
      ("check", &self.check),
      ("state", &self.state),
      ("q", &self.q),
    ] {
      if !v.is_empty() {
        parts.push(format!("{k}={}", view::urlencode(v)));
      }
    }
    parts.join("&")
  }
}

/// The full page.
pub async fn board(
  identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<BoardQuery>,
) -> Result<Response> {
  let filter = query.to_filter()?;
  let (rows, checks, systems, counts) = state.db.read(|r| -> Result<_> {
    Ok((
      r.violations_where(&filter)?,
      r.checks()?,
      r.known_systems()?,
      r.counts()?,
    ))
  })?;

  let content = html! {
    (layout::head(
      "Violations",
      "Every standing failure, worst first, then longest ignored.",
      html! {
        a class="btn" href="/sweeps" { "Sweeps" }
        a class="btn primary" href="/rules" { "Rules" }
      },
    ))

    div class="stats" {
      (view::stat(counts.violations, "open or acknowledged"))
      (view::stat(counts.entities, "entities observed"))
      (view::stat(counts.checks, "checks"))
      (view::stat(
        rows.iter().filter(|v| v.new_since).count(),
        "new since last sweep",
      ))
    }

    form class="filters"
         hx-get="/violations/rows"
         hx-target="#board"
         hx-push-url="true"
         hx-trigger="change, search, keyup changed delay:300ms from:find input[name='q']" {
      label class="field" {
        span { "Severity" }
        select name="severity" {
          option value="" selected[query.severity.is_empty()] { "All" }
          @for s in Severity::ALL {
            option value=(s.as_str()) selected[query.severity == s.as_str()] {
              (s.as_str())
            }
          }
        }
      }
      label class="field" {
        span { "State" }
        select name="state" {
          option value="" selected[query.state.is_empty()] {
            "Open & acknowledged"
          }
          @for s in [
            ViolationState::Open,
            ViolationState::Acknowledged,
            ViolationState::Suppressed,
            ViolationState::FalsePositive,
            ViolationState::Resolved,
          ] {
            option value=(s.as_str()) selected[query.state == s.as_str()] {
              (s.as_str())
            }
          }
          option value="all" selected[query.state == "all"] { "Everything" }
        }
      }
      label class="field" {
        span { "System" }
        select name="system" {
          option value="" selected[query.system.is_empty()] { "All" }
          @for (id, _) in &systems {
            option value=(id.as_str()) selected[query.system == id.as_str()] {
              (id.as_str())
            }
          }
        }
      }
      label class="field" {
        span { "Check" }
        select name="check" {
          option value="" selected[query.check.is_empty()] { "All" }
          @for c in &checks {
            option value=(c.draft.id.as_str())
                   selected[query.check == c.draft.id.as_str()] {
              (c.draft.name)
            }
          }
        }
      }
      label class="field" {
        span { "Subject" }
        input type="search" name="q" value=(query.q)
              placeholder="email, key or uid";
      }
      noscript { button type="submit" { "Filter" } }
    }

    div id="board" { (table(&rows, &query)) }
  };

  Ok(
    Html(
      layout::page(&identity, "Violations", Section::Violations, content)
        .into_string(),
    )
    .into_response(),
  )
}

/// The htmx fragment: just the board, re-rendered under new filters.
pub async fn rows(
  _identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<BoardQuery>,
) -> Result<Response> {
  let filter = query.to_filter()?;
  let rows = state.db.read(|r| r.violations_where(&filter))?;
  Ok(Html(table(&rows, &query).into_string()).into_response())
}

/// The board itself: the new section, the loud tiers, and the quiet ones
/// folded away.
fn table(rows: &[ViolationRow], query: &BoardQuery) -> Markup {
  let new_since: Vec<&ViolationRow> =
    rows.iter().filter(|v| v.new_since).collect();
  // SPEC.md section 5: `low` and `info` are collapsed by default. They
  // are detected, stored and counted — they simply do not compete for
  // the top of the screen (SPEC.md section 2).
  let (loud, quiet): (Vec<&ViolationRow>, Vec<&ViolationRow>) = rows
    .iter()
    .partition(|v| !v.severity.collapsed_by_default());

  html! {
    @if rows.is_empty() {
      div class="panel" {
        (layout::empty(if query.is_narrowed() {
          "Nothing matches those filters."
        } else {
          "Nothing is open. Run a sweep to check again."
        }))
      }
    } @else {
      @if !new_since.is_empty() {
        h2 {
          "New since the last sweep of each system ("
          (new_since.len()) ")"
        }
        p class="lede" {
          "Compared per system, so restricting a sweep does not \
           manufacture change."
        }
        div class="panel" { (rows_table(&new_since, query)) }
      }

      h2 { "All (" (rows.len()) ")" }
      div class="panel" {
        @if loud.is_empty() {
          (layout::empty("Nothing above low severity."))
        } @else {
          (rows_table(&loud, query))
        }
      }

      @if !quiet.is_empty() {
        details {
          summary { "Low and info (" (quiet.len()) ")" }
          div class="panel" style="margin-top:0.6rem" {
            (rows_table(&quiet, query))
          }
        }
      }
    }
  }
}

fn rows_table(rows: &[&ViolationRow], query: &BoardQuery) -> Markup {
  html! {
    table {
      thead {
        tr {
          th class="shrink" { "Severity" }
          th { "Check" }
          th { "Subject" }
          th class="shrink" { "State" }
          th class="shrink" { "Opened" }
          th class="shrink right" { "Actions" }
        }
      }
      tbody {
        @for v in rows { (row(v, query)) }
      }
    }
  }
}

/// One violation. Also returned on its own by the action handlers, so an
/// acknowledgement re-renders exactly this and nothing else.
#[must_use]
pub fn row(v: &ViolationRow, query: &BoardQuery) -> Markup {
  html! {
    tr id=(row_id(&v.check_id, &v.subject, v.episode)) {
      td class="shrink" { (view::severity(v.severity)) }
      td {
        a href={ "/rules/edit?id=" (view::urlencode(v.check_id.as_str())) } {
          (v.check_name)
        }
        @if v.episode > 1 {
          " "
          span class="tag" title="Reopened after it had resolved" {
            "episode " (v.episode)
          }
        }
        (view::evidence(&v.evidence))
      }
      td {
        (view::subject(&v.subject, None))
        div { (view::flags(v.stale, v.ambiguous, v.overlay_stale)) }
      }
      td class="shrink" { (view::state(v.state)) }
      td class="shrink" { (view::when(v.opened_at)) }
      td class="shrink right" { (actions(v, query)) }
    }
  }
}

/// The DOM id an action handler swaps its re-rendered row into.
#[must_use]
pub fn row_id(check: &CheckId, subject: &SubjectRef, episode: i64) -> String {
  // Neither a check id nor a subject ref is safe as an HTML id, so the
  // identity is hashed rather than spelled. The value only has to be
  // stable and unique within one rendered page.
  let hash = blake3::hash(format!("{check}|{subject}|{episode}").as_bytes());
  format!("v-{}", &hash.to_hex()[..16])
}

/// The overlay verbs available from a row, given its current state
/// (SPEC.md section 9).
fn actions(v: &ViolationRow, query: &BoardQuery) -> Markup {
  let target = format!("#{}", row_id(&v.check_id, &v.subject, v.episode));
  let common = html! {
    input type="hidden" name="check" value=(v.check_id.as_str());
    input type="hidden" name="subject" value=(v.subject.to_string());
    input type="hidden" name="episode" value=(v.episode.to_string());
    input type="hidden" name="back" value=(query.query_string());
    // Minted per rendered form, so a double submit is a no-op returning
    // the original command (SPEC.md section 6.2).
    input type="hidden" name="idempotency_key" value=(new_key());
  };

  html! {
    div class="row shrink" style="gap:0.25rem;justify-content:flex-end" {
      @if v.state == ViolationState::Open {
        form class="inline-form" hx-post="/violations/act"
             hx-target=(target) hx-swap="outerHTML" {
          (common)
          input type="hidden" name="verb" value="acknowledge";
          button class="linkish" title="Seen, not fixed" { "Ack" }
        }
      }
      @if matches!(
        v.state,
        ViolationState::Open | ViolationState::Acknowledged
      ) {
        a class="btn linkish"
          href={ "/violation?check=" (view::urlencode(v.check_id.as_str()))
                 "&subject=" (view::urlencode(&v.subject.to_string())) } {
          "Suppress…"
        }
      }
      @if v.state != ViolationState::FalsePositive
        && v.state != ViolationState::Resolved {
        form class="inline-form" hx-post="/violations/act"
             hx-target=(target) hx-swap="outerHTML" {
          (common)
          input type="hidden" name="verb" value="false_positive";
          button class="linkish" title="The rule is wrong, not the subject" {
            "False positive"
          }
        }
      }
      @if v.state.is_overlay() {
        form class="inline-form" hx-post="/violations/act"
             hx-target=(target) hx-swap="outerHTML" {
          (common)
          input type="hidden" name="verb" value="revoke";
          button class="linkish" title="Undo the overlay and recompute" {
            "Revoke"
          }
        }
      }
      a class="btn linkish"
        href={ "/violation?check=" (view::urlencode(v.check_id.as_str()))
               "&subject=" (view::urlencode(&v.subject.to_string())) } {
        "History"
      }
    }
  }
}

/// A fresh idempotency key for one rendered form.
#[must_use]
pub fn new_key() -> String { format!("web:{}", ulid::Ulid::new()) }

// --- one violation's history -------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DetailQuery {
  pub check:   String,
  pub subject: String,
}

/// Every episode of one `(check, subject)`, with the suppression form.
pub async fn detail(
  identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<DetailQuery>,
) -> Result<Response> {
  let check = CheckId::new(query.check.clone());
  let subject: SubjectRef = query.subject.parse()?;

  let (episodes, record) = state.db.read(|r| -> Result<_> {
    let episodes = r.violation_episodes(&check, &subject)?;
    let record = r.checks()?.into_iter().find(|c| c.draft.id == check);
    Ok((episodes, record))
  })?;

  if episodes.is_empty() {
    return Err(WebError::not_found(format!("{check} on {subject}")));
  }
  let current = &episodes[0];

  let content = html! {
    (layout::head(
      &record.as_ref().map_or_else(
        || check.to_string(),
        |r| r.draft.name.clone(),
      ),
      "",
      html! {
        a class="btn" href="/" { "Back to the board" }
      },
    ))

    div class="panel" {
      div class="panel-body stack" {
        div class="row" {
          div {
            div class="k muted" { "Subject" }
            (view::subject(&subject, None))
          }
          div {
            div class="k muted" { "Severity" }
            (view::severity(current.severity))
          }
          div {
            div class="k muted" { "State" }
            (view::state(current.state))
          }
          div {
            div class="k muted" { "Weight" }
            (current.weight)
          }
        }
        @if let Some(r) = &record {
          @if let Some(remediation) = &r.draft.remediation {
            div {
              div class="k muted" { "Remediation" }
              p { (remediation) }
            }
          }
          @if let Some(rationale) = &r.draft.rationale {
            div {
              div class="k muted" { "Why this matters" }
              p class="soft" { (rationale) }
            }
          }
          @if !r.draft.references.is_empty() {
            div {
              div class="k muted" { "References" }
              ul {
                @for reference in &r.draft.references {
                  li { (reference) }
                }
              }
            }
          }
        }
      }
    }

    h2 { "Record a decision" }
    (overlay_form(&check, &subject, current.episode))

    h2 { "History" }
    p class="lede" {
      "A regression opens a new episode rather than reviving the old one, \
       so an acknowledgement never silences a problem that came back."
    }
    @for e in &episodes {
      div class="panel" {
        div class="panel-body" {
          div class="row" {
            div { b { "Episode " (e.episode) } }
            div { (view::state(e.state)) }
            div { "opened " (view::when(e.opened_at)) }
            div { "sweep " (e.opened_sweep) }
            div { "rule revision " (e.revision_open) }
            div { (view::flags(e.stale, e.ambiguous, false)) }
          }
          @if let Some(reason) = &e.suppress_reason {
            p class="soft" {
              "Suppressed: " (reason)
              @if let Some(until) = e.suppress_until {
                " until " (view::when(until))
              }
            }
          }
          @if let Some(err) = &e.eval_error {
            p class="banner banner-warn" {
              "This rule could not be evaluated for this subject: " (err)
            }
          }
          (view::evidence(&e.evidence))
        }
        table {
          thead {
            tr {
              th class="shrink" { "When" }
              th class="shrink" { "Event" }
              th { "Who" }
              th { "Detail" }
            }
          }
          tbody {
            @for ev in &e.events {
              tr {
                td class="shrink" { (view::when(ev.at)) }
                td class="shrink" { span class="tag" { (ev.kind.as_str()) } }
                td {
                  @match &ev.actor {
                    Some(a) => span class="ref" { (a.as_str()) },
                    None => span class="muted" {
                      @match ev.sweep {
                        Some(s) => { "sweep " (s) },
                        None => { "—" },
                      }
                    },
                  }
                }
                td class="soft" {
                  @if let Some(note) = &ev.note { (note) " " }
                  @if let Some(detail) = &ev.detail { (detail) }
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
      layout::page(&identity, "Violation", Section::None, content)
        .into_string(),
    )
    .into_response(),
  )
}

/// Acknowledge, suppress with a reason and optional expiry, mark a false
/// positive, or revoke — the four verbs of SPEC.md section 9.
fn overlay_form(check: &CheckId, subject: &SubjectRef, episode: i64) -> Markup {
  html! {
    div class="panel" {
      div class="panel-body" {
        form method="post" action="/violations/act" class="stack" {
          input type="hidden" name="check" value=(check.as_str());
          input type="hidden" name="subject" value=(subject.to_string());
          input type="hidden" name="episode" value=(episode.to_string());
          input type="hidden" name="idempotency_key" value=(new_key());
          div class="row" {
            label class="field" {
              span { "Action" }
              select name="verb" {
                option value="acknowledge" { "Acknowledge — seen, not fixed" }
                option value="suppress" {
                  "Suppress — does not count as bad state"
                }
                option value="false_positive" {
                  "False positive — the rule is wrong"
                }
                option value="revoke" { "Revoke — undo the overlay" }
              }
            }
            label class="field" {
              span { "Suppression reason" }
              select name="reason" {
                option value="accepted_risk" { "accepted risk" }
                option value="bad_source_data" { "bad source data" }
                option value="expected" { "expected" }
              }
            }
            label class="field" {
              span { "Suppress until" }
              input type="datetime-local" name="until";
              div class="hint" {
                "Optional. Expiry is evaluated at sweep time."
              }
            }
          }
          label class="field" {
            span { "Note" }
            input type="text" name="note"
                  placeholder="Why — recorded against your name, forever";
          }
          div { button class="primary" type="submit" { "Record" } }
        }
      }
    }
  }
}
