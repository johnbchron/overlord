# overlord: Progress

Working log for the build described in [PLAN.md](PLAN.md). Updated as work
lands, not in advance. Spec references like (§7) point at
[SPEC.md](SPEC.md); plan references at PLAN.md.

**Current milestone:** M4 — Breadth. Google Workspace (task 24) complete;
task 26's access/SSO half (UniFi Access) and its MDM half (Grandstream
UCM, `mode = "devices"`) have both landed.

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

| # | M2 task | State |
| --- | --- | --- |
| 12 | `overlord-web` skeleton, css, vendored htmx, hashed assets | done |
| 13 | Violations board with filters and "new since last sweep" | done |
| 14 | Violation actions, stale acknowledgements flagged | done |
| 15 | Rules screen and check editor | done |
| 16 | Users, person/entity detail, sweeps + coverage, systems, settings | done |
| 17 | Sweep from the UI | done |
| 18 | OIDC, `--dev-actor` confined to loopback | done |

| # | M3 task | State |
| --- | --- | --- |
| 19 | Suggestion computation, explainable, stored read-only | done |
| 20 | Confirm / manual link / unlink / primary-per-system-kind | done |
| 21 | Merge with permanent alias, split with recorded provenance | done |
| 22 | Promotion of implicit persons, carrying violation history | done |
| 23 | `suppress_if_pending_links` honoured during evaluation | done |

| # | M4 task | State |
| --- | --- | --- |
| 24 | `overlord-connector-gworkspace` | done |
| 25 | IdP connector (Okta, then Entra ID) | not started |
| 26 | Access/SSO assignments, then MDM | part done — UniFi Access and Grandstream UCM landed |
| 27 | Per-connector normalization rulesets, authored as commands | not started |

`cargo test --workspace`: **358 passing, 0 failing.**
`cargo clippy --workspace --all-targets -- -D warnings`: **clean.**
`cargo fmt --all -- --check`: **clean.**

M4 is part done and M5 (lifecycle polish) is untouched. The tree has now
met one vendor's idea of an identifier, which is what M4 was ordered for.

### Try it

No credentials, no network:

```sh
cargo run -- -c examples/fixture.toml checks import examples/starter-checks.json
cargo run -- -c examples/fixture.toml checks dry-run mfa-missing
cargo run -- -c examples/fixture.toml checks enable mfa-missing
cargo run -- -c examples/fixture.toml sweep
cargo run -- -c examples/fixture.toml suggestions
cargo run -- -c examples/fixture.toml serve --dev-actor you
```

Then http://127.0.0.1:8080, and the **Identity** tab. `--dev-actor`
authenticates nobody and refuses to bind anything but a loopback address.

For the identity signals in isolation, point a `[[systems]]` pair at
`fixtures/identity-ws.json` and `fixtures/identity-idp.json`: one person
found per signal, and one deliberately found by none. `fixtures/README.md`
has the cast.

Against a real tenant, `examples/gworkspace.toml` is the worked example —
it documents the scopes to grant before the first sweep, and where the
credential comes from. With none set, `sweep` records a failed system
saying exactly which environment variable is missing, which is the right
first thing to see.

---

## M4 handoff

Written at the end of M3, amended when the Google Workspace connector
landed. PLAN.md §4 lists the M4 tasks: the Google Workspace connector
first (so the overlay vocabulary is shaped by workspace semantics), then
an IdP, then access/SSO and MDM, with per-connector normalization
rulesets authored as commands. Task 24 is done, and "What task 24 leaves
the IdP connector" below is the part written after it. This is what the
code actually leaves you.

### What M3 added that M4 has to feed

Identity now runs off the **normalized overlay**, which means a real
connector's normalization ruleset is no longer only about checks — it
decides whether overlord can match a person across systems at all. The
signals `overlord-engine/src/identity.rs` looks for, strongest first:

| signal | overlay fields, in order | fallback |
| --- | --- | --- |
| `exact-email` | `email`, `primary_email` | the entity key, if email-shaped |
| `directory-id` | `employee_id`, `external_id`, `directory_id` | none |
| `username` | `username`, `user_name`, `login` | the local part of an email-shaped key |

**A connector that maps none of these can only be matched by its key.**
So when the Workspace ruleset lands, map `primaryEmail` to `email` and
`externalIds` to `employee_id` even though no check needs them: identity
does. The same goes for Okta (`profile.login`, `profile.employeeNumber`)
and Entra (`userPrincipalName`, `employeeId`, `onPremisesImmutableId`).
Adding a field to a ruleset is a `normalization.upsert` revision, so this
is correctable later — but a tenant whose first sweep proposes nothing
will conclude the feature does not work.

### The identity surface, in one paragraph each

- **`overlord-engine/src/identity.rs`** is the whole of it: the
  suggestion computation, and the operator verbs (`confirm`, `link`,
  `link_to_new_person`, `unlink`, `set_primary`, `merge`, `split`). The
  two halves never touch — a suggestion emits no command.
- **`overlord-store/src/identity.rs`** owns the `suggestion` projection:
  `replace_suggestions` (wholesale, each sweep) and
  `pending_suggestions` (the queue, mirrors folded).
- **`overlord-web/src/pages/identity.rs`** is the queue screen and the
  person picker; `pages/subject.rs` carries the same verbs where they
  belong, on one account or one person. `actions.rs` has the handlers.
- **CLI:** `suggestions`, `link`, `unlink`, `merge`, which is what §5
  asks for.

### Read models worth knowing

`overlord_store::Reader`, across `read.rs` and `detail.rs`:

- `resolve_person(uid)` — **the one that matters.** Follows a uid to the
  person it means today, through a merge alias *and* through a link that
  promoted an implicit uid. Anything that looks up a person goes through
  it.
- `retired_uids(uid)` / `person_subject_refs(uid)` — the uids a person
  has absorbed, and the subject refs their violations may be filed under
- `person_detail(uid)`, `entity_detail(entity)`, `suggestions_for(entity)`
- `search_subjects(query, limit)` — persons and entities by name or key
- `violation_episodes(check, subject)` — every episode with its events
- `sweeps`, `sweep`, `coverage`, `systems`, `check_revisions`, `dryrun`

`ViolationFilter` (in `read.rs`) is how the board narrows: states,
severities, systems, checks, a set of exact subjects, and a case-folded
subject substring, all applied in SQL so `limit` keeps meaning "the worst
N that match".

### Traps, carried forward and new

**The three M1 traps still hold.** `ViolationRow.new_since` is computed
by the read model — read the flag, never recompute it. A `SubjectRef`
never goes in a URL path segment; `view::subject_href` puts it in a
query parameter and `view::urlencode` encodes it. Every command needs an
`Actor` and an idempotency key.

**`Db` must stay `Send + Sync`.** The web server shares one handle
across every request task. M2 found the in-memory keeper connection was a
bare `Connection` field, which is `Send` but not `Sync`, and moved it
behind a `Mutex`. `the_handle_can_be_shared_across_threads` in `db.rs`
asserts it so the next bare connection field fails at test time rather
than in a handler signature.

**A store refusal usually arrives wrapped in an `EngineError`.**
`WebError::status` originally matched only `WebError::Store(Rejected)`,
so `checks::enable` refusing a revision with no dry-run — a §7 contract —
rendered as a blank 500 instead of a 409 with the reason. It now unwraps
`Engine(Store(_))` as well. Anything new that goes through the engine
inherits this; anything that bypasses it will need its own arm.

**Person-scoped violations survive the system facet.** A person
spans systems, so filtering the board to one system keeps them rather
than excluding them — excluding them would hide exactly the
cross-system findings the filter is being used to investigate. Asserted
by `a_person_scoped_violation_survives_the_system_facet`.

**The dry-run gate is enforced twice, deliberately.** The editor
hides the Enable button without a dry-run for the revision on screen, and
the store refuses the command regardless. Do not "simplify" this to one:
a gate that exists only in a template is not a gate, and
`enabling_is_refused_without_a_dry_run_for_that_revision` posts straight
past the template to prove it.

**New: a violation's `subject_ref` is not always its subject's ref.** An
episode keeps the ref it opened under, forever. Link the account behind
`implicit:ws/user/ada@…` to a person and that episode is *that person's*
now, still filed under the implicit uid — §12 resolves history through a
retired uid rather than rewriting it. Two consequences, and both have
bitten already:

- anything asking "what is wrong with this person" must use
  `person_subject_refs(uid)`, not `SubjectRef::Person(uid)`, or it
  silently under-reports;
- `resolve_person` is the only correct way from a uid to a person, and
  `recompute_scores` re-implements exactly its two steps in SQL. If one
  changes, the other has to.

**New: `SubjectRef` serializes as its canonical string, and must.** It
was a derived internally tagged enum, which cannot wrap a transparent
newtype — so `CommandKind` carrying a person subject failed at
serialization, and acknowledging an orphan-account violation was
impossible. Nothing caught it because every test acknowledged an entity.
The `Deserialize` impl still accepts the old tagged map so a store
written before the fix replays;
`a_stored_command_in_the_old_tagged_shape_still_reads` holds that open.

**New: only an observed account can be linked.** `require_entity` refuses
a link to an entity with no row in `entity`. Without it a typo in a
manual link creates a person holding an account no sweep will ever
produce — invisible on every screen and impossible to unlink from the UI.

**New: scores are recomputed by the command, not by the next sweep.**
`project_command` calls `recompute_scores` for every person and violation
verb. Suppressing a finding stops it counting and linking an account
moves its weight; making the Users screen wait for a sweep to agree would
make it wrong for however long that is. It lives in `project_command`
rather than `append_command` because replay calls that one, and the two
paths must produce identical projections.

### Smaller things worth knowing

- **Sessions.** A signed cookie, no server-side table:
  `blake3::keyed_hash` over the claims. The key is
  `OVERLORD_SESSION_KEY` (64 hex characters) or an ephemeral one derived
  at startup with a warning. OIDC discovery is lazy and cached, so
  overlord boots with the provider down.
- **`--dev-actor` refuses a non-loopback bind**, and OIDC with neither
  `allowed_subjects` nor `required_group` refuses to start at all —
  authentication alone is not sufficient (§14). Both are checked in
  `overlord_web::serve`, before the listener is bound.
- **Assets are hashed at startup** from their own bytes
  (`assets.rs`), served immutable, and 404 on a stale path. There is no
  build step and nothing to invalidate.
- **The sweep row is the progress record, and each system's coverage row
  is written as that system finishes.** `sweeprun::SweepRunner` holds only
  what the store cannot answer: which system is in flight, how many are
  done, whether evaluation has begun, the tail of what that system is
  reading (folded from the engine's `SweepProgress` sink), and why the
  last run died if it died before recording anything. The page polls
  `/sweeps/progress`, which shows a bar, the system being collected, a
  live feed of the connector's own reports, and the coverage table as it
  fills in, then stops polling by returning markup with no trigger and an
  `HX-Refresh`.
- **A connector narrates through `overlord_connect::Progress`**, carried
  on `ObserveCtx` and forwarded by the engine as `SweepProgress::Detail`.
  It is inert without a sink, so a connector reports unconditionally.
  `RestrictedHttp` reports every request on its own, which means even a
  connector that says nothing shows the pages and calls it is making —
  the difference between a hung sweep and a slow one.
- **Two sweeps at once are refused, not queued.** They would interleave
  facts under two different definitions of "now" (§10).
- **`view.rs` is the shared vocabulary** — severity, state, timestamps,
  subject links, evidence, flags, diagnostics. A new screen should reach
  for it rather than restyling a badge.


### The stylesheet, and how to add to it

`assets/overlord.css` is one hand-rolled file, no framework and no build
step, compiled into the binary by `include_str!` and served at a
content-hashed path. Editing it is enough — the hash is recomputed from
the bytes at startup, so there is nothing to invalidate and no way to
serve a stale copy. `cargo` does track the `include_str!` dependency, but
if the served hash ever looks wrong the cause is almost always a stale
server process still holding the port, not a stale build.

**The organising idea is that severity is structural, not decorative.**
SPEC.md §2 asks for loud problems to dominate and quiet ones to be merely
recorded, so:

- every violation row carries a 3px colour **spine** in its own severity
  (`tbody tr:has(.sev-critical) td:first-child`), so a queue scans
  vertically as a heat trace before a word of it is read;
- **badge weight falls off a cliff** below `high` — `critical` is the
  only solid fill on the screen, `high`/`medium` are tinted, `low`/`info`
  are outline-only and recede;
- critical rows carry a low-alpha warm tint, so the worst of a long queue
  is findable by glance down the page.

Keep that. A new screen that shows violations should get the spine for
free by rendering `view::severity`, and anything that reimplements a
severity badge by hand breaks the one place the rule lives.

**The class vocabulary is closed and small.** `panel` / `panel-body`,
`row` / `field` / `hint`, `tag` (+ `tag-ok`, `tag-warn`), `sev-*`,
`banner-*`, `stat` / `n` / `k`, `evidence`, `ref`, `muted` / `soft`,
`shrink` / `num` / `nowrap` / `right`, `stack`, `empty`, `lede`. Reach for
these; `view.rs` already wraps most of them. `k` is the label style,
shared by `.field > span` and the detail pages' key/value pairs.

**Two traps in the CSS itself:**

- `.panel` uses `overflow: clip`, **not** `hidden`. `hidden` would make
  it a scroll container, and the sticky `th` headings would then stick to
  a box that never scrolls — which is to say, not at all. `clip` rounds
  the same corners without creating one. This was a real bug caught
  during the pass; do not "tidy" it back.
- Sticky headings are offset by `--masthead-h`, which the masthead also
  uses as its literal `height`. If the masthead's height ever becomes
  content-driven again, the two silently disagree and the headings sit
  under it.

**Browser baseline.** `:has()` and `color-mix()` are used deliberately
(Baseline 2023). Both degrade safely — no spine, no tint, never a *wrong*
colour — and the badge always states the severity in text. No webfonts,
ever: an air-gapped deployment should not need a network fetch to render
a stylesheet, which also keeps the asset story to two files.

**Unreviewed.** The visual direction landed after M2 was committed and
has not had operator feedback yet. Three things were flagged as most
likely to change: the teal accent, the critical-row tint (possibly too
much at real volume), and the spine width. Treat them as provisional.

### What task 24 leaves the IdP connector

Written when Google Workspace landed. The connector crate is the pattern:
`crates/overlord-connector-gworkspace/` is `auth.rs` (credentials and the
token grant), `directory.rs` (paging), `lib.rs` (the trait impl and the
envelope), `ruleset.json` (the shipped normalization), and two test files.
An IdP connector should be the same five things.

**Four things in `overlord-connect` grew to fit a real vendor**, and all
four are the kind an IdP will want too:

- **The allowlist covers origins.** `Allow::at(base)` points an entry at
  a second host; a client is built for exactly one origin and carries
  only that origin's entries, so declaring a token endpoint does not
  widen what the vendor's own API accepts. `Connector::http_for(declared,
  via)` builds one, where `via` sends the requests through an egress
  proxy or a test double without changing which of them are permitted.
  Okta's token endpoint is on the tenant's own host, so it may not need
  this; Entra's is on `login.microsoftonline.com`, so it will.
- **`RestrictedHttp` carries a bearer token**, set with `set_bearer`
  during `observe` — `observe` is handed `&RestrictedHttp`, and the token
  is not known until the run starts. It is behind a lock, never logged,
  and absent from the `Debug` rendering.
- **`post_form`**, for token grants. A grant is a POST that reads, and
  `ReadMethod` still cannot name a mutating method.
- **Three additions to the ruleset language**, each forced by Google and
  each general: `StatusRule` is now a list of clauses tried in order (a
  vendor's lifecycle state is not always one field — Google spells it
  across `archived` and `suspended`, and reading only `suspended` calls a
  deprovisioned account active); `FieldRule.null_if` names the sentinels
  a vendor uses for absence (Google's `lastLoginTime` of
  `1970-01-01T00:00:00.000Z`, Entra's `0001-01-01T00:00:00Z`); and
  `Coerce::EmailLocal` takes the part before the first `@`, because
  identity's `username` signal needs a username and a directory ships an
  address. The one-clause `StatusRule` shape still deserializes, so a
  stored `normalization.upsert` body written before this replays —
  `the_one_clause_shape_a_stored_ruleset_was_written_in_still_reads`
  holds that open.

**The key is the vendor's immutable id, not the address.** The Workspace
ruleset keys on `user.id`. An address is the readable choice and the
wrong one: a rename would tombstone the account and open a new one,
losing every episode filed against it. Okta's `id` and Entra's `id` are
the equivalents. The cost is that the identity fallbacks — "the entity
key, if email-shaped" — no longer fire, which is exactly why `email`,
`username` and `employee_id` are all mapped explicitly.

**An observation is an envelope, not a response body.** A user's facts
come from three APIs, so the raw payload is
`{ user, external_ids, organization, groups, licenses }` with the vendor
objects verbatim inside it. `raw.user.<anything>` still reaches what
Google sent. `external_ids` and `organization` are indexed views of two
repeated Directory fields, because `Value::get_path` refuses to index a
list by number (M1's decision: vendor array order is not stable, so
`externalIds.0.value` would be a check that quietly changes its mind).
Keying by the vendor's own `type`, and picking the entry it marked
`primary`, says what was meant. Do the same rather than reopening that
decision.

**Absent is not empty.** `groups` and `licenses` are left out of the
envelope entirely when they were not collected, and are an empty list
when they were collected and there were none. A group read that failed
must not report "in no external group" for every account in the tenant.
Which is also why:

**A degraded overlay makes the snapshot partial, not just a warning.**
If the group or domain read fails, the whole snapshot is `Partial` even
though every account was enumerated. §10's partial handling — no
tombstones, violations marked stale — is the right answer for a half-read
*overlay* as well as a half-read enumeration, because the alternative is
resolving every sharing violation at once and being confidently wrong.
An enumeration whose *first* page failed is different: that is
`ConnectorError::Incomplete`, a failed system, and
`directory::require_something` is the one line that tells them apart.

**The Systems screen now shows the allowlist**, per configured system,
with the reason each entry was asked for. `describe_allowlist` had been
sitting unused since M1 for want of a connector with anything in it. An
operator about to grant a vendor scope can now see that the grant is
wider than the use without reading Rust.

**Tests cannot set an environment variable.** `std::env::set_var` is
unsafe in this edition and `unsafe_code` is forbidden workspace-wide, so
`GoogleWorkspaceConnector::with_credential` is the seam the wiremock
tests use. It is reachable only from Rust — a `[[systems]]` entry names a
connector and the binary's registry calls `new()` — so the environment
stays the only way a credential reaches a real deployment. An IdP
connector will need the same seam, and should document it the same way.

**`crates/overlord-connect/tests/read_only.rs`** is PLAN.md §5's second
promise, finally testable: a connector crate must not depend on an HTTP
client, because a crate that can build its own client can reach anything.
It reads the manifests, and it enumerates `crates/overlord-connector-*`
by directory, so a new connector is covered the moment it exists.

## Open questions for the operator

1. ~~**rustfmt needs nightly.**~~ Resolved: the flake now pulls nightly,
   and the tree is formatted by it. `cargo fmt --all -- --check` is clean.

2. **The absence guard is sharp at small N.** §10 specifies "more than a
   configured share (default 10%)", implemented literally. In a
   four-person system one ordinary departure is 25%, so the guard refuses
   it and asks for confirmation. Safe, but a small tenant would see the
   guard on nearly every real departure. An absolute floor ("…and at
   least N entities") would fix it; that is policy the spec does not
   state, so it is not invented here. Recorded as behaviour in
   `the_percentage_guard_is_sharp_in_a_small_system`. This sharpens
   §17's second open question.

   M2 does not change the rule, but it does make the consequence
   visible: the coverage view and the Systems screen both show a tripped
   guard in place, so an operator meets it as a prompt to confirm a
   snapshot rather than as a silent non-event.

3. **`checks import` outlived its bootstrap.** §15 makes the UI the only
   place checks are authored and forbids a file-based rule format. The
   editor now exists, so the CLI's `checks import` is no longer needed to
   get a rule into the system and should probably go. It is still the
   fastest way to seed a demo, and `examples/fixture.toml` documents it
   as such — so it is left in place for M3 rather than removed on the
   same day the editor landed. The decision is yours. (Still open after
   M3; it is now the only CLI verb that authors anything.)

4. **Evaluation errors still have no home projection.** A condition that
   fails against a subject is recorded on the standing episode's
   `eval_error` and returned in the sweep report, but a check with no
   standing violation has nowhere to put it. M2's Rules screen shows
   open counts, zero-match flags and false-positive rates, and the
   violation detail page surfaces `eval_error` where an episode carries
   one — but a rule that errors against *every* subject still shows as a
   quiet zero-match rule rather than as a broken one. A `check_problem`
   projection would close this; noted again rather than built, because it
   is a schema addition and M2 was not the milestone for it.

5. **Suppression expiry is entered as UTC.** The suppress form uses an
   HTML `datetime-local` input, which sends wall-clock with no zone.
   overlord records UTC only (§13), so the value is read as UTC and the
   field says so, rather than being quietly reinterpreted in the
   server's local zone — which would expire a suppression at an hour
   nobody chose. If operators find that surprising, the fix is a zone
   picker, not a silent conversion.

6. **No check ships with `suppress_if_pending_links` set.** It works —
   `a_check_can_stay_quiet_about_an_account_with_unreviewed_links` proves
   it — but `examples/starter-checks.json` leaves it off everywhere,
   including on `orphan-workspace-account`, which is the rule it was
   designed for: before linking, every workspace account looks like an
   orphan. Turning it on would make the first sweep of a new tenant far
   quieter, at the cost of hiding genuine orphans until somebody works
   the identity queue. That trade is a policy call, and §12 describes the
   flag as something a check *may* set, so the starter library does not
   decide it for you. Set it in the editor if the first board is noise.

7. **Suggestions cross systems only.** Two accounts in the *same* system
   are never proposed to each other, even when they share an employee id
   — an admin account alongside an ordinary one is the obvious real case.
   §12 frames identity as cross-system unification and §6.4 allows a
   person to hold several accounts of the same type, so the capability is
   there and only the proposal is missing; it is a one-line change to the
   grouping in `recompute_suggestions`. Left out because within one
   system a shared id is at least as likely to be a service account
   convention as a person, and a wrong suggestion costs more than a
   missing one. Worth revisiting against a real tenant.

8. **Workspace sharing settings are not collected, and cannot be from
   the Directory API.** §11 lists sharing settings among a workspace
   connector's subjects. Domain-level Drive sharing lived in the Admin
   Settings API, which Google retired; per-account sharing behaviour is a
   Drive API question, and the narrowest Drive scope that answers it
   still reads across every user's content. That is a much larger grant
   than the four the connector asks for, and it is not overlord's to make
   on an operator's behalf — so it is a separate connector you can
   decline, not a field quietly added here. Nothing approximates it: a
   check written against a guessed `external_sharing` would be
   confidently wrong, which is worse than absent. The starter library's
   `external-group-sharing` rule is answered instead by real group
   membership, which is collected.

9. **No retries, and Google rate-limits.** A 429 or 503 mid-enumeration
   truncates the read and marks the snapshot partial — correct, and safe,
   but a large tenant may see it routinely rather than exceptionally,
   because group membership is one call per group. Bounded backoff
   belongs in `RestrictedHttp` where every connector inherits it, not in
   this one; it is not built because the right ceiling is a question a
   real tenant answers and inventing one would be guessing. If the first
   sweeps come back partial, this is why, and `groups = false` is the
   configuration that makes them complete at the cost of the sharing
   overlay.

10. **A sweep fetches one token and does not renew it.** Google's access
    tokens last an hour. A sweep of a tenant with several thousand groups
    could outlive that, and would then fail partway with a 401 —
    reported as a truncated read, so nothing is corrupted, but the sweep
    is wasted. `set_bearer` takes `&self` precisely so a renewal can be
    added inside `observe` without changing any signature. Left until a
    tenant is slow enough to need it.

11. **`apps.licensing` is not read-only, and Google offers no narrower
    scope.** It is requested only when `licenses` is configured, which is
    empty by default — so the decision is yours and it is made by adding
    a SKU, not by installing overlord. §11 asks for exactly this to be
    declared; it is, in the crate docs, in `examples/gworkspace.toml`,
    and now on the Systems screen.

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

### 2026-09-17 — M2: the operator UI (tasks 12–18)

`crates/overlord-web`: axum routing, maud pages, one hand-rolled
stylesheet, vendored htmx 2.0.4. Nine page modules, `actions.rs` for
everything that writes, `view.rs` for the vocabulary every screen shares.
The binary gained `serve`.

**Fragments are the pages, narrowed.** A filtered board and a
re-rendered single row call the same functions the full page does, so
they cannot drift. `/violations/rows` returns the board without the
chrome; `/violations/act` with an `HX-Request` header returns exactly one
`<tr>`. No JSON crosses the wire and there is no client-side model.

**Filters went into SQL, not into the handler.** `ViolationFilter` carries
states, severities, systems, checks, an exact subject and a case-folded
subject substring, all bound as parameters. Filtering the result instead
would have made `limit` a lie: it has to mean "the worst N that match",
and trimming afterwards drops matches behind the cut. The system facet
deliberately keeps person-scoped violations — a person spans systems, so
narrowing to one cannot sensibly exclude them, and doing so would hide
exactly the cross-system findings the filter is used to investigate.

**Subject refs travel as query parameters.** `entity/<system>/<type>/<key>`
has embedded slashes by construction and vendor keys add more, so
`/entity?ref=…` and `/person?uid=…` with whole-value percent-encoding,
per the M1 handoff's second trap. Row DOM ids are a blake3 prefix of
`(check, subject, episode)` rather than the ref itself, because neither
a check id nor a subject ref is safe as an HTML id.

**The dry-run gate is enforced twice on purpose.** The editor disables
Enable without a dry-run for the revision on screen; the store refuses
the command regardless. `enabling_is_refused_without_a_dry_run_for_that_revision`
posts straight past the template — and caught a real bug doing it:
`WebError::status` matched only `Store(Rejected)`, so a refusal arriving
wrapped in `EngineError::Store` rendered as a blank 500 instead of a 409
with the operator-facing reason. That path covers every engine-mediated
refusal, not just this one.

**`Db` had to become `Sync`.** The server shares one handle across every
request task. The in-memory keeper connection was a bare `Connection` —
`Send` but not `Sync` — so the whole handle was unshareable. It is behind
a `Mutex` now, and `the_handle_can_be_shared_across_threads` asserts the
property so the next bare connection field fails at test time rather than
as an inscrutable `Handler` trait error.

**Check editor diagnostics use spans, as PLAN.md §5 promised.**
`/rules/validate` compiles on blur and returns every diagnostic at once,
each rendered as the source line with a caret run under exactly the
reported bytes and the help text beneath. It takes only the condition and
the scope, not the whole form: a half-written new check must still get
its condition underlined, and demanding an id first would make the editor
useless exactly when it is most wanted.

**Sweeps needed no new machinery**, as the M1 handoff said. A background
tokio task, the `sweep` row as the progress record, and an htmx poll on
`/sweeps/progress` that stops by returning markup with no trigger plus an
`HX-Refresh`. Two concurrent sweeps are refused rather than queued: they
would interleave facts under two different definitions of "now" (§10).

**Auth is an extractor, not a middleware call.** A handler that takes
`Identity` cannot run without one, so there is no "check the session"
step to forget — `without_oidc_no_command_can_reach_the_store` walks
every read and write route and asserts nothing is appended. Sessions are
a signed cookie (`blake3::keyed_hash`) with no server-side table; OIDC
discovery is lazy and cached so overlord boots with the provider down.
`--dev-actor` refuses a non-loopback bind, and OIDC with neither
`allowed_subjects` nor `required_group` refuses to start at all —
authentication alone is not sufficient (§14).

The `groups` claim is read from the id token's payload after the library
has verified it. The signature covers that payload, so re-reading it for
a provider-specific claim adds no trust; the alternative was threading a
custom `AdditionalClaims` type through six generic parameters of
`openidconnect`'s client.

Three M1 read models that had no caller now have one:
`false_positive_rate` on the Rules screen, `has_dryrun` behind the enable
gate, and `latest_sweep_per_system` behind the board's "new since"
split.

`cargo test --workspace`: 193 passing, 0 failing.

### 2026-09-17 — visual pass on the stylesheet

Landed after M2 was committed, on the brief "pick a direction". Not a
milestone task; recorded because it changes a contract the next screen
will inherit.

**Severity became structural rather than decorative.** The old sheet
expressed priority only as a coloured chip, so every row was otherwise
identical and the board read as a uniform list that had to be parsed.
Now: a colour spine down each row, badge weight that falls off a cliff
below `high` (critical is the only solid fill anywhere), and a low-alpha
warm tint on critical rows. §2's "loud problems dominate, quiet ones are
merely recorded" is now legible in the pixels rather than only in the
ordering. The accent moved from a generic blue to a deep teal, and the
brand mark became a filled square — the same shape as the spine, so the
mark and the board share one vocabulary.

Supporting work: a real type scale, `tabular-nums` on `body` so every
count and column lines up without each call site asking, sticky table
headings, `:focus-visible` rings, and a disclosure triangle on
`<details>`.

**`.k` was styled only inside `.stat`.** The detail pages use
`div class="k muted"` sixteen times as a key/value label and were getting
nothing but the muted colour. It is now a first-class label style shared
with `.field > span`.

**A bug introduced and caught inside the same pass.** Adding sticky `th`
headings while `.panel` still had `overflow: hidden` would have been
silently inert — `hidden` makes the panel a scroll container, so the
headings stick to a box that never scrolls. `overflow: clip` clips the
same rounded corners without creating one. Worth knowing because the
symptom is *nothing happening*, which is easy to misread as unsupported
`position: sticky`.

`:has()` and `color-mix()` are used deliberately (Baseline 2023); both
degrade to no spine and no tint rather than to a wrong colour, and the
badge always states the severity in text.

The direction has not had operator feedback yet. Flagged as most likely
to change: the teal, the critical-row tint at real volume, and the spine
width.

`cargo test --workspace`: 193 passing, 0 failing.

### 2026-09-17 — M3: identity (tasks 19–23)

`overlord-engine/src/identity.rs` and `overlord-store/src/identity.rs`,
plus the `/identity` screen, the verbs on both detail pages, and four CLI
verbs. The schema needed nothing: M1 built `suggestion`, `link`,
`link_primary` and `person_alias` and M3 finally writes to all four.

**The three signals, and one rule about all of them.** §12 asks for
conservative and explainable candidates and names exact email, directory
id attributes, and username conventions. Each is a named lookup across a
short list of overlay fields, with a narrow fallback to the entity key
where it is email-shaped — most connectors key users by address, and a
key that is not an address says nothing about identity.

The conservatism is one rule, stated once: **a signal counts only when it
names exactly one account on each side.** Two directory accounts sharing
a username have not identified anybody, which is the same refusal §6.4
makes for `entity(...)` selectors, applied a step earlier. Requiring it
in both directions also keeps the proposal symmetric — an operator
looking at either account sees the same suggestion, or neither does.
`fixtures/identity-{ws,idp}.json` exist to prove this: one person per
signal, and Sam, whose two directory accounts both claim `username:
"sam"` and who is therefore proposed to nobody.

**Suggestions are computed before evaluation, not after.** They are the
input to `suppress_if_pending_links`, and a check that exists to stay
quiet about an account overlord has just proposed a link for cannot be a
sweep behind — the noise it prevents would already be on the board.
Because it runs inside `evaluate_sweep`, replay reproduces it at each
sweep's commit position like everything else, and `suggestion` is in the
rebuild's projection list.

**The queue folds mirrors; the table does not.** A match between two
unlinked accounts is stored from both sides, because each account's
detail page has to show it. As a queue that is two rows for one decision,
and confirming either resolves both — so `pending_suggestions` keeps one
row per pair, preferring the higher-scoring account's. The stored set
stays symmetric.

**Promotion is derived, not recorded.** §6.4 promotes an implicit
singleton person on its first confirmed link, and the obvious
implementation is to write a `person_alias` row. It is also wrong: an
`unlink` would then have to remember to delete it, and a `split` to
repoint it. An implicit uid *names its entity*, so the `link` table
already says what it became. `resolve_person` now resolves an implicit
uid through `link` and a retired uid through `person_alias`, in that
order, and there is no cleanup anywhere to forget. `recompute_scores`
does the same two steps in SQL, which is the whole of why the total is
invariant: every violation is named by a uid first and resolved second,
so it lands on exactly one person and linking can only move weight
between rows.

**Carrying an episode was the hard part.** "Promoted … carrying their
violation history with them" cannot mean rewriting the episode onto the
new uid — §12 is explicit that history resolves *through* a retired uid
rather than being edited, and two episodes for one check would collide.
So evaluation now asks for standing episodes across a subject's own ref
*and* the refs it has absorbed, continues the oldest ("ignored longest"
is the board's tiebreak, and it holds the acknowledgement), and resolves
the rest with a new `ResolveReason::Merged`. Without the last part a
promotion would leave two standing episodes describing one person and
score them both. The same machinery fixes merge, which had the same hole:
before this, merging two people silently dropped the acknowledgement on
the retired one's violations at the next sweep.

**Two bugs found by building on M2.**

- `SubjectRef` derived `#[serde(tag = "kind")]`, which cannot wrap a
  transparent newtype — so any command carrying a *person* subject failed
  at serialization. Acknowledging an orphan-account violation, the most
  obvious thing to do with a person-scoped check, was impossible. Every
  existing test acknowledged an entity, so nothing caught it. It now
  serializes as its canonical string, which is also what the
  `subject_ref` column and every URL already hold; the old tagged shape
  still deserializes so a store written before the fix replays.
- Scores were only recomputed at sweep time, so suppressing a violation
  or linking an account left the Users screen wrong until the next run.
  `project_command` now recomputes for every person and violation verb.

**What the store refuses, and why each one.** Linking to an implicit uid
(that is an account, not a person — create a person and link both);
linking an account overlord has never observed (a typo would otherwise
create an invisible person); unlinking an account somebody else now holds
(a stale form must not detach it); merging a person into itself, or
merging an unlinked account (promotion is what linking does); splitting
off an account the person does not hold; designating a primary for an
account they do not hold — the selector it exists to disambiguate would
never find it.

**The UI.** `/identity` is a queue, not a graph: one account, one
proposed person, one signal, one Confirm. Confirming a proposal that
names an unlinked account creates the person and links both — the button
says "confirm" either way, and the row says what will happen. The same
verbs appear where they belong on the detail pages, and a person picker
serves both manual link and merge because only the button differs. A
suggestion is resolved before it is acted on, so confirming the second of
three stale proposals joins the person the first created instead of
minting a rival.

`cargo test --workspace`: 223 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-17 — M4: Google Workspace (task 24)

The first connector that meets a vendor. Admin SDK Directory API, plus
the Licensing API when an operator asks for it, all read-only.

**What it collects.** Accounts (`projection=full`, which is what carries
2-step verification, external ids, organizations and aliases), the
customer's verified domains, groups with their members inverted onto
accounts, and licence assignments per configured SKU. Everything pages,
and a `nextPageToken` that repeats stops the loop rather than spinning.

**Four scopes, and one of them is not read-only.** `apps.licensing` has
no readonly variant — reading which accounts hold which licence takes a
scope that can also assign and revoke. §11 asks every connector to
declare exactly this, so it is declared in three places and requested
only when `licenses` is non-empty. The other three are the `.readonly`
Directory scopes for users, groups and domains.

**Sharing settings are not collected.** §11 names them, the Directory API
does not expose them, and the Drive scope that would is far wider than
anything else here. Recorded as open question 8 rather than approximated:
a check against a guessed `external_sharing` would be confidently wrong.

**The vendor forced four additions to `overlord-connect`,** and this was
the point of PLAN.md's ordering note — Workspace lands first so the
overlay vocabulary is shaped by workspace semantics. The allowlist now
covers origins (a token endpoint is a second host); `RestrictedHttp`
carries a bearer token set during `observe`; `post_form` exists for token
grants; and the ruleset language gained ordered status clauses, `null_if`
for vendor absence sentinels, and `email_local`. Each was needed to make
the Workspace overlay *correct*, not convenient:

- Google spells an account's state across `archived` and `suspended`.
  Read by `suspended` alone, a deprovisioned account is "active", and
  `status == "active"` is in nearly every check.
- `lastLoginTime` for an account that has never signed in is the epoch.
  Coerced literally, "has never signed in" becomes "signed in during the
  Nixon administration" — the same verdict from a dormancy check, for a
  reason the evidence would state wrongly.
- The entity key is `user.id`, Google's immutable one, because keying on
  the address would turn a rename into a tombstone plus a new account and
  lose every episode filed against it. That kills the identity
  fallbacks, which is why `email`, `username` and `employee_id` are all
  mapped — and `username` is the address's local part, which needed
  `email_local` to express.

The one-clause `StatusRule` shape still deserializes, so a stored
`normalization.upsert` body written before today replays.

**An observation is an envelope.** Three APIs feed one account, so the
raw payload is `{ user, external_ids, organization, groups, licenses }`
with the vendor objects verbatim inside. `external_ids` and
`organization` are indexed views of two repeated Directory fields —
keyed by the vendor's own `type`, and the entry it marked `primary` —
because M1 decided `get_path` will not index a list by number, vendor
array order being unstable. That decision was left standing rather than
reopened; the connector indexes explainably instead.

**Absent is not empty, and a degraded overlay is a partial snapshot.**
`groups` and `licenses` are left out entirely when not collected and are
`[]` when collected and empty. A failed group or domain read makes the
whole snapshot `Partial` even though every account was enumerated,
because §10's partial handling — no tombstones, violations stale — is
the right answer for a half-read overlay too. The alternative is
resolving every sharing violation in the tenant at once.
`directory::require_something` is what separates that from a failed
system: an enumeration whose first page failed returns nothing, and
returning nothing is not a tenant that lost everybody.

**Credentials.** Both shapes Google writes, read from the environment as
either the JSON or a path to it: a service account (domain-wide
delegation, `impersonate` required and refused without) and the
`authorized_user` file `gcloud` leaves behind. The assertion's `iat` is
the sweep's `started_at`, not a clock read — PLAN.md §5 allows the run
one clock, taken at the edge, and an assertion is as happy with it.
`Credential`'s `Debug` says "redacted", it has no `Serialize`, and the
two error paths that could quote key material report the serde category
and the variable name instead.

**Tests.** 49 new: wiremock against recorded Directory shapes (two pages,
a rate-limited second page, a 403 on the first, a failed group read, the
licensing path), the RS256 signing path against a throwaway key, and the
allowlist refusing a single-user read, the Reports API and Drive without
a request reaching the mock server. Plus PLAN.md §5's second promise,
unfulfilled since M1 for want of a second connector:
`crates/overlord-connect/tests/read_only.rs` asserts no connector crate
depends on an HTTP client, by directory scan so the next one is covered
the moment it exists.

**The Systems screen shows the allowlist**, per system, with the reason
each entry was asked for. `describe_allowlist` had been written in M1 and
marked "unused today"; a real connector is what made it worth showing.

`cargo test --workspace`: 272 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-17 — M4: the UniFi Access connector (task 26, access half)

`overlord-connector-unifi-access`, read-only against Ubiquiti's Access
developer API. This is the third crate through the connector pattern
(`api.rs` for the reads, `lib.rs` for the trait impl and the envelope,
`ruleset.json` for the shipped normalization, two test files) and the
first with nothing new to add to `overlord-connect` — which is the
useful signal: the seams the Workspace connector grew are the right
shape for a vendor that shares none of its semantics.

**It reports `SystemKind::Sso`.** §11 categorises this connector as
"access and SSO applications", and `sso` is the kind the spec's four
names give that category. Adding a fifth kind for door access would put
one vendor's vocabulary into core; scoping `entity("sso")` to reach the
access system is consistent with how the category is defined.

**Four GETs and no way to write.** Accounts (`expand[]=access_policy`
folds each user's entitlements in), doors (to name a policy's
resources), user groups, and one group's members. Unlocking a door is a
`PUT`, which `ReadMethod` cannot name; the credential and policy
collection endpoints are `GET`s that the allowlist refuses.

**A self-hosted console needed two new seams, one safe and one blunt.**
`reqwest` under the workspace's `rustls-tls` trusts the public roots and
nothing else, so a console serving `:12445` with its own CA is
unreachable — and the host's trust store would not have helped. The fix
is `Connector::root_certificates`, defaulting to empty, plus
`RestrictedHttp::trusted(pem)`: a connector names the CA it needs and
verification stays on. `ca_cert` in the systems config points at the
PEM, because a certificate is per-system configuration; a typo in the
path is a config error rather than an empty trust. The PEM is parsed as
a **bundle** and every certificate in it is trusted — `s_client
-showcerts` prints leaf first, and a single-certificate parse trusts the
one certificate that is not an issuer, which reproduces `UnknownIssuer`
exactly. That was the first cut's bug;
`every_certificate_in_a_bundle_is_trusted_not_just_the_first` holds it
closed.

Some consoles defeat even that: they send a self-signed leaf `rustls`
will not accept as a trust anchor and no CA to pin, so there is no
certificate to name. The blunt seam is `RestrictedHttp::insecure()` via
`Connector::accept_invalid_certificates`, off by default and warned
about each sweep. The read-only guarantee does not depend on TLS — the
allowlist and `ReadMethod` bound every request either way — so what it
gives up is knowing which host answered, and for a LAN appliance whose
alternative is no observability at all, that is a trade an operator
gets to make. `Debug` reports `verify_tls` so it is visible in a log.

**The transport error now reports its cause.** `ConnectorError::Transport`
used `reqwest::Error`'s own `Display`, which stops at "error sending
request for url (…)" — the sentence that made this undiagnosable. It now
walks the `source` chain, so the same failure reads "…: invalid peer
certificate: UnknownIssuer". A one-line change that would have answered
the question directly.

**Issued door credentials are stripped, not stored.** A user payload
carries `pin_code.token` and each `nfc_cards[].token`. Those are secrets
an operator issues, and §2 keeps the fact stream forever in a plaintext
SQLite file, so the envelope drops every token before observation —
keeping the card's id and type, and a `has_pin` boolean for the PIN.
Nothing else is altered. This is a deliberate departure from "store the
raw payload verbatim" and it only applies to credential material; a
test asserts neither token reaches a fact.

**Entitlement is derived, not guessed.** `doors` is the union of a
user's policies' resources, resolved against the door list for names.
`groups` is absent when not collected and `[]` when collected and
empty, and a failed group or door read makes the snapshot `Partial` for
the same reason it does in Workspace: a half-read overlay must not
resolve every entitlement violation at once.

**Tests.** 17 new: the shipped ruleset mapping the identity signals and
all three console lifecycle states; redaction asserted on the envelope
and again end to end; door derivation; the allowlist refusing a
single-user read, the credential collection and the policies endpoint;
wiremock reads across two pages, a failed group read degrading to
partial, and a failed first page reported as a failed system rather
than a partial one; a bogus CA PEM refused rather than silently trusted;
a named CA that cannot be read treated as a config error; no `ca_cert`
meaning no extra roots; verification on unless `tls_insecure` is set,
and `Debug` reporting `verify_tls`; every certificate in a bundle
trusted rather than only the first; and the transport error's cause
chain.

`cargo test --workspace`: 289 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-21 — identity policy: entity types that are not people (§6.4)

Groundwork for the UCM connector, and worth having on its own. §6.4
evaluates every unlinked entity as an implicit singleton person, which is
what makes orphan-account checks possible. That reasoning is about
accounts. A device has no counterpart to be missing, and an implicit
singleton over one holds exactly the entity an entity-scoped check
already sees — so all it adds is a row on the Users roster per device and
a subject handed to every person check that declared no scope. An
unscoped `not has_entity("idp")` over a 200-handset fleet is 200 criticals.

Authored in `overlord.toml` as `[identity] non_person_entity_types`, but
the file is **not** what evaluation reads. §13 admits no input to
evaluation outside the two streams, and this decides which subjects
exist, so a change to the file appends an `identity.policy` command and
evaluation reads the projection — the same shape as a check or a ruleset.
`crates/overlord-engine/src/policy.rs` reconciles the two on every
invocation and appends only on a real difference; set equality is what
keeps a steady state silent. Denylist rather than allowlist so that empty
is exactly the behaviour every existing store already had.

It bites in three places, all reading the same projection: `world.rs`
(no implicit singleton), `read.rs` `all_subjects` (the same predicate in
SQL, so the roster cannot disagree with evaluation), and `identity.rs`
(those types leave the suggestion index — proposing that two handsets be
unified into a person would contradict the policy). Turning it on
resolves the violations it orphans as `subject_absent`, through the
existing `reconcile` pass; no new lifecycle.

Tested: the before/after difference end to end, the orphaned violations
resolving, an unchanged policy appending nothing across repeated runs,
the roster filter, and `identity_policy` added to the replay dump so
`replay(streams) == live` actually covers it.

### 2026-09-21 — M4: the Grandstream UCM connector (task 26, MDM half)

A UCM6308A, read twice. `crates/overlord-connector-grandstream-ucm/` is
`api.rs` (the challenge/login handshake and the four actions) and
`lib.rs` (config, the two modes, redaction, the device envelope), with a
shipped ruleset per mode.

**One appliance is two systems, deliberately.** A UCM holds extensions,
which belong to people, and Zero Config handsets, which do not. A ruleset
declares one `entity_type`, so `mode` picks between
`ruleset-extension.json` (`phone-extension`, kind `sso`) and
`ruleset-device.json` (`phone-device`, kind `mdm`) — the same seam the
fixture connector uses to front two system kinds at once. It is not a
workaround: the two populations sweep at different rates, are scoped by
different checks, and fail independently, which is the property that
matters because Zero Config is the half that may not answer.

Extensions report `sso`, not `workspace`. An extension is an assignment
to a person within one application. Reporting `workspace` would make
`has_entity("workspace")` true for somebody who has only a desk phone and
quietly break the shipped `workspace-without-idp` rule.

**Zero Config is not in the documented API, and this is the thing to
know.** Grandstream's HTTPS API reference enumerates every action the
appliance answers — extensions, trunks, routes, queues, call control —
and none of them returns Zero Config's inventory; the web UI reaches it
by a route outside the documented API. Two independent readings of the
reference agree, and the 221-page PDF guide's action list agrees.

So `zero_config_action` is configuration with a documented guess
(`listZeroConfig`) as its default, and the failure mode is the load-
bearing part: a firmware that does not answer produces a **partial**
snapshot naming the action tried and the status returned. Nothing is
tombstoned, no handset is invented, and the fix is a line of TOML. The
device payload's field names are undocumented for the same reason, so
`device_envelope` tries the plausible spellings of each attribute rather
than trusting one, and a row whose MAC cannot be found is skipped with a
warning naming the fields it did have — an entity with no stable key
cannot survive to the next sweep, and inventing one would re-provision
the fleet on every read.

**The allowlist is thinner here than for a REST vendor, and the docs say
so.** The whole UCM API is `POST /api` with the verb in the body, so a
method-and-path allowlist cannot separate reading an extension from
editing one. What does separate them: `api.rs` is the only place a
request body is built, it names four actions, none mutates, and
`ReadMethod` still cannot spell `PUT` or `DELETE`. Stating the weaker
guarantee is better than implying the stronger one.

Secrets are stripped by field *name*, recursively — anything ending in
`secret` or `password` becomes `<field>_len`. A fixed list would have let
the next firmware's new secret into a plaintext SQLite file kept forever
before anybody noticed. `has_secret` and `secret_len` are enough to write
"this extension has a four-character SIP password" without storing it.

MAC addresses normalize to bare lowercase hex. Vendors are inconsistent
about case and separators, and a firmware upgrade that switched spelling
would otherwise deprovision every handset and provision it again.

Tested: the handshake in order; extensions normalized through the shipped
ruleset; `out_of_service` beating a healthy `status`; `Unavailable`
mapping to `unknown` rather than a claim of deprovisioning; secrets never
reaching the observation at three nesting depths; detail off by default
and one detail failure degrading to partial rather than failing the
system; an empty extension list treated as a failure rather than a
tombstone sweep; a refused login reported with the UCM's own status; the
device path through both the default and a configured action and list
key; an unanswered action staying partial with the knob named in the
warning; a MAC-less row skipped; every non-`/api` path refused before any
call; and `base_url` missing reported as itself.

`cargo test --workspace`: 358 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-21 — UCM connector: first contact with a real UCM6308A

Three defects, all found by pointing it at the appliance.

**The options list was wrong.** `listAccount` returns only the columns
named in `options`, and I had assumed an unknown name would come back
absent. It does not: one undocumented field fails the whole call with
invalid parameters. The list carried two — `secret` and `department` —
so every extension in the system was lost to a speculative field name.
Trimmed to exactly the documented set. `secret` had no business being
there anyway: it was requested and then redacted on arrival, which is a
SIP password crossing the wire for no reason. Its length reaches the
overlay through `detail` instead.

**Paging values were sent as JSON numbers.** The vendor's own examples
spell them as strings (`"page": "1"`), and a firmware that accepts one
and not the other is indistinguishable until it answers invalid
parameters. Both the extension and device reads now send strings.

**A read that got nothing reported `partial` instead of failing.** This
is the one that made the other two hard to diagnose: a partial snapshot
of zero extensions reads as "the sweep worked and found nothing", which
is the only thing that had not happened. The UniFi connector already had
the right convention in `require_something` — nothing read *and*
something wrong is a failed system, not a partial one — and this now
follows it. Zero Config keeps the opposite treatment for an empty list
with no error, because a UCM with no provisioned handsets is a real
answer rather than a failed read.

Also: a rejected `listAccount` now names the fields it asked for, and
status codes carry their documented meaning where Grandstream publishes
one (-1, -5, -6, -8, -37, -45). `-47`, which is what a wrong API user
produced, is documented nowhere — not in the reference, the 221-page
guide, or the forums — so it is reported as the number it is, with the
failing action named. Which action failed is most of the diagnosis: the
challenge is unauthenticated, so a failure there is about the API user
existing, while a failure at login is about the password.

Tested: the request shape asserted field by field against the documented
option list, so a speculative name cannot be added back silently; paging
values asserted as strings; the rejected-options path asserted to fail
the system and name what it sent; an empty Zero Config list asserted to
stay complete.

`cargo test --workspace`: 363 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-21 — the entity browser and full-text search

A screen SPEC.md section 5 does not list, added because the identity
policy made its absence a hole: the Users roster is person-shaped and now
deliberately excludes the entity types that are not people, so a fleet of
handsets was collected, evaluated and violation-tracked with nowhere to
look at it. `/entities` lists entities as entities.

**Facets: connector, system, system kind, entity type, presence.** The
first three are the ones an operator actually has in mind; entity type is
what separates two populations read from one appliance. `SYSTEM_FACETS`
resolves connector and kind separately on purpose — the connector is the
latest one *recorded*, matching the `connector:` check selector so the
screen and a check scope never disagree, while the kind comes from the
entity's own overlay and falls back to the system's last sweep, which is
what keeps a tombstone classifiable.

A facet that does not parse is **refused**, not ignored and not
defaulted, which is what the violations board already does with a bad
severity. `?kind=mdmm` quietly listing the IdP was the first version and
it is the worst of the three behaviours: it answers a question nobody
asked.

**Search runs over the latest fact's content** — the normalization
overlay and the vendor payload behind it — as an FTS5 index maintained by
trigger. Two decisions are load-bearing.

*Leaf values, not the JSON around them.* `json_tree` keeps the scalars
and drops the keys, so `grandstream` finds the handsets whose vendor is
Grandstream rather than every entity that has a `vendor` key.

*`tokenchars '.-:@_'`.* Almost everything searched for here is an
identifier carrying punctuation — a firmware version, an address, an IP,
a colon-written MAC. The default tokenizer splits those into fragments,
and searching `9.9.9.9` matched a handset running `1.0.9.10`, which is
not a near miss but the wrong answer. Keeping the characters inside
tokens makes an identifier one token; prefix matching is what still lets
`ada` find `ada@example.com`. The cost is that `x.com` no longer matches
`ada@example.com`, which is the right side of the trade. `fts_query` and
the migration have to spell the same set, so `read::TOKEN_CHARS` is the
one definition and both cite it.

Operator input is never an FTS5 query: the syntax has its own operators
and an email address alone has enough punctuation to turn a search into a
database error. `fts_query` rebuilds the input as quoted prefix terms and
drops everything else.

**By trigger rather than in `project.rs`**, where every other projection
is built. `entity` is written from two places — the sweep's upsert and
replay — and an index one of them forgot would not fail; it would return
fewer results than the store holds, which is the worst way for a search
box to be wrong. It also means `rebuild` needs no special case, and the
migration backfills so search is not empty until the next sweep.

Also a `overlord entities` CLI command with the same facets, and the
screen in the nav after Identity — the two screens before it are about
the person/not-a-person distinction, and this is the list that does not
care.

Tested: each facet alone and two facets narrowing together; search
reaching the overlay, the raw payload and the entity key; values indexed
rather than keys; identifiers whole and by prefix; ten kinds of
punctuation in the box asserted not to error; the index following the
latest fact rather than the first; tombstones excluded by default and
findable on request with the kind still resolving; survival of a rebuild;
and at the route level, the browser listing what the roster excludes, the
htmx fragment carrying no nav, and a bad facet returning 400.

`cargo test --workspace`: 379 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-21 — a blank display name is not a display name

`subject_label` returned `Some("")` as the label, which rendered
`<a class="ref" href="..."></a>`: a link with no text, invisible in the
table and impossible to click. Everything downstream treats "there is a
display name" as a reason to show it *instead of* the entity key, so a
blank one removes the only thing identifying the row.

Reachable rather than theoretical. Vendors spell "not filled in" as
`null` and as `""` interchangeably — Grandstream's own `listAccount`
example does both — and an unnamed extension or a handset provisioned
without a label is the ordinary case, not the edge.

Fixed at both ends, because they cover different populations. In
`normalize`, a blank never becomes a `display_name`, which keeps it out
of the fact stream going forward; the overlay still records a real name
untrimmed, since trimming for display is the view's business. In
`view::named`, a blank is treated as absent at the one point every label
passes through — which is what covers facts already in the store, and
any connector whose ruleset maps a blank in future. The CLI has the same
guard for the same reason.

Also the two detail-page titles, which used `unwrap_or_else` directly
and would have opened with an empty heading.

Tested at each layer: the overlay dropping blank and whitespace names
while keeping a padded real one; `subject_label` and `subject` falling
back to the ref for `""`, whitespace and `None`; and end to end, a fact
stored with `display_name: ""` rendering a row that identifies itself,
an anchor that is not empty, and a detail page that is not titled
nothing.

`cargo test --workspace`: 383 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-21 — UCM: email addresses, and three bugs found getting them

**The address is not on the extension.** `listAccount`'s documented
options carry `email_to_user`, a `"yes"`/`"no"` flag, and no address;
`getSIPAccount`'s documented response has no address either. The address
is on the appliance's *user* record — a separate object joined to the
extension by `user_name` — which `listUser` returns. That is the address
an extension's voicemail is emailed to. `users = true` (default) reads it
as one paged call and folds each record onto its extension; a user read
that fails is a gap, not a failure, so extensions still collect.

The guide documents the address and the flag but does not say the flag is
voicemail-specific rather than governing user email generally, so both
reach the overlay under the appliance's own names rather than under one
that would assert the connection.

Three defects found on the way, all in what shipped this morning.

**`email` was mapped to `email_to_user`.** The overlay's email field held
the string `"no"`. That field is what identity resolution reads to
propose cross-system links, so the connector was feeding a yes/no flag
into the signal that decides whether a PBX extension and a Workspace
account are the same person. The per-system uniqueness rule would have
suppressed most of the damage, which is the only reason this was not
visible.

**Every boolean was null.** The UCM spells booleans `"yes"`/`"no"`; the
shared normalizer accepts `"true"`/`"false"` only, deliberately, as the
two unambiguous spellings. So `has_voicemail`, `dnd` and `nat` were null
plus a warning on every extension on every sweep — not a degraded check
but one that could never fire. `yes_no_to_bool` rewrites them inside the
connector, which keeps the vendor's dialect where the vendor is, rather
than widening a shared coercion for one appliance.

**`out_of_service` never suspended anything.** Same root cause: the
status rule keyed on `"1"`/`"true"` and the appliance says `"yes"`, so a
disabled extension read as active. The map now carries all three
spellings, belt and braces, since the rewrite already handles it.

Also: a user record carries the live session id of whoever is logged into
the web UI as that user. `redact` now drops any `cookie` field outright —
unlike a password, even its length is worth nothing — and `clean` is the
one function every stored record passes through, so a path that redacted
without rewriting booleans, or the reverse, is not expressible.

Tested: the address arriving from the user record; an extension with no
user record having no address rather than a wrong one; an unset address
staying null rather than becoming the flag; the session cookie never
reaching the stream; a failed user read staying partial; `users = false`;
a user holding several extensions attaching to each; `"yes"`/`"no"`
converting while `"off"`, `"internal"` and `"Yes"` do not; and an
out-of-service extension finally reading as suspended.

`cargo test --workspace`: 393 passing. `clippy -D warnings` and
`fmt --check`: clean.

### 2026-09-21 — listUser's parameters, and recording why a sweep was partial

Sweep 16 came up partial with all 80 extensions collected and none
carrying `raw.user`: `listUser` had failed wholesale, with status -26.
Permissions were ruled out on the appliance — `listUser` is in the cdrapi
set — which leaves a parameter.

**The suspect was `sidx`.** The UCM62xx guide's `listUser` example sorts
by `extension`, which is not a column the user record has; it has
`user_name`. Nothing in this connector needs an order — the records go
into a map keyed by extension — so `sidx` and `sord` are simply not sent
any more. An optional parameter that buys nothing is only a way for the
call to fail.

This is a reasoned hypothesis, not a confirmed diagnosis: -26 is
documented nowhere, and the appliance says nothing but the number. So
the call also **retries once with no parameters at all** when the first
attempt fails — asking for the whole collection in one request is the
shape that depends on the least. If both fail, both are reported,
because "it refused paging and it refused nothing at all" says something
neither attempt says alone.

**A partial system now records why.** This was the real reason the above
took a round trip: `complete` is a boolean and `error` is only written
when a system *failed*, so `Completeness::Partial { reason }` was
discarded at the point of writing. A partial sweep recorded that it was
partial and threw away the one thing an operator wants from it, and
answering "partial why?" meant re-running the sweep to watch the
warnings scroll past. A reason only observable while it happens is not a
record. `sweep_system.partial_reason` holds it, `CoverageRow` carries it,
and the sweep detail screen prints it under the row — beside `error`
rather than instead of it, because a system that collected something and
knows it is incomplete is not the same as one that failed.

Migrations verified against a copy of the live 11MB store: the column
adds cleanly and 0004's backfill indexed all 1337 entities.

`cargo test --workspace`: 400 passing. `clippy -D warnings` and
`fmt --check`: clean.
