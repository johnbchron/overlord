//! The subjects a sweep evaluates against.
//!
//! Built once per evaluation pass from the projections, then handed to
//! the expression crate through its [`Subject`] trait — which is the
//! only thing `overlord-expr` knows about overlord's data model.

use std::collections::BTreeMap;

use overlord_core::{
  EntityRef, PersonUid, SubjectKind, SystemId, SystemKind, SystemSelector,
};
use overlord_expr::{EntityAttrs, Primary, Subject};
use overlord_store::Reader;

use crate::error::Result;

/// Every entity and person in scope for one evaluation pass.
#[derive(Debug, Default)]
pub struct World {
  entities:   Vec<EntityAttrs>,
  /// The connector each system was last read through, for a
  /// `connector:` scope selector. Read from the projections rather than
  /// from the configuration file: evaluation runs inside the sweep and
  /// replays from the streams, neither of which can see a TOML file.
  connectors: BTreeMap<SystemId, String>,
  by_ref:     BTreeMap<EntityRef, usize>,
  /// Members of each person, confirmed or implicit.
  members:    BTreeMap<PersonUid, Vec<usize>>,
  /// Operator-designated primaries, keyed by person and system kind.
  primaries:  BTreeMap<(PersonUid, SystemKind), usize>,
}

impl World {
  /// Load from the projections.
  ///
  /// SPEC.md section 6.4: every entity not linked to a confirmed person
  /// is additionally evaluated as an implicit singleton person, so
  /// orphan-account checks work before linking is complete — an orphan
  /// is by definition unlinked, so without this the check that finds
  /// them could never fire.
  ///
  /// # Errors
  /// On a store failure.
  pub fn load(r: &Reader<'_>) -> Result<Self> {
    let mut w = Self {
      connectors: r.system_connectors()?,
      ..Self::default()
    };

    for state in r.entity_states(None)? {
      let idx = w.entities.len();
      w.by_ref.insert(state.entity.clone(), idx);
      w.entities.push(EntityAttrs {
        normalized: state.normalized,
        raw:        state.raw,
        fact_id:    state.fact_id,
      });
    }

    let mut linked = BTreeMap::new();
    for (entity, uid) in r.links()? {
      let uid = r.resolve_person(&uid)?;
      if let Some(&idx) = w.by_ref.get(&entity) {
        linked.insert(entity, uid.clone());
        w.members.entry(uid).or_default().push(idx);
      }
    }

    for (entity, &idx) in &w.by_ref {
      if !linked.contains_key(entity) {
        w.members
          .entry(PersonUid::implicit(entity))
          .or_default()
          .push(idx);
      }
    }

    for (uid, kind, entity) in r.primaries()? {
      let uid = r.resolve_person(&uid)?;
      if let (Ok(kind), Some(&idx)) =
        (kind.parse::<SystemKind>(), w.by_ref.get(&entity))
      {
        w.primaries.insert((uid, kind), idx);
      }
    }

    Ok(w)
  }

  #[must_use]
  pub fn entity_refs(&self) -> Vec<EntityRef> {
    self.by_ref.keys().cloned().collect()
  }

  #[must_use]
  pub fn person_uids(&self) -> Vec<PersonUid> {
    self.members.keys().cloned().collect()
  }

  #[must_use]
  pub fn attrs(&self, entity: &EntityRef) -> Option<&EntityAttrs> {
    self.by_ref.get(entity).map(|&i| &self.entities[i])
  }

  /// The connector a system was last read through, for a `connector:`
  /// selector. `None` for a system swept only before connectors were
  /// recorded, which no connector selector matches.
  #[must_use]
  pub fn connector(&self, system: &SystemId) -> Option<&str> {
    self.connectors.get(system).map(String::as_str)
  }

  #[must_use]
  pub fn has_person(&self, uid: &PersonUid) -> bool {
    self.members.contains_key(uid)
  }

  /// The entities a person holds.
  #[must_use]
  pub fn member_refs(&self, uid: &PersonUid) -> Vec<EntityRef> {
    let Some(idx) = self.members.get(uid) else {
      return Vec::new();
    };
    let inverse: BTreeMap<usize, &EntityRef> =
      self.by_ref.iter().map(|(r, i)| (*i, r)).collect();
    idx
      .iter()
      .filter_map(|i| inverse.get(i).copied().cloned())
      .collect()
  }

  #[must_use]
  pub fn entity_subject<'a>(
    &'a self,
    entity: &EntityRef,
  ) -> Option<EntitySubject<'a>> {
    self.attrs(entity).map(|a| EntitySubject { attrs: a })
  }

  #[must_use]
  pub fn person_subject<'a>(
    &'a self,
    uid: &PersonUid,
  ) -> Option<PersonSubject<'a>> {
    let idx = self.members.get(uid)?;
    Some(PersonSubject {
      world: self,
      uid:   uid.clone(),
      idx:   idx.clone(),
    })
  }

  /// The fact ids a subject's state came from, for evidence.
  #[must_use]
  pub fn person_fact_ids(&self, uid: &PersonUid) -> Vec<i64> {
    self
      .members
      .get(uid)
      .map(|idx| idx.iter().map(|&i| self.entities[i].fact_id).collect())
      .unwrap_or_default()
  }
}

/// One entity, evaluated on its own.
pub struct EntitySubject<'a> {
  attrs: &'a EntityAttrs,
}

impl Subject for EntitySubject<'_> {
  fn kind(&self) -> SubjectKind { SubjectKind::Entity }

  fn own(&self) -> Option<&EntityAttrs> { Some(self.attrs) }

  /// An entity-scoped check has no selectors — the type checker rejects
  /// them — so these are unreachable rather than empty by accident.
  fn select(&self, _: &SystemSelector) -> Vec<&EntityAttrs> { Vec::new() }

  fn primary(&self, _: &SystemSelector) -> Primary<'_> { Primary::Missing }
}

/// A person: a set of entities, with optional designated primaries.
pub struct PersonSubject<'a> {
  world: &'a World,
  uid:   PersonUid,
  idx:   Vec<usize>,
}

impl PersonSubject<'_> {
  fn matching(&self, sel: &SystemSelector) -> Vec<usize> {
    self
      .idx
      .iter()
      .copied()
      .filter(|&i| {
        let n = &self.world.entities[i].normalized;
        sel.matches(&n.system, n.system_kind, self.world.connector(&n.system))
      })
      .collect()
  }
}

impl Subject for PersonSubject<'_> {
  fn kind(&self) -> SubjectKind { SubjectKind::Person }

  fn own(&self) -> Option<&EntityAttrs> { None }

  fn select(&self, sel: &SystemSelector) -> Vec<&EntityAttrs> {
    self
      .matching(sel)
      .into_iter()
      .map(|i| &self.world.entities[i])
      .collect()
  }

  fn primary(&self, sel: &SystemSelector) -> Primary<'_> {
    let matched = self.matching(sel);
    match matched.len() {
      0 => return Primary::Missing,
      1 => return Primary::Found(&self.world.entities[matched[0]]),
      _ => {}
    }

    // Several candidates. Only an operator's designation resolves it:
    // SPEC.md section 6.4 is explicit that the selector returns null
    // and the check is flagged ambiguous rather than guessing.
    let kinds: Vec<SystemKind> = matched
      .iter()
      .map(|&i| self.world.entities[i].normalized.system_kind)
      .collect();
    for kind in kinds {
      if let Some(&designated) =
        self.world.primaries.get(&(self.uid.clone(), kind))
        && matched.contains(&designated)
      {
        return Primary::Found(&self.world.entities[designated]);
      }
    }
    Primary::Ambiguous
  }
}
