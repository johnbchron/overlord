//! Applying commands to the projections.
//!
//! Every function here is deterministic given the command stream: the
//! same commands in the same order produce the same rows. That is what
//! makes [`crate::rebuild`] a real contract rather than an approximation,
//! so nothing in this module may read a clock or mint an identifier.

use overlord_core::{
  CheckDraft, CommandKind, EntityRef, NewCommand, PersonUid, Revision, Seq,
  SubjectRef, Timestamp, ViolationEventKind, ViolationState,
};
use rusqlite::{OptionalExtension, params};

use crate::{
  db::Writer,
  error::{Result, StoreError},
};

/// The outcome of appending a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
  pub id: i64,
  pub seq: Seq,
  /// The idempotency key had already been used; nothing was appended and
  /// no projection moved. The id is the original command's.
  pub duplicate: bool,
}

impl Writer<'_> {
  /// Validate, append and project one command, in this transaction.
  ///
  /// # Errors
  /// [`StoreError::Rejected`] if the command contradicts something only
  /// the store can see — enabling a check with no dry-run, acting on a
  /// violation that does not exist — or a SQLite failure.
  pub fn append_command(&self, cmd: &NewCommand) -> Result<Applied> {
    if let Some(key) = &cmd.idempotency_key
      && let Some((id, seq)) = self.command_by_key(key)?
    {
      return Ok(Applied {
        id,
        seq: Seq(seq),
        duplicate: true,
      });
    }

    let (kind, args) = cmd.kind.to_parts()?;
    let seq = self.take_seq(1)?;
    self.conn().execute(
      "INSERT INTO command (
         seq, at, actor, kind, subject, args, note, idempotency_key,
         batch_id)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
      params![
        seq,
        cmd.at.to_string(),
        cmd.actor.as_str(),
        kind,
        cmd.kind.subject(),
        serde_json::to_string(&args)?,
        cmd.note.as_deref(),
        cmd.idempotency_key.as_deref(),
        cmd.batch_id.as_deref(),
      ],
    )?;
    let id = self.conn().last_insert_rowid();
    self.project_command(id, cmd.at, &cmd.kind)?;
    Ok(Applied {
      id,
      seq: Seq(seq),
      duplicate: false,
    })
  }

  fn command_by_key(&self, key: &str) -> Result<Option<(i64, i64)>> {
    Ok(
      self
        .conn()
        .query_row(
          "SELECT id, seq FROM command WHERE idempotency_key = ?1",
          [key],
          |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?,
    )
  }

  /// Apply one already-appended command to the projections.
  ///
  /// Public because replay calls it directly, with the command's own id
  /// and recorded time.
  ///
  /// # Errors
  /// As [`Self::append_command`].
  pub fn project_command(
    &self,
    id: i64,
    at: Timestamp,
    kind: &CommandKind,
  ) -> Result<()> {
    match kind {
      CommandKind::PersonCreate {
        person_uid,
        display_name,
      } => self.upsert_person(person_uid, display_name.as_deref(), false, id),

      CommandKind::PersonLink {
        person_uid, entity, ..
      } => {
        // A person the operator links to may not have been created by an
        // explicit command: confirming a suggestion creates it.
        self.upsert_person(person_uid, None, false, id)?;
        self.conn().execute(
          "INSERT INTO link (
             system, entity_type, entity_key, person_uid, command_id)
           VALUES (?1, ?2, ?3, ?4, ?5)
           ON CONFLICT (system, entity_type, entity_key) DO UPDATE SET
             person_uid = excluded.person_uid,
             command_id = excluded.command_id",
          params![
            entity.system.as_str(),
            entity.entity_type.as_str(),
            entity.entity_key.as_str(),
            person_uid.as_str(),
            id,
          ],
        )?;
        Ok(())
      }

      CommandKind::PersonUnlink { entity, .. } => {
        self.delete_link(entity)?;
        Ok(())
      }

      CommandKind::PersonMerge { surviving, retired } => {
        self.upsert_person(surviving, None, false, id)?;
        // The retired uid becomes a permanent alias. Prior violations,
        // acknowledgements and suppressions keep pointing at it and
        // resolve through the alias, rather than being rewritten
        // (SPEC.md section 12).
        self.conn().execute(
          "INSERT INTO person_alias (retired_uid, surviving_uid, merged_cmd)
           VALUES (?1, ?2, ?3)
           ON CONFLICT (retired_uid) DO UPDATE SET
             surviving_uid = excluded.surviving_uid,
             merged_cmd = excluded.merged_cmd",
          params![retired.as_str(), surviving.as_str(), id],
        )?;
        self.conn().execute(
          "UPDATE link SET person_uid = ?2, command_id = ?3
             WHERE person_uid = ?1",
          params![retired.as_str(), surviving.as_str(), id],
        )?;
        self.conn().execute(
          "UPDATE link_primary SET person_uid = ?2, command_id = ?3
             WHERE person_uid = ?1",
          params![retired.as_str(), surviving.as_str(), id],
        )?;
        // Any alias that pointed at the retired uid now points at the
        // survivor, so a chain of merges still resolves in one hop.
        self.conn().execute(
          "UPDATE person_alias SET surviving_uid = ?2
             WHERE surviving_uid = ?1",
          params![retired.as_str(), surviving.as_str()],
        )?;
        self.conn().execute(
          "DELETE FROM person WHERE person_uid = ?1",
          [retired.as_str()],
        )?;
        Ok(())
      }

      CommandKind::PersonSplit {
        new_uid, entities, ..
      } => {
        // The original keeps the history; the new uid is traceable to
        // the command that created it.
        self.upsert_person(new_uid, None, false, id)?;
        for entity in entities {
          self.conn().execute(
            "UPDATE link SET person_uid = ?4, command_id = ?5
               WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3",
            params![
              entity.system.as_str(),
              entity.entity_type.as_str(),
              entity.entity_key.as_str(),
              new_uid.as_str(),
              id,
            ],
          )?;
        }
        Ok(())
      }

      CommandKind::PersonSetPrimary {
        person_uid,
        system_kind,
        entity,
      } => {
        self.conn().execute(
          "INSERT INTO link_primary (
             person_uid, system_kind, system, entity_type, entity_key,
             command_id)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6)
           ON CONFLICT (person_uid, system_kind) DO UPDATE SET
             system = excluded.system,
             entity_type = excluded.entity_type,
             entity_key = excluded.entity_key,
             command_id = excluded.command_id",
          params![
            person_uid.as_str(),
            system_kind.as_str(),
            entity.system.as_str(),
            entity.entity_type.as_str(),
            entity.entity_key.as_str(),
            id,
          ],
        )?;
        Ok(())
      }

      CommandKind::ViolationAcknowledge { check_id, subject } => self.overlay(
        check_id.as_str(),
        subject,
        ViolationState::Acknowledged,
        ViolationEventKind::Acknowledged,
        None,
        None,
        id,
        at,
      ),

      CommandKind::ViolationSuppress {
        check_id,
        subject,
        reason,
        until,
      } => self.overlay(
        check_id.as_str(),
        subject,
        ViolationState::Suppressed,
        ViolationEventKind::Suppressed,
        Some(reason.as_str()),
        until.map(|t| t.to_string()),
        id,
        at,
      ),

      CommandKind::ViolationFalsePositive { check_id, subject } => self
        .overlay(
          check_id.as_str(),
          subject,
          ViolationState::FalsePositive,
          ViolationEventKind::FalsePositive,
          None,
          None,
          id,
          at,
        ),

      // Undo an overlay. The state returns to `open`: the condition
      // still held when the episode was last evaluated, and the next
      // sweep will resolve it if that is no longer so.
      CommandKind::ViolationRevoke { check_id, subject } => self.overlay(
        check_id.as_str(),
        subject,
        ViolationState::Open,
        ViolationEventKind::Revoked,
        None,
        None,
        id,
        at,
      ),

      CommandKind::CheckUpsert { draft } => {
        self.upsert_check(draft, id, at)?;
        Ok(())
      }

      CommandKind::CheckDryrun {
        check_id,
        revision,
        match_count,
        samples,
      } => {
        let known: Option<i64> = self
          .conn()
          .query_row(
            "SELECT 1 FROM check_revision WHERE check_id = ?1
               AND revision = ?2",
            params![check_id.as_str(), revision.0],
            |r| r.get(0),
          )
          .optional()?;
        if known.is_none() {
          return Err(StoreError::rejected(format!(
            "check {check_id} has no revision {revision}"
          )));
        }
        self.conn().execute(
          "INSERT INTO check_dryrun (
             check_id, revision, at, match_count, samples, command_id)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6)
           ON CONFLICT (check_id, revision) DO UPDATE SET
             at = excluded.at,
             match_count = excluded.match_count,
             samples = excluded.samples,
             command_id = excluded.command_id",
          params![
            check_id.as_str(),
            revision.0,
            at.to_string(),
            i64::try_from(*match_count).unwrap_or(i64::MAX),
            serde_json::to_string(samples)?,
            id,
          ],
        )?;
        Ok(())
      }

      CommandKind::CheckEnable { check_id, revision } => {
        // SPEC.md section 7: dry-run before enable is enforced. The
        // store is where it is enforced, so the CLI and the UI cannot
        // each forget it separately.
        let dry: Option<i64> = self
          .conn()
          .query_row(
            "SELECT 1 FROM check_dryrun WHERE check_id = ?1
               AND revision = ?2",
            params![check_id.as_str(), revision.0],
            |r| r.get(0),
          )
          .optional()?;
        if dry.is_none() {
          return Err(StoreError::rejected(format!(
            "check {check_id} revision {revision} has no dry-run; run one \
             before enabling it"
          )));
        }
        let head: Option<u32> = self
          .conn()
          .query_row(
            "SELECT revision FROM check_head WHERE check_id = ?1",
            [check_id.as_str()],
            |r| r.get(0),
          )
          .optional()?;
        match head {
          None => {
            return Err(StoreError::not_found(format!("check {check_id}")));
          }
          Some(h) if h != revision.0 => {
            return Err(StoreError::rejected(format!(
              "check {check_id} is at revision {h}, not {revision}"
            )));
          }
          Some(_) => {}
        }
        self.conn().execute(
          "UPDATE check_head
             SET enabled = 1, enabled_rev = ?2, enabled_cmd = ?3
           WHERE check_id = ?1",
          params![check_id.as_str(), revision.0, id],
        )?;
        Ok(())
      }

      CommandKind::CheckDisable { check_id } => {
        let n = self.conn().execute(
          "UPDATE check_head SET enabled = 0, enabled_cmd = ?2
             WHERE check_id = ?1",
          params![check_id.as_str(), id],
        )?;
        if n == 0 {
          return Err(StoreError::not_found(format!("check {check_id}")));
        }
        // SPEC.md section 6.5: disabling resolves the check's open
        // violations. Doing it here rather than at the next sweep means
        // the board is honest the moment the operator acts.
        self.resolve_check_violations(check_id.as_str(), at, id)?;
        Ok(())
      }

      CommandKind::NormalizationUpsert {
        ruleset_id,
        system_kind,
        version,
        body,
      } => {
        self.conn().execute(
          "INSERT INTO normalization_ruleset (
             ruleset_id, version, system_kind, body, command_id)
           VALUES (?1, ?2, ?3, ?4, ?5)
           ON CONFLICT (ruleset_id, version) DO UPDATE SET
             system_kind = excluded.system_kind,
             body = excluded.body,
             command_id = excluded.command_id",
          params![
            ruleset_id,
            version,
            system_kind.as_str(),
            serde_json::to_string(body)?,
            id,
          ],
        )?;
        Ok(())
      }
    }
  }

  fn upsert_person(
    &self,
    uid: &PersonUid,
    display_name: Option<&str>,
    implicit: bool,
    cmd: i64,
  ) -> Result<()> {
    self.conn().execute(
      "INSERT INTO person (person_uid, display_name, implicit, created_cmd)
       VALUES (?1, ?2, ?3, ?4)
       ON CONFLICT (person_uid) DO UPDATE SET
         display_name = coalesce(excluded.display_name, person.display_name),
         implicit = excluded.implicit",
      params![uid.as_str(), display_name, i32::from(implicit), cmd],
    )?;
    Ok(())
  }

  fn delete_link(&self, entity: &EntityRef) -> Result<()> {
    self.conn().execute(
      "DELETE FROM link
        WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3",
      params![
        entity.system.as_str(),
        entity.entity_type.as_str(),
        entity.entity_key.as_str(),
      ],
    )?;
    self.conn().execute(
      "DELETE FROM link_primary
        WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3",
      params![
        entity.system.as_str(),
        entity.entity_type.as_str(),
        entity.entity_key.as_str(),
      ],
    )?;
    Ok(())
  }

  fn upsert_check(
    &self,
    draft: &CheckDraft,
    cmd: i64,
    at: Timestamp,
  ) -> Result<()> {
    let current: Option<u32> = self
      .conn()
      .query_row(
        "SELECT revision FROM check_head WHERE check_id = ?1",
        [draft.id.as_str()],
        |r| r.get(0),
      )
      .optional()?;
    let revision = current.map_or(Revision::FIRST, |r| Revision(r).next());

    let actor: String = self.conn().query_row(
      "SELECT actor FROM command WHERE id = ?1",
      [cmd],
      |r| r.get(0),
    )?;

    self.conn().execute(
      "INSERT INTO check_revision (
         check_id, revision, draft, command_id, at, actor)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
      params![
        draft.id.as_str(),
        revision.0,
        serde_json::to_string(draft)?,
        cmd,
        at.to_string(),
        actor,
      ],
    )?;

    // A new check starts disabled, and a revision never changes the
    // enabled flag: only check.enable / check.disable do (SPEC.md s7).
    self.conn().execute(
      "INSERT INTO check_head (check_id, revision, enabled)
       VALUES (?1, ?2, 0)
       ON CONFLICT (check_id) DO UPDATE SET revision = excluded.revision",
      params![draft.id.as_str(), revision.0],
    )?;
    Ok(())
  }

  /// Apply an operator overlay to a violation's current episode.
  #[allow(clippy::too_many_arguments)]
  fn overlay(
    &self,
    check_id: &str,
    subject: &SubjectRef,
    state: ViolationState,
    event: ViolationEventKind,
    suppress_reason: Option<&str>,
    suppress_until: Option<String>,
    cmd: i64,
    at: Timestamp,
  ) -> Result<()> {
    let subject_ref = subject.to_string();
    let Some((episode, revision)) =
      self.current_episode(check_id, &subject_ref)?
    else {
      return Err(StoreError::not_found(format!(
        "violation {check_id} on {subject_ref}"
      )));
    };

    // The revision the overlay was applied under is recorded so the UI
    // can flag an acknowledgement made against a rule that has since
    // been rewritten (SPEC.md section 6.5).
    let (overlay_cmd, overlay_rev) = if state == ViolationState::Open {
      (None, None)
    } else {
      (Some(cmd), Some(revision))
    };

    self.conn().execute(
      "UPDATE violation
          SET state = ?4, overlay_cmd = ?5, overlay_rev = ?6,
              suppress_reason = ?7, suppress_until = ?8
        WHERE check_id = ?1 AND subject_ref = ?2 AND episode = ?3",
      params![
        check_id,
        &subject_ref,
        episode,
        state.as_str(),
        overlay_cmd,
        overlay_rev,
        suppress_reason,
        suppress_until,
      ],
    )?;

    self.record_event(
      check_id,
      &subject_ref,
      episode,
      at,
      event,
      None,
      Some(cmd),
      None,
    )
  }

  /// The latest episode of a violation, with the check revision in
  /// effect when it opened.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn current_episode(
    &self,
    check_id: &str,
    subject_ref: &str,
  ) -> Result<Option<(i64, u32)>> {
    Ok(
      self
        .conn()
        .query_row(
          "SELECT episode, revision_open FROM violation
            WHERE check_id = ?1 AND subject_ref = ?2
            ORDER BY episode DESC LIMIT 1",
          params![check_id, subject_ref],
          |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?,
    )
  }

  /// Append one entry to a violation's history.
  ///
  /// # Errors
  /// On a SQLite failure.
  #[allow(clippy::too_many_arguments)]
  pub fn record_event(
    &self,
    check_id: &str,
    subject_ref: &str,
    episode: i64,
    at: Timestamp,
    kind: ViolationEventKind,
    sweep_id: Option<i64>,
    command_id: Option<i64>,
    detail: Option<&str>,
  ) -> Result<()> {
    self.conn().execute(
      "INSERT INTO violation_event (
         check_id, subject_ref, episode, at, kind, sweep_id, command_id,
         detail)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
      params![
        check_id,
        subject_ref,
        episode,
        at.to_string(),
        kind.as_str(),
        sweep_id,
        command_id,
        detail,
      ],
    )?;
    Ok(())
  }

  fn resolve_check_violations(
    &self,
    check_id: &str,
    at: Timestamp,
    cmd: i64,
  ) -> Result<()> {
    let open: Vec<(String, i64)> = {
      let mut stmt = self.conn().prepare(
        "SELECT subject_ref, episode FROM violation
          WHERE check_id = ?1 AND state != 'resolved'",
      )?;
      let rows = stmt.query_map([check_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
      rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (subject_ref, episode) in open {
      self.conn().execute(
        "UPDATE violation
            SET state = 'resolved', resolved_at = ?4,
                resolve_reason = 'check_disabled'
          WHERE check_id = ?1 AND subject_ref = ?2 AND episode = ?3",
        params![check_id, &subject_ref, episode, at.to_string()],
      )?;
      self.record_event(
        check_id,
        &subject_ref,
        episode,
        at,
        ViolationEventKind::Cleared,
        None,
        Some(cmd),
        Some("check_disabled"),
      )?;
    }
    Ok(())
  }
}
