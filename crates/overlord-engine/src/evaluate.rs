//! The evaluation pass: SPEC.md sections 7, 8 and 9 put together.
//!
//! Runs inside the sweep's transaction, at its recorded commit position,
//! so a replay can re-run exactly this against exactly these facts.

use std::collections::{BTreeMap, BTreeSet};

use overlord_core::{
  CheckDraft, CheckId, EntityRef, PersonUid, ResolveReason, Revision,
  SubjectKind, SubjectRef, SweepId, SystemId, SystemKind, Timestamp,
};
use overlord_expr::{EvalCtx, Program, Schema, Subject, Tri, compile, eval};
use overlord_store::{EpisodeFacts, Writer, violations::Episode};

use crate::{error::Result, world::World};

/// What one evaluation pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvalReport {
  pub subjects_evaluated: usize,
  pub opened:             usize,
  pub regressed:          usize,
  pub resolved:           usize,
  pub standing:           usize,
  pub expired:            usize,
  pub ambiguous:          usize,
  /// Episodes carried onto a subject that absorbed theirs — an implicit
  /// singleton person promoted by a link, or a uid retired by a merge.
  /// Identity work, visible as such (SPEC.md section 12).
  pub carried:            usize,
  /// Link suggestions this sweep computed. Proposals only: nothing here
  /// was applied (SPEC.md section 12).
  pub suggested:          usize,
  /// Conditions that would not compile, or that failed against a
  /// subject. Rule-quality signals, not violations.
  pub errors:             Vec<CheckProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckProblem {
  pub check_id: CheckId,
  pub subject:  Option<String>,
  pub message:  String,
}

/// Evaluate every pinned check against every in-scope subject.
///
/// # Errors
/// On a store failure. A check that will not compile, or that errors
/// against a subject, is reported rather than raised: one bad rule must
/// not take down a sweep.
pub fn evaluate_sweep(w: &Writer<'_>, sweep: SweepId) -> Result<EvalReport> {
  let r = w.reader();
  let now = r.sweep_started_at(sweep)?;
  let mut report = EvalReport::default();

  // 1. Expiry first, against the sweep's start time, so a suppression that has
  //    run out is reopened before the condition is retested — and resolved in
  //    the same pass if it no longer holds.
  for (check_id, subject_ref, episode) in r.expired_suppressions(now)? {
    w.expire_suppression(&check_id, &subject_ref, episode, now, sweep)?;
    report.expired += 1;
  }

  // 2. Recompute link suggestions before anything is evaluated, not after.
  //    `suppress_if_pending_links` exists so an account overlord has just
  //    proposed a link for stays quiet until an operator has looked at it
  //    (SPEC.md section 12); computing suggestions after evaluation would make
  //    that a sweep late, and the noise it exists to prevent would have been on
  //    the board already.
  report.suggested = crate::identity::recompute_suggestions(w, sweep)?;

  let world = World::load(&r)?;
  let swept: BTreeSet<SystemId> = r
    .swept_systems(sweep)?
    .into_iter()
    .map(|(id, ..)| id)
    .collect();
  let known: Vec<(SystemId, SystemKind)> = r.known_systems()?;
  let pending: BTreeSet<EntityRef> =
    r.entities_with_pending_suggestions()?.into_iter().collect();

  // 3. Compile the pinned revisions, not the current ones.
  let mut checks = Vec::new();
  for (id, revision) in r.sweep_pins(sweep)? {
    let draft = r.check_revision(&id, revision)?;
    match compile(&draft.condition, &Schema::new(draft.applies_to)) {
      Ok(program) => checks.push((id, revision, draft, program)),
      Err(ds) => {
        report.errors.push(CheckProblem {
          check_id: id,
          subject:  None,
          message:  ds
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
        });
      }
    }
  }

  let ctx = EvalCtx::at(now);
  let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

  for (id, revision, draft, program) in &checks {
    match draft.applies_to {
      SubjectKind::Entity => {
        for entity in world.entity_refs() {
          if !entity_in_scope(draft, &entity, &world) {
            continue;
          }
          // SPEC.md section 10: entity-scoped checks for unswept
          // systems are not re-evaluated. Leaving the episode alone is
          // the point — re-confirming it against stale facts would be
          // a claim the sweep did not earn.
          if !swept.contains(&entity.system) {
            continue;
          }
          if draft.suppress_if_pending_links && pending.contains(&entity) {
            continue;
          }
          let Some(subject) = world.entity_subject(&entity) else {
            continue;
          };
          let fact_ids = world
            .attrs(&entity)
            .map(|a| vec![a.fact_id])
            .unwrap_or_default();
          apply(
            w,
            &mut report,
            &mut seen,
            &Pass {
              id,
              revision: *revision,
              draft,
              program,
              subject_ref: SubjectRef::Entity(entity.clone()),
              // An entity ref is the account's own key. It absorbs
              // nothing and is never retired: linking moves who the
              // account belongs to, not what it is.
              absorbed: Vec::new(),
              stale: false,
              now,
              sweep,
              fact_ids,
            },
            &subject,
            &ctx,
          )?;
        }
      }

      SubjectKind::Person => {
        // A person check that reaches into a system this sweep did not
        // visit is answered from last-known state. It is still worth
        // answering — the alternative is a board that goes blank during
        // a partial sweep — but it is marked rather than silently
        // trusted (SPEC.md section 10).
        let stale = references_unswept(program, &known, &swept);

        for uid in world.person_uids() {
          if !person_in_scope(draft, &uid, &world) {
            continue;
          }
          if draft.suppress_if_pending_links
            && world.member_refs(&uid).iter().any(|e| pending.contains(e))
          {
            continue;
          }
          let Some(subject) = world.person_subject(&uid) else {
            continue;
          };
          let absorbed = r
            .retired_uids(&uid)?
            .into_iter()
            .map(|retired| SubjectRef::Person(retired).to_string())
            .collect();
          apply(
            w,
            &mut report,
            &mut seen,
            &Pass {
              id,
              revision: *revision,
              draft,
              program,
              subject_ref: SubjectRef::Person(uid.clone()),
              absorbed,
              stale,
              now,
              sweep,
              fact_ids: world.person_fact_ids(&uid),
            },
            &subject,
            &ctx,
          )?;
        }
      }
    }
  }

  // 4. Standing episodes this pass did not re-confirm.
  reconcile(w, &mut report, &seen, &world, &swept, &checks, now, sweep)?;

  w.recompute_scores()?;
  Ok(report)
}

/// Everything about one (check, subject) evaluation.
struct Pass<'a> {
  id:          &'a CheckId,
  revision:    Revision,
  draft:       &'a CheckDraft,
  program:     &'a Program,
  subject_ref: SubjectRef,
  /// Subject refs this subject has absorbed: the implicit singleton
  /// persons of the accounts it now holds, and the uids merged into it.
  /// Episodes opened under those refs are this subject's episodes now
  /// (SPEC.md section 12).
  absorbed:    Vec<String>,
  stale:       bool,
  now:         Timestamp,
  sweep:       SweepId,
  fact_ids:    Vec<i64>,
}

fn apply(
  w: &Writer<'_>,
  report: &mut EvalReport,
  seen: &mut BTreeSet<(String, String)>,
  pass: &Pass<'_>,
  subject: &dyn Subject,
  ctx: &EvalCtx,
) -> Result<()> {
  let mut outcome = eval(pass.program, subject, ctx);
  let subject_ref = pass.subject_ref.to_string();
  let check_id = pass.id.to_string();
  seen.insert((check_id.clone(), subject_ref.clone()));
  report.subjects_evaluated += 1;

  if !outcome.ambiguous.is_empty() {
    report.ambiguous += 1;
  }
  outcome.evidence.attribute(&pass.fact_ids);

  // Everything still standing against this subject, under its own ref or
  // under one it has absorbed. An episode is never rewritten onto the
  // surviving ref (SPEC.md section 12 is explicit that history resolves
  // through a retired uid rather than being edited), so carrying it
  // means continuing to write to the row where it already lives.
  let mut refs = Vec::with_capacity(1 + pass.absorbed.len());
  refs.push(subject_ref.clone());
  for absorbed in &pass.absorbed {
    seen.insert((check_id.clone(), absorbed.clone()));
    refs.push(absorbed.clone());
  }
  let mut standing = w.standing_episodes_among(&check_id, &refs)?;

  // Oldest first. Whichever episode has been open longest is the one
  // that continues: it holds the operator's acknowledgement, and it is
  // the one the board has been ranking by "ignored longest" (SPEC.md
  // section 8). The rest described the same person all along and are
  // closed as merged, which is also what stops a promotion from
  // double-counting a finding it now holds twice.
  let carried = (!standing.is_empty()).then(|| standing.remove(0));
  for extra in &standing {
    w.resolve_episode(
      &check_id,
      &extra.subject_ref,
      extra.episode,
      pass.now,
      ResolveReason::Merged,
      Some(pass.sweep),
    )?;
    report.resolved += 1;
  }
  if carried
    .as_ref()
    .is_some_and(|e| e.subject_ref != subject_ref)
  {
    report.carried += 1;
  }

  let facts = EpisodeFacts {
    check_id:   check_id.clone(),
    subject:    pass.subject_ref.clone(),
    severity:   pass.draft.severity,
    weight:     pass.draft.effective_weight(),
    revision:   pass.revision.0,
    sweep:      pass.sweep,
    at:         pass.now,
    evidence:   outcome.evidence,
    stale:      pass.stale,
    ambiguous:  !outcome.ambiguous.is_empty(),
    eval_error: outcome.outcome.as_ref().err().map(ToString::to_string),
  };

  match &outcome.outcome {
    // Only `true` opens a violation.
    Ok(Tri::True) => match &carried {
      Some(Episode {
        subject_ref,
        episode,
        ..
      }) => {
        w.touch_episode(subject_ref, *episode, &facts)?;
        report.standing += 1;
      }
      None => {
        let episode = w.open_episode(&facts)?;
        if episode > 1 {
          report.regressed += 1;
        } else {
          report.opened += 1;
        }
      }
    },

    Ok(Tri::False | Tri::Null) => {
      if let Some(e) = &carried {
        w.resolve_episode(
          &check_id,
          &e.subject_ref,
          e.episode,
          pass.now,
          ResolveReason::ConditionCleared,
          Some(pass.sweep),
        )?;
        report.resolved += 1;
      }
    }

    // The condition could not be answered for this subject. Neither
    // opening nor resolving would be honest, so the episode is left
    // exactly as it was and the failure is recorded against it.
    Err(err) => {
      report.errors.push(CheckProblem {
        check_id: pass.id.clone(),
        subject:  Some(subject_ref.clone()),
        message:  err.to_string(),
      });
      if let Some(e) = &carried {
        w.touch_episode(&e.subject_ref, e.episode, &facts)?;
        report.standing += 1;
      }
    }
  }
  Ok(())
}

/// Close episodes whose subject went away, and leave alone the ones this
/// sweep simply did not look at.
#[allow(clippy::too_many_arguments)]
fn reconcile(
  w: &Writer<'_>,
  report: &mut EvalReport,
  seen: &BTreeSet<(String, String)>,
  world: &World,
  swept: &BTreeSet<SystemId>,
  checks: &[(CheckId, Revision, CheckDraft, Program)],
  now: Timestamp,
  sweep: SweepId,
) -> Result<()> {
  let pinned: BTreeMap<String, &CheckDraft> = checks
    .iter()
    .map(|(id, _, draft, _)| (id.to_string(), draft))
    .collect();

  for (check_id, subject_ref, episode) in w.standing_episodes()? {
    if seen.contains(&(check_id.clone(), subject_ref.clone())) {
      continue;
    }
    // Not pinned: the check is disabled, and disabling already resolved
    // its violations. Re-resolving here would double the history.
    let Some(draft) = pinned.get(&check_id) else {
      continue;
    };

    let subject: SubjectRef = subject_ref.parse()?;
    let reason = match &subject {
      SubjectRef::Entity(entity) => {
        if !swept.contains(&entity.system) {
          continue; // not re-evaluated, so nothing is known
        }
        if world.attrs(entity).is_none() {
          // The latest fact is a tombstone, or the entity was never
          // seen again: absent entities are out of scope entirely.
          ResolveReason::SubjectAbsent
        } else {
          ResolveReason::OutOfScope
        }
      }
      SubjectRef::Person(uid) => {
        // Through aliases, so an episode left over from a promoted
        // implicit person or a retired uid is judged by the person it
        // means today. The pass above carries those it could; one that
        // reaches here belongs to somebody real whom the check simply
        // no longer selects.
        let resolved = w.reader().resolve_person(uid)?;
        if world.has_person(&resolved) {
          ResolveReason::OutOfScope
        } else {
          ResolveReason::SubjectAbsent
        }
      }
    };
    let _ = draft;

    w.resolve_episode(
      &check_id,
      &subject_ref,
      episode,
      now,
      reason,
      Some(sweep),
    )?;
    report.resolved += 1;
  }
  Ok(())
}

fn entity_in_scope(
  draft: &CheckDraft,
  entity: &EntityRef,
  world: &World,
) -> bool {
  if !draft.entity_types.is_empty()
    && !draft.entity_types.contains(&entity.entity_type)
  {
    return false;
  }
  if draft.systems.is_empty() {
    return true;
  }
  let Some(attrs) = world.attrs(entity) else {
    return false;
  };
  draft
    .systems
    .iter()
    .any(|s| s.matches(&entity.system, attrs.normalized.system_kind))
}

/// A person is in scope when it holds at least one entity the check's
/// scope admits. An unrestricted check admits every person, including
/// the implicit singletons.
fn person_in_scope(draft: &CheckDraft, uid: &PersonUid, world: &World) -> bool {
  if draft.systems.is_empty() && draft.entity_types.is_empty() {
    return true;
  }
  world
    .member_refs(uid)
    .iter()
    .any(|e| entity_in_scope(draft, e, world))
}

/// Whether a person-scoped condition reaches into a system this sweep
/// did not cover.
fn references_unswept(
  program: &Program,
  known: &[(SystemId, SystemKind)],
  swept: &BTreeSet<SystemId>,
) -> bool {
  program.selectors().iter().any(|sel| {
    known
      .iter()
      .any(|(id, kind)| sel.matches(id, *kind) && !swept.contains(id))
  })
}
