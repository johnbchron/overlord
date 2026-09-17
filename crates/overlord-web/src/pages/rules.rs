//! Rules and the check editor (SPEC.md section 5, section 7).
//!
//! The UI is the only place checks are authored (SPEC.md section 15), so
//! this screen carries the whole authoring contract: validate, dry-run,
//! enable. Enabling is gated on a dry-run for that exact
//! `(id, revision)` here *and* in the store — the UI hides the button,
//! and the store refuses the command regardless, because a gate that
//! only exists in a template is not a gate.

use axum::{
  extract::{Query, State},
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::{
  CheckDraft, CheckId, CheckRecord, Revision, Severity, SubjectKind,
};
use overlord_engine::{EngineError, checks};
use overlord_expr::Diagnostic;
use overlord_store::CheckRevisionRow;
use serde::Deserialize;

use crate::{
  AppState,
  actions::CheckForm,
  auth::Identity,
  error::{Result, WebError},
  layout::{self, Section},
  pages::violations::new_key,
  view,
};

/// Every check, with the quality signals SPEC.md section 5 asks for.
pub async fn list(
  identity: Identity,
  State(state): State<AppState>,
) -> Result<Response> {
  struct Line {
    record: CheckRecord,
    open:   i64,
    fp:     Option<f64>,
  }

  let lines = state.db.read(|r| -> Result<Vec<Line>> {
    let counts = r.open_counts_by_check()?;
    r.checks()?
      .into_iter()
      .map(|record| {
        let open = counts
          .iter()
          .find(|(id, _)| *id == record.draft.id)
          .map_or(0, |(_, n)| *n);
        Ok(Line {
          fp: r.false_positive_rate(&record.draft.id)?,
          record,
          open,
        })
      })
      .collect()
  })?;

  let content = html! {
    (layout::head(
      "Rules",
      "Checks are the only detection mechanism. Each one is a versioned \
       record in the command stream, not a file.",
      html! { a class="btn primary" href="/rules/new" { "New check" } },
    ))

    div class="panel" {
      @if lines.is_empty() {
        (layout::empty("No checks yet. Write the first one."))
      } @else {
        table {
          thead {
            tr {
              th class="shrink" { "Severity" }
              th { "Check" }
              th class="shrink" { "Applies to" }
              th { "Scope" }
              th class="shrink" { "Revision" }
              th class="shrink" { "State" }
              th class="shrink num" { "Open" }
              th class="shrink num" { "False positive" }
            }
          }
          tbody {
            @for line in &lines {
              tr {
                td class="shrink" { (view::severity(line.record.draft.severity)) }
                td {
                  a href={ "/rules/edit?id="
                           (view::urlencode(line.record.draft.id.as_str())) } {
                    (line.record.draft.name)
                  }
                  div class="evidence" { (line.record.draft.condition) }
                }
                td class="shrink" {
                  span class="tag" { (line.record.draft.applies_to.as_str()) }
                }
                td class="soft" { (scope(&line.record.draft)) }
                td class="shrink" { "r" (line.record.revision) }
                td class="shrink" {
                  @if line.record.enabled {
                    span class="tag tag-ok" { "enabled" }
                  } @else {
                    span class="tag" { "disabled" }
                  }
                }
                td class="shrink num" {
                  (line.open)
                  @if line.record.enabled && line.open == 0 {
                    " "
                    span class="tag" title="This rule has never matched \
                                            anything — worth a second look, \
                                            but not a verdict" {
                      "zero"
                    }
                  }
                }
                td class="shrink num" {
                  @match line.fp {
                    // A rule's own quality signal, not a fact about any
                    // subject (SPEC.md section 9).
                    Some(rate) => (format!("{:.0}%", rate * 100.0)),
                    None => span class="muted" { "—" },
                  }
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
      layout::page(&identity, "Rules", Section::Rules, content).into_string(),
    )
    .into_response(),
  )
}

fn scope(draft: &CheckDraft) -> String {
  let mut parts = Vec::new();
  if !draft.systems.is_empty() {
    parts.push(
      draft
        .systems
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", "),
    );
  }
  if !draft.entity_types.is_empty() {
    parts.push(
      draft
        .entity_types
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", "),
    );
  }
  if parts.is_empty() {
    "everything".to_owned()
  } else {
    parts.join(" · ")
  }
}

// --- the editor ---------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct EditQuery {
  #[serde(default)]
  pub id:       String,
  /// Show an older revision's source, for comparison before reviving it.
  #[serde(default)]
  pub revision: Option<u32>,
  #[serde(default)]
  pub saved:    Option<String>,
}

/// A blank editor.
pub async fn new(identity: Identity) -> Result<Response> {
  let content = editor(
    &CheckForm {
      severity: Severity::High.as_str().to_owned(),
      applies_to: SubjectKind::Entity.as_str().to_owned(),
      ..CheckForm::default()
    },
    None,
    &[],
    false,
    None,
  );

  Ok(
    Html(
      layout::page(&identity, "New check", Section::Rules, content)
        .into_string(),
    )
    .into_response(),
  )
}

/// The editor for an existing check, with its revision history and the
/// dry-run panel for the revision on screen.
pub async fn edit(
  identity: Identity,
  State(state): State<AppState>,
  Query(query): Query<EditQuery>,
) -> Result<Response> {
  let id = CheckId::new(query.id.clone());
  let (record, revisions) = state.db.read(|r| -> Result<_> {
    let record = r.checks()?.into_iter().find(|c| c.draft.id == id);
    Ok((record, r.check_revisions(&id)?))
  })?;

  let Some(record) = record else {
    return Err(WebError::not_found(format!("check {id}")));
  };

  // The editor opens on the current revision unless the operator asked
  // to look at an older one.
  let showing = query.revision.map(Revision).unwrap_or(record.revision);
  let shown = revisions
    .iter()
    .find(|r| r.revision == showing)
    .ok_or_else(|| {
      WebError::not_found(format!("check {id} revision {showing}"))
    })?;

  let content = editor(
    &form_of(&shown.draft),
    Some(&record),
    &revisions,
    query.saved.is_some(),
    Some(shown),
  );

  Ok(
    Html(
      layout::page(&identity, &record.draft.name, Section::Rules, content)
        .into_string(),
    )
    .into_response(),
  )
}

fn form_of(draft: &CheckDraft) -> CheckForm {
  CheckForm {
    id: draft.id.to_string(),
    name: draft.name.clone(),
    description: draft.description.clone().unwrap_or_default(),
    rationale: draft.rationale.clone().unwrap_or_default(),
    remediation: draft.remediation.clone().unwrap_or_default(),
    references: draft.references.join("\n"),
    severity: draft.severity.as_str().to_owned(),
    weight: draft.weight.map(|w| w.to_string()).unwrap_or_default(),
    applies_to: draft.applies_to.as_str().to_owned(),
    systems: draft
      .systems
      .iter()
      .map(ToString::to_string)
      .collect::<Vec<_>>()
      .join(", "),
    entity_types: draft
      .entity_types
      .iter()
      .map(ToString::to_string)
      .collect::<Vec<_>>()
      .join(", "),
    condition: draft.condition.clone(),
    suppress_if_pending_links: if draft.suppress_if_pending_links {
      "on".to_owned()
    } else {
      String::new()
    },
    idempotency_key: String::new(),
  }
}

#[allow(clippy::too_many_lines)]
fn editor(
  form: &CheckForm,
  record: Option<&CheckRecord>,
  revisions: &[CheckRevisionRow],
  saved: bool,
  shown: Option<&CheckRevisionRow>,
) -> Markup {
  let is_new = record.is_none();
  let has_dryrun = shown.is_some_and(|s| s.dryrun.is_some());
  let is_current = shown.is_none_or(|s| s.is_current);

  html! {
    (layout::head(
      if is_new { "New check" } else { &form.name },
      "",
      html! {
        @if let Some(r) = record {
          @if r.enabled {
            form class="inline-form" method="post" action="/rules/disable" {
              input type="hidden" name="id" value=(form.id);
              button { "Disable" }
            }
          } @else if has_dryrun && is_current {
            form class="inline-form" method="post" action="/rules/enable" {
              input type="hidden" name="id" value=(form.id);
              button class="primary" { "Enable" }
            }
          } @else {
            button disabled
                   title="Enabling needs a dry-run for this exact revision" {
              "Enable"
            }
          }
        }
      },
    ))

    @if saved {
      div class="banner banner-ok" {
        "Saved. Enabling this revision needs its own dry-run — a rule that \
         was safe to run yesterday is a different rule today."
      }
    }

    @if let Some(r) = record {
      @if r.enabled && !is_current {
        div class="banner banner-warn" {
          "This check is enabled at revision " (r.revision)
          ", which is not the revision on screen."
        }
      }
    }

    form method="post" action="/rules/save" {
      input type="hidden" name="idempotency_key" value=(new_key());

      div class="panel" {
        div class="panel-body" {
          div class="row" {
            label class="field" {
              span { "Id" }
              input type="text" name="id" value=(form.id)
                    required readonly[!is_new]
                    placeholder="idp-mfa-missing";
              @if !is_new {
                div class="hint" {
                  "An id is the violation's identity across sweeps, so it \
                   never changes."
                }
              }
            }
            label class="field" {
              span { "Name" }
              input type="text" name="name" value=(form.name) required
                    placeholder="MFA missing on an active account";
            }
            label class="field shrink" {
              span { "Severity" }
              select name="severity" {
                @for s in Severity::ALL {
                  option value=(s.as_str()) selected[form.severity == s.as_str()] {
                    (s.as_str()) " (" (s.default_weight()) ")"
                  }
                }
              }
            }
            label class="field shrink" {
              span { "Weight override" }
              input type="number" name="weight" value=(form.weight)
                    placeholder="tier default";
            }
          }
        }
      }

      div class="panel" {
        div class="panel-body" {
          div class="row" {
            label class="field shrink" {
              span { "Applies to" }
              select name="applies_to"
                     hx-post="/rules/validate"
                     hx-target="#diagnostics"
                     hx-include="closest form" {
                @for k in [SubjectKind::Entity, SubjectKind::Person] {
                  option value=(k.as_str()) selected[form.applies_to == k.as_str()] {
                    (k.as_str())
                  }
                }
              }
              div class="hint" {
                "A person spans systems; an entity is one account."
              }
            }
            label class="field" {
              span { "Systems" }
              input type="text" name="systems" value=(form.systems)
                    placeholder="idp, okta-prod";
              div class="hint" {
                "System kinds or instance ids, comma separated. Empty means \
                 every system."
              }
            }
            label class="field" {
              span { "Entity types" }
              input type="text" name="entity_types" value=(form.entity_types)
                    placeholder="user, group";
            }
          }

          label class="field" {
            span { "Condition" }
            textarea name="condition" rows="5" required
                     spellcheck="false"
                     hx-post="/rules/validate"
                     hx-trigger="blur, change"
                     hx-target="#diagnostics"
                     hx-include="closest form"
                     placeholder="status == \"active\" and not mfa_enrolled" {
              (form.condition)
            }
            div class="hint" {
              "Three-valued: only a true result opens a violation. Null is \
               not false."
            }
          }
          div id="diagnostics" class="diagnostics" {}

          label class="field" {
            span {
              input type="checkbox" name="suppress_if_pending_links"
                    style="width:auto;margin-right:0.4rem"
                    checked[!form.suppress_if_pending_links.is_empty()];
              "Skip subjects with unreviewed link suggestions"
            }
          }
        }
      }

      div class="panel" {
        div class="panel-body" {
          label class="field" {
            span { "Description" }
            input type="text" name="description" value=(form.description);
          }
          label class="field" {
            span { "Why this matters" }
            input type="text" name="rationale" value=(form.rationale);
          }
          label class="field" {
            span { "Remediation" }
            input type="text" name="remediation" value=(form.remediation)
                  placeholder="What the operator should actually do";
          }
          label class="field" {
            span { "References" }
            textarea name="references" rows="2" { (form.references) }
            div class="hint" { "One per line." }
          }
        }
      }

      div class="row shrink" style="margin-top:1rem" {
        button class="primary" type="submit" {
          @if is_new { "Create" } @else { "Save as a new revision" }
        }
        a class="btn" href="/rules" { "Cancel" }
      }
    }

    @if let Some(shown) = shown {
      h2 { "Dry run" }
      p class="lede" {
        "A dry run evaluates this revision against current facts and opens \
         nothing. Enabling a revision needs one."
      }
      div class="panel" {
        div class="panel-body" {
          form method="post" action="/rules/dry-run" class="row shrink" {
            input type="hidden" name="id" value=(form.id);
            input type="hidden" name="revision" value=(shown.revision.0);
            button { "Run against current facts" }
          }
          @match &shown.dryrun {
            Some(run) => {
              p {
                b { (run.match_count) }
                " subjects would match, as of " (view::when(run.at)) "."
              }
              @if run.samples.is_empty() {
                p class="muted" { "No samples recorded." }
              } @else {
                table {
                  thead {
                    tr { th { "Subject" } th { "Evidence" } }
                  }
                  tbody {
                    @for sample in &run.samples {
                      tr {
                        td { (view::subject(&sample.subject, None)) }
                        td { (view::evidence(&sample.evidence)) }
                      }
                    }
                  }
                }
              }
            },
            None => p class="muted" {
              "No dry run for revision " (shown.revision) " yet."
            },
          }
        }
      }
    }

    @if revisions.len() > 1 {
      h2 { "Revision history" }
      div class="panel" {
        table {
          thead {
            tr {
              th class="shrink" { "Revision" }
              th class="shrink" { "Saved" }
              th { "By" }
              th { "Condition" }
              th class="shrink" { "" }
            }
          }
          tbody {
            @for r in revisions {
              tr {
                td class="shrink" {
                  "r" (r.revision)
                  @if r.is_current { " " span class="tag" { "current" } }
                  @if r.is_enabled { " " span class="tag tag-ok" { "enabled" } }
                }
                td class="shrink" { (view::when(r.at)) }
                td class="ref" { (r.actor.as_str()) }
                td class="evidence" { (r.draft.condition) }
                td class="shrink" {
                  @match &r.dryrun {
                    Some(d) => span class="tag tag-ok"
                                    title="Has a dry run" {
                      (d.match_count) " matched"
                    },
                    None => span class="tag" { "no dry run" },
                  }
                  " "
                  a href={ "/rules/edit?id=" (view::urlencode(&form.id))
                           "&revision=" (r.revision.0) } { "View" }
                }
              }
            }
          }
        }
      }
    }
  }
}

// --- inline validation --------------------------------------------------

/// Just the two fields validation needs.
///
/// The editor posts its whole form, but this takes only the condition
/// and the scope it is checked against: a half-filled new check must
/// still get its condition underlined, and requiring an id or a name to
/// see a syntax error would make the editor useless exactly when it is
/// most wanted.
#[derive(Debug, Default, Deserialize)]
pub struct ValidateForm {
  #[serde(default)]
  pub condition:  String,
  #[serde(default)]
  pub applies_to: String,
}

/// Compile the condition and return the diagnostics panel.
///
/// PLAN.md section 5: `ParseError` and `TypeError` carry byte spans, and
/// this is what they are for — the editor underlines exactly the
/// offending bytes rather than saying "syntax error" and leaving the
/// operator to find it. Every diagnostic is returned at once, not just
/// the first.
pub async fn validate(
  _identity: Identity,
  axum::Form(form): axum::Form<ValidateForm>,
) -> Result<Response> {
  let condition = form.condition.trim();
  if condition.is_empty() {
    return Ok(Html(String::new()).into_response());
  }

  let applies_to = form
    .applies_to
    .parse::<SubjectKind>()
    .unwrap_or(SubjectKind::Entity);

  let markup = match checks::validate(condition, applies_to) {
    Ok(()) => html! {
      div class="diagnostics ok" {
        div class="diagnostic" {
          div class="msg" { "This condition compiles." }
        }
      }
    },
    Err(EngineError::BadCondition(diagnostics)) => {
      render_diagnostics(&diagnostics, condition)
    }
    Err(other) => html! {
      div class="diagnostic" { div class="msg" { (other.to_string()) } }
    },
  };

  Ok(Html(markup.into_string()).into_response())
}

fn render_diagnostics(diagnostics: &[Diagnostic], src: &str) -> Markup {
  html! {
    @for d in diagnostics { (view::diagnostic(d, src)) }
  }
}
