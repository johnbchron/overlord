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
  Actor, CheckDraft, CheckId, CommandKind, EntityRef, EntityType, NewCommand,
  PersonUid, Severity, SubjectKind, SubjectRef, SuppressReason, SystemId,
  SystemKind, SystemSelector, Timestamp, ViolationState,
};
use overlord_engine::{checks, identity};
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
      subjects: vec![subject.clone()],
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

// --- identity (SPEC.md section 12) --------------------------------------

/// Where to send the operator back to. Every identity verb changes a
/// page that is *about* the thing being changed, so a redirect back is
/// the whole response — there is no single row to re-render the way a
/// violation overlay has.
fn back_to(back: &str, fallback: &str) -> Response {
  // Only same-origin paths: `back` arrives in a form field, and an
  // absolute URL there would turn an authenticated POST into an open
  // redirect.
  let target = if back.starts_with('/') && !back.starts_with("//") {
    back
  } else {
    fallback
  };
  Redirect::to(target).into_response()
}

#[derive(Debug, Deserialize)]
pub struct LinkForm {
  pub entity:          String,
  /// A person uid, the implicit uid of another unlinked account, or
  /// empty to create a person for this account alone.
  #[serde(default)]
  pub person:          String,
  #[serde(default)]
  pub display_name:    String,
  /// Which signal the operator agreed with, when they confirmed a
  /// proposal rather than searching for somebody.
  #[serde(default)]
  pub signal:          String,
  pub idempotency_key: String,
  #[serde(default)]
  pub back:            String,
}

/// Confirm a suggestion, or link an account by hand. Both are the same
/// command; only where the person came from differs.
pub async fn identity_link(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<LinkForm>,
) -> Result<Response> {
  let entity: EntityRef = form.entity.parse()?;
  let name = optional(&form.display_name);
  let signal = optional(&form.signal);
  let key = Some(form.idempotency_key.clone());
  let at = Timestamp::now();

  let uid = match form.person.trim() {
    "" => {
      let name = match name {
        Some(name) => Some(name),
        None => identity::display_name_of(&state.db, &entity)?,
      };
      identity::link_to_new_person(
        &state.db,
        &identity.actor,
        name,
        &entity,
        signal,
        at,
        key,
      )?
    }
    person => identity::confirm(
      &state.db,
      &identity.actor,
      &entity,
      &PersonUid::new(person),
      signal,
      at,
      key,
    )?,
  };

  tracing::info!(%entity, person = %uid, actor = %identity.actor.as_str(), "account linked");
  Ok(back_to(
    &form.back,
    &format!("/person?uid={}", crate::view::urlencode(uid.as_str())),
  ))
}

#[derive(Debug, Deserialize)]
pub struct UnlinkForm {
  pub person:          String,
  pub entity:          String,
  pub idempotency_key: String,
  #[serde(default)]
  pub back:            String,
}

/// Detach an account. It becomes an unlinked account again — its own
/// implicit person, with its own violations (SPEC.md section 6.4).
pub async fn identity_unlink(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<UnlinkForm>,
) -> Result<Response> {
  let entity: EntityRef = form.entity.parse()?;
  let uid = PersonUid::new(form.person.clone());
  identity::unlink(
    &state.db,
    &identity.actor,
    &uid,
    &entity,
    Timestamp::now(),
    Some(form.idempotency_key.clone()),
  )?;
  Ok(back_to(
    &form.back,
    &format!("/person?uid={}", crate::view::urlencode(uid.as_str())),
  ))
}

#[derive(Debug, Deserialize)]
pub struct PrimaryForm {
  pub person:          String,
  pub system_kind:     String,
  pub entity:          String,
  pub idempotency_key: String,
  #[serde(default)]
  pub back:            String,
}

/// Designate the account an `entity(...)` selector resolves to for one
/// system kind — the operator's answer to an `ambiguous` flag.
pub async fn identity_primary(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<PrimaryForm>,
) -> Result<Response> {
  let entity: EntityRef = form.entity.parse()?;
  let uid = PersonUid::new(form.person.clone());
  let kind = form
    .system_kind
    .parse::<SystemKind>()
    .map_err(|e| WebError::bad_request(e.to_string()))?;
  identity::set_primary(
    &state.db,
    &identity.actor,
    &uid,
    kind,
    &entity,
    Timestamp::now(),
    Some(form.idempotency_key.clone()),
  )?;
  Ok(back_to(
    &form.back,
    &format!("/person?uid={}", crate::view::urlencode(uid.as_str())),
  ))
}

#[derive(Debug, Deserialize)]
pub struct MergeForm {
  pub surviving:       String,
  pub retired:         String,
  pub idempotency_key: String,
  #[serde(default)]
  pub back:            String,
}

/// Combine two persons. The uid on the page survives; the one chosen in
/// the picker is retired into it and resolves through it forever.
pub async fn identity_merge(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<MergeForm>,
) -> Result<Response> {
  let surviving = PersonUid::new(form.surviving.clone());
  let retired = PersonUid::new(form.retired.clone());
  identity::merge(
    &state.db,
    &identity.actor,
    &surviving,
    &retired,
    Timestamp::now(),
    Some(form.idempotency_key.clone()),
  )?;
  Ok(back_to(
    &form.back,
    &format!("/person?uid={}", crate::view::urlencode(surviving.as_str())),
  ))
}

#[derive(Debug, Deserialize)]
pub struct SplitForm {
  pub from:            String,
  /// One checkbox per account to move. A form with none checked sends
  /// nothing, hence the default.
  #[serde(default)]
  pub entity:          Vec<String>,
  #[serde(default)]
  pub display_name:    String,
  pub idempotency_key: String,
}

/// Move accounts onto a new person. The original keeps its uid, and so
/// its history (SPEC.md section 12).
pub async fn identity_split(
  identity: Identity,
  State(state): State<AppState>,
  Form(form): Form<SplitForm>,
) -> Result<Response> {
  let from = PersonUid::new(form.from.clone());
  let entities = form
    .entity
    .iter()
    .map(|e| e.parse::<EntityRef>())
    .collect::<std::result::Result<Vec<_>, _>>()?;
  if entities.is_empty() {
    return Err(WebError::bad_request(
      "choose at least one account to split off",
    ));
  }

  let new_uid = identity::split(
    &state.db,
    &identity.actor,
    &from,
    optional(&form.display_name),
    &entities,
    Timestamp::now(),
    Some(form.idempotency_key.clone()),
  )?;
  Ok(
    Redirect::to(&format!(
      "/person?uid={}",
      crate::view::urlencode(new_uid.as_str())
    ))
    .into_response(),
  )
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
