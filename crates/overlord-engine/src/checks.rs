//! Authoring checks (SPEC.md section 7).
//!
//! Checks are operator-authored records stored as revisions in the
//! command stream. Validation happens here, above the store, because it
//! needs the expression compiler; the store enforces the invariants only
//! it can see, such as refusing to enable a revision with no dry-run.

use std::collections::BTreeSet;

use overlord_core::{
  Actor, CheckDraft, CheckId, CheckRecord, CommandKind, DryrunSample,
  EntityRef, NewCommand, Revision, SubjectKind, SubjectRef, Timestamp,
};
use overlord_expr::{EvalCtx, Schema, Tri, compile, eval};
use overlord_store::Db;

use crate::{
  error::{EngineError, Result},
  evaluate,
  world::World,
};

/// The most samples a dry-run keeps. Enough to judge a rule by; few
/// enough that recording it does not bloat the command stream.
const MAX_SAMPLES: usize = 10;

/// Validate a condition without saving anything.
///
/// # Errors
/// [`EngineError::BadCondition`] with every diagnostic, so the editor
/// can underline them all at once.
pub fn validate(condition: &str, applies_to: SubjectKind) -> Result<()> {
  compile(condition, &Schema::new(applies_to))
    .map(|_| ())
    .map_err(EngineError::BadCondition)
}

/// Create a check, or append a revision to it.
///
/// # Errors
/// If the condition will not compile, or the store refuses the command.
pub fn upsert(
  db: &Db,
  actor: &Actor,
  draft: &CheckDraft,
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<Revision> {
  validate(&draft.condition, draft.applies_to)?;

  let mut cmd = NewCommand::new(
    actor.clone(),
    CommandKind::CheckUpsert {
      draft: draft.clone(),
    },
    at,
  );
  cmd.idempotency_key = idempotency_key;

  db.write(|w| -> Result<_> {
    w.append_command(&cmd)?;
    let record = w
      .reader()
      .checks()?
      .into_iter()
      .find(|c| c.draft.id == draft.id)
      .ok_or_else(|| {
        EngineError::Config(format!("check {} vanished", draft.id))
      })?;
    Ok(record.revision)
  })
}

/// What a dry-run found.
#[derive(Debug, Clone)]
pub struct DryRun {
  pub check_id:    CheckId,
  pub revision:    Revision,
  pub match_count: u64,
  /// How many subjects the check's scope admitted at all.
  ///
  /// The denominator `match_count` needs: without it, "0 would match"
  /// reads as "nothing is wrong" when it may mean "this rule selects
  /// nobody". A scope naming a system that does not exist is a typo, not
  /// a clean bill of health.
  pub in_scope:    u64,
  pub samples:     Vec<DryrunSample>,
  /// Subjects the condition could not be answered for.
  pub errors:      Vec<String>,
}

/// Evaluate a check revision against current facts and record the
/// result.
///
/// SPEC.md section 7: a dry-run opens no violations and touches no other
/// projection. It exists so an operator can see what a rule would do
/// before it can do it — `check.enable` is refused without one.
///
/// # Errors
/// If the revision is unknown, the condition will not compile, or the
/// store refuses the command.
pub fn dry_run(
  db: &Db,
  actor: &Actor,
  check_id: &CheckId,
  revision: Revision,
  at: Timestamp,
) -> Result<DryRun> {
  let draft =
    db.read(|r| -> Result<_> { Ok(r.check_revision(check_id, revision)?) })?;
  let program = compile(&draft.condition, &Schema::new(draft.applies_to))
    .map_err(EngineError::BadCondition)?;

  // Relative windows resolve against the last sweep's start time, so a
  // dry-run answers "what would the next sweep see", not "what would a
  // sweep at this exact instant see". With no sweep yet, the wall clock
  // is all there is.
  let (world, now, pending) = db.read(|r| -> Result<_> {
    let now = match r.latest_sweep()? {
      Some(s) => r.sweep_started_at(s)?,
      None => at,
    };
    let pending: BTreeSet<EntityRef> =
      r.entities_with_pending_suggestions()?.into_iter().collect();
    Ok((World::load(r)?, now, pending))
  })?;

  let ctx = EvalCtx::at(now);
  let mut out = DryRun {
    check_id: check_id.clone(),
    revision,
    match_count: 0,
    in_scope: 0,
    samples: Vec::new(),
    errors: Vec::new(),
  };

  // Exactly the subjects a sweep would evaluate, chosen by the sweep's
  // own predicates. A dry-run that counted every subject in the world
  // overstated a scoped check — it reported matches for accounts the
  // sweep would never look at, so a rule could dry-run full and then
  // open nothing, with nothing on either screen to explain it.
  //
  // The one filter deliberately not applied is the sweep's "skip
  // systems this run did not cover": a dry-run has no run to speak of,
  // and answers for the full sweep that `enable` is a prelude to.
  let skip_pending = |refs: &[EntityRef]| {
    draft.suppress_if_pending_links && refs.iter().any(|e| pending.contains(e))
  };
  let subjects: Vec<SubjectRef> = match draft.applies_to {
    SubjectKind::Entity => world
      .entity_refs()
      .into_iter()
      .filter(|e| {
        evaluate::entity_in_scope(&draft, e, &world)
          && !skip_pending(std::slice::from_ref(e))
      })
      .map(SubjectRef::Entity)
      .collect(),
    SubjectKind::Person => world
      .person_uids()
      .into_iter()
      .filter(|p| {
        evaluate::person_in_scope(&draft, p, &world)
          && !skip_pending(&world.member_refs(p))
      })
      .map(SubjectRef::Person)
      .collect(),
  };
  out.in_scope = subjects.len() as u64;

  for subject in subjects {
    let evaluation = match &subject {
      SubjectRef::Entity(e) => {
        world.entity_subject(e).map(|s| eval(&program, &s, &ctx))
      }
      SubjectRef::Person(p) => {
        world.person_subject(p).map(|s| eval(&program, &s, &ctx))
      }
    };
    let Some(mut evaluation) = evaluation else {
      continue;
    };
    match &evaluation.outcome {
      Ok(Tri::True) => {
        out.match_count += 1;
        if out.samples.len() < MAX_SAMPLES {
          let fact_ids = match &subject {
            SubjectRef::Entity(e) => {
              world.attrs(e).map(|a| vec![a.fact_id]).unwrap_or_default()
            }
            SubjectRef::Person(p) => world.person_fact_ids(p),
          };
          evaluation.evidence.attribute(&fact_ids);
          out.samples.push(DryrunSample {
            subject:  subject.clone(),
            evidence: evaluation.evidence,
          });
        }
      }
      Ok(_) => {}
      Err(e) => out.errors.push(format!("{subject}: {e}")),
    }
  }

  let cmd = NewCommand::new(
    actor.clone(),
    CommandKind::CheckDryrun {
      check_id: check_id.clone(),
      revision,
      match_count: out.match_count,
      samples: out.samples.clone(),
    },
    at,
  );
  db.write(|w| -> Result<_> {
    w.append_command(&cmd)?;
    Ok(())
  })?;

  Ok(out)
}

/// Enable a check at its current revision.
///
/// # Errors
/// If the check is unknown, or has no dry-run for that revision — the
/// store refuses it, so the CLI and the UI cannot each forget the rule.
pub fn enable(
  db: &Db,
  actor: &Actor,
  check_id: &CheckId,
  at: Timestamp,
) -> Result<Revision> {
  let record = current(db, check_id)?;
  let cmd = NewCommand::new(
    actor.clone(),
    CommandKind::CheckEnable {
      check_id: check_id.clone(),
      revision: record.revision,
    },
    at,
  );
  db.write(|w| -> Result<_> {
    w.append_command(&cmd)?;
    Ok(record.revision)
  })
}

/// Disable a check, resolving its open violations with reason
/// `check_disabled`.
///
/// # Errors
/// If the check is unknown.
pub fn disable(
  db: &Db,
  actor: &Actor,
  check_id: &CheckId,
  at: Timestamp,
) -> Result<()> {
  let cmd = NewCommand::new(
    actor.clone(),
    CommandKind::CheckDisable {
      check_id: check_id.clone(),
    },
    at,
  );
  db.write(|w| -> Result<_> {
    w.append_command(&cmd)?;
    Ok(())
  })
}

/// The current revision of a check.
///
/// # Errors
/// If the check is unknown.
pub fn current(db: &Db, check_id: &CheckId) -> Result<CheckRecord> {
  db.read(|r| -> Result<_> {
    r.checks()?
      .into_iter()
      .find(|c| &c.draft.id == check_id)
      .ok_or_else(|| {
        EngineError::Store(overlord_store::StoreError::not_found(format!(
          "check {check_id}"
        )))
      })
  })
}
