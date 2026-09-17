# Fixture scenarios

Input for `overlord-connector-fixture`. Each file is a sequence of
**stages**, one per sweep: a system's configuration names the file and the
stage, so a scenario can be swept repeatedly and tell a story.

The cast is shared across files so cross-system checks have something to
work with:

| key | workspace | idp | note |
| --- | --- | --- | --- |
| `ada@example.com` | yes | yes | healthy |
| `grace@example.com` | yes | yes | admin, no MFA, dormant |
| `alan@example.com` | yes (stage 0) | no | departs at stage 1 |
| `svc-deploy@example.com` | yes | no | orphan service account |

| file | what it is for |
| --- | --- |
| `baseline.json` | the ordinary case; stage 1 fixes one violation and removes one account |
| `idp.json` | the second system, so person-scoped checks have two sides |
| `truncated.json` | a partial snapshot, which may never produce tombstones |
| `mass-absence.json` | a complete-looking snapshot that lost most of its accounts; the absence guard must refuse it |
| `unreachable.json` | a connector that fails mid-sweep |
| `identity-ws.json` + `identity-idp.json` | one account per person on each side, spelled differently, so each link signal is exercised in isolation |

## The identity pair

`baseline.json` and `idp.json` spell everybody the same way, which makes
every link a one-line email match and tests nothing. The identity pair
gives each signal of SPEC.md section 12 exactly one person to find, and
one case that must find nobody:

| person | workspace | idp | the only signal that connects them |
| --- | --- | --- | --- |
| Ada | `ada@example.com` | `ada@example.com` | `exact-email` |
| Grace | `grace@example.com` | `ghopper@corp.example.com` | `directory-id` (`E-1002`) |
| Alan | `alan@example.com` | `alan@corp.example.com` | `username` (the local part) |
| Sam | `sam@example.com` | `sam@corp…` **and** `sam.admin@corp…` | none: two candidates in one system is not an identification |
| the robot | `svc-deploy@example.com` | — | none: a genuine orphan |

Sam is the point of the file. Both directory accounts carry
`username: "sam"`, so the signal names two accounts on the idp side and
overlord proposes nothing in either direction — the same conservatism
section 6.4 applies to `entity(...)` selectors, applied a step earlier.
