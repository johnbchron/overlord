//! Reconciling the configuration file's identity policy into the stream.
//!
//! SPEC.md section 13 admits no input to evaluation outside the two
//! streams, and section 6.4's implicit singleton persons are squarely an
//! input to evaluation: they decide which subjects exist. So the
//! configuration file cannot be read *by* evaluation. It is read here
//! instead, compared with what the stream already says, and appended as
//! an `identity.policy` command when the two differ.
//!
//! The reconcile is idempotent and safe to call from anywhere — a sweep,
//! a server start, a CLI command. Skipping it never makes evaluation
//! wrong, only stale: the projection still holds the last policy that
//! was recorded, which is a policy that was once true rather than a
//! guess.

use std::collections::BTreeSet;

use overlord_core::{Actor, CommandKind, EntityType, NewCommand, Timestamp};
use overlord_store::Db;
use tracing::info;

use crate::error::Result;

/// Append an `identity.policy` command if `configured` differs from what
/// the stream already says.
///
/// Returns whether one was appended.
///
/// # Errors
/// On a store failure.
pub fn sync_identity_policy(
  db: &Db,
  configured: &[EntityType],
  actor: &Actor,
  at: Timestamp,
) -> Result<bool> {
  let wanted: BTreeSet<EntityType> = configured.iter().cloned().collect();
  let current = db.read(|r| r.non_person_entity_types())?;
  if wanted == current {
    return Ok(false);
  }

  // No idempotency key. The key guards a *retried submission* of one
  // operator action; this is a reconcile, and two runs that both find a
  // difference are two real changes. Equality above is what stops a
  // steady state from appending anything.
  db.write(|w| {
    w.append_command(&NewCommand::new(
      actor.clone(),
      CommandKind::identity_policy(wanted.iter().cloned()),
      at,
    ))
  })?;
  info!(
    was = ?current.iter().map(EntityType::as_str).collect::<Vec<_>>(),
    now = ?wanted.iter().map(EntityType::as_str).collect::<Vec<_>>(),
    "identity policy changed"
  );
  Ok(true)
}
