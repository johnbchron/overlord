# overlord: Implementation Plan

Companion to [SPEC.md](SPEC.md). The spec says *what* overlord is; this document
says *how* it gets built — the crate layout, the storage schema, the order of
work, and the decisions taken along the way.

Section references like (§7) point at SPEC.md.

---

## 1. Technology decisions

| Area | Choice | Why |
| --- | --- | --- |
| Language / toolchain | Rust stable, via the existing `flake.nix` devshell (`bacon` included) | Already set up; pinned per-machine by nix |
| Workspace | Cargo workspace, edition 2024, `resolver = "3"`, shared `[workspace.dependencies]` and `[workspace.lints]` | Enforces the crate boundaries the architecture depends on |
| HTTP server | `axum` + `tower` / `tower-http` | Minimal, composable, the ecosystem default |
| Templating | `maud` | Compile-time-checked HTML in Rust; no template files, no runtime errors |
| CSS | Hand-rolled, single `overlord.css`, served from the binary at a content-hashed path | Small surface, no build step, no framework to fight |
| Browser interactivity | `htmx` (vendored, pinned) plus a few lines of hand-written JS | Fragment swaps rendered by maud; no JSON API, no bundler |
| Database | SQLite, WAL mode, `foreign_keys = ON` | §13: single embedded store, target scale is thousands of people |
| DB driver | `rusqlite` (bundled SQLite) — one serialized writer connection, a reader pool, reached from axum via `spawn_blocking` | Single-writer append-only model maps exactly; full control over transactions and savepoints; identical code path from CLI, web, and replay tests |
| Migrations | `rusqlite_migration` with embedded SQL files | Versioned, embedded in the binary, no external tooling |
| Expression language | Hand-rolled: lexer → Pratt parser → typed AST → static type-check pass → tree-walking interpreter | §7 semantics (Kleene null, `count(x where p)`, sweep-pinned `days_ago`) diverge too far from any off-the-shelf engine, and §17 wants span-accurate inline errors |
| Regex | `regex` crate | RE2 semantics, linear time — satisfies §7's "cannot hang a sweep" requirement by construction |
| HTTP client | `reqwest` (rustls), wrapped in a restricted client that connectors cannot bypass | §11 read-only-by-construction |
| Time | `jiff` for arithmetic; stored as ISO-8601 UTC `TEXT` | Correct duration/timestamp arithmetic for `days_ago`; text storage keeps the DB human-readable and sortable |
| Serialization | `serde` + `serde_json`; JSON columns, `json_extract` only where a query genuinely needs it | Evaluation happens in Rust, not in SQL |
| CLI | `clap` (derive) | Standard |
| Errors | `thiserror` in libraries, `anyhow` in the binary | Typed errors where callers branch, context where they don't |
| Logging | `tracing` + `tracing-subscriber` | Structured sweep/connector spans |
| Auth | `openidconnect` crate (authorization-code + PKCE), signed cookie session | §14 |
| Testing | `insta` snapshots, `proptest` for the expression language, `wiremock` for connector HTTP | See §7 of this plan |

### Deliberately deferred

No async DB layer, no ORM, no JS build step, no CSS framework, no message
queue, no scheduler. Each of these is addable later behind an existing boundary.

---

## 2. Workspace layout

```
overlord/
  Cargo.toml                    # workspace root: members, shared deps, lints
  bacon.toml
  crates/
    overlord/                   # the single binary (§13): clap dispatch + `serve`
    overlord-core/              # domain types, ids, time, severity, errors
    overlord-expr/              # the check expression language (§7)
    overlord-store/             # SQLite: schema, migrations, streams, projections
    overlord-connect/           # connector trait, restricted HTTP client, normalization
    overlord-connector-fixture/ # deterministic synthetic connector (test bed, forever)
    overlord-connector-gworkspace/
    overlord-engine/            # sweeps, evaluation, lifecycle, identity, scoring
    overlord-web/               # axum + maud + css + htmx
  assets/
    overlord.css
    htmx.min.js                 # vendored, pinned version
  fixtures/                     # scenario data for the fixture connector
```

**Dependency direction** (strictly one-way; enforced by review and by the fact
that adding a back-edge fails to compile):

```
        overlord-core
        /     |      \
   expr    store    connect
        \     |      /   \
        overlord-engine   connector-{fixture,gworkspace}
              |     \        /
          overlord-web   (registered in the binary)
              \       /
             overlord (bin)
```

### Crate responsibilities

**`overlord-core`** — no dependencies on sibling crates. Domain vocabulary:
`SystemId`, `SystemKind`, `EntityRef`, `PersonUid`, `CheckId`, `Revision`,
`Severity` (+ default weights), `EntityStatus`, `SubjectRef`, `ViolationState`,
`SuppressReason`, `Actor`, `SweepId`, `Seq`, `Timestamp` newtype over `jiff`.
Plus the `NormalizedRecord` shape (§11): the guaranteed flat core as typed
fields, everything else as a flat `Map<String, Value>`, with `raw` alongside.

**`overlord-expr`** — self-contained language implementation. Public surface:
`parse(src) -> Result<Ast, ParseError>`,
`typecheck(&Ast, &Schema) -> Result<Typed, Vec<TypeError>>`,
`eval(&Typed, &dyn Subject, &EvalCtx) -> Outcome`. `Subject` is a trait the
engine implements for entities and persons; `EvalCtx` carries the sweep's
`started_at` (the only clock, §7) and compiled regexes. Errors carry byte spans
for inline display. `Outcome` is `True { evidence } | False | Null | Ambiguous`
— note **only `True` opens a violation** (§7).

**`overlord-store`** — owns every SQL statement. Append APIs
(`append_command`, `append_facts`), projection writers, and read models shaped
for the screens. Exposes `Db` with `writer()` (serialized, transactional) and
`reader()` (pooled, read-only). `rebuild()` lives here. No business logic:
the store never decides whether a violation opens.

**`overlord-connect`** — the `Connector` trait:

```rust
#[async_trait]
trait Connector {
  fn system_kind(&self) -> SystemKind;
  fn allowlist(&self) -> &[(Method, PathPattern)];
  async fn observe(&self, http: &RestrictedHttp, ctx: &ObserveCtx)
    -> Result<Snapshot, ConnectorError>;
}

struct Snapshot { completeness: Completeness, entities: Vec<Observation> }
enum Completeness { Complete, Partial { reason: String } }
```

Plus `RestrictedHttp` (wraps `reqwest`, checks every request against the
connector's allowlist, fails closed) and the normalization engine that turns a
raw payload + a ruleset revision into the overlay. A connector receives only
`&RestrictedHttp` — it is never handed a raw client, and it has no store handle,
so "connectors write nowhere but the fact stream" (§11) is a type-level fact,
not a policy.

**`overlord-engine`** — the sweep orchestrator, the evaluator, the violation
lifecycle state machine, identity suggestion computation, and scoring. This is
where the spec's rules actually live.

**`overlord-web`** — axum router, maud pages and fragments, the check editor,
session/OIDC middleware, asset serving.

**`overlord`** — clap dispatch (§5 CLI verbs) and `serve`. Registers the
connector implementations into a registry; nothing else depends on the concrete
connectors, so adding one is a single line here plus a crate.

---

## 3. Storage schema

SQLite, WAL, `foreign_keys = ON`, `synchronous = NORMAL`.

### 3.1 Global sequence — a decision the spec leaves open

SPEC.md §6 defines two independent streams and §13 requires `replay(streams) ==
live`. For replay to be exact, the *interleaving* of the two streams must be
recorded, not reconstructed from wall-clock `at` values (a command can land
while a sweep is running).

**Decision:** one global monotonic sequence, allocated on append to *either*
stream. Every `fact`, `command`, and sweep boundary carries a `seq`. Replay is a
merge on `seq`, with no dependence on clocks at all. A sweep records
`opened_seq` and `committed_seq`; evaluation replays at the commit point.

### 3.2 Streams (append-only, never updated, never deleted)

```sql
CREATE TABLE payload (               -- content-addressed dedup (§14)
  hash TEXT PRIMARY KEY,             -- blake3 of body
  body BLOB NOT NULL
);

CREATE TABLE sweep (
  id             INTEGER PRIMARY KEY,   -- monotonic, is the SweepId (§6.1)
  opened_seq     INTEGER NOT NULL,
  committed_seq  INTEGER,
  started_at     TEXT NOT NULL,         -- the definition of "now" (§10)
  finished_at    TEXT,
  status         TEXT NOT NULL,         -- running | ok | partial | failed
  requested      TEXT NOT NULL,         -- JSON: system ids in scope
  pinned_checks  TEXT NOT NULL,         -- JSON: [(check_id, revision)]
  pinned_norm    TEXT NOT NULL          -- JSON: [(ruleset_id, version)]
);

CREATE TABLE fact (
  id            INTEGER PRIMARY KEY,
  seq           INTEGER NOT NULL UNIQUE,
  sweep_id      INTEGER NOT NULL REFERENCES sweep(id),
  system        TEXT NOT NULL,
  entity_type   TEXT NOT NULL,
  entity_key    TEXT NOT NULL,
  observed_at   TEXT NOT NULL,
  present       INTEGER NOT NULL,       -- 0 = tombstone
  raw_hash      TEXT REFERENCES payload(hash),
  norm_hash     TEXT REFERENCES payload(hash),
  norm_version  TEXT NOT NULL
);
CREATE INDEX fact_entity ON fact(system, entity_key, sweep_id DESC, id DESC);

CREATE TABLE command (
  id              INTEGER PRIMARY KEY,
  seq             INTEGER NOT NULL UNIQUE,
  at              TEXT NOT NULL,
  actor           TEXT NOT NULL,
  kind            TEXT NOT NULL,
  subject         TEXT,
  args            TEXT NOT NULL,        -- JSON
  note            TEXT,
  idempotency_key TEXT UNIQUE,          -- guards duplicate submission (§6.2)
  batch_id        TEXT
);
```

An unchanged entity costs a `fact` row plus two hash references, not a payload
copy (§14).

### 3.3 Projections (droppable, rebuildable)

`entity`, `person`, `person_alias`, `link`, `link_primary`, `suggestion`,
`check_head`, `check_revision`, `check_dryrun`, `violation`,
`violation_event`, `person_score`, `normalization_ruleset`.

`sweep` and `sweep_system` are **not** projections, though an earlier draft of
this plan listed the latter as one. They record what a connector actually
reported during a run — including whether the absence guard tripped, a
decision taken with the snapshot in hand — which is not derivable from the
streams. Clearing them would lose data, not rebuild it.

Shapes worth pinning down now:

```sql
CREATE TABLE violation (              -- one row per (check, subject) episode
  check_id       TEXT NOT NULL,
  subject_ref    TEXT NOT NULL,       -- entity ref or person_uid (§6.5)
  episode        INTEGER NOT NULL,    -- increments on regression
  state          TEXT NOT NULL,       -- open|acknowledged|suppressed|
                                      --   false_positive|resolved
  opened_sweep   INTEGER NOT NULL,
  opened_at      TEXT NOT NULL,
  revision_open  INTEGER NOT NULL,    -- revision in effect at open (§6.5)
  overlay_cmd    INTEGER REFERENCES command(id),
  overlay_rev    INTEGER,             -- revision when the overlay was applied;
                                      --   differs => UI flags a stale ack
  suppress_reason TEXT,
  suppress_until  TEXT,
  weight         INTEGER NOT NULL,
  evidence       TEXT NOT NULL,       -- JSON: leaf values + fact ids (§7)
  stale          INTEGER NOT NULL,    -- partial sweep, last-known state (§10)
  ambiguous      INTEGER NOT NULL,    -- selector returned null (§6.4)
  PRIMARY KEY (check_id, subject_ref, episode)
);

CREATE TABLE person_alias (           -- retired uids resolve forever (§12)
  retired_uid   TEXT PRIMARY KEY,
  surviving_uid TEXT NOT NULL
);
```

`person_alias` is why merge never rewrites history: every lookup of a
`person_uid` goes through alias resolution, so prior violations,
acknowledgements, and suppressions resolve to the survivor untouched.

### 3.4 Write path

Every operator action is one transaction: validate → allocate `seq` → append
command → apply projection deltas → commit. Every sweep system is one
transaction: append facts → update `entity` → record `sweep_system`. Evaluation
is a final transaction at sweep commit. A connector failure therefore cannot
corrupt another system's results (§10).

---

## 4. Milestones

Each milestone is shippable and independently testable. "Done when" is the
acceptance bar.

### M1 — Core and one system

Corresponds to SPEC.md §16 M1, with the fixture connector standing in for a real
one so the engine is built against provokable edge cases.

1. **Workspace scaffolding.** Root `Cargo.toml`, all nine crates, `bacon.toml`,
   shared lint config (`clippy::pedantic` selectively), CI-equivalent
   `cargo fmt --check && cargo clippy -- -D warnings && cargo test`.
2. **`overlord-core`.** Domain types and the `NormalizedRecord` shape.
3. **`overlord-store`.** Migrations for §3 above; append APIs; the `Db` handle
   with serialized writer + reader pool; projection writers; `rebuild()`.
4. **`overlord-expr`.** Lexer, Pratt parser, type checker, interpreter. Full
   §7 surface: three-valued null with Kleene `and`/`or`, `matches` via `regex`,
   `in`, `exists`, `is null`, `??`, `count`/`any`/`all` with `where` predicates,
   one-directional ISO-8601→timestamp coercion, `days_ago` bound to the sweep's
   `started_at`, and the person selectors (`has_entity`, `count_entities`,
   `entity(...)` returning null-and-`ambiguous` rather than guessing).
5. **`overlord-connect`.** `Connector` trait, `RestrictedHttp` with fail-closed
   allowlisting, normalization engine and ruleset versioning.
6. **`overlord-connector-fixture`.** Reads scenario JSON from `fixtures/`.
   Scenarios are first-class: a normal population, a partial snapshot, a
   mass-absence run that must trip the guard, an entity that disappears
   legitimately, ambiguous multi-entity persons, and cross-system orphans.
7. **`overlord-engine`, sweep path.** Pin revisions at `started_at`; run
   connectors; append facts; tombstones **only** from complete snapshots; the
   absence guard (default 10% threshold — record the anomaly, write no
   tombstones, flag the system); per-system transactionality; sweep summary
   with counts and deltas.
8. **`overlord-engine`, evaluation path.** Subject enumeration including
   implicit singleton persons; scope filtering by system id / kind /
   entity_type; evidence capture with fact ids; the full §9 state table
   including regression as a new episode at `open`; suppression expiry against
   sweep time; `check_disabled` resolution; staleness marking on partial sweeps.
9. **Checks as commands.** `check.upsert` with validation (duplicate id, missing
   severity, empty/unparseable condition, static type error), `check.dryrun`,
   and `check.enable` **rejected without a dry-run for that exact
   `(id, revision)`** (§7).
10. **CLI.** `sweep`, `checks list|dry-run|export`, `violations list`,
    `users top`, `rebuild`. A named CLI principal as `actor`.
11. **The replay test.** `replay(streams) == live`, as an integration test over
    a multi-sweep fixture scenario with interleaved commands.

**Done when:** a fixture scenario can be swept repeatedly from the CLI, checks
authored via `check.upsert` open and resolve violations correctly across
sweeps, `rebuild` reproduces the live projections byte-for-byte, and the
absence guard demonstrably refuses to tombstone a truncated snapshot.

### M2 — Operator UI

12. **`overlord-web` skeleton.** Axum router, maud layout, `overlord.css`,
    vendored htmx, content-hashed asset serving.
13. **Violations board (home).** Ranked by weight then earliest-opened episode;
    filters by severity, system, check, subject, lifecycle state; "new since
    last sweep" as a distinct section compared **per system**; `low`/`info`
    collapsed by default. Filters are htmx fragment swaps.
14. **Violation actions.** Acknowledge, suppress (reason + optional `until`),
    false-positive, revoke — each a form POST carrying a client-supplied
    idempotency key, returning the re-rendered row. Stale acknowledgements
    (`overlay_rev != revision_open`) are flagged inline.
15. **Rules screen + check editor.** List with severity, enabled state, scope,
    revision, violation counts, zero-match flags, false-positive rate. Editor
    with revision history, inline span-anchored parse/type errors (htmx
    validation on blur), and a dry-run panel showing match count and sample
    evidence. Enable is gated on dry-run in the UI as well as the command layer.
16. **Users, person/entity detail, Sweeps + coverage, Systems, Settings.**
    Coverage shows per-system entity counts and deltas so a silent connector is
    visible.
17. **Sweep from the UI.** Background tokio task; the sweep row is the progress
    record; the page polls it with htmx.
18. **OIDC.** Authorization-code + PKCE, signed-cookie session, subject/group
    allowlist, `actor` recorded on every command. A `--dev-actor` flag serves a
    fixed principal for local work and **refuses to bind a non-loopback address**
    unless OIDC is configured.

**Done when:** every §5 screen is reachable, the check editor can author,
dry-run, enable, and revise a rule end to end, and no command reaches the store
without an authenticated `actor`.

### M3 — Identity

19. Suggestion computation per sweep with explainable evidence (exact email,
    directory id attributes, username conventions), stored read-only.
20. Confirm / manual link / unlink / primary-per-system-kind, each a command.
21. Merge with permanent `person_alias`, and split with recorded provenance.
22. Promotion of implicit singleton persons on first confirmed link, carrying
    violation history.
23. `suppress_if_pending_links` honoured during evaluation.

**Done when:** confirming a link never changes an organization's total score
(§8) — linking merges scores rather than revealing them — and this is asserted
by a test.

### M4 — Breadth

24. **`overlord-connector-gworkspace`** — Admin SDK Directory API: users,
    status, groups, aliases, licenses, sharing settings. Documents its minimum
    scopes and any scope broader than read-only because Google offers nothing
    narrower. Tested against `wiremock` with recorded response shapes.
25. **IdP connector** (Okta, then Entra ID): accounts, status, MFA, groups, last
    login, admin roles.
26. Access/SSO assignments, then MDM (Intune/Jamf).
27. Per-connector normalization rulesets, authored and revised as commands.

Ordering note: Google Workspace lands first, so the overlay vocabulary (§11) is
shaped by workspace semantics and the IdP connector normalizes onto it.

### M5 — Lifecycle polish

28. Suppression expiry surfacing, false-positive quality signals on the Rules
    screen, JSON/CSV exports, and a starter check library shipped as a set of
    `check.upsert` commands the operator can import and edit.

---

## 5. Cross-cutting design notes

**Read-only by construction.** `RestrictedHttp` is the only HTTP surface a
connector sees, it has no escape hatch, and connector crates do not depend on
`reqwest` directly — a test asserts an unlisted call fails closed, and a second
test asserts the connector crates' dependency lists.

**Determinism.** Evaluation takes `started_at` from the sweep and nothing else.
No `SystemTime::now()` anywhere in `overlord-expr` or in the evaluator — checked
by a clippy `disallowed_methods` entry, not by vigilance.

**Idempotency.** Every command carries a client-supplied key; the UI generates
one per rendered form, the CLI per invocation. Duplicate submission is a no-op
returning the original command id.

**Ambiguity over guessing.** `entity("idp")` with two candidates and no
designated primary returns `null` and marks the violation `ambiguous` (§6.4).
The board shows ambiguous subjects distinctly — they are an identity-work
signal, not a rule failure.

**Error presentation.** `ParseError`/`TypeError` carry byte spans; the check
editor renders the source with the offending span underlined. This is the
answer to §17's last open question, and it is why the parser is hand-rolled.

---

## 6. Configuration

A TOML file plus environment overrides. Secrets — connector credentials, the
OIDC client secret — come from the environment **only**, never from the file and
never from the streams (§14).

```toml
[server]
bind = "127.0.0.1:8080"

[store]
path = "overlord.db"

[auth]
issuer = "https://id.example.com"
client_id = "overlord"
allowed_subjects = ["..."]
required_group = "..."

[sweep]
absence_guard_pct = 10

[[systems]]
id   = "gws-prod"
kind = "workspace"
connector = "google-workspace"
```

---

## 7. Testing strategy

| Layer | Approach |
| --- | --- |
| Expression language | `insta` snapshots of parse trees and of rendered error messages; `proptest` asserting the interpreter never panics and that Kleene semantics hold; a corpus of the §7 examples |
| Store | Round-trip tests per projection; the replay-equals-live integration test as the headline contract |
| Sweep engine | Fixture scenarios: complete/partial snapshots, mass absence, connector failure mid-sweep, partial sweeps producing `stale` violations |
| Lifecycle | A table-driven test walking every row of the §9 state table, including regression, expiry, and `check_disabled` |
| Scoring | Property test: confirming a link leaves the total score unchanged |
| Connectors | `wiremock` with recorded response shapes; an allowlist-violation test per connector |
| Web | Fragment-level `insta` snapshots of maud output; route tests via `tower::ServiceExt::oneshot` |

---

## 8. Open questions carried from SPEC.md §17

These stay open; the plan takes a default where one is needed to proceed.

1. **Tier weights** — default `100 / 50 / 20 / 5 / 1`; per-check override
   supported from M1, so retuning costs nothing.
2. **Connector-level framing** ("system unreachable", "snapshot partial") —
   implemented as sweep/coverage state in M1, *not* as a severity tier.
   Promoting it to a tier later is additive.
3. **Absence-guard threshold per connector** — global config in M1, with the
   value stored on the sweep so history explains itself; per-connector override
   is a config change, not a schema change.
4. **Check-wide suppression** — not built. The `(check, subject)` pair stays the
   only unit; revisit when a real rule needs it.
5. **Hiding implicit persons in the Users view** — shown, ranked alongside
   confirmed persons, with a filter toggle. Revisit on real volume.
6. **Check editor error contract** — answered above: byte spans surfaced inline.
