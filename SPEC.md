# overlord: Next-Generation Blueprint

Status: design blueprint, greenfield. Audience: an implementation agent with
no prior context. This document is self-contained and assumes nothing already
exists. The product name is **overlord**.

---

## 1. Purpose of this document

This is a build spec for **overlord**, a new, single-operator, observe-only
system that answers one question continuously and readably: **are we in a good
state?**

It covers what the product is for, the concepts and data model, the check
language, prioritization, the operator workflows, the collection model, and the
architecture that supports them. It is product-first; architecture appears only
where the product requires it.

---

## 2. Vision and principles

**Vision.** overlord is a general account-health and adherence platform. It
connects to many systems, observes their accounts and configuration, and reports
where the organization is not in a good state. It is deliberately general: it is
not a certification tool, a framework-compliance tool, or a domain-specific rule
pack. Operators express their own notion of "good" and overlord measures reality
against it.

**Prioritize loud problems, record quiet ones.** Glaring security and
lifecycle problems should dominate the operator's attention. Pedantic hygiene
issues must still be detected and stored, but must not compete for the top of
the screen. Priority is explicit and operator-defined.

**Observe only.** overlord reads systems and never writes to them. Connecting
a new system must never create a side effect, a risk, or a change request in that
system. This is a hard safety property, not a current limitation.

**Breadth is the product.** Value scales with the number of connected systems and
the quality of insight drawn from them. Making a new system connectable should be
routine.

**Everything is inspected; nothing is discarded.** Every observation, every
operator decision, and every violation's history is retained and attributable.

**Defer to the operator's judgment.** Matching, exceptions, and suppressions are
operator decisions. overlord may *suggest*, never *decide*.

**Replay is the contract.** Everything that determines what the board says —
facts, checks, normalization, and operator decisions — lives in the two streams.
Nothing that affects evaluation lives outside them.

---

## 3. Personas and jobs

**Primary persona: the operator.** A single technical owner of overlord. They
connect systems, author checks, and work the resulting queue. They are the only
role in v1.

**Jobs to be done**

1. "Show me the worst problems right now, ranked." (top violations)
2. "Which people are the biggest sources of risk?" (top violating users)
3. "What does my rule set look like, and is each rule doing anything?" (checks)
4. "What did the last sweep actually see?" (coverage and freshness)
5. "Let me mark this as known/expected without lying about the data."
6. "Let me add a new system and a new rule without writing a service."

**Cadence.** Episodic sweeps triggered by the operator ("run it every so
often"), not streaming and not scheduled notifications. The design must not
*preclude* scheduling or notifications later, but v1 is pull-only.

---

## 4. Core concepts

- **System**: a connected, read-only source. Each connected system has a
  **system id** (this instance, e.g. `okta-prod`) and a **system kind**
  (`idp`, `workspace`, `sso`, `mdm`). Checks may restrict by either.
- **Entity**: one account or object observed in a system, identified by a stable
  key within that system (for example a user's primary email).
- **Fact**: an immutable observation of an entity's state at a point in time,
  produced by a sweep. Facts are raw payload plus a normalization overlay. A
  **tombstone fact** records that an entity was absent from a complete snapshot.
- **Person**: a unified human. A person is formed by operator-confirmed links
  across systems; every unlinked entity is additionally treated as an **implicit
  singleton person** for evaluation, so cross-system checks work before linking
  is complete.
- **Suggestion**: a machine-proposed link between entities or to a person,
  computed each sweep, visible to the operator, and never applied automatically.
- **Check (Rule)**: an operator-authored condition over normalized facts, with a
  severity, a scope, and remediation guidance. Checks are **versioned records in
  the command stream** (section 7), not files. Checks are the only detection
  mechanism.
- **Violation**: a standing failure of a check against a subject (an entity or a
  person), with a lifecycle. Violations are derived; their lifecycle state is
  operator-recorded.
- **Suppression**: an operator-recorded reason a violation should not count as
  bad state, always attributed and usually with an expiry.
- **Command**: an append-only, attributed operator action (confirm a link,
  acknowledge a violation, revise a check, merge persons).
- **Sweep**: one collection run across one or more systems, producing facts.
  Its start time is the single definition of "now" for that run.
- **Projection**: a rebuildable read model derived from the fact and command
  streams.

---

## 5. Product surfaces

**UI-first.** The web UI is the primary surface, and the only place checks are
authored. The CLI exists for automation and administration. Authentication is
external OIDC.

### Screens

1. **Violations (home).** The default view. Ranked list of active violations,
   highest severity first, with filters by severity, system, check, subject, and
   lifecycle state. A distinct **"new since last sweep"** section. `low` and
   `info` are collapsed by default.
2. **Rules (checks).** All checks with severity, enabled state, scope, current
   revision, violation counts, zero-match flags, and false-positive rate (a
   rule's own quality signal). Entry point to the **check editor**, which
   supports authoring, revision history, and **dry-run**.
3. **Users.** Persons and unlinked entities ranked by risk score ("top violating
   users"), searchable by name, email, or key.
4. **Person / entity detail.** Identities and links (with pending suggestions),
   all violations and their history, lifecycle state, and a fact timeline.
5. **Sweeps.** Run history with per-system status, counts, and errors; a sweep
   detail showing **coverage** (what each system reported, with entity-count
   deltas against the previous sweep) so a silent or empty connector is visible.
6. **Systems.** Connected systems, credential/config status, last successful
   sweep, and entity counts. Lightweight; not a full asset inventory.
7. **Settings.** OIDC configuration, connector configuration, normalization
   versions.

### CLI (non-interactive)

- `sweep` (run collection across systems; flags to restrict systems)
- `checks list` / `checks dry-run <id>` / `checks export` (JSON, for review and
  backup; authoring itself is UI-only)
- `violations list` / `users top`
- `link`, `merge`, `acknowledge`, `suppress` (mirroring commands)
- `export` (JSON/CSV of violations, users, or a person's history)
- `rebuild` (rebuild projections from the streams)

---

## 6. Domain model

overlord has **two independent append-only streams** and a set of
**rebuildable projections**.

### 6.1 Fact stream (observed reality)

Append-only. Written only by sweeps. One row per entity observation.

```
fact
  id
  sweep_id
  system                 -- system id
  entity_type            -- e.g. "user", "group", "device"
  entity_key             -- stable key within the system
  observed_at            -- recorded time (UTC)
  present                -- false for a tombstone (entity absent from a
                         --   complete snapshot)
  raw                    -- vendor payload as received (JSON)
  normalized             -- normalization overlay (JSON), see section 11
  normalization_version  -- the ruleset revision that produced `normalized`
```

"Current state" for an entity is the latest fact by `(sweep_id, id)`; sweep ids
are monotonic, so ordering never depends on connector clocks. An entity whose
latest fact is a tombstone is **absent**: it is excluded from evaluation and its
violations resolve. Facts are never updated or deleted. The stored `normalized`
overlay is authoritative; changing normalization affects future sweeps only, so a
rebuild always reproduces the overlay that was live at the time.

### 6.2 Command stream (operator intent)

Append-only. Written only by operator actions, each attributed and idempotent.

```
command
  id
  at
  actor             -- OIDC subject/email, or the named CLI principal
  kind              -- see below
  subject           -- entity ref, person_uid, check id, or violation ref
  args              -- JSON payload
  note              -- optional free text
  idempotency_key   -- client-supplied; guards duplicate submissions
  batch_id          -- optional, groups multi-step actions (e.g. merge)
```

Command kinds (v1):

*Identity* — `person.create`, `person.link`, `person.unlink`, `person.merge`,
`person.split`, `person.set_primary`.

*Violations* — `violation.acknowledge`, `violation.suppress` (with `reason` and
optional `until`), `violation.false_positive`, `violation.revoke` (undo an
overlay).

*Checks* — `check.upsert` (creates the check or appends a new revision),
`check.dryrun` (records the result of a dry-run for one `(check id, revision)`),
`check.enable`, `check.disable`.

*Normalization* — `normalization.upsert` (a new ruleset revision for a system
kind, keyed by id and version).

*Identity policy* — `identity.policy` (the entity types that are not people, see
section 6.4). Authored in the configuration file; a change there appends one of
these, and evaluation reads only the command.

Checks, normalization rulesets and the identity policy are configuration **and**
operational state: because they determine what the board says, they live in the
command stream with everything else. Connector credentials and endpoints remain
external configuration, because they do not affect evaluation.

### 6.3 Projections (derived, rebuildable)

All reading happens against projections. Projections can be dropped and rebuilt
by replaying the two streams; a test must assert `replay(streams) == live`. With
checks and normalization in the stream, that assertion is now total: no input to
evaluation lives outside it.

- `entity` (latest normalized + raw per `(system, entity_key)`, with presence)
- `person` (`person_uid`, derived display identity, membership)
- `link` (entity to person, with confirmation command id)
- `suggestion` (candidate links, recomputed per sweep)
- `check` (latest revision per id, plus enabled state and revision history)
- `violation` (derived condition + lifecycle overlay + evidence)
- `violation_event` (lifecycle history per violation: opened, acknowledged,
  suppressed, cleared, regressed)
- `sweep` (run metadata and per-system summary)

### 6.4 Entities, accounts, persons, links

An **entity** is always keyed within its system. A **person** is created only by
an operator command, or implicitly when the operator confirms a link. Before
confirmation, related entities are merely a **suggestion**.

Every entity not linked to a confirmed person is evaluated as an **implicit
singleton person** with a derived `person_uid`. This keeps orphan-account checks
("workspace account with no IdP counterpart") meaningful, since an orphan is by
definition unlinked. Implicit persons are visible in the Users view and are
promoted to real persons on the first confirmed link, carrying their violation
history with them.

**Not every entity type is a person.** That reasoning is about accounts: a device
has no counterpart to be missing, and an implicit singleton over one holds
exactly the entity an entity-scoped check already sees. What it would add is a
subject on the Users roster per device, and a few hundred subjects handed to
every person check that declared no scope. The **identity policy** names the
entity types that are not people. Entities of those types are still collected,
still evaluated by entity-scoped checks, still shown on their own entity pages,
and still become members of a real person once one is confirmed for them — they
are simply never *implicit* people, and are not proposed as link candidates.

The policy is authored in the configuration file, which is the operator's
natural place for it, but the file is not what evaluation reads: a change there
appends an `identity.policy` command, and evaluation reads the projection.
Section 13 admits no input to evaluation outside the two streams, and this
decides which subjects exist. Turning the policy on resolves the violations it
orphans with reason `subject_absent`, the same as any subject leaving scope.

A person may hold many entities, potentially several of the same type; the
operator may designate a **primary** entity per system kind for display and for
`entity(...)` selectors. Where no primary is designated and more than one
candidate exists, the selector returns `null` and the check is flagged
`ambiguous` on that subject rather than guessing.

### 6.5 Checks, violations, lifecycle

A **check** declares a scope (which subjects it is evaluated against) and a
**condition**. Each sweep, every enabled check revision is evaluated against
every in-scope subject.

A **violation** is identified by `(check_id, subject_ref)` where `subject_ref` is
an entity ref (entity-scoped checks) or a `person_uid` (person-scoped checks).
This stable identity is what lets acknowledgements and suppressions persist
across sweeps, so a check's `id` is chosen once and never reused.

**Revisions.** Overlays survive a revision bump, but each violation episode
records the revision in effect, and the UI flags an acknowledgement made under an
earlier revision so a rewritten rule cannot silently inherit someone's "I've
seen it". Disabling a check resolves its open violations with reason
`check_disabled`; re-enabling reopens them as regressions if the condition still
holds.

---

## 7. Checks: definition and expression language

Checks are **operator-authored records**, created and edited only in the UI and
stored as revisions in the command stream. Every edit is attributed, diffable,
and replayable; `checks export` produces JSON for review or backup, but the
stream is authoritative and there is no file to hand-edit or reconcile.

### Check record

| field | notes |
| --- | --- |
| `id` | stable, operator-chosen, never reused |
| `revision` | monotonic per id; each `check.upsert` appends one |
| `name`, `description`, `rationale`, `remediation`, `references` | human-facing |
| `severity` | `critical` \| `high` \| `medium` \| `low` \| `info` |
| `weight` | optional override of the tier default |
| `applies_to` | `entity` \| `person` |
| `systems`, `entity_types` | optional scope restriction (system ids or kinds) |
| `condition` | expression source (below) |
| `suppress_if_pending_links` | optional; skip subjects with unconfirmed suggestions |
| `enabled` | set by `check.enable` / `check.disable`, never by `upsert` |

Validation on upsert rejects a missing or duplicate `id`, a missing `severity`,
an empty condition, an unparseable condition, and a type error found by static
analysis. Zero-match warnings live on the Rules screen, where the history to
judge them exists.

**Dry-run before enable is enforced.** `check.enable` is rejected unless a
`check.dryrun` record exists for that exact `(id, revision)`. Dry-run evaluates
the condition against current facts and records match count and sample evidence;
it opens no violations and touches no other projection.

### Expression language

Deliberately small, statically typed, and deterministic. It evaluates over
**normalized attributes**, with an explicit escape hatch to the raw payload.

**Types.** `string`, `number`, `boolean`, `timestamp`, `list`, `null`. Timestamp
literals are ISO-8601 strings in a timestamp comparison (`last_login_at <
"2025-01-01"`); the coercion is one-directional and any other mismatched
comparison is a validation error, not a silent `false`.

**Paths.** Dotted paths into the normalized record (`status`, `mfa_enrolled`,
`groups`, `department`). The overlay is flat at the root (section 11), so there
is one place to look. Vendor fields are reachable as `raw.<path>`. A missing path
is `null`.

**Null is three-valued.** Comparison with `null` yields `null`, `not null` is
`null`, and `and` / `or` follow Kleene logic. **Only `true` opens a violation**,
so a check never fires on data that simply was not collected. Use `is null`,
`exists`, or `a ?? b` (default) to handle absence deliberately.

**Operators.** `==`, `!=`, `<`, `<=`, `>`, `>=`, `in` (membership), `matches`
(RE2 regular expressions; no backtracking, so a check cannot hang a sweep),
`is null`, `exists`, `and`, `or`, `not`, and parentheses.

**Collections.** One form, one predicate:

```text
count(groups)
count(groups where external)          # predicate fields refer to the element
any(groups where external)
all(groups where not external)
```

Predicates see element fields only; they cannot reference the enclosing subject.
`none(...)` is spelled `not any(...)`. Aggregates over element values
(`sum`, `min`, `max`) are deferred until a real rule needs them.

**Relative time.** `days_ago(90)` returns a timestamp derived from the **sweep's
start time**, which is recorded on the sweep, so a replay reproduces the original
result exactly. There is no other clock inside evaluation.

**Person-scoped selectors.**

```text
has_entity("idp")                            # by system kind or system id
has_entity("workspace" where status == "active")
count_entities("mdm") > 1
entity("idp").mfa_enrolled                   # primary entity; null if ambiguous
```

### Examples

```text
# idp-mfa-missing (entity, critical)
status == "active" and not mfa_enrolled

# workspace-without-idp (person, high)
has_entity("workspace" where status == "active") and not has_entity("idp")

# dormant-admin (entity, high)
is_admin and (last_login_at is null or last_login_at < days_ago(90))

# external-sharing (entity, medium)
external_sharing == true or count(groups where external) > 0
```

### Evidence

Every opened violation stores the evaluated leaf values that made its condition
true, alongside the fact ids they came from. This is what makes the board
actionable and the audit trail meaningful, and it is what dry-run shows as
samples.

---

## 8. Severity, priority, and scoring

**Fixed tiers.** `critical`, `high`, `medium`, `low`, `info`, with default
weights (for example `100, 50, 20, 5, 1`) used for ranking and scoring. A check
may override its weight; weight breaks ranking ties and feeds user scoring.

**Ranking.** Higher weight first. Within a tier, the violation whose current
episode opened earliest comes first (it has been ignored longest). Severity is
entirely operator-defined; there is no system-imposed floor or ceiling.

**Top violating users.** A subject's score is the **sum of weights of its active
violations**, where active excludes `suppressed` and `false_positive` but
includes `acknowledged` (acknowledged means seen, not fixed). A person's score
includes the entity-scoped violations of all its entities, and because unlinked
entities are implicit persons, confirming a link never changes a total — linking
merges scores rather than revealing them, so identity work is never penalized.
Display the score, the count, and the worst single severity. Ties break by worst
severity, then by name.

**The board.** Violations are presented as: (1) top violations, (2) the rule set,
(3) top violating users. A "new since last sweep" section highlights change
without making full diff analysis a feature; it compares against the previous
sweep that covered the same system.

---

## 9. Violation lifecycle

States: `open`, `acknowledged`, `suppressed`, `false_positive`, `resolved`.
`open` and `acknowledged` count as bad state; the rest do not. Expiry is
evaluated at sweep time, using the sweep's start time.

| from | event | to |
| --- | --- | --- |
| — | condition holds | `open` |
| `open` | `violation.acknowledge` | `acknowledged` |
| `open`, `acknowledged` | `violation.suppress` | `suppressed` |
| any | `violation.false_positive` | `false_positive` |
| `open`, `acknowledged`, `suppressed` | condition clears | `resolved` (overlay dropped) |
| `suppressed` | `until` reached, condition holds | `open` |
| any overlay state | `violation.revoke` | recomputed from condition |
| `resolved` | condition holds again | `open` (new episode, flagged as regression) |
| any | check disabled | `resolved` (reason `check_disabled`) |

Notes:

- **Auto-resolve is the only close.** There is no manual "resolve": marking
  something fixed that the next sweep still sees would be a lie the system then
  has to reconcile. Use suppression or false-positive.
- **Regression starts clean.** A reopened violation returns to `open`, never to a
  carried-over `acknowledged`. Prior episodes and their acknowledgements stay
  visible in history, so the regression is loud rather than pre-silenced.
- **Suppression carries a reason** — `accepted_risk`, `bad_source_data`, or
  `expected` — plus attribution and an optional `until`. This replaces the
  separate exception and override verbs; the distinction between them was only
  ever the reason. Suppression applies to a violation `(check, subject)`
  regardless of scope, so person-scoped violations are covered.
- **False positive is check feedback**, not subject truth, and is surfaced as a
  quality signal on the Rules screen.
- Every transition above is either a recorded command or a derived consequence of
  a fact; both are replayable.

---

## 10. Sweeps and collection

A **sweep** is an explicit operator action (UI button or CLI command). It:

1. Records `started_at`, which is the definition of "now" for the entire run,
   and pins the set of enabled check revisions and normalization versions it
   will use. Later edits do not affect a run in progress or its replay.
2. Runs each configured connector's `observe` (read-only) to collect current
   state for its system.
3. Normalizes payloads and **appends facts**, including tombstones for entities
   missing from a snapshot the connector declared **complete**.
4. Updates projections: current entity state, identity suggestions, and check
   evaluation.
5. Evaluates every pinned check revision against every in-scope subject, opening,
   updating, or resolving violations, and stores evidence for each.
6. Records a `sweep` summary: per-system status, row counts and deltas,
   durations, errors.

**Absence guard.** Tombstones are written only from complete snapshots, and a
sweep that would tombstone more than a configured share of a system's entities
(default 10%) records the anomaly, writes no tombstones for that system, and
flags the system for operator confirmation. A truncated snapshot must never
present as a mass deprovisioning.

**Partial sweeps.** When a sweep covers only some systems, entity-scoped checks
for unswept systems are not re-evaluated, and person-scoped checks that reference
an unswept system are evaluated against last-known state and marked **stale** on
the board rather than silently trusted. "New since last sweep" compares
per-system, so restricting a sweep does not manufacture change.

A sweep is transactional per system: a connector failure records a failed system
status without corrupting facts already collected or other systems' results. The
**coverage** view surfaces what each system actually reported.

Cadence is operator-driven in v1. The design keeps scheduling and per-system
cadence as a future addition, not a re-architecture.

---

## 11. Connectors and normalization

A **connector** is a read-only adapter for one system kind. It emits, per entity,
the **raw vendor payload** plus a **normalization overlay** computed from it, and
declares whether the snapshot it returned is **complete** (a full enumeration) or
**partial** (paged out, rate-limited, permission-denied). Only complete snapshots
may produce tombstones.

**Guaranteed overlay core**, flat at the root so checks have one vocabulary:

- `system`, `system_kind`, `entity_type`, `entity_key` (stable within the system)
- `display_name`
- `status` — one of `active`, `suspended`, `deprovisioned`, `invited`, `unknown`
- everything else the connector can map, as flat top-level fields
  (`mfa_enrolled`, `is_admin`, `last_login_at`, `groups`, `department`, …)

Vendor-specific detail stays reachable at `raw.<path>`.

**Normalization is versioned and operator-visible.** A connector ships a default
ruleset; every change, shipped or operator-made, is a `normalization.upsert`
revision in the command stream, and each fact records the version that produced
its overlay. Rebuilds use the stored overlay, so history stays exactly as it was
read.

**Read-only by construction.** Connectors use a restricted HTTP client with a
per-connector allowlist of method-and-path pairs; anything outside it fails
closed, and a test asserts no connector can issue an unlisted call. Connectors
write nowhere but the fact stream.

**Connector priority (v1 target order)**, aimed at a managed-service-provider core:

1. Identity provider (Okta, Entra ID): accounts, status, MFA, groups, last login,
   admin roles.
2. Workspaces (Google Workspace, Microsoft 365): users, status, groups, aliases,
   sharing settings, licenses.
3. Access and SSO applications: assignments and entitlements.
4. MDM (Intune, Jamf): devices, enrollment, compliance state, ownership.

Each connector documents its minimum required scopes, and any scope that is
unavoidably broader than read-only because the vendor offers nothing narrower.

---

## 12. Identity resolution

Cross-system unification is a core insight generator, and is **operator-driven**.

- Each sweep computes **candidate links** using conservative, explainable
  signals (exact email, directory id attributes, username conventions). Each
  candidate shows its evidence.
- Suggestions are **read-only**. They are never applied automatically, and no
  command is emitted on their behalf. A check may set
  `suppress_if_pending_links` to stay quiet about a subject whose suggestions are
  still unreviewed, so an un-linked-yet account does not generate noise.
- The operator **confirms** a suggestion, which emits `person.link` (creating the
  person first if needed). Manual link, unlink, and primary designation are the
  same kind of command.
- **Merge** combines two persons: the operator chooses the surviving
  `person_uid`, and the retired uid becomes a permanent alias so every prior
  violation, acknowledgement, and suppression resolves through it rather than
  being rewritten. **Split** creates a new uid for the departing entities; the
  original keeps the history, and the split is recorded so the provenance of the
  new person is traceable.
- A surviving `person_uid` never changes meaning, and a retired one never
  disappears.

Unlinked entities remain first-class: they appear in the Users view as implicit
persons and are subject to both entity- and person-scoped checks.

---

## 13. Architecture

Chosen shape: **command-sourced state with rebuildable projections**, embedded
and single-node.

- **Two streams, one direction.** Facts and commands are append-only. Reads come
  from projections; nothing reads state by mutating it.
- **Projections are disposable.** Any projection can be rebuilt by replaying the
  streams. Because checks and normalization are in the command stream and each
  sweep pins the revisions it used, replay is exact, and the replay-equals-live
  test is a real contract rather than an approximation.
- **Write path.** Commands are validated, appended, and projected in a single
  transaction. Facts are appended by sweeps and projected likewise. Commands
  carry idempotency keys so retried submissions do not duplicate actions.
- **Time.** Recorded time only (UTC). Evaluation's only clock is the sweep's
  `started_at`, which is stored; expiries and relative windows resolve against
  it. There is no validity-interval model; "as of" and late-arriving data are out
  of scope for v1 but the append-only streams leave room for them.
- **Storage.** A single embedded database inside the binary, WAL-mode, foreign
  keys on. Appropriate for the target scale (hundreds to thousands of people,
  tens of systems). The projection boundary is the abstraction that would let
  storage move later.
- **Process.** One binary. The web server and the CLI are two front ends over the
  same core library; collection and evaluation live in the library.
- **Determinism.** Given the same streams, evaluation is deterministic, with no
  remaining external input.

---

## 14. Security, authentication, and data

- **Authentication.** External OIDC (authorization-code with PKCE). Every command
  records the authenticated `actor`. CLI commands record a named CLI principal
  and supply their own idempotency keys.
- **Authorization.** Access requires membership in an allowlist of OIDC subjects
  or a required group claim. Authentication alone is not sufficient: the store
  holds personal data for the whole organization. Beyond the allowlist there are
  no roles in v1, but the `actor` field makes RBAC addable.
- **Read-only by construction.** Enforced by the connector call allowlist
  (section 11), not by policy alone.
- **Secrets.** Connector credentials and the OIDC client secret come from
  configuration and environment, never the streams.
- **Retention and durability.** Keep all facts and commands; they are the audit
  trail and the product's memory. Facts are deduplicated by payload hash, so an
  unchanged entity costs a reference rather than a copy. The store is encrypted
  at rest and has a documented backup and restore procedure, since losing it
  loses everything. Compaction is future work and the append-only model makes it
  safe to add.
- **Sensitive data.** Facts may contain personal data. overlord does not redact
  in v1, but treats the store as sensitive and keeps it local to the deployment.

---

## 15. Non-goals for v1

- Any write or remediation action against connected systems.
- Notifications, email, chat, webhooks, or scheduled delivery.
- Multiple operators, RBAC, approval workflows, or review campaigns.
- Multi-tenant hosting or organization isolation.
- Bitemporal/as-of queries and late-arriving-data correction.
- Real-time streaming ingestion.
- A heavy asset/system inventory or CMDB.
- Diff-as-a-product (change surfaces only as "new since last sweep").
- Domain-specific certification rule packs.
- Editing checks outside the UI, or any file-based rule format.

---

## 16. Roadmap

**M1, Core and one system.** Two streams and projections; sweep engine with
pinned revisions, tombstones, completeness and the absence guard; one connector
with a versioned normalization ruleset; checks as commands with revisions and
enforced dry-run; the expression language; the full violation state table;
implicit singleton persons so person-scoped checks work from the start; CLI
sweep, dry-run and violations listing; the replay-equals-live test.

**M2, Operator UI.** Violations board (ranked, filterable, with "new since last
sweep"); Rules list and the check editor with dry-run and revision history; Users
view with risk ranking; person and entity detail; Sweeps and coverage; OIDC
sign-in with the subject allowlist.

**M3, Identity.** Suggestion computation with evidence; confirm/unlink; primary
entity per system kind; merge with uid aliasing and split; promotion of implicit
persons; person detail linking workflow.

**M4, Breadth.** Connectors for IdP, workspaces, access/SSO, and MDM; per-system
coverage reporting; normalization revisions authored per connector.

**M5, Lifecycle polish.** Suppression expiry handling, false-positive quality
signals on the Rules screen, exports, and a starter check library.

**Later (not v1).** Scheduling and cadence, notifications, RBAC, as-of queries,
retention/compaction, aggregates in the expression language, additional systems.

---

## 17. Open questions

- Exact tier weights, and whether a tier is reserved for connector-level framing
  ("system unreachable", "snapshot partial").
- Whether the absence guard's threshold should be per connector rather than
  global, and what the confirmation flow looks like when it trips.
- Whether suppression should ever be scoped to a whole check rather than a
  `(check, subject)` pair, and how that would be shown.
- Whether implicit singleton persons should be hidden from the Users view once an
  organization has many of them, or ranked alongside confirmed persons as now.
- The validation error contract for the check editor: how parse and type errors
  are surfaced inline.
