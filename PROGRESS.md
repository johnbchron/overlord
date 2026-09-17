# overlord: Progress

Working log for the build described in [PLAN.md](PLAN.md). Updated as work
lands, not in advance. Spec references like (§7) point at
[SPEC.md](SPEC.md); plan references at PLAN.md.

**Current milestone:** M1 — Core and one system. **Complete.**

---

## Status

| # | M1 task | State |
| --- | --- | --- |
| 1 | Workspace scaffolding | done |
| 2 | `overlord-core` | done |
| 3 | `overlord-store` | done |
| 4 | `overlord-expr` | done |
| 5 | `overlord-connect` | done |
| 6 | `overlord-connector-fixture` | done |
| 7 | Sweep path (tombstones, absence guard, per-system transactions) | done |
| 8 | Evaluation path (scope, lifecycle, evidence, scoring) | done |
| 9 | Checks as commands, dry-run enforced | done |
| 10 | CLI | done |
| 11 | `replay(streams) == live` | done |

`cargo test --workspace`: **154 passing, 0 failing.**
`cargo clippy --workspace --all-targets`: **no warnings.**

M2 (UI), M3 (identity), M4 (real connectors), M5 (lifecycle polish) are
untouched. `suggestion` is a table with no writer yet — M3 fills it, and
`suppress_if_pending_links` is implemented against it and therefore inert
until then.

Three read models were written in M1 and still have no caller:
`Reader::false_positive_rate`, `Reader::has_dryrun` and
`Reader::last_sweep_covering`. They are not dead code to delete — each
answers a question an M2 screen asks — but nothing exercises them yet,
so treat them as untested until a screen does.

### Try it

No credentials, no network:

```sh
cargo run -- -c examples/fixture.toml checks import examples/starter-checks.json
cargo run -- -c examples/fixture.toml checks dry-run mfa-missing
cargo run -- -c examples/fixture.toml checks enable mfa-missing
cargo run -- -c examples/fixture.toml sweep
cargo run -- -c examples/fixture.toml violations
cargo run -- -c examples/fixture.toml users
cargo run -- -c examples/fixture.toml rebuild
```

---

## Starting M2 — handoff

Written at the end of M1 for a reader with no prior context. PLAN.md §4
lists the M2 tasks; this is what the code actually leaves you, and the
traps that are not visible from the outside.

### Where it plugs in

Add `crates/overlord-web` (axum + maud) and a `serve` subcommand to
`crates/overlord/src/main.rs`, which is already `#[tokio::main]`. The
dependency edge is `overlord-web → overlord-engine, overlord-store,
overlord-core`; nothing should make the store or the engine depend on the
web crate. `crates/overlord/src/render.rs` is the CLI's terminal
rendering and is **not** a template to port — maud pages should be
written fresh; only the field choices are worth copying.

`assets/` exists but is empty, so it is untracked and a fresh clone will
not have it. htmx has to be vendored there at a pinned version and
`overlord.css` written from scratch.

### Read models: what exists, what each screen still needs

`overlord_store::Reader` (see `crates/overlord-store/src/read.rs`) has 22
methods. These cover M2 as-is:

- Violations board — `violations(states, limit)`, which already carries
  the per-system `new_since` flag
- Rules list — `checks()`, `open_counts_by_check()`,
  `false_positive_rate(check)` (written in M1, never called yet)
- Check editor — `check_revision(id, rev)`, `has_dryrun(id, rev)`
- Users — `top_subjects(limit)`
- Systems — `known_systems()`, `counts()`

These do **not** exist yet and each screen below needs one written:

| screen | missing read model |
| --- | --- |
| Violations board | filters. `violations()` takes only `(states, limit)` — no severity, system, check or subject filter. §5 wants all four. The "new since" split is done: read `ViolationRow.new_since` |
| Person / entity detail | the whole thing: identities and links for one person, one entity's current state, its violation history, its fact timeline |
| Violation detail | `violation_event` rows for one `(check, subject, episode)` — the table is written, never read |
| Check editor | stored dry-run samples. `check_dryrun.samples` holds them as JSON; nothing reads them back |
| Rules list | revision history for one check — `check_revision` is written, and only the *current* revision is ever read |
| Sweeps + coverage | sweep list, and `sweep_system` rows for one sweep. Both written every run, neither read |

`Writer::standing_episodes()` and `Reader::expired_suppressions()` are
evaluation internals, not board queries — do not build screens on them.

### Three traps

**1. "New since last sweep" is already computed — don't recompute it.**
`ViolationRow.new_since` is set by the read model, per system. Read the
flag; do not filter on `opened_sweep == latest_sweep()` in a handler.
That was the original CLI bug (fixed 2026-09-17, see the log): a sweep
restricted to one system emptied the "new" section for every *other*
system, which is precisely the manufactured change §10 forbids.

The semantics, so the board renders them honestly: an entity-scoped
violation is new when it opened in the latest sweep covering **its own
system**; a person-scoped one when it opened in the latest sweep
overall, because person checks are re-evaluated on every run whatever it
covered. A system never swept has no benchmark, so nothing on it is new.
`Reader::latest_sweep_per_system()` is the underlying query if a screen
needs the benchmark itself.

**2. A `SubjectRef` must never go in a URL path segment.** It renders as
`entity/<system>/<type>/<key>` — embedded slashes by construction — and
the key itself may contain more (`EntityRef` parses with `splitn(3)`
precisely because some vendors use path-shaped ids; there is a test).
Put it in a query parameter, or percent-encode it whole. Routing on
`/violations/:subject` will silently mangle real data.

**3. Every command needs an `Actor`, and there is no web one yet.**
`NewCommand::new(actor, kind, at)` — the CLI passes `cli:<name>`. The web
layer must pass the authenticated OIDC subject (§14: every command
records the authenticated actor). Until OIDC lands, PLAN.md task 18's
`--dev-actor` should refuse to bind a non-loopback address. Also give
each rendered form its own idempotency key: `append_command` returns
`Applied { duplicate: true }` and does nothing on a repeat, which is what
makes a double-submitted acknowledge harmless.

### Smaller things worth knowing

- **Sweep progress needs no new machinery.** The `sweep` row is the
  progress record: `status = 'running'` from `open_sweep` until
  `commit_sweep`. A background tokio task plus an htmx poll on that row
  is the whole feature.
- **The check editor's errors are already built.** `overlord_expr::compile`
  returns `Vec<Diagnostic>`, each with a byte `span`, a `message` and an
  optional `help`; `EngineError::BadCondition` carries them through.
  `Diagnostic::render` draws a caret underline for the terminal — the
  editor should use `span` directly and underline in HTML instead.
  `engine::checks::validate` is the no-save validation entry point.
- **Evidence renders itself.** `EvidenceLeaf.expr` is the condition's own
  source text for that leaf and `.value` is a `Value` with a `Display`
  impl, so a row reads `count(groups where external) = 2` with no
  formatting logic in the view.
- **Three flags belong on every violation row** and are already
  populated: `stale` (evaluated from last-known state during a partial
  sweep), `ambiguous` (a selector matched several entities with no
  designated primary — an identity-work signal, not a rule failure), and
  `overlay_stale` (the acknowledgement was made under an earlier check
  revision, which §6.5 wants flagged).
- **`Db::read` / `Db::write` are generic over the error type.** Closures
  that mix raw rusqlite calls need `-> overlord_store::Result<_>`;
  closures calling typed read models infer fine. Handlers returning a web
  error type will need the annotation or a `From<StoreError>` impl.
- **Don't reach for `Reader::conn()`.** It is the read-only escape hatch
  for exports and tests. Screens should get typed read models, so the SQL
  stays in one crate.

## Open questions for the operator

1. **rustfmt needs nightly.** `rustfmt.toml` sets ten options
   (`imports_granularity`, `group_imports`,
   `struct_field_align_threshold`, `wrap_comments`, `format_strings`,
   `fn_single_line`, and four more) that only nightly rustfmt honours.
   The flake pins stable, so `cargo fmt` ignores all ten with a warning
   each. The tree is currently formatted by *stable* rustfmt. Either add
   a nightly rustfmt to the flake or trim `rustfmt.toml` to the stable
   subset.

2. **The absence guard is sharp at small N.** §10 specifies "more than a
   configured share (default 10%)", implemented literally. In a
   four-person system one ordinary departure is 25%, so the guard refuses
   it and asks for confirmation. Safe, but a small tenant would see the
   guard on nearly every real departure. An absolute floor ("…and at
   least N entities") would fix it; that is policy the spec does not
   state, so it is not invented here. Recorded as behaviour in
   `the_percentage_guard_is_sharp_in_a_small_system`. This sharpens
   §17's second open question.

3. **`checks import` is an M1 bootstrap.** §15 makes the UI the only
   place checks are authored and forbids a file-based rule format. There
   is no UI until M2, so the CLI has `checks import`, which reads JSON
   and emits ordinary `check.upsert` commands. The stream stays
   authoritative and there is no rule file to reconcile — the file is an
   input, never a source of truth. It should be reconsidered once the
   editor exists.

4. **Evaluation errors have no home projection yet.** A condition that
   fails against a subject is recorded on the standing episode's
   `eval_error` and returned in the sweep report, but a check with no
   standing violation has nowhere to put it. The Rules screen (M2) wants
   a `check_problem` projection; noted rather than built.

---

## Log

### 2026-09-17 — workspace scaffolding (task 1)

Workspace at the repo root, `crates/*` members, edition 2024,
`resolver = "3"`, shared `[workspace.dependencies]` and
`[workspace.lints]`. `bacon.toml` with `check` / `clippy` / `test` / `fmt`
jobs. Toolchain in the flake is 1.98.1.

`clippy.toml` denies `SystemTime::now`, `Instant::now`,
`jiff::Timestamp::now` and `jiff::Zoned::now` workspace-wide, with a
single annotated exemption at `overlord_core::Timestamp::now`. This is
PLAN.md §5's determinism rule made mechanical: evaluation's only clock is
a sweep's `started_at`, and a stray wall-clock read would break
`replay(streams) == live` in a way that is hard to catch by test.

### 2026-09-17 — `overlord-core` (task 2)

Modules: `ids`, `time`, `severity`, `value`, `normalized`, `check`,
`violation`, `command`, `error`.

**Tagged timestamps in the stored overlay.** A normalized overlay
round-trips through a `TEXT` column, and JSON has no timestamp type. Left
alone, `last_login_at` would serialize as a bare ISO string and come back
as a `string`, so after a rebuild `last_login_at < "2025-01-01"` would be
a lexicographic comparison rather than a timestamp one — quietly breaking
§13's promise that a rebuild reproduces the overlay that was live at the
time. `Value` has hand-written serde impls encoding a timestamp as
`{"$ts": "..."}`. `raw` is exempt: it is converted from
`serde_json::Value` and never deserialized as a `Value`, so a vendor
field named `$ts` is never mistaken for one.

**Fixed-width timestamp rendering.** Found while writing the store: jiff's
default `Display` uses variable fractional precision, and the store both
orders by timestamps (`ORDER BY opened_at`) and compares them
(`suppress_until <= started_at`) *as SQL text*. `…:00Z` and `…:00.5Z`
compare backwards that way, because `.` sorts before `Z` — a suppression
would have expired early or late by up to a second, silently. Timestamps
now render at fixed nanosecond precision, so lexicographic and
chronological order are the same thing and the SQL is correct by
construction. Test: `text_order_matches_chronological_order`.

**Generated ids travel in the command payload.** `person.create` and
`person.split` carry the uid they mint rather than minting it while the
command is applied. A uid generated during replay would differ from the
one generated live, falsifying the replay test on the first merge.

**`SystemSelector` resolves kind-before-id.** §7 lets one field name
either a system instance or a system kind (`has_entity("idp")`), and the
namespaces overlap. A selector spelling a known kind means the kind. The
cost is that a system instance named `idp` cannot be selected alone; the
spec already accepts that trade by letting the field mean both.

Smaller points: `EntityRef` parses `system/entity_type/entity_key` with
`splitn(3)`, because some vendors use path-shaped keys. `Severity` orders
worst-first so a plain `sort()` produces board order. Implicit singleton
persons (§6.4) get a derived `implicit:<entity ref>` uid, so promotion on
first confirmed link is detectable rather than guessed at.

### 2026-09-17 — `overlord-expr` (task 4)

Lexer → Pratt parser → type checker → tree-walking interpreter, plus a
`Diagnostic` carrying byte spans. All ten of §7's example conditions
parse, type-check in their declared scope, and evaluate.

**Decisions the spec left open.** Precedence is `or` < `and` < `not` <
comparison < `??` < primary, so `not a == b` reads as `not (a == b)` and
`a ?? 0 > 3` as `(a ?? 0) > 3`. Comparisons do not chain. `matches` takes
a literal pattern only, so it compiles when the check is saved and is
reused across subjects — which is what makes both halves of "a check
cannot hang a sweep" true. `any`/`all` require a `where`; `count` does
not. A person-scoped check has no fields of its own: a bare `status` is a
type error whose help gives the rewrite. A condition must be boolean, so
`count(groups)` alone is rejected at save time.

**Runtime mismatches are errors, not `false`.** §7 asks for this
explicitly and it needed a channel: `Evaluation.outcome` is a `Result`,
and the engine records a failed evaluation as a rule-quality signal
rather than a violation. Ambiguous `entity(...)` selectors come back in a
separate `ambiguous` list, since §6.4 wants the *subject* flagged.

**Evidence records evaluated leaves, not every value.** Short-circuited
branches are omitted; a collection reports its aggregate rather than one
line per element — an operator wants `count(groups where external) = 2`,
not the group list.

**A bug the property tests found.** The string lexer advanced by bytes,
so a backslash before a multi-byte character (`"\Ѩ"`) left the cursor
mid-character and the next slice panicked — a server panic on a typo in
the check editor. Fixed by scanning strings character-wise. The property
that found it also asserts every diagnostic span lands on a character
boundary.

### 2026-09-17 — `overlord-store` (task 3)

Schema in `sql/0001_initial.sql` (`STRICT` throughout), `Db` with a
serialized writer and a reader pool, stream appends, command projection,
read models, violation primitives, scoring, replay.

**The global sequence works.** Facts, commands and sweep boundaries all
draw from `stream_seq`, and a test asserts the recorded interleaving is
`sweep, fact, command, fact` when that is the order things happened. The
replay merge never consults a clock.

**Correction to PLAN.md.** The plan listed `sweep_system` among the
rebuildable projections. It is not one: it records what a connector
actually reported, including whether the absence guard tripped — a
decision taken with the snapshot in hand, not derivable from the streams.
Clearing it would lose data rather than rebuild it. PLAN.md §3.3 now says
so, and `rebuild` leaves `sweep` and `sweep_system` alone.

**Replay takes a hook, not a dependency.** Violations come from
evaluation, which lives in the engine, and the engine depends on the
store rather than the reverse. `rebuild_with` calls back at each sweep's
recorded commit position — exactly where the live path evaluated.

**Dry-run enforcement lives in the store**, in `project_command`, so the
CLI and the UI cannot each forget it. A dry-run is keyed to an exact
`(check, revision)`, so a rewritten rule cannot inherit the old one's
evidence.

**A tombstone clears the cached overlay.** An absent entity is excluded
from evaluation (§6.1), so `entity.normalized` goes to NULL rather than
keeping a stale overlay that reads as current; the last known state stays
in the fact stream where it is unambiguously historical.

**Scoring is one SQL statement.** Entity-scoped violations attribute to
the linked person or to `implicit:<entity ref>` when unlinked, and retired
uids resolve through `person_alias` — so §8's "linking never changes a
total" falls out of the query shape rather than being maintained by hand.

**One ergonomic cost.** `Db::read` / `Db::write` became generic over the
error type so the engine can use its own inside a transaction. Closures
that mix raw rusqlite calls now need an explicit
`-> overlord_store::Result<_>`; typed read-model calls infer fine. Worth
it, but it is friction, confined to tests and exports.

### 2026-09-17 — `overlord-connect` and the fixture connector (tasks 5, 6)

**Read-only is structural, not policy.** §11 wants it "by construction".
Two mechanisms: `ReadMethod` has no mutating variant, so a connector
cannot *name* a `PUT`, `PATCH` or `DELETE` — there is no value to pass;
and every request is matched against the connector's allowlist of
method-and-path pairs before it is sent, failing closed with no network
call. A connector receives a `RestrictedHttp` and nothing else: no store
handle, no raw client. The test that proves it uses an *offline* client
that errors on any dial, so an `Offline` error would mean the allowlist
let the request through — getting `NotAllowed` proves the check ran
first.

**Normalization is data, not code.** §11 requires every ruleset change,
shipped or operator-made, to be a `normalization.upsert` revision in the
command stream. An operator cannot edit a compiled function, so a
`Ruleset` is JSON: field paths, coercions, and a status map. Coercion
failures produce null *and a warning* rather than vanishing — a timestamp
that silently became null would make a dormancy check quietly wrong.

**The fixture connector stays.** It is the test bed for the cases a real
tenant will not produce on demand: a snapshot that paged out halfway, a
connector that failed mid-sweep, a directory that appears to have lost
90% of its accounts. Five scenarios in `fixtures/`, documented in
`fixtures/README.md`. Its allowlist is empty, which is itself meaningful:
it can reach nothing.

**A bug found by running it.** `Connector::default_ruleset` took no
context, so the fixture connector normalized *every* system as
`workspace` — and `orphan-workspace-account` fired on IdP-only accounts,
claiming an Okta user had an orphaned workspace account. The trait method
now takes the `ObserveCtx`, which is right in general (a ruleset is per
system, not per connector), and the recorded `system_kind` comes from the
ruleset rather than the connector's declaration.

### 2026-09-17 — sweep and evaluation (tasks 7, 8, 9)

**The sweep's clock is an input.** `SweepPlan.started_at` is set by the
caller — `Timestamp::now()` in the CLI, a fixed instant in tests. It is
the one clock read the run is allowed, so making it a parameter is what
lets a test pin it and a replay reproduce it.

**The absence guard.** Tombstones are written only from snapshots the
connector declared complete; a partial snapshot never produces one
whatever it omits. A complete snapshot that would tombstone more than the
configured share records the anomaly, writes nothing, and flags the
system. See open question 2 above.

**Per-system transactionality.** A connector that fails records a failed
system and leaves every other system's facts intact — tested with a
two-system sweep where one connector refuses to connect.

**Not re-evaluated is not resolved.** §10 says entity-scoped checks for
unswept systems are not re-evaluated; the reconciliation pass therefore
leaves their episodes alone rather than closing them, because
re-confirming *or* resolving against facts the sweep did not read would
be a claim it has not earned. Person-scoped checks reaching into an
unswept system are answered from last-known state and marked `stale`.

**The lifecycle.** Suppression expiry runs first, against the sweep's own
start time, so an expired suppression is reopened before the condition is
retested and resolved in the same pass if it no longer holds. A
regression opens a *new episode* at `open`, never inheriting the previous
episode's acknowledgement; the earlier episode and its acknowledgement
stay in `violation_event`, which a test asserts reads
`opened, acknowledged, cleared, regressed`.

**`replay(streams) == live` holds.** The test builds a history with two
systems, operator commands landing between sweeps, a partial sweep, a
regression and a link; then drops every projection, replays, and compares
`entity`, `person`, `person_alias`, `link`, `check_head`,
`check_revision`, `violation` and `person_score` row for row and column
for column. Not counts — the actual bytes.

### 2026-09-17 — CLI (task 10)

`sweep`, `checks list|export|import|dry-run|enable|disable`,
`violations`, `users`, `acknowledge`, `suppress`, `rebuild`, `status`.
Each invocation supplies its own idempotency key, so a retried script
does not act twice (§14). A condition that will not compile is printed
against its own source with the offending span underlined — the same
`Diagnostic` the M2 editor will use.

Verified by hand against `examples/fixture.toml`: enabling a check
without a dry-run is refused; a sweep opens eleven violations across two
systems with evidence attached; `rebuild` reproduces them; acknowledge and
suppress move the right rows.

### 2026-09-17 — "new since last sweep", per system (M1 defect fix)

The CLI filtered `opened_sweep == latest_sweep()`: one global
comparison. §8 says the section "compares against the previous sweep that
covered the same system" and §10 says the per-system comparison is what
stops a restricted sweep manufacturing change. The global version did
exactly what the spec forbids — a sweep of one system emptied the "new"
section for every other system, claiming nothing anywhere was new when
the unswept systems' findings were untouched and unexamined.

Fixed in the read model rather than the renderer, so the M2 board cannot
inherit the bug: `ViolationRow` gained a `new_since` flag, computed in
`Reader::violations` from a new `Reader::latest_sweep_per_system()`.

The rule needed a decision the spec does not spell out, because a
person spans systems and so has no single one to ask. Entity-scoped
violations compare against the latest sweep covering their own system;
person-scoped ones against the latest sweep overall, which is right
because `evaluate_sweep` re-evaluates every person on every run whatever
it covered (marking them `stale` when a referenced system was not
visited). A system never swept has no benchmark, so nothing on it is new.

Three tests, the first of which is the spec's own requirement stated
directly: sweep two systems, then sweep one of them, and assert the
other's section is unchanged. It also asserts that the old global
comparison would have found nothing, so the test would fail against the
previous implementation rather than passing vacuously.

`cargo test --workspace`: 154 passing, 0 failing.
