//! Read models. All reading happens against projections (SPEC.md
//! section 13); nothing here touches the streams except to show history.

use overlord_core::{
  CheckDraft, CheckId, CheckRecord, EntityRef, EntityStatus, EntityType,
  NormalizedRecord, PersonUid, Revision, Severity, SubjectKind, SubjectRef,
  SweepId, SystemId, Timestamp, ViolationState,
};
use rusqlite::{OptionalExtension, params};

use crate::{
  db::{Reader, get_payload},
  error::{Result, StoreError},
};

/// One entity as evaluation needs it.
#[derive(Debug, Clone)]
pub struct EntityState {
  pub entity:     EntityRef,
  pub normalized: NormalizedRecord,
  pub raw:        serde_json::Value,
  /// The fact this state came from, which evidence cites.
  pub fact_id:    i64,
}

/// A violation as the board shows it.
#[derive(Debug, Clone)]
pub struct ViolationRow {
  pub check_id:      CheckId,
  pub check_name:    String,
  pub subject:       SubjectRef,
  pub episode:       i64,
  pub state:         ViolationState,
  pub severity:      Severity,
  pub weight:        i64,
  pub opened_at:     Timestamp,
  pub opened_sweep:  SweepId,
  pub evidence:      overlord_core::Evidence,
  pub stale:         bool,
  pub ambiguous:     bool,
  /// The overlay was applied under an older revision of the check, so
  /// the acknowledgement predates the rule as it now reads.
  pub overlay_stale: bool,
  /// This episode opened in the most recent sweep that covered its
  /// subject (SPEC.md sections 5 and 8).
  ///
  /// Computed per system, not against the latest sweep overall. That
  /// distinction is the whole point: with a single global comparison, a
  /// sweep restricted to one system would empty the "new" section for
  /// every other system, and SPEC.md section 10 is explicit that
  /// restricting a sweep must not manufacture change.
  pub new_since:     bool,
}

/// A subject's risk score (SPEC.md section 8).
#[derive(Debug, Clone)]
pub struct ScoreRow {
  pub person_uid:     PersonUid,
  pub display_name:   Option<String>,
  pub implicit:       bool,
  pub score:          i64,
  pub count:          i64,
  pub worst_severity: Option<Severity>,
}

/// Which half of the roster a subject listing asks for.
///
/// The Users screen offers this as a filter, and it belongs in the query
/// rather than in the caller: confirmed persons and unlinked accounts
/// are ranked together, so filtering after a `LIMIT` would silently
/// answer "the confirmed persons among the worst N subjects" — a
/// shorter list than the one asked for, with nothing to say it was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubjectFilter {
  /// Confirmed persons and unlinked accounts alike.
  #[default]
  Everyone,
  /// Confirmed persons only: somebody an operator has linked.
  Confirmed,
  /// Unlinked accounts only, each an implicit singleton person.
  Unlinked,
}

impl SubjectFilter {
  /// The `implicit` value this filter selects, or `None` for no
  /// restriction.
  fn as_sql(self) -> Option<i64> {
    match self {
      Self::Everyone => None,
      Self::Confirmed => Some(0),
      Self::Unlinked => Some(1),
    }
  }
}

/// One row of a subject ranking, however the ranking was assembled.
///
/// `implicit` is stored on `person`, so a query that reaches the row
/// through that table knows the answer; one that does not falls back to
/// the shape of the uid, which carries it (SPEC.md section 6.4).
fn score_row(
  uid: String,
  score: i64,
  count: i64,
  worst: Option<String>,
  display_name: Option<String>,
  implicit: Option<i64>,
) -> Result<ScoreRow> {
  let uid = PersonUid::new(uid);
  let implicit = implicit.map_or_else(|| uid.is_implicit(), |i| i != 0);
  Ok(ScoreRow {
    person_uid: uid,
    display_name,
    implicit,
    score,
    count,
    worst_severity: worst.as_deref().map(str::parse).transpose()?,
  })
}

impl Reader<'_> {
  // --- checks ---------------------------------------------------------

  /// Every check with its current revision and enabled state.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn checks(&self) -> Result<Vec<CheckRecord>> {
    let mut stmt = self.conn().prepare(
      "SELECT h.check_id, h.revision, h.enabled, r.draft
         FROM check_head h
         JOIN check_revision r
           ON r.check_id = h.check_id AND r.revision = h.revision
        ORDER BY h.check_id",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((
        r.get::<_, u32>(1)?,
        r.get::<_, i64>(2)? != 0,
        r.get::<_, String>(3)?,
      ))
    })?;
    let mut out = Vec::new();
    for row in rows {
      let (revision, enabled, draft) = row?;
      out.push(CheckRecord {
        draft: serde_json::from_str::<CheckDraft>(&draft)?,
        revision: Revision(revision),
        enabled,
      });
    }
    Ok(out)
  }

  /// The checks a sweep would pin: enabled, at their current revision.
  ///
  /// # Errors
  /// As [`Self::checks`].
  pub fn enabled_checks(&self) -> Result<Vec<CheckRecord>> {
    Ok(self.checks()?.into_iter().filter(|c| c.enabled).collect())
  }

  /// One specific revision, which is what a sweep evaluates — a sweep
  /// pins revisions at its start, so a later edit cannot change a run
  /// in progress.
  ///
  /// # Errors
  /// [`StoreError::NotFound`] if that revision was never recorded.
  pub fn check_revision(
    &self,
    id: &CheckId,
    revision: Revision,
  ) -> Result<CheckDraft> {
    let draft: Option<String> = self
      .conn()
      .query_row(
        "SELECT draft FROM check_revision
          WHERE check_id = ?1 AND revision = ?2",
        params![id.as_str(), revision.0],
        |r| r.get(0),
      )
      .optional()?;
    let draft = draft.ok_or_else(|| {
      StoreError::not_found(format!("check {id} revision {revision}"))
    })?;
    Ok(serde_json::from_str(&draft)?)
  }

  /// Whether a dry-run exists for an exact `(check, revision)`.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn has_dryrun(&self, id: &CheckId, revision: Revision) -> Result<bool> {
    let n: i64 = self.conn().query_row(
      "SELECT count(*) FROM check_dryrun
        WHERE check_id = ?1 AND revision = ?2",
      params![id.as_str(), revision.0],
      |r| r.get(0),
    )?;
    Ok(n > 0)
  }

  // --- entities -------------------------------------------------------

  /// Present entities, optionally restricted to some systems.
  ///
  /// Absent entities are excluded: an entity whose latest fact is a
  /// tombstone is out of scope entirely (SPEC.md section 6.1).
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn entity_states(
    &self,
    systems: Option<&[SystemId]>,
  ) -> Result<Vec<EntityState>> {
    let mut stmt = self.conn().prepare(
      "SELECT system, entity_type, entity_key, normalized, raw_hash,
              latest_fact_id
         FROM entity
        WHERE present = 1 AND normalized IS NOT NULL
        ORDER BY system, entity_type, entity_key",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((
        EntityRef::new(
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, String>(2)?,
        ),
        r.get::<_, String>(3)?,
        r.get::<_, Option<String>>(4)?,
        r.get::<_, i64>(5)?,
      ))
    })?;

    let mut out = Vec::new();
    for row in rows {
      let (entity, normalized, raw_hash, fact_id) = row?;
      if let Some(only) = systems
        && !only.contains(&entity.system)
      {
        continue;
      }
      let raw = match raw_hash {
        Some(h) => serde_json::from_str(&get_payload(self.conn(), &h)?)?,
        None => serde_json::Value::Null,
      };
      out.push(EntityState {
        entity,
        normalized: serde_json::from_str(&normalized)?,
        raw,
        fact_id,
      });
    }
    Ok(out)
  }

  /// The confirmed person an entity belongs to, if any.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn person_of(&self, entity: &EntityRef) -> Result<Option<PersonUid>> {
    let uid: Option<String> = self
      .conn()
      .query_row(
        "SELECT person_uid FROM link
          WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3",
        params![
          entity.system.as_str(),
          entity.entity_type.as_str(),
          entity.entity_key.as_str(),
        ],
        |r| r.get(0),
      )
      .optional()?;
    Ok(uid.map(PersonUid::new))
  }

  /// Every confirmed link, as `(entity, person)`.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn links(&self) -> Result<Vec<(EntityRef, PersonUid)>> {
    let mut stmt = self.conn().prepare(
      "SELECT system, entity_type, entity_key, person_uid FROM link",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((
        EntityRef::new(
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, String>(2)?,
        ),
        PersonUid::new(r.get::<_, String>(3)?),
      ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
  }

  /// Operator-designated primaries, as `(person, system_kind, entity)`.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn primaries(&self) -> Result<Vec<(PersonUid, String, EntityRef)>> {
    let mut stmt = self.conn().prepare(
      "SELECT person_uid, system_kind, system, entity_type, entity_key
         FROM link_primary",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((
        PersonUid::new(r.get::<_, String>(0)?),
        r.get::<_, String>(1)?,
        EntityRef::new(
          r.get::<_, String>(2)?,
          r.get::<_, String>(3)?,
          r.get::<_, String>(4)?,
        ),
      ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
  }

  /// Follow a uid to the person it means today.
  ///
  /// Two things can retire a uid, and both resolve here so that every
  /// lookup — a score, a detail page, a violation's subject — agrees
  /// about who a stale reference points at:
  ///
  /// - a **merge** records the retired uid in `person_alias` (SPEC.md section
  ///   12);
  /// - a **promotion** links the entity behind an implicit singleton person to
  ///   a confirmed one (SPEC.md section 6.4). That needs no alias row: an
  ///   implicit uid names its entity, so the `link` table already says who it
  ///   became, and deriving it means an `unlink` restores the implicit person
  ///   without any cleanup to forget.
  ///
  /// A uid that was never retired resolves to itself.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn resolve_person(&self, uid: &PersonUid) -> Result<PersonUid> {
    let mut current = uid.clone();
    // Merges rewrite existing aliases to point at the new survivor, so
    // this terminates in one hop in practice; the bound is belt and
    // braces against a cycle in a damaged store.
    for _ in 0..16 {
      if let Some(entity) = current.implicit_entity() {
        let linked: Option<String> = self
          .conn()
          .query_row(
            "SELECT person_uid FROM link
              WHERE system = ?1 AND entity_type = ?2 AND entity_key = ?3",
            params![
              entity.system.as_str(),
              entity.entity_type.as_str(),
              entity.entity_key.as_str(),
            ],
            |r| r.get(0),
          )
          .optional()?;
        match linked {
          Some(n) => {
            current = PersonUid::new(n);
            continue;
          }
          None => return Ok(current),
        }
      }

      let next: Option<String> = self
        .conn()
        .query_row(
          "SELECT surviving_uid FROM person_alias WHERE retired_uid = ?1",
          [current.as_str()],
          |r| r.get(0),
        )
        .optional()?;
      match next {
        Some(n) => current = PersonUid::new(n),
        None => return Ok(current),
      }
    }
    Ok(current)
  }

  /// Every uid that resolves to `uid` but is not `uid` itself: the
  /// implicit singletons its entities were before they were linked, and
  /// the uids retired into it by merges.
  ///
  /// Evaluation needs these to carry a standing episode across a
  /// promotion or a merge instead of resolving it and opening a fresh
  /// one — which would drop the operator's acknowledgement and reset
  /// "ignored longest" on the board.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn retired_uids(&self, uid: &PersonUid) -> Result<Vec<PersonUid>> {
    // An implicit person has no entities of its own beyond the one it
    // names, and nothing can have been retired into it.
    if uid.is_implicit() {
      return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut stmt = self.conn().prepare(
      "SELECT system, entity_type, entity_key FROM link
        WHERE person_uid = ?1 ORDER BY system, entity_type, entity_key",
    )?;
    let rows = stmt.query_map([uid.as_str()], |r| {
      Ok(EntityRef::new(
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
      ))
    })?;
    for row in rows {
      out.push(PersonUid::implicit(&row?));
    }

    // Merge chains are flattened when they are recorded, so one level
    // of retirement is the whole set.
    let mut stmt = self.conn().prepare(
      "SELECT retired_uid FROM person_alias
        WHERE surviving_uid = ?1 ORDER BY retired_uid",
    )?;
    let rows = stmt.query_map([uid.as_str()], |r| r.get::<_, String>(0))?;
    for row in rows {
      out.push(PersonUid::new(row?));
    }
    Ok(out)
  }

  /// Every subject ref a person's own violations may be keyed by: their
  /// uid, and the uids they have absorbed.
  ///
  /// Anything showing "this person's violations" wants this rather than
  /// a single ref — an episode keeps the ref it opened under, so a
  /// person who was an unlinked account last week still carries an
  /// episode filed against that account's implicit uid.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn person_subject_refs(
    &self,
    uid: &PersonUid,
  ) -> Result<Vec<SubjectRef>> {
    let canonical = self.resolve_person(uid)?;
    let mut out = vec![SubjectRef::Person(canonical.clone())];
    out.extend(
      self
        .retired_uids(&canonical)?
        .into_iter()
        .map(SubjectRef::Person),
    );
    Ok(out)
  }
}

// --- violations -------------------------------------------------------

/// What the board is narrowed to (SPEC.md section 5: filters by
/// severity, system, check, subject, and lifecycle state).
///
/// An empty vector means "no restriction on this facet", not "match
/// nothing" — the screen's unfiltered state is the default value.
#[derive(Debug, Clone)]
pub struct ViolationFilter {
  pub states:       Vec<ViolationState>,
  pub severities:   Vec<Severity>,
  /// Restricts entity-scoped violations to these systems. A
  /// person-scoped violation spans systems and so is never excluded
  /// by this facet.
  pub systems:      Vec<SystemId>,
  pub checks:       Vec<CheckId>,
  /// Exact subject refs. More than one is passed when a subject has
  /// absorbed another: a person's own episodes plus those still keyed by
  /// an implicit uid a link promoted, or a uid a merge retired. Those
  /// rows are never rewritten (SPEC.md section 12), so a detail page
  /// that asked only for the surviving ref would show a person fewer
  /// violations than they have.
  pub subjects:     Vec<SubjectRef>,
  /// A case-folded substring of the subject ref, for the board's search
  /// box. Applied in SQL alongside the other facets so `limit` keeps
  /// meaning "the worst N that match".
  pub subject_like: Option<String>,
  pub limit:        usize,
}

impl Default for ViolationFilter {
  /// The board's own default: what currently counts as bad state
  /// (SPEC.md section 9).
  fn default() -> Self {
    Self {
      states:       vec![ViolationState::Open, ViolationState::Acknowledged],
      severities:   Vec::new(),
      systems:      Vec::new(),
      checks:       Vec::new(),
      subjects:     Vec::new(),
      subject_like: None,
      limit:        500,
    }
  }
}

impl Reader<'_> {
  /// The board: active violations, worst first, then longest-ignored
  /// (SPEC.md section 8).
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn violations(
    &self,
    states: &[ViolationState],
    limit: usize,
  ) -> Result<Vec<ViolationRow>> {
    self.violations_where(&ViolationFilter {
      states: states.to_vec(),
      limit,
      ..ViolationFilter::default()
    })
  }

  /// The board, narrowed by [`ViolationFilter`].
  ///
  /// Every facet is applied in SQL rather than by filtering the result,
  /// because `limit` has to mean "the worst N that match" — trimming
  /// after the fact would silently drop matches behind the cut.
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  #[allow(clippy::too_many_lines)]
  pub fn violations_where(
    &self,
    filter: &ViolationFilter,
  ) -> Result<Vec<ViolationRow>> {
    if filter.states.is_empty() {
      return Ok(Vec::new());
    }
    let latest_per_system = self.latest_sweep_per_system()?;
    let latest_overall = self.latest_sweep()?;

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();

    let bind =
      |params: &mut Vec<Box<dyn rusqlite::ToSql>>, v: String| -> String {
        params.push(Box::new(v));
        format!("?{}", params.len())
      };

    let list = |params: &mut Vec<Box<dyn rusqlite::ToSql>>,
                values: Vec<String>|
     -> String {
      values
        .into_iter()
        .map(|v| bind(params, v))
        .collect::<Vec<_>>()
        .join(", ")
    };

    let states = list(
      &mut params,
      filter
        .states
        .iter()
        .map(|s| s.as_str().to_owned())
        .collect(),
    );
    clauses.push(format!("v.state IN ({states})"));

    if !filter.severities.is_empty() {
      let s = list(
        &mut params,
        filter
          .severities
          .iter()
          .map(|s| s.as_str().to_owned())
          .collect(),
      );
      clauses.push(format!("v.severity IN ({s})"));
    }

    if !filter.checks.is_empty() {
      let c = list(
        &mut params,
        filter.checks.iter().map(ToString::to_string).collect(),
      );
      clauses.push(format!("v.check_id IN ({c})"));
    }

    if !filter.subjects.is_empty() {
      let s = list(
        &mut params,
        filter.subjects.iter().map(ToString::to_string).collect(),
      );
      clauses.push(format!("v.subject_ref IN ({s})"));
    }

    if let Some(text) = &filter.subject_like
      && !text.trim().is_empty()
    {
      // Escaped explicitly: an entity key is vendor-supplied, and a `%`
      // or `_` typed into the search box must match itself rather than
      // silently widening the search.
      let escaped = text
        .to_lowercase()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
      let s = bind(&mut params, format!("%{escaped}%"));
      clauses.push(format!("lower(v.subject_ref) LIKE {s} ESCAPE '\\'"));
    }

    if !filter.systems.is_empty() {
      // A prefix comparison on the rendered ref rather than a LIKE:
      // `subject_ref` is `entity/<system>/<type>/<key>`, and an exact
      // prefix needs no escaping of whatever a vendor put in the key.
      //
      // Person-scoped violations are kept regardless. A person spans
      // systems, so "only show me okta-prod" cannot sensibly exclude
      // one, and dropping them would hide exactly the cross-system
      // findings the system filter is being used to investigate.
      let ors: Vec<String> = filter
        .systems
        .iter()
        .map(|sys| {
          let p = bind(&mut params, format!("entity/{sys}/"));
          format!("substr(v.subject_ref, 1, length({p})) = {p}")
        })
        .collect();
      clauses.push(format!(
        "(v.subject_kind = 'person' OR {})",
        ors.join(" OR ")
      ));
    }

    params.push(Box::new(i64::try_from(filter.limit).unwrap_or(i64::MAX)));
    let limit_param = format!("?{}", params.len());

    let sql = format!(
      "SELECT v.check_id, v.subject_ref, v.episode, v.state, v.severity,
              v.weight, v.opened_at, v.opened_sweep, v.evidence, v.stale,
              v.ambiguous, v.overlay_rev, h.revision, r.draft
         FROM violation v
         LEFT JOIN check_head h ON h.check_id = v.check_id
         LEFT JOIN check_revision r
           ON r.check_id = v.check_id AND r.revision = h.revision
        WHERE {}
        ORDER BY v.weight DESC, v.opened_at ASC, v.check_id
        LIMIT {limit_param}",
      clauses.join(" AND ")
    );

    let refs: Vec<&dyn rusqlite::ToSql> =
      params.iter().map(AsRef::as_ref).collect();

    let mut stmt = self.conn().prepare(&sql)?;
    let rows = stmt.query_map(refs.as_slice(), |r| {
      Ok((
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, i64>(2)?,
        r.get::<_, String>(3)?,
        r.get::<_, String>(4)?,
        r.get::<_, i64>(5)?,
        r.get::<_, String>(6)?,
        r.get::<_, i64>(7)?,
        r.get::<_, String>(8)?,
        r.get::<_, i64>(9)? != 0,
        r.get::<_, i64>(10)? != 0,
        r.get::<_, Option<u32>>(11)?,
        r.get::<_, Option<u32>>(12)?,
        r.get::<_, Option<String>>(13)?,
      ))
    })?;

    let mut out = Vec::new();
    for row in rows {
      let (
        check_id,
        subject_ref,
        episode,
        state,
        severity,
        weight,
        opened_at,
        opened_sweep,
        evidence,
        stale,
        ambiguous,
        overlay_rev,
        head_rev,
        draft,
      ) = row?;
      let name = match &draft {
        Some(d) => serde_json::from_str::<CheckDraft>(d)?.name,
        None => check_id.clone(),
      };
      let subject: SubjectRef = subject_ref.parse()?;
      let opened_sweep = SweepId(opened_sweep);

      // An entity-scoped violation is measured against the last sweep
      // that covered *its* system. A person-scoped one is measured
      // against the latest sweep overall, because person checks are
      // re-evaluated on every run whatever it covered — a person spans
      // systems, so there is no single system to ask.
      let benchmark = match &subject {
        SubjectRef::Entity(e) => latest_per_system.get(&e.system).copied(),
        SubjectRef::Person(_) => latest_overall,
      };

      out.push(ViolationRow {
        check_id: CheckId::new(check_id),
        check_name: name,
        new_since: benchmark == Some(opened_sweep),
        subject,
        episode,
        state: state.parse()?,
        severity: severity.parse()?,
        weight,
        opened_at: opened_at.parse()?,
        opened_sweep,
        evidence: serde_json::from_str(&evidence)?,
        stale,
        ambiguous,
        overlay_stale: matches!(
          (overlay_rev, head_rev),
          (Some(o), Some(h)) if o != h
        ),
      });
    }
    Ok(out)
  }

  /// Subjects ranked by risk (SPEC.md section 8).
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn top_subjects(&self, limit: usize) -> Result<Vec<ScoreRow>> {
    let mut stmt = self.conn().prepare(
      "SELECT s.person_uid, s.score, s.violation_count, s.worst_severity,
              p.display_name, p.implicit
         FROM person_score s
         LEFT JOIN person p ON p.person_uid = s.person_uid
        WHERE s.score > 0
        ORDER BY s.score DESC, s.worst_severity ASC, s.person_uid
        LIMIT ?1",
    )?;
    let rows =
      stmt.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
        Ok((
          r.get::<_, String>(0)?,
          r.get::<_, i64>(1)?,
          r.get::<_, i64>(2)?,
          r.get::<_, Option<String>>(3)?,
          r.get::<_, Option<String>>(4)?,
          r.get::<_, Option<i64>>(5)?,
        ))
      })?;

    let mut out = Vec::new();
    for row in rows {
      let (uid, score, count, worst, display_name, implicit) = row?;
      out.push(score_row(uid, score, count, worst, display_name, implicit)?);
    }
    Ok(out)
  }

  /// Every subject overlord knows about, ranked the same way.
  ///
  /// [`Self::top_subjects`] answers "who is worst", so it reads
  /// `person_score` alone — and an account with nothing against it has
  /// no row in that table at all. The Users screen asks the other
  /// question, "who is there", where a clean account missing from the
  /// list reads as overlord never having collected it. So the universe
  /// here is the one evaluation itself builds (SPEC.md section 6.4):
  /// every confirmed person, plus an implicit singleton for every
  /// present unlinked entity, with a score left-joined on and its
  /// absence meaning zero.
  ///
  /// An implicit person is labelled with its account's own display
  /// name, which `person` cannot supply because an unlinked account has
  /// no row there.
  ///
  /// `kind` narrows the roster *before* `limit` applies. It has to
  /// happen here rather than in the caller: the two kinds are
  /// interleaved by score, so a caller that took the top `limit`
  /// subjects and then kept the confirmed ones would drop every
  /// confirmed person ranked below the cut and show a short list as if
  /// it were the whole one.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn all_subjects(
    &self,
    kind: SubjectFilter,
    limit: usize,
  ) -> Result<Vec<ScoreRow>> {
    let mut stmt = self.conn().prepare(
      "WITH subject AS (
         SELECT p.person_uid AS uid, p.display_name AS display_name,
                0 AS implicit
           FROM person p
          WHERE p.implicit = 0
         UNION ALL
         SELECT 'implicit:' || e.system || '/' || e.entity_type || '/'
                  || e.entity_key,
                json_extract(e.normalized, '$.display_name'),
                1
           FROM entity e
           LEFT JOIN link l
             ON l.system = e.system AND l.entity_type = e.entity_type
                AND l.entity_key = e.entity_key
          WHERE e.present = 1 AND e.normalized IS NOT NULL
            AND l.person_uid IS NULL
            -- The same policy evaluation applies (SPEC.md s6.4): a
            -- type that is not a person is not an implicit person
            -- here either, or the roster would list subjects no
            -- person-scoped check was run against. An absent row
            -- leaves the subquery empty, which admits everything.
            AND e.entity_type NOT IN (
              SELECT j.value
                FROM identity_policy p, json_each(p.non_person_types) j
               WHERE p.id = 1
            )
       )
       SELECT subject.uid, coalesce(s.score, 0),
              coalesce(s.violation_count, 0), s.worst_severity,
              subject.display_name, subject.implicit
         FROM subject
         LEFT JOIN person_score s ON s.person_uid = subject.uid
        WHERE ?2 IS NULL OR subject.implicit = ?2
        ORDER BY coalesce(s.score, 0) DESC, s.worst_severity ASC,
                 lower(coalesce(subject.display_name, subject.uid)),
                 subject.uid
        LIMIT ?1",
    )?;
    let rows = stmt.query_map(
      params![i64::try_from(limit).unwrap_or(i64::MAX), kind.as_sql()],
      |r| {
        Ok((
          r.get::<_, String>(0)?,
          r.get::<_, i64>(1)?,
          r.get::<_, i64>(2)?,
          r.get::<_, Option<String>>(3)?,
          r.get::<_, Option<String>>(4)?,
          r.get::<_, Option<i64>>(5)?,
        ))
      },
    )?;

    let mut out = Vec::new();
    for row in rows {
      let (uid, score, count, worst, display_name, implicit) = row?;
      out.push(score_row(uid, score, count, worst, display_name, implicit)?);
    }
    Ok(out)
  }

  // --- sweeps ---------------------------------------------------------

  /// The most recent committed sweep.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn latest_sweep(&self) -> Result<Option<SweepId>> {
    let id: Option<i64> = self
      .conn()
      .query_row(
        "SELECT id FROM sweep WHERE committed_seq IS NOT NULL
          ORDER BY id DESC LIMIT 1",
        [],
        |r| r.get(0),
      )
      .optional()?;
    Ok(id.map(SweepId))
  }

  /// A sweep's start time, which is the definition of "now" for
  /// everything it produced.
  ///
  /// # Errors
  /// [`StoreError::NotFound`] if the sweep is unknown.
  pub fn sweep_started_at(&self, sweep: SweepId) -> Result<Timestamp> {
    let at: Option<String> = self
      .conn()
      .query_row(
        "SELECT started_at FROM sweep WHERE id = ?1",
        [sweep.0],
        |r| r.get(0),
      )
      .optional()?;
    at.ok_or_else(|| StoreError::not_found(format!("sweep {sweep}")))?
      .parse()
      .map_err(Into::into)
  }
}

/// Summary counts used by the CLI and the Systems screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
  pub entities:   usize,
  pub persons:    usize,
  pub checks:     usize,
  pub violations: usize,
}

impl Reader<'_> {
  /// Row counts across the projections.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn counts(&self) -> Result<Counts> {
    let one = |sql: &str| -> Result<usize> {
      let n: i64 = self.conn().query_row(sql, [], |r| r.get(0))?;
      Ok(usize::try_from(n).unwrap_or(0))
    };
    Ok(Counts {
      entities:   one("SELECT count(*) FROM entity WHERE present = 1")?,
      persons:    one("SELECT count(*) FROM person")?,
      checks:     one("SELECT count(*) FROM check_head")?,
      violations: one(
        "SELECT count(*) FROM violation
          WHERE state IN ('open', 'acknowledged')",
      )?,
    })
  }
}

/// The status an entity reports, for display.
#[must_use]
pub fn status_of(normalized: &NormalizedRecord) -> EntityStatus {
  normalized.status
}

/// Which scope a subject belongs to.
#[must_use]
pub fn subject_kind(subject: &SubjectRef) -> SubjectKind { subject.kind() }

impl Reader<'_> {
  /// The systems a sweep actually covered, with whether each returned a
  /// complete enumeration.
  ///
  /// SPEC.md section 10: a partial sweep must not re-evaluate
  /// entity-scoped checks for systems it did not visit, and must mark
  /// person-scoped results that lean on last-known state.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn swept_systems(
    &self,
    sweep: SweepId,
  ) -> Result<Vec<(SystemId, overlord_core::SystemKind, bool)>> {
    let mut stmt = self.conn().prepare(
      "SELECT system, system_kind, complete FROM sweep_system
        WHERE sweep_id = ?1 AND status IN ('ok', 'partial')",
    )?;
    let rows = stmt.query_map([sweep.0], |r| {
      Ok((
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, i64>(2)? != 0,
      ))
    })?;
    let mut out = Vec::new();
    for row in rows {
      let (system, kind, complete) = row?;
      out.push((SystemId::new(system), kind.parse()?, complete));
    }
    Ok(out)
  }

  /// Entities with unreviewed link suggestions, for a check that sets
  /// `suppress_if_pending_links`.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn entities_with_pending_suggestions(&self) -> Result<Vec<EntityRef>> {
    let mut stmt = self.conn().prepare(
      "SELECT DISTINCT s.system, s.entity_type, s.entity_key
         FROM suggestion s
         LEFT JOIN link l
           ON l.system = s.system AND l.entity_type = s.entity_type
          AND l.entity_key = s.entity_key
        WHERE l.person_uid IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok(EntityRef::new(
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
      ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
  }

  /// Episodes whose suppression has expired as of `now`.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn expired_suppressions(
    &self,
    now: Timestamp,
  ) -> Result<Vec<(String, String, i64)>> {
    let mut stmt = self.conn().prepare(
      "SELECT check_id, subject_ref, episode FROM violation
        WHERE state = 'suppressed' AND suppress_until IS NOT NULL
          AND suppress_until <= ?1",
    )?;
    let rows = stmt.query_map([now.to_string()], |r| {
      Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
  }
}

impl Reader<'_> {
  /// The check revisions a sweep pinned when it started.
  ///
  /// Evaluation uses these, not the current ones: a later edit must not
  /// change a run in progress or its replay (SPEC.md section 10).
  ///
  /// # Errors
  /// On a SQLite failure or unreadable stored JSON.
  pub fn sweep_pins(&self, sweep: SweepId) -> Result<Vec<(CheckId, Revision)>> {
    let json: String = self.conn().query_row(
      "SELECT pinned_checks FROM sweep WHERE id = ?1",
      [sweep.0],
      |r| r.get(0),
    )?;
    let pins: Vec<(String, u32)> = serde_json::from_str(&json)?;
    Ok(
      pins
        .into_iter()
        .map(|(id, rev)| (CheckId::new(id), Revision(rev)))
        .collect(),
    )
  }

  /// Every system overlord has ever observed, with its kind.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn known_systems(
    &self,
  ) -> Result<Vec<(SystemId, overlord_core::SystemKind)>> {
    let mut stmt = self.conn().prepare(
      "SELECT DISTINCT system, system_kind FROM sweep_system
        ORDER BY system",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
      let (id, kind) = row?;
      out.push((SystemId::new(id), kind.parse()?));
    }
    Ok(out)
  }

  /// The connector each system was last read through.
  ///
  /// Answers `connector:` scope selectors without reaching for the
  /// configuration file, which evaluation cannot see and a replay would
  /// not have. The *latest* sweep wins, so a system moved from one
  /// connector to another is scoped by the one reading it now rather
  /// than by every connector that ever did.
  ///
  /// A system swept only before the connector was recorded is absent
  /// from the map, and a connector selector matches nothing for it until
  /// its next sweep.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn system_connectors(
    &self,
  ) -> Result<std::collections::BTreeMap<SystemId, String>> {
    let mut stmt = self.conn().prepare(
      "SELECT system, connector FROM sweep_system s
        WHERE connector IS NOT NULL
          AND sweep_id = (SELECT max(sweep_id) FROM sweep_system t
                           WHERE t.system = s.system
                             AND t.connector IS NOT NULL)",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = std::collections::BTreeMap::new();
    for row in rows {
      let (system, connector) = row?;
      out.insert(SystemId::new(system), connector);
    }
    Ok(out)
  }

  /// The entity types that are not people (SPEC.md section 6.4).
  ///
  /// Read from the projection rather than from `overlord.toml` for the
  /// same reason [`Self::system_connectors`] is: this decides which
  /// subjects evaluation sees, and a replay has no configuration file.
  /// The configuration is the authoring surface; an `identity.policy`
  /// command is what evaluation actually reads.
  ///
  /// Empty until the first such command, which is the behaviour every
  /// store had before the policy existed.
  ///
  /// # Errors
  /// On a SQLite failure, or if the stored JSON is not an array of
  /// entity types — which would mean a command this binary cannot
  /// understand, and is not something to paper over with a default.
  pub fn non_person_entity_types(
    &self,
  ) -> Result<std::collections::BTreeSet<EntityType>> {
    let json: Option<String> = self
      .conn()
      .query_row(
        "SELECT non_person_types FROM identity_policy WHERE id = 1",
        [],
        |r| r.get(0),
      )
      .optional()?;
    let Some(json) = json else {
      return Ok(std::collections::BTreeSet::new());
    };
    Ok(serde_json::from_str(&json)?)
  }
}

impl Reader<'_> {
  /// Open violations per check, for the Rules screen's counts and its
  /// zero-match flag (SPEC.md section 5).
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn open_counts_by_check(&self) -> Result<Vec<(CheckId, i64)>> {
    let mut stmt = self.conn().prepare(
      "SELECT check_id, count(*) FROM violation
        WHERE state IN ('open', 'acknowledged')
        GROUP BY check_id",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((CheckId::new(r.get::<_, String>(0)?), r.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
  }

  /// A check's false-positive rate: the share of its episodes an
  /// operator marked as the rule's fault rather than the subject's.
  /// SPEC.md section 9 calls this a rule-quality signal.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn false_positive_rate(&self, check: &CheckId) -> Result<Option<f64>> {
    let (total, fp): (i64, i64) = self.conn().query_row(
      "SELECT count(*),
              sum(CASE WHEN state = 'false_positive' THEN 1 ELSE 0 END)
         FROM violation WHERE check_id = ?1",
      [check.as_str()],
      |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
    )?;
    if total == 0 {
      return Ok(None);
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(Some(fp as f64 / total as f64))
  }
}

impl Reader<'_> {
  /// The most recent committed sweep that covered each system.
  ///
  /// The benchmark "new since last sweep" measures against (SPEC.md
  /// sections 8 and 10). One grouped statement rather than a query per
  /// system, because the board asks for all of them at once.
  ///
  /// A system that has never been swept successfully is absent from the
  /// map, so nothing on it counts as new — there is no previous look to
  /// compare against.
  ///
  /// # Errors
  /// On a SQLite failure.
  pub fn latest_sweep_per_system(
    &self,
  ) -> Result<std::collections::BTreeMap<SystemId, SweepId>> {
    let mut stmt = self.conn().prepare(
      "SELECT ss.system, max(s.id)
         FROM sweep s
         JOIN sweep_system ss ON ss.sweep_id = s.id
        WHERE s.committed_seq IS NOT NULL
          AND ss.status IN ('ok', 'partial')
        GROUP BY ss.system",
    )?;
    let rows = stmt.query_map([], |r| {
      Ok((SystemId::new(r.get::<_, String>(0)?), SweepId(r.get(1)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
  }
}
