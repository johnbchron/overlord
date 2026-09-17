//! Identity resolution (SPEC.md section 12).
//!
//! Two halves, deliberately kept apart. [`recompute_suggestions`] runs
//! inside a sweep and proposes links; everything below it is the set of
//! operator verbs that act on one. Nothing in the first half can reach
//! the second: a suggestion is read-only, is never applied
//! automatically, and emits no command on its own behalf. The operator
//! is the only thing that turns a proposal into a link.

use std::collections::{BTreeMap, BTreeSet};

use overlord_core::{
  Actor, CommandKind, EntityRef, EntityType, NewCommand, PersonUid, SweepId,
  SystemId, SystemKind, Timestamp, Value,
};
use overlord_store::{Db, Suggestion, Writer};

use crate::error::{EngineError, Result};

// --- suggestion computation -------------------------------------------

/// The signals a suggestion may be built from, strongest first.
///
/// SPEC.md section 12 asks for conservative and explainable signals, and
/// names these three. Strength ordering matters twice: an entity keeps
/// only its strongest reason for any one person, and a weaker signal is
/// never allowed to add a person a stronger one already rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Signal {
  /// The same email address on both sides.
  Email,
  /// The same value in a directory id attribute.
  DirectoryId,
  /// The same username, or the same local part of an email address.
  Username,
}

impl Signal {
  const ALL: [Self; 3] = [Self::Email, Self::DirectoryId, Self::Username];

  fn name(self) -> &'static str {
    match self {
      Self::Email => "exact-email",
      Self::DirectoryId => "directory-id",
      Self::Username => "username",
    }
  }

  /// Overlay fields that carry this signal, in the order they are
  /// preferred. A connector that maps none of them falls back to the
  /// entity key where that is meaningful.
  fn fields(self) -> &'static [&'static str] {
    match self {
      Self::Email => &["email", "primary_email"],
      Self::DirectoryId => &["employee_id", "external_id", "directory_id"],
      Self::Username => &["username", "user_name", "login"],
    }
  }
}

/// What one entity offers for one signal: the value, and where it came
/// from, so the evidence can say so.
struct Offer {
  value: String,
  field: &'static str,
}

/// Recompute every link suggestion from the state this sweep left.
///
/// Returns how many were written. Wholesale replacement, because a
/// suggestion is a claim about currently observed facts: an account that
/// no longer matches should stop being proposed, not linger.
///
/// # Errors
/// On a store failure.
pub fn recompute_suggestions(w: &Writer<'_>, sweep: SweepId) -> Result<usize> {
  let r = w.reader();

  // Every present entity, with the values it offers for each signal.
  let mut offers: BTreeMap<EntityRef, BTreeMap<Signal, Offer>> =
    BTreeMap::new();
  let mut kinds: BTreeMap<EntityRef, SystemKind> = BTreeMap::new();
  for state in r.entity_states(None)? {
    kinds.insert(state.entity.clone(), state.normalized.system_kind);
    let mut mine = BTreeMap::new();
    for signal in Signal::ALL {
      if let Some(offer) = offer_for(signal, &state) {
        mine.insert(signal, offer);
      }
    }
    offers.insert(state.entity, mine);
  }

  // Who each entity already belongs to. An unlinked one belongs to its
  // own implicit singleton person (SPEC.md section 6.4).
  let mut holder: BTreeMap<EntityRef, PersonUid> = BTreeMap::new();
  for (entity, uid) in r.links()? {
    holder.insert(entity, r.resolve_person(&uid)?);
  }

  // (signal, entity type, value) -> system -> the entities offering it.
  // Keyed by entity type because a user and a group that happen to share
  // a string are not the same person, and grouped by system because the
  // uniqueness rule below is per-system.
  let mut index: BTreeMap<
    (Signal, EntityType, String),
    BTreeMap<SystemId, Vec<EntityRef>>,
  > = BTreeMap::new();
  for (entity, mine) in &offers {
    for (signal, offer) in mine {
      index
        .entry((*signal, entity.entity_type.clone(), offer.value.clone()))
        .or_default()
        .entry(entity.system.clone())
        .or_default()
        .push(entity.clone());
    }
  }

  let mut out: Vec<Suggestion> = Vec::new();
  for (entity, mine) in &offers {
    // Only unlinked accounts are proposed. A linked one has already had
    // the operator's attention, and re-proposing it would be noise.
    if holder.contains_key(entity) {
      continue;
    }
    let mut proposed: BTreeSet<PersonUid> = BTreeSet::new();

    for signal in Signal::ALL {
      let Some(offer) = mine.get(&signal) else {
        continue;
      };
      let key = (signal, entity.entity_type.clone(), offer.value.clone());
      let Some(by_system) = index.get(&key) else {
        continue;
      };

      // The conservatism SPEC.md section 12 asks for, stated once: a
      // signal counts only when it names exactly one account on each
      // side. Two accounts in one system sharing a username have not
      // identified anybody, and section 6.4 is explicit that overlord
      // refuses to guess between candidates — a suggestion is not the
      // place to start. Requiring it in *both* directions is what keeps
      // the proposal symmetric: an operator looking at either account
      // sees the same suggestion, or neither does.
      if by_system.get(&entity.system).map(Vec::len) != Some(1) {
        continue;
      }

      for (system, candidates) in by_system {
        if system == &entity.system {
          continue;
        }
        let [other] = &candidates[..] else {
          continue;
        };
        let person = holder
          .get(other)
          .cloned()
          .unwrap_or_else(|| PersonUid::implicit(other));
        // An entity keeps only its strongest reason for a person:
        // "they share an email address" is the whole answer, and
        // "...and a username" adds nothing an operator would weigh.
        if !proposed.insert(person.clone()) {
          continue;
        }
        out.push(Suggestion {
          entity:     entity.clone(),
          person_uid: person,
          signal:     signal.name().to_owned(),
          evidence:   serde_json::json!({
            "field": offer.field,
            "value": offer.value,
            "account": other.to_string(),
            "system_kind": kinds.get(other).map(|k| k.as_str()),
            "matched_field": offers
              .get(other)
              .and_then(|o| o.get(&signal))
              .map(|o| o.field),
          }),
        });
      }
    }
  }

  w.replace_suggestions(sweep, &out)?;
  Ok(out.len())
}

/// What an entity offers for one signal, or nothing if it says nothing
/// useful.
fn offer_for(
  signal: Signal,
  state: &overlord_store::EntityState,
) -> Option<Offer> {
  for field in signal.fields() {
    if let Value::String(s) = state.normalized.get(field) {
      let s = s.trim().to_lowercase();
      if !s.is_empty() {
        return Some(Offer { value: s, field });
      }
    }
  }

  // Fallbacks, for the common case of a connector whose stable key *is*
  // the address. They are deliberately narrow: a key that is not
  // email-shaped says nothing about identity, and guessing from one
  // would produce exactly the unexplainable suggestion section 12 rules
  // out.
  let key = state.entity.entity_key.as_str().trim().to_lowercase();
  let (local, domain) = key.split_once('@')?;
  if local.is_empty() || domain.is_empty() || !domain.contains('.') {
    return None;
  }
  match signal {
    Signal::Email => Some(Offer {
      value: key,
      field: "entity_key",
    }),
    Signal::Username => Some(Offer {
      value: local.to_owned(),
      field: "entity_key",
    }),
    Signal::DirectoryId => None,
  }
}

// --- the operator verbs ------------------------------------------------

/// Link an account to a person, creating the person if this is the first
/// time the operator has named it.
///
/// This is also what confirming a suggestion emits: `from_suggestion`
/// records which signal the operator agreed with, so a rule that turns
/// out to over-propose can be found later.
///
/// # Errors
/// If the store refuses the command — an unknown account, or a uid that
/// names an unlinked account rather than a person.
pub fn link(
  db: &Db,
  actor: &Actor,
  person_uid: &PersonUid,
  entity: &EntityRef,
  from_suggestion: Option<String>,
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<()> {
  let mut cmd = NewCommand::new(
    actor.clone(),
    CommandKind::PersonLink {
      person_uid: person_uid.clone(),
      entity: entity.clone(),
      from_suggestion,
    },
    at,
  );
  cmd.idempotency_key = idempotency_key;
  db.write(|w| w.append_command(&cmd))?;
  Ok(())
}

/// Create a person and link one account to it, as one batch.
///
/// The uid is minted here, at the edge, and travels in the command
/// payload: minting it while the command was applied would produce a
/// different uid on replay and falsify `replay(streams) == live` on the
/// first link anybody made.
///
/// # Errors
/// If the store refuses either command.
pub fn link_to_new_person(
  db: &Db,
  actor: &Actor,
  display_name: Option<String>,
  entity: &EntityRef,
  from_suggestion: Option<String>,
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<PersonUid> {
  let person_uid = PersonUid::generate();
  let batch = person_uid.to_string();

  let mut create = NewCommand::new(
    actor.clone(),
    CommandKind::PersonCreate {
      person_uid: person_uid.clone(),
      display_name,
    },
    at,
  )
  .with_batch(batch.clone());
  let mut link = NewCommand::new(
    actor.clone(),
    CommandKind::PersonLink {
      person_uid: person_uid.clone(),
      entity: entity.clone(),
      from_suggestion,
    },
    at,
  )
  .with_batch(batch);

  // One operator action, two commands, so the key is derived per command
  // rather than shared: a retry must be a no-op for both halves, and a
  // shared key would make the second look like a duplicate of the first.
  if let Some(key) = &idempotency_key {
    create = create.with_idempotency_key(format!("{key}:create"));
    link = link.with_idempotency_key(format!("{key}:link"));
  }

  db.write(|w| -> Result<_> {
    w.append_command(&create)?;
    w.append_command(&link)?;
    Ok(())
  })?;
  Ok(person_uid)
}

/// Act on a proposed link: attach `entity` to `person_uid`.
///
/// The interesting case is the ordinary one. Most suggestions name an
/// *implicit* person — the singleton of another unlinked account — and
/// there is no person there to link to yet, so confirming creates one
/// and links both accounts to it. An operator should not have to know
/// that; the button says "confirm" either way.
///
/// Returns the uid the accounts now belong to.
///
/// # Errors
/// If the store refuses any of the commands.
pub fn confirm(
  db: &Db,
  actor: &Actor,
  entity: &EntityRef,
  person_uid: &PersonUid,
  from_suggestion: Option<String>,
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<PersonUid> {
  // Resolve first. A suggestion is a snapshot of the last sweep, and
  // between then and now the account it names may have been linked —
  // three accounts matching each other produce three proposals, and
  // confirming the second must join the person the first created rather
  // than mint a rival and steal the account away from it.
  let resolved = db.read(|r| r.resolve_person(person_uid))?;

  let Some(other) = resolved.implicit_entity() else {
    link(
      db,
      actor,
      &resolved,
      entity,
      from_suggestion,
      at,
      idempotency_key,
    )?;
    return Ok(resolved);
  };

  let created = link_to_new_person(
    db,
    actor,
    display_name_of(db, &other)?,
    &other,
    from_suggestion.clone(),
    at,
    idempotency_key.as_ref().map(|k| format!("{k}:a")),
  )?;
  link(
    db,
    actor,
    &created,
    entity,
    from_suggestion,
    at,
    idempotency_key.map(|k| format!("{k}:b")),
  )?;
  Ok(created)
}

/// What to call a person created for an account.
///
/// A person with no name is a ULID on every screen, which is unusable.
/// The account's own display name is the best guess available and is
/// display only — nothing evaluates over it — so borrowing it is safe,
/// and the operator can correct it.
///
/// # Errors
/// On a store failure.
pub fn display_name_of(db: &Db, entity: &EntityRef) -> Result<Option<String>> {
  Ok(
    db.read(|r| r.entity_detail(entity))?
      .and_then(|d| d.normalized.and_then(|n| n.display_name)),
  )
}

/// Detach an account from the person holding it. It becomes an implicit
/// singleton person again, carrying its own violations with it.
///
/// # Errors
/// If the account is not linked, or is linked to somebody else.
pub fn unlink(
  db: &Db,
  actor: &Actor,
  person_uid: &PersonUid,
  entity: &EntityRef,
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<()> {
  let mut cmd = NewCommand::new(
    actor.clone(),
    CommandKind::PersonUnlink {
      person_uid: person_uid.clone(),
      entity:     entity.clone(),
    },
    at,
  );
  cmd.idempotency_key = idempotency_key;
  db.write(|w| w.append_command(&cmd))?;
  Ok(())
}

/// Designate the account an `entity(...)` selector should resolve to for
/// one system kind (SPEC.md section 6.4).
///
/// # Errors
/// If the account is not one the person holds.
pub fn set_primary(
  db: &Db,
  actor: &Actor,
  person_uid: &PersonUid,
  system_kind: SystemKind,
  entity: &EntityRef,
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<()> {
  let mut cmd = NewCommand::new(
    actor.clone(),
    CommandKind::PersonSetPrimary {
      person_uid: person_uid.clone(),
      system_kind,
      entity: entity.clone(),
    },
    at,
  );
  cmd.idempotency_key = idempotency_key;
  db.write(|w| w.append_command(&cmd))?;
  Ok(())
}

/// Combine two persons. The operator chooses which uid survives; the
/// other becomes a permanent alias (SPEC.md section 12).
///
/// # Errors
/// If either uid is unknown, they are the same, or either names an
/// unlinked account rather than a person.
pub fn merge(
  db: &Db,
  actor: &Actor,
  surviving: &PersonUid,
  retired: &PersonUid,
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<()> {
  for uid in [surviving, retired] {
    let known = db.read(|r| r.person_detail(uid))?;
    if known.is_none() {
      return Err(EngineError::Config(format!("no person {uid}")));
    }
  }
  let mut cmd = NewCommand::new(
    actor.clone(),
    CommandKind::PersonMerge {
      surviving: surviving.clone(),
      retired:   retired.clone(),
    },
    at,
  );
  cmd.idempotency_key = idempotency_key;
  db.write(|w| w.append_command(&cmd))?;
  Ok(())
}

/// Move accounts off a person onto a new one. The original keeps the
/// history; the new uid is traceable to the command that created it.
///
/// # Errors
/// If no accounts are named, or one of them is not held by `from`.
pub fn split(
  db: &Db,
  actor: &Actor,
  from: &PersonUid,
  display_name: Option<String>,
  entities: &[EntityRef],
  at: Timestamp,
  idempotency_key: Option<String>,
) -> Result<PersonUid> {
  let new_uid = PersonUid::generate();
  let batch = new_uid.to_string();

  let mut create = NewCommand::new(
    actor.clone(),
    CommandKind::PersonCreate {
      person_uid: new_uid.clone(),
      display_name,
    },
    at,
  )
  .with_batch(batch.clone());
  let mut split = NewCommand::new(
    actor.clone(),
    CommandKind::PersonSplit {
      from:     from.clone(),
      new_uid:  new_uid.clone(),
      entities: entities.to_vec(),
    },
    at,
  )
  .with_batch(batch);

  if let Some(key) = &idempotency_key {
    create = create.with_idempotency_key(format!("{key}:create"));
    split = split.with_idempotency_key(format!("{key}:split"));
  }

  db.write(|w| -> Result<_> {
    w.append_command(&create)?;
    w.append_command(&split)?;
    Ok(())
  })?;
  Ok(new_uid)
}
