//! Everything that writes.
//!
//! Every handler here appends a command with the authenticated operator
//! as its `actor` and a client-supplied idempotency key, then returns
//! either the re-rendered fragment htmx asked for or a redirect back to
//! where the operator was. Nothing writes a projection directly: the
//! store does that inside the same transaction as the append, so there
//! is no path by which the board can disagree with the stream.

use axum::{
  Form,
  extract::State,
  response::{Html, IntoResponse, Redirect, Response},
};
use overlord_core::{
  Actor, CheckDraft, CheckId, CommandKind, EntityType, NewCommand, Severity,
  SubjectKind, SubjectRef, SuppressReason, SystemId, SystemSelector, Timestamp,
  ViolationState,
};
use overlord_engine::checks;
use overlord_store::ViolationFilter;
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::{Result, WebError},
  pages::violations::{BoardQuery, row},
};

// --- violation overlays -------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ViolationForm {
  pub check:           String,
  pub subject:         String,
  #[serde(default)]
  pub episode:         i64,
  pub verb:            String,
  #[serde(default)]
  pub reason:          String,
  #[serde(default)]
  pub until:           String,
  #[serde(default)]
  pub note:            String,
  pub idempotency_key: String,
  /// The board's filter state, so a full-page submission returns the
  /// operator to the queue they were working rather than to the top.
  #[serde(default)]
  pub back:            String,
}

/// Acknowledge, suppress, mark a false positive, or revoke.
pub async fn violation_action(
  identity: Identity,
  State(state): State<AppState>,
  headers: axum::http::HeaderMap,
  Form(form): Form<ViolationForm>,
) -> Result<Response> {
  let check = CheckId::new(form.check.clone());
  let subject: SubjectRef = form.subject.parse()?;

  let kind = match form.verb.as_str() {
    "acknowledge" => CommandKind::ViolationAcknowledge {
      check_id: check.clone(),
      subject:  subject.clone(),
    },
    "suppress" => CommandKind::ViolationSuppress {
      check_id: check.clone(),
      subject:  subject.clone(),
      reason:   form
        .reason
        .parse::<SuppressReason>()
        .map_err(|e| WebError::bad_request(e.to_string()))?,
      until:    parse_until(&form.until)?,
    },
    "false_positive" => CommandKind::ViolationFalsePositive {
      check_id: check.clone(),
      subject:  subject.clone(),
    },
    "revoke" => CommandKind::ViolationRevoke {
      check_id: check.clone(),
      subject:  subject.clone(),
    },
    other => {
      return Err(WebError::bad_request(format!("unknown action {other:?}")));
    }
  };

  let mut command =
    NewCommand::new(identity.actor.clone(), kind, Timestamp::now())
      .with_idempotency_key(form.idempotency_key.clone());
  if !form.note.trim().is_empty() {
    command = command.with_note(form.note.trim());
  }

  state.db.write(|w| w.append_command(&command))?;

  // htmx asked for one row; give it exactly that row and nothing else.
  if headers.contains_key("hx-request") {
    let filter = ViolationFilter {
      states: ALL_STATES.to_vec(),
      subject: Some(subject.clone()),
      checks: vec![check.clone()],
      limit: 64,
      ..ViolationFilter::default()
    };
    let rows = state.db.read(|r| r.violations_where(&filter))?;
    let query = BoardQuery::default();
    let found = rows.iter().find(|v| v.episode == form.episode);
    return Ok(match found {
      Some(v) => Html(row(v, &query).into_string()).into_response(),
      // The episode is gone from every state the board can show, which
      // means the projection no longer has a row to re-render. Ask htmx
      // to reload rather than leaving a stale one in place.
      None => ([("hx-refresh", "true")], "").into_response(),
    });
  }

  Ok(
    Redirect::to(&if form.back.is_empty() {
      "/".to_owned()
    } else {
      format!("/?{}", form.back)
    })
    .into_response(),
  )
}

const ALL_STATES: [ViolationState; 5] = [
  ViolationState::Open,
  ViolationState::Acknowledged,
  ViolationState::Suppressed,
  ViolationState::FalsePositive,
  ViolationState::Resolved,
];

/// Read the `datetime-local` field.
///
/// The browser sends wall-clock with no zone. overlord records UTC only
/// (SPEC.md section 13), so it is read as UTC and the field says so —
/// quietly reinterpreting it in the server's local zone would make a
/// suppression expire at an hour nobody chose.
fn parse_until(raw: &str) -> Result<Option<Timestamp>> {
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(None);
  }
  let normalized = if raw.ends_with('Z') {
    raw.to_owned()
  } else if raw.len() == 16 {
    format!("{raw}:00Z")
  } else {
    format!("{raw}Z")
  };
  normalized
    .parse::<Timestamp>()
    .map(Some)
    .map_err(|e| WebError::bad_request(format!("suppress until: {e}")))
}

// --- checks -------------------------------------------------------------

/// The check editor's form (SPEC.md section 7).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CheckForm {
  pub id: String,
  pub name: String,
  #[serde(default)]
  pub description: String,
  #[serde(default)]
  pub rationale: String,
  #[serde(default)]
  pub remediation: String,
  #[serde(default)]
  pub references: String,
  pub severity: String,
  #[serde(default)]
  pub weight: String,
  pub applies_to: String,
  #[serde(default)]
  pub systems: String,
  #[serde(default)]
  pub entity_types: String,
  pub condition: String,
  #[serde(default)]
  pub suppress_if_pending_links: String,
  #[serde(default)]
  pub idempotency_key: String,
}

impl CheckForm {
  /// Turn the form into a draft, reporting the first field an operator
  /// has to fix.
  ///
  /// # Errors
  /// [`WebError::BadRequest`] for a missing id or name, an unknown
  /// severity or scope, or a non-numeric weight override.
  pub fn to_draft(&self) -> Result<CheckDraft> {
    let id = self.id.trim();
    if id.is_empty() {
      return Err(WebError::bad_request("a check needs an id"));
    }
    let name = self.name.trim();
    if name.is_empty() {
      return Err(WebError::bad_request("a check needs a name"));
    }

    Ok(CheckDraft {
      id: CheckId::new(id),
      name: name.to_owned(),
      description: optional(&self.description),
      rationale: optional(&self.rationale),
      remediation: optional(&self.remediation),
      references: lines(&self.references),
      severity: self
        .severity
        .parse::<Severity>()
        .map_err(|e| WebError::bad_request(e.to_string()))?,
      weight: match self.weight.trim() {
        "" => None,
        w => Some(w.parse::<i64>().map_err(|_| {
          WebError::bad_request("weight must be a whole number")
        })?),
      },
      applies_to: self
        .applies_to
        .parse::<SubjectKind>()
        .map_err(|e| WebError::bad_request(e.to_string()))?,
      systems: comma(&self.systems)
        .into_iter()
        .map(|s| s.parse::<SystemSelector>())
        .collect::<std::result::Result<Vec<_>, _>>()?,
      entity_types: comma(&self.entity_types)
        .into_iter()
        .map(EntityType::new)
        .collect(),
      condition: self.condition.trim().to_owned(),
      // An unchecked HTML checkbox sends nothing at all, so presence is
      // the signal rather than the value.
      suppress_if_pending_links: !self.suppress_if_pending_links.is_empty(),
    })
  }
}

fn optional(s: &str) -> Option<String> {
  let s = s.trim();
  (!s.is_empty()).then(|| s.to_owned())
}

fn lines(s: &str) -> Vec<String> {
  s.lines()
    .map(str::trim)
    .filter(|l| !l.is_empty())
    .map(ToOwned::to_owned)
    .collect()
}

fn comma(s: &str) -> Vec<String> {
  s.split(',')
    .map(str::trim)
    .filter(|p| !p.is_empty())
    .map(ToOwned::to_owned)
    .collect()
}

/// Save a new check or a new revision of one.
///
/// A condition that will not compile is refused here, before anything is
/// appended: the command stream should not hold a revision that could
/// never be evaluated.
pub async fn check_save(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<CheckForm>,
) -> Result<Response> {
  let draft = form.to_draft()?;
  let key =
    (!form.idempotency_key.is_empty()).then(|| form.idempotency_key.clone());

  let revision =
    checks::upsert(&state.db, &identity.actor, &draft, Timestamp::now(), key)?;

  tracing::info!(check = %draft.id, %revision, actor = %identity.actor.as_str(), "check saved");
  Ok(
    Redirect::to(&format!(
      "/rules/edit?id={}&saved=1",
      crate::view::urlencode(draft.id.as_str())
    ))
    .into_response(),
  )
}

#[derive(Debug, Deserialize)]
pub struct CheckRef {
  pub id:       String,
  #[serde(default)]
  pub revision: Option<u32>,
}

/// Evaluate a revision against current facts without opening anything.
pub async fn check_dry_run(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<CheckRef>,
) -> Result<Response> {
  let id = CheckId::new(form.id.clone());
  let record = checks::current(&state.db, &id)?;
  let revision = form
    .revision
    .map_or(record.revision, overlord_core::Revision);

  checks::dry_run(&state.db, &identity.actor, &id, revision, Timestamp::now())?;

  Ok(
    Redirect::to(&format!(
      "/rules/edit?id={}&revision={}",
      crate::view::urlencode(id.as_str()),
      revision.0
    ))
    .into_response(),
  )
}

/// Enable a check. Refused without a dry-run for that exact revision —
/// the store enforces it, so the UI and the CLI cannot each forget
/// (SPEC.md section 7).
pub async fn check_enable(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<CheckRef>,
) -> Result<Response> {
  let id = CheckId::new(form.id.clone());
  checks::enable(&state.db, &identity.actor, &id, Timestamp::now())?;
  Ok(back_to_rule(&id))
}

/// Disable a check, resolving its open violations with reason
/// `check_disabled` (SPEC.md section 9).
pub async fn check_disable(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<CheckRef>,
) -> Result<Response> {
  let id = CheckId::new(form.id.clone());
  checks::disable(&state.db, &identity.actor, &id, Timestamp::now())?;
  Ok(back_to_rule(&id))
}

fn back_to_rule(id: &CheckId) -> Response {
  Redirect::to(&format!(
    "/rules/edit?id={}",
    crate::view::urlencode(id.as_str())
  ))
  .into_response()
}

// --- sweeps -------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SweepForm {
  /// Empty means every configured system.
  #[serde(default)]
  pub system: String,
}

/// Start a sweep on a background task and return to the Sweeps screen,
/// which polls the sweep row for progress.
pub async fn sweep_run(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<SweepForm>,
) -> Result<Response> {
  let only: Vec<SystemId> = if form.system.trim().is_empty() {
    Vec::new()
  } else {
    vec![SystemId::new(form.system.trim())]
  };
  let actor: &Actor = &identity.actor;
  state.sweeps.start(actor, &only)?;
  Ok(Redirect::to("/sweeps").into_response())
}
