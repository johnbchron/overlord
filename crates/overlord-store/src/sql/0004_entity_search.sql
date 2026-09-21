-- Full-text search over the current state of every entity.
--
-- "Current state" is the latest fact's content (SPEC.md section 6.1),
-- which is what the `entity` row already holds: the normalization
-- overlay inline, and the vendor payload by hash in `payload`.
--
-- Leaf VALUES are indexed, not the JSON around them. `json_tree` walks
-- both documents and keeps the scalars, so searching `grandstream`
-- finds the handsets whose vendor is Grandstream rather than every
-- entity that happens to have a `vendor` key. Keys are structure; an
-- operator searching a box is looking for content.
--
-- Not an external-content table. That form keeps only the index and
-- reads the columns back from its content table on demand, which would
-- mean re-walking two JSON documents per result row and a contentless
-- index that silently returns nothing when the two drift. The duplicated
-- text is worth the index being self-contained.
--
-- `entity_key` is indexed separately from `body` so that an extension
-- number or a MAC address is findable as itself, and so a future caller
-- can weight a key hit above a body hit without reindexing.
CREATE VIRTUAL TABLE entity_search USING fts5(
  system UNINDEXED,
  entity_type UNINDEXED,
  entity_key,
  body,
  -- `tokenchars` is the part worth explaining. Almost everything an
  -- operator searches for here is an identifier containing
  -- punctuation: a firmware version (1.0.11.76), an address
  -- (ada@example.com), an IP, a MAC written with colons, a
  -- hyphenated entity type. The default tokenizer splits all of
  -- those into digits and fragments, so one firmware version
  -- matches every device sharing any component of it -- `9.9.9.9`
  -- finding a handset on `1.0.9.10` is not a near miss, it is the
  -- wrong answer. Keeping these characters inside tokens makes an
  -- identifier one token, and prefix matching (see `fts_query`) is
  -- what still lets `ada` find `ada@example.com`.
  --
  -- `read::TOKEN_CHARS` must spell the same set.
  tokenize = 'unicode61 remove_diacritics 2 tokenchars ''.-:@_'''
);

-- Maintained by trigger rather than in `project.rs`, which is where
-- every other projection is built.
--
-- The reason is that `entity` is written from two places — the sweep's
-- upsert and `rebuild`'s replay — and a search index that one of them
-- forgot would not fail: it would quietly return fewer results than the
-- store holds, which is the worst way for a search box to be wrong. A
-- trigger cannot be forgotten by a new writer. It also means `rebuild`
-- needs no special case: clearing `entity` empties this, and replaying
-- it fills this back in.
CREATE TRIGGER entity_search_insert AFTER INSERT ON entity BEGIN
  INSERT INTO entity_search (rowid, system, entity_type, entity_key, body)
  VALUES (
    NEW.rowid,
    NEW.system,
    NEW.entity_type,
    NEW.entity_key,
    (SELECT coalesce(group_concat(t.value, ' '), '')
       FROM json_tree(coalesce(NEW.normalized, '{}')) t
      WHERE t.type IN ('text', 'integer', 'real'))
    || ' ' ||
    (SELECT coalesce(group_concat(t.value, ' '), '')
       FROM json_tree(
              coalesce(
                (SELECT p.body FROM payload p WHERE p.hash = NEW.raw_hash),
                '{}'
              )
            ) t
      WHERE t.type IN ('text', 'integer', 'real'))
  );
END;

-- An upsert lands here: the sweep writes each entity with
-- ON CONFLICT DO UPDATE, so this is the path a changed account takes.
CREATE TRIGGER entity_search_update AFTER UPDATE ON entity BEGIN
  DELETE FROM entity_search WHERE rowid = OLD.rowid;
  INSERT INTO entity_search (rowid, system, entity_type, entity_key, body)
  VALUES (
    NEW.rowid,
    NEW.system,
    NEW.entity_type,
    NEW.entity_key,
    (SELECT coalesce(group_concat(t.value, ' '), '')
       FROM json_tree(coalesce(NEW.normalized, '{}')) t
      WHERE t.type IN ('text', 'integer', 'real'))
    || ' ' ||
    (SELECT coalesce(group_concat(t.value, ' '), '')
       FROM json_tree(
              coalesce(
                (SELECT p.body FROM payload p WHERE p.hash = NEW.raw_hash),
                '{}'
              )
            ) t
      WHERE t.type IN ('text', 'integer', 'real'))
  );
END;

CREATE TRIGGER entity_search_delete AFTER DELETE ON entity BEGIN
  DELETE FROM entity_search WHERE rowid = OLD.rowid;
END;

-- Backfill what is already in the store. A migration that only caught
-- future writes would leave every existing entity unfindable until its
-- next sweep, which reads as "search is broken" rather than "search is
-- new".
INSERT INTO entity_search (rowid, system, entity_type, entity_key, body)
SELECT
  e.rowid,
  e.system,
  e.entity_type,
  e.entity_key,
  (SELECT coalesce(group_concat(t.value, ' '), '')
     FROM json_tree(coalesce(e.normalized, '{}')) t
    WHERE t.type IN ('text', 'integer', 'real'))
  || ' ' ||
  (SELECT coalesce(group_concat(t.value, ' '), '')
     FROM json_tree(
            coalesce(
              (SELECT p.body FROM payload p WHERE p.hash = e.raw_hash),
              '{}'
            )
          ) t
    WHERE t.type IN ('text', 'integer', 'real'))
FROM entity e;
