-- Which entity types are not people.
--
-- SPEC.md section 6.4 evaluates every unlinked entity as an implicit
-- singleton person, which is what makes orphan-account checks possible:
-- an orphan is by definition unlinked. A device is not an orphan
-- account. Sweeping in a fleet of handsets would otherwise put one
-- implicit person on the Users roster per handset, and hand every
-- unscoped person check a few hundred subjects that cannot answer it.
--
-- The value lives here rather than in `overlord.toml` because it
-- decides which subjects exist, and SPEC.md section 13 requires every
-- input to evaluation to be in the streams -- `replay(streams) == live`
-- is not approximate. The configuration file is the authoring surface;
-- a change there appends an `identity.policy` command, and evaluation
-- reads this projection, exactly as a check or a ruleset does.
--
-- One row, replaced in place: the policy is current state, not a
-- history. The history is the command stream.
CREATE TABLE identity_policy (
  id               INTEGER PRIMARY KEY CHECK (id = 1),
  -- JSON: a sorted array of entity types.
  non_person_types TEXT NOT NULL,
  command_id       INTEGER REFERENCES command (id)
) STRICT;
