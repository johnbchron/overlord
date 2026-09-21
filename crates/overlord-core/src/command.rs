use serde::{Deserialize, Serialize};

use crate::{
  check::{CheckDraft, DryrunSample},
  ids::{
    Actor, CheckId, EntityRef, EntityType, PersonUid, Revision, Seq,
    SubjectRef, SystemId, SystemKind,
  },
  time::Timestamp,
  violation::SuppressReason,
};

/// The operator actions of SPEC.md section 6.2.
///
/// Every identifier a command creates is carried *in the payload*, never
/// minted while applying it. Replay must reproduce the live projections
/// exactly, and a uid generated during replay would differ from the one
/// generated live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "args")]
pub enum CommandKind {
  #[serde(rename = "person.create")]
  PersonCreate {
    person_uid:   PersonUid,
    display_name: Option<String>,
  },
  #[serde(rename = "person.link")]
  PersonLink {
    person_uid:      PersonUid,
    entity:          EntityRef,
    /// The suggestion evidence the operator confirmed, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_suggestion: Option<String>,
  },
  #[serde(rename = "person.unlink")]
  PersonUnlink {
    person_uid: PersonUid,
    entity:     EntityRef,
  },
  #[serde(rename = "person.merge")]
  PersonMerge {
    /// The uid the operator chose to keep.
    surviving: PersonUid,
    /// Becomes a permanent alias, so prior violations, acknowledgements
    /// and suppressions resolve through it rather than being rewritten.
    retired:   PersonUid,
  },
  #[serde(rename = "person.split")]
  PersonSplit {
    /// Keeps the history.
    from:     PersonUid,
    new_uid:  PersonUid,
    entities: Vec<EntityRef>,
  },
  #[serde(rename = "person.set_primary")]
  PersonSetPrimary {
    person_uid:  PersonUid,
    system_kind: SystemKind,
    entity:      EntityRef,
  },

  #[serde(rename = "violation.acknowledge")]
  ViolationAcknowledge {
    check_id: CheckId,
    subject:  SubjectRef,
  },
  #[serde(rename = "violation.suppress")]
  ViolationSuppress {
    check_id: CheckId,
    subject:  SubjectRef,
    reason:   SuppressReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    until:    Option<Timestamp>,
  },
  #[serde(rename = "violation.false_positive")]
  ViolationFalsePositive {
    check_id: CheckId,
    subject:  SubjectRef,
  },
  /// Undo an overlay; the violation is recomputed from its condition.
  #[serde(rename = "violation.revoke")]
  ViolationRevoke {
    check_id: CheckId,
    subject:  SubjectRef,
  },

  /// Creates the check, or appends a new revision.
  #[serde(rename = "check.upsert")]
  CheckUpsert { draft: CheckDraft },
  /// Records the result of a dry-run for one `(check id, revision)`.
  /// `check.enable` is rejected without one.
  #[serde(rename = "check.dryrun")]
  CheckDryrun {
    check_id:    CheckId,
    revision:    Revision,
    match_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    samples:     Vec<DryrunSample>,
  },
  #[serde(rename = "check.enable")]
  CheckEnable {
    check_id: CheckId,
    revision: Revision,
  },
  #[serde(rename = "check.disable")]
  CheckDisable { check_id: CheckId },

  #[serde(rename = "normalization.upsert")]
  NormalizationUpsert {
    ruleset_id:  String,
    system_kind: SystemKind,
    version:     String,
    body:        serde_json::Value,
  },

  /// Declare which entity types are not people (SPEC.md section 6.4).
  ///
  /// Authored in the configuration file, but recorded here because it
  /// decides which subjects evaluation sees, and section 13 admits no
  /// input to evaluation outside the two streams. The whole set travels
  /// in each command rather than a delta: the projection is current
  /// state, so a command that carried only what changed would leave
  /// replay dependent on reading every prior one correctly.
  ///
  /// Sorted and deduplicated on construction, so an unchanged policy
  /// spelled in a different order is recognisably unchanged and appends
  /// nothing.
  #[serde(rename = "identity.policy")]
  IdentityPolicy {
    non_person_entity_types: Vec<EntityType>,
  },
}

impl CommandKind {
  /// An [`Self::IdentityPolicy`] with its set put in canonical order.
  ///
  /// Sorting and deduplicating here rather than at the call site is
  /// what lets the caller decide "has this changed?" by comparing the
  /// set to the projection: two spellings of the same policy must not
  /// append a command, or every sweep would append one.
  #[must_use]
  pub fn identity_policy(types: impl IntoIterator<Item = EntityType>) -> Self {
    let mut v: Vec<EntityType> = types.into_iter().collect();
    v.sort();
    v.dedup();
    Self::IdentityPolicy {
      non_person_entity_types: v,
    }
  }

  /// The `command.kind` column value.
  #[must_use]
  pub fn tag(&self) -> &'static str {
    match self {
      Self::PersonCreate { .. } => "person.create",
      Self::PersonLink { .. } => "person.link",
      Self::PersonUnlink { .. } => "person.unlink",
      Self::PersonMerge { .. } => "person.merge",
      Self::PersonSplit { .. } => "person.split",
      Self::PersonSetPrimary { .. } => "person.set_primary",
      Self::ViolationAcknowledge { .. } => "violation.acknowledge",
      Self::ViolationSuppress { .. } => "violation.suppress",
      Self::ViolationFalsePositive { .. } => "violation.false_positive",
      Self::ViolationRevoke { .. } => "violation.revoke",
      Self::CheckUpsert { .. } => "check.upsert",
      Self::CheckDryrun { .. } => "check.dryrun",
      Self::CheckEnable { .. } => "check.enable",
      Self::CheckDisable { .. } => "check.disable",
      Self::NormalizationUpsert { .. } => "normalization.upsert",
      Self::IdentityPolicy { .. } => "identity.policy",
    }
  }

  /// The `command.subject` column value: what this command is about.
  /// Indexed for "show me everything that touched this subject".
  #[must_use]
  pub fn subject(&self) -> Option<String> {
    match self {
      Self::PersonCreate { person_uid, .. }
      | Self::PersonLink { person_uid, .. }
      | Self::PersonUnlink { person_uid, .. }
      | Self::PersonSetPrimary { person_uid, .. } => {
        Some(person_uid.to_string())
      }
      Self::PersonMerge { surviving, .. } => Some(surviving.to_string()),
      Self::PersonSplit { from, .. } => Some(from.to_string()),
      Self::ViolationAcknowledge { check_id, subject }
      | Self::ViolationSuppress {
        check_id, subject, ..
      }
      | Self::ViolationFalsePositive { check_id, subject }
      | Self::ViolationRevoke { check_id, subject } => {
        Some(format!("{check_id}@{subject}"))
      }
      Self::CheckUpsert { draft } => Some(draft.id.to_string()),
      Self::CheckDryrun { check_id, .. }
      | Self::CheckEnable { check_id, .. }
      | Self::CheckDisable { check_id } => Some(check_id.to_string()),
      Self::NormalizationUpsert { ruleset_id, .. } => Some(ruleset_id.clone()),
      // A fixed subject rather than none: the policy is a single
      // standing object, and "everything that touched it" is the
      // question the `command_subject` index exists to answer.
      Self::IdentityPolicy { .. } => Some("identity.policy".to_owned()),
    }
  }

  /// Split into the `(kind, args)` column pair.
  ///
  /// # Errors
  /// Only if a payload contains a value serde cannot represent as JSON,
  /// which the type system makes unreachable in practice.
  pub fn to_parts(&self) -> serde_json::Result<(String, serde_json::Value)> {
    let mut v = serde_json::to_value(self)?;
    let args = v
      .get_mut("args")
      .map_or(serde_json::Value::Null, serde_json::Value::take);
    Ok((self.tag().to_owned(), args))
  }

  /// Rebuild from the `(kind, args)` column pair.
  ///
  /// # Errors
  /// If the tag is unknown or the payload does not match its shape —
  /// which means the store holds a command this binary is too old to
  /// understand, and replay must stop rather than skip it.
  pub fn from_parts(
    kind: &str,
    args: serde_json::Value,
  ) -> serde_json::Result<Self> {
    serde_json::from_value(serde_json::json!({ "kind": kind, "args": args }))
  }

  /// Whether the projection layer treats two of these as the same action.
  #[must_use]
  pub fn touches_system(&self) -> Option<&SystemId> {
    match self {
      Self::PersonLink { entity, .. }
      | Self::PersonUnlink { entity, .. }
      | Self::PersonSetPrimary { entity, .. } => Some(&entity.system),
      _ => None,
    }
  }
}

/// A command about to be appended.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCommand {
  pub at:              Timestamp,
  pub actor:           Actor,
  pub kind:            CommandKind,
  pub note:            Option<String>,
  /// Client-supplied; a duplicate submission is a no-op returning the
  /// original command id (SPEC.md section 6.2).
  pub idempotency_key: Option<String>,
  /// Groups multi-step actions, such as a merge.
  pub batch_id:        Option<String>,
}

impl NewCommand {
  #[must_use]
  pub fn new(
    actor: impl Into<Actor>,
    kind: CommandKind,
    at: Timestamp,
  ) -> Self {
    Self {
      at,
      actor: actor.into(),
      kind,
      note: None,
      idempotency_key: None,
      batch_id: None,
    }
  }

  #[must_use]
  pub fn with_note(mut self, note: impl Into<String>) -> Self {
    self.note = Some(note.into());
    self
  }

  #[must_use]
  pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
    self.idempotency_key = Some(key.into());
    self
  }

  #[must_use]
  pub fn with_batch(mut self, batch: impl Into<String>) -> Self {
    self.batch_id = Some(batch.into());
    self
  }
}

/// A command as stored: a [`NewCommand`] plus its stream position.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandRecord {
  pub id:              i64,
  pub seq:             Seq,
  pub at:              Timestamp,
  pub actor:           Actor,
  pub kind:            CommandKind,
  pub note:            Option<String>,
  pub idempotency_key: Option<String>,
  pub batch_id:        Option<String>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    check::CheckDraft, ids::SubjectKind, severity::Severity,
    violation::SuppressReason,
  };

  fn round_trip(k: &CommandKind) -> CommandKind {
    let (tag, args) = k.to_parts().unwrap();
    assert_eq!(tag, k.tag());
    CommandKind::from_parts(&tag, args).unwrap()
  }

  #[test]
  fn every_kind_round_trips_through_its_columns() {
    let entity = EntityRef::new("gws-prod", "user", "ada@example.com");
    let uid = PersonUid::new("01J0ABCD");
    let subject = SubjectRef::Entity(entity.clone());
    let kinds = vec![
      CommandKind::PersonCreate {
        person_uid:   uid.clone(),
        display_name: Some("Ada".to_owned()),
      },
      CommandKind::PersonLink {
        person_uid:      uid.clone(),
        entity:          entity.clone(),
        from_suggestion: Some("exact-email".to_owned()),
      },
      CommandKind::PersonUnlink {
        person_uid: uid.clone(),
        entity:     entity.clone(),
      },
      CommandKind::PersonMerge {
        surviving: uid.clone(),
        retired:   PersonUid::new("01J0WXYZ"),
      },
      CommandKind::PersonSplit {
        from:     uid.clone(),
        new_uid:  PersonUid::new("01J0SPLT"),
        entities: vec![entity.clone()],
      },
      CommandKind::PersonSetPrimary {
        person_uid: uid,
        system_kind: SystemKind::Idp,
        entity,
      },
      CommandKind::ViolationAcknowledge {
        check_id: CheckId::new("idp-mfa-missing"),
        subject:  subject.clone(),
      },
      CommandKind::ViolationSuppress {
        check_id: CheckId::new("idp-mfa-missing"),
        subject:  subject.clone(),
        reason:   SuppressReason::AcceptedRisk,
        until:    Some("2026-06-01T00:00:00Z".parse().unwrap()),
      },
      CommandKind::ViolationFalsePositive {
        check_id: CheckId::new("c"),
        subject:  subject.clone(),
      },
      CommandKind::ViolationRevoke {
        check_id: CheckId::new("c"),
        subject,
      },
      CommandKind::CheckUpsert {
        draft: CheckDraft {
          id: CheckId::new("c"),
          name: "C".to_owned(),
          description: None,
          rationale: None,
          remediation: None,
          references: vec![],
          severity: Severity::High,
          weight: None,
          applies_to: SubjectKind::Entity,
          systems: vec![],
          entity_types: vec![],
          condition: "not mfa_enrolled".to_owned(),
          suppress_if_pending_links: false,
        },
      },
      CommandKind::CheckDryrun {
        check_id:    CheckId::new("c"),
        revision:    Revision::FIRST,
        match_count: 3,
        samples:     vec![],
      },
      CommandKind::CheckEnable {
        check_id: CheckId::new("c"),
        revision: Revision::FIRST,
      },
      CommandKind::CheckDisable {
        check_id: CheckId::new("c"),
      },
      CommandKind::NormalizationUpsert {
        ruleset_id:  "gworkspace-default".to_owned(),
        system_kind: SystemKind::Workspace,
        version:     "3".to_owned(),
        body:        serde_json::json!({"status": {"suspended": "suspended"}}),
      },
      CommandKind::identity_policy([EntityType::new("phone")]),
    ];
    for k in &kinds {
      assert_eq!(&round_trip(k), k, "{}", k.tag());
      assert!(k.subject().is_some(), "{} has no subject", k.tag());
    }
  }

  #[test]
  fn an_identity_policy_is_sorted_and_deduplicated() {
    // The caller decides "has this changed?" by comparing the set to
    // the projection. Two spellings of one policy must compare equal,
    // or every invocation would append a command and the stream would
    // fill with changes that changed nothing.
    let a = CommandKind::identity_policy([
      EntityType::new("phone"),
      EntityType::new("device"),
      EntityType::new("phone"),
    ]);
    let b = CommandKind::identity_policy([
      EntityType::new("device"),
      EntityType::new("phone"),
    ]);
    assert_eq!(a, b);
    let CommandKind::IdentityPolicy {
      non_person_entity_types,
    } = &a
    else {
      panic!("wrong variant");
    };
    assert_eq!(non_person_entity_types, &[
      EntityType::new("device"),
      EntityType::new("phone")
    ]);
  }

  #[test]
  fn an_unknown_kind_fails_rather_than_being_skipped() {
    let e = CommandKind::from_parts("person.teleport", serde_json::json!({}));
    assert!(e.is_err());
  }

  #[test]
  fn person_create_carries_its_uid_so_replay_is_deterministic() {
    let k = CommandKind::PersonCreate {
      person_uid:   PersonUid::generate(),
      display_name: None,
    };
    let (_, args) = k.to_parts().unwrap();
    assert!(args.get("person_uid").is_some());
  }
}
