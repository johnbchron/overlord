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
