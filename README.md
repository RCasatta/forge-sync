# forge-sync

`forge-sync` incrementally mirrors the raw issue and code-review metadata used
for local project briefings. GitHub access is anonymous, HTTPS-only and GET-only.
Private GitLab access is performed by the immutable Nix-provided `glab api`
executable using the user's existing login.

```text
forge-sync github <OWNER/REPOSITORY> <ABSOLUTE_OUTPUT_DIRECTORY>
forge-sync gitlab <HOST> <GROUP/PROJECT> <ABSOLUTE_OUTPUT_DIRECTORY>
forge-sync status <ABSOLUTE_OUTPUT_DIRECTORY>
```

Arguments have exact arity. Provider identifiers are validated before any I/O,
and destinations must be absolute paths without symlink components. Cache files
are private (`0600`, directories `0700`) and replaced atomically. Each cache has
a warning sentinel because it may contain private material.

Synchronization writes an in-progress manifest before requests begin and only
advances the cursor after the changed-item query and its required detail data
have succeeded. Normal GitHub runs request items updated since the successful
cursor, newest first, using a five-minute overlap. They update the complete local
open snapshot from observed state transitions instead of enumerating every open
item again. Multi-page change sets are checkpointed in `github-delta.json`;
resumed runs recheck the newest page before continuing their saved page cursor.

The first GitHub synchronization enumerates open items newest first and stores
its accumulator and next page in `github-bootstrap.json`. A later invocation
checks for new updates before resuming older pages. Detail bundles have a
provider timestamp marker written last, so interrupted bundles are safely
retried while completed bundles are reused. Rate exhaustion leaves the manifest
`partial` and GitHub stops with two requests remaining.

Open items are fully reconciled every seven days to catch discussion activity
which does not update its parent. Reconciliation has its own persistent page and
item progress. A successful incremental snapshot remains usable while this
background reconciliation spans multiple rate-limit windows; `status` reports
whether reconciliation is still in progress.

`status` never uses the network. It returns nonzero for incomplete or
inconsistent data and reports the last completed timestamp. It deliberately
does not impose an age limit: consuming projects decide freshness requirements
from that timestamp.

Development and installation use the flake:

```sh
direnv exec . cargo test --quiet
nix build
./result/bin/forge-sync status /absolute/cache/path
```
