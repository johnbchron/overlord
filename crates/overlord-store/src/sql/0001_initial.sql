-- overlord: initial schema (PLAN.md section 3).
--
-- Two append-only streams, a set of rebuildable projections, and one
-- global sequence that orders them against each other.

-- ---------------------------------------------------------------------
-- The global stream sequence (PLAN.md section 3.1)
--
-- SPEC.md section 13 requires replay(streams) == live. Two independent
-- streams do not define their own interleaving, and a command can land
-- while a sweep is running, so the interleaving is recorded rather than
-- reconstructed from clocks: every fact, command and sweep boundary
-- takes a number from here, and replay is a merge on it.
-- ---------------------------------------------------------------------
CREATE TABLE stream_seq (
  id   INTEGER PRIMARY KEY CHECK (id = 1),
  next INTEGER NOT NULL
) STRICT;
INSERT INTO stream_seq (id, next) VALUES (1, 1);

-- ---------------------------------------------------------------------
-- Content-addressed payload storage (SPEC.md section 14)
--
-- Facts are deduplicated by payload hash, so an unchanged entity costs a
-- fact row and two references rather than a copy of the vendor payload.
-- ---------------------------------------------------------------------
CREATE TABLE payload (
  hash TEXT PRIMARY KEY,
  body TEXT NOT NULL
) STRICT;

-- ---------------------------------------------------------------------
-- Stream 1: sweeps and the facts they produce (SPEC.md section 6.1)
-- ---------------------------------------------------------------------
CREATE TABLE sweep (
  id                INTEGER PRIMARY KEY,
  opened_seq        INTEGER NOT NULL,
  -- NULL while the sweep is running. Evaluation replays at this point.
  committed_seq     INTEGER,
  -- The single definition of "now" for the whole run (SPEC.md s10).
  started_at        TEXT NOT NULL,
  finished_at       TEXT,
  status            TEXT NOT NULL
                    CHECK (status IN ('running','ok','partial','failed')),
  requested         TEXT NOT NULL,   -- JSON: system ids in scope
  pinned_checks     TEXT NOT NULL,   -- JSON: [[check_id, revision], ...]
  pinned_norm       TEXT NOT NULL,   -- JSON: [[ruleset_id, version], ...]
  absence_guard_pct INTEGER NOT NULL
) STRICT;

CREATE TABLE fact (
  id           INTEGER PRIMARY KEY,
  seq          INTEGER NOT NULL UNIQUE,
  sweep_id     INTEGER NOT NULL REFERENCES sweep (id),
  system       TEXT NOT NULL,
  entity_type  TEXT NOT NULL,
  entity_key   TEXT NOT NULL,
  observed_at  TEXT NOT NULL,
  -- 0 marks a tombstone: the entity was absent from a snapshot the
  -- connector declared complete.
  present      INTEGER NOT NULL CHECK (present IN (0, 1)),
  raw_hash     TEXT REFERENCES payload (hash),
  norm_hash    TEXT REFERENCES payload (hash),
  norm_version TEXT NOT NULL
) STRICT;

-- "Current state" is the latest fact by (sweep_id, id), so the index
-- that answers it descends.
CREATE INDEX fact_entity
  ON fact (system, entity_key, entity_type, sweep_id DESC, id DESC);
CREATE INDEX fact_sweep ON fact (sweep_id);

-- ---------------------------------------------------------------------
-- Stream 2: operator intent (SPEC.md section 6.2)
-- ---------------------------------------------------------------------
CREATE TABLE command (
  id              INTEGER PRIMARY KEY,
  seq             INTEGER NOT NULL UNIQUE,
  at              TEXT NOT NULL,
  actor           TEXT NOT NULL,
  kind            TEXT NOT NULL,
  subject         TEXT,
  args            TEXT NOT NULL,   -- JSON
  note            TEXT,
  -- Client-supplied. A retried submission returns the original id
  -- rather than acting twice.
  idempotency_key TEXT UNIQUE,
  batch_id        TEXT
) STRICT;

CREATE INDEX command_subject ON command (subject);
CREATE INDEX command_kind ON command (kind, id);

-- ---------------------------------------------------------------------
-- Projections (SPEC.md section 6.3). Everything below is derived and
-- can be dropped and rebuilt by replaying the two streams above.
-- ---------------------------------------------------------------------

CREATE TABLE entity (
  system          TEXT NOT NULL,
  entity_type     TEXT NOT NULL,
  entity_key      TEXT NOT NULL,
  present         INTEGER NOT NULL CHECK (present IN (0, 1)),
  latest_fact_id  INTEGER NOT NULL REFERENCES fact (id),
  normalized      TEXT,             -- JSON overlay, NULL for a tombstone
  raw_hash        TEXT REFERENCES payload (hash),
  first_seen_sweep INTEGER NOT NULL REFERENCES sweep (id),
  last_seen_sweep  INTEGER NOT NULL REFERENCES sweep (id),
  PRIMARY KEY (system, entity_type, entity_key)
) STRICT;

CREATE INDEX entity_present ON entity (present, system);

CREATE TABLE person (
  person_uid   TEXT PRIMARY KEY,
  display_name TEXT,
  -- 1 for an implicit singleton person (SPEC.md section 6.4): an
  -- unlinked entity, evaluated as a person so cross-system checks work
  -- before linking is complete.
  implicit     INTEGER NOT NULL CHECK (implicit IN (0, 1)),
  created_cmd  INTEGER REFERENCES command (id)
) STRICT;

-- A retired uid never disappears: every prior violation,
-- acknowledgement and suppression resolves through it (SPEC.md s12).
CREATE TABLE person_alias (
  retired_uid   TEXT PRIMARY KEY,
  surviving_uid TEXT NOT NULL REFERENCES person (person_uid),
  merged_cmd    INTEGER NOT NULL REFERENCES command (id)
) STRICT;

CREATE TABLE link (
  system      TEXT NOT NULL,
  entity_type TEXT NOT NULL,
  entity_key  TEXT NOT NULL,
  person_uid  TEXT NOT NULL REFERENCES person (person_uid),
  command_id  INTEGER NOT NULL REFERENCES command (id),
  PRIMARY KEY (system, entity_type, entity_key)
) STRICT;

CREATE INDEX link_person ON link (person_uid);

-- The operator may designate one primary entity per system kind, for
-- display and for entity(...) selectors.
CREATE TABLE link_primary (
  person_uid  TEXT NOT NULL REFERENCES person (person_uid),
  system_kind TEXT NOT NULL,
  system      TEXT NOT NULL,
  entity_type TEXT NOT NULL,
  entity_key  TEXT NOT NULL,
  command_id  INTEGER NOT NULL REFERENCES command (id),
  PRIMARY KEY (person_uid, system_kind)
) STRICT;

-- Machine-proposed, never applied automatically. Recomputed per sweep.
CREATE TABLE suggestion (
  system      TEXT NOT NULL,
  entity_type TEXT NOT NULL,
  entity_key  TEXT NOT NULL,
  person_uid  TEXT NOT NULL,
  signal      TEXT NOT NULL,   -- how it was found, shown to the operator
  evidence    TEXT NOT NULL,   -- JSON
  sweep_id    INTEGER NOT NULL REFERENCES sweep (id),
  PRIMARY KEY (system, entity_type, entity_key, person_uid)
) STRICT;

-- `check` is a SQL keyword, hence the names.
CREATE TABLE check_head (
  check_id     TEXT PRIMARY KEY,
  revision     INTEGER NOT NULL,
  -- Set only by check.enable / check.disable, never by an upsert.
  enabled      INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  enabled_rev  INTEGER,
  enabled_cmd  INTEGER REFERENCES command (id)
) STRICT;

CREATE TABLE check_revision (
  check_id   TEXT NOT NULL,
  revision   INTEGER NOT NULL,
  draft      TEXT NOT NULL,   -- JSON CheckDraft
  command_id INTEGER NOT NULL REFERENCES command (id),
  at         TEXT NOT NULL,
  actor      TEXT NOT NULL,
  PRIMARY KEY (check_id, revision)
) STRICT;

-- check.enable is rejected without a row here for that exact
-- (check_id, revision) (SPEC.md section 7).
CREATE TABLE check_dryrun (
  check_id    TEXT NOT NULL,
  revision    INTEGER NOT NULL,
  at          TEXT NOT NULL,
  match_count INTEGER NOT NULL,
  samples     TEXT NOT NULL,   -- JSON
  command_id  INTEGER NOT NULL REFERENCES command (id),
  PRIMARY KEY (check_id, revision)
) STRICT;

CREATE TABLE normalization_ruleset (
  ruleset_id  TEXT NOT NULL,
  version     TEXT NOT NULL,
  system_kind TEXT NOT NULL,
  body        TEXT NOT NULL,   -- JSON
  command_id  INTEGER REFERENCES command (id),
  PRIMARY KEY (ruleset_id, version)
) STRICT;

CREATE TABLE violation (
  check_id        TEXT NOT NULL,
  subject_ref     TEXT NOT NULL,
  -- Increments on regression, so a reopened violation is a new episode
  -- and never inherits a carried-over acknowledgement.
  episode         INTEGER NOT NULL,
  subject_kind    TEXT NOT NULL CHECK (subject_kind IN ('entity','person')),
  state           TEXT NOT NULL CHECK (state IN (
                    'open','acknowledged','suppressed','false_positive',
                    'resolved')),
  severity        TEXT NOT NULL,
  weight          INTEGER NOT NULL,
  opened_sweep    INTEGER NOT NULL REFERENCES sweep (id),
  opened_at       TEXT NOT NULL,
  revision_open   INTEGER NOT NULL,
  last_seen_sweep INTEGER NOT NULL REFERENCES sweep (id),
  resolved_at     TEXT,
  resolve_reason  TEXT,
  -- The overlay, and the revision it was applied under. When that
  -- differs from the check's current revision the UI flags the
  -- acknowledgement as made against a rule that has since been
  -- rewritten (SPEC.md section 6.5).
  overlay_cmd     INTEGER REFERENCES command (id),
  overlay_rev     INTEGER,
  suppress_reason TEXT,
  suppress_until  TEXT,
  evidence        TEXT NOT NULL,   -- JSON
  -- Evaluated against last-known state on a partial sweep, so the board
  -- marks it rather than silently trusting it (SPEC.md section 10).
  stale           INTEGER NOT NULL CHECK (stale IN (0, 1)),
  -- A selector matched several entities with no designated primary.
  ambiguous       INTEGER NOT NULL CHECK (ambiguous IN (0, 1)),
  -- A condition that could not be evaluated against this subject. Not a
  -- violation; a rule-quality signal.
  eval_error      TEXT,
  PRIMARY KEY (check_id, subject_ref, episode)
) STRICT;

CREATE INDEX violation_board ON violation (state, weight DESC, opened_at);
CREATE INDEX violation_subject ON violation (subject_ref);
CREATE INDEX violation_check ON violation (check_id);

CREATE TABLE violation_event (
  id          INTEGER PRIMARY KEY,
  check_id    TEXT NOT NULL,
  subject_ref TEXT NOT NULL,
  episode     INTEGER NOT NULL,
  at          TEXT NOT NULL,
  kind        TEXT NOT NULL,
  sweep_id    INTEGER REFERENCES sweep (id),
  command_id  INTEGER REFERENCES command (id),
  detail      TEXT
) STRICT;

CREATE INDEX violation_event_violation
  ON violation_event (check_id, subject_ref, episode, id);

CREATE TABLE sweep_system (
  sweep_id       INTEGER NOT NULL REFERENCES sweep (id),
  system         TEXT NOT NULL,
  system_kind    TEXT NOT NULL,
  status         TEXT NOT NULL
                 CHECK (status IN ('ok','partial','failed','skipped')),
  complete       INTEGER NOT NULL CHECK (complete IN (0, 1)),
  observed_count INTEGER NOT NULL,
  tombstoned     INTEGER NOT NULL,
  previous_count INTEGER,
  -- Set when the absence guard tripped: the snapshot would have
  -- tombstoned more than the configured share of the system's entities,
  -- so no tombstones were written and the system needs confirmation.
  guard_tripped  INTEGER NOT NULL CHECK (guard_tripped IN (0, 1)),
  duration_ms    INTEGER NOT NULL,
  error          TEXT,
  PRIMARY KEY (sweep_id, system)
) STRICT;

CREATE TABLE person_score (
  person_uid     TEXT PRIMARY KEY,
  score          INTEGER NOT NULL,
  violation_count INTEGER NOT NULL,
  worst_severity TEXT
) STRICT;

CREATE INDEX person_score_rank ON person_score (score DESC);
