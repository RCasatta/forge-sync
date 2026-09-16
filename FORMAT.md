# `forge-sync` data format

This document defines snapshot schema version 1. `forge-sync` writes
provider-owned raw JSON together with a small normalized index. Consumers
should use the index for discovery and the raw files for evidence.

## Snapshot validity

Every repository or project snapshot has a `manifest.json`. Treat a snapshot
as current and internally consistent only when all of these are true:

- `schema_version` is supported by the consumer;
- `provider` and `identity` match the requested source;
- `state` is `"complete"`;
- `last_complete_at` and `finished_at` are non-null; and
- the provider's required top-level files exist.

`forge-sync status <ABSOLUTE_OUTPUT_DIRECTORY>` performs these checks without
network access. It returns nonzero for partial, failed, or inconsistent data.

During a partial or failed run, some resource files may have been refreshed
while top-level snapshots still describe the previous complete run. Do not
combine them as one point-in-time snapshot. Use the last complete snapshot only
when the consuming workflow can identify it consistently; otherwise report the
sync failure and stop.

## `manifest.json`

Schema version 1 contains:

| Field | Meaning |
| --- | --- |
| `schema_version` | Data-contract version; currently `1`. |
| `program_version` | `forge-sync` package version. |
| `provider` | `github` or `gitlab`. |
| `identity` | `OWNER/REPOSITORY` for GitHub or `HOST/GROUP/PROJECT` for GitLab. |
| `state` | `complete`, `partial`, or `failed`. |
| `started_at`, `finished_at` | UTC timestamps for the latest attempt. |
| `last_complete_at` | UTC timestamp of the latest completed synchronization. |
| `cursor.used` | Overlapped lower bound used by the latest incremental scan. |
| `cursor.next_safe` | Safe cursor retained for the next scan. |
| `reconciliation_in_progress` | Whether a periodic full open-item reconciliation remains unfinished. |
| `last_reconciliation_at` | UTC completion time of the last full reconciliation. |
| `pages_completed`, `requests_completed` | Work completed by the latest attempt. |
| `rate_limit` | GitHub rate-limit data when available; otherwise null. |
| `counts` | Counts of open and changed issues and reviews. |
| `unsupported_resources` | Known resources intentionally absent from the snapshot. |
| `failure` | Bounded, sanitized failure text or null. |

In `counts`, `open_reviews` means GitHub pull requests or GitLab merge requests;
`changed_reviews` has the corresponding provider-specific meaning.

## `index.json`

`index.json` is an array of normalized current open items. Each object contains:

| Field | Meaning |
| --- | --- |
| `provider` | `github` or `gitlab`. |
| `kind` | `issue`, `pull`, or `merge-request`. |
| `number` | GitHub number or GitLab project IID. |
| `state` | Provider state string. |
| `title` | Item title. |
| `author` | Login/username or null. |
| `draft` | Draft/WIP state or null. |
| `created_at`, `updated_at` | Provider UTC timestamps or null. |
| `url` | Direct provider web URL or null. |
| `labels` | Label-name strings. |
| `assignees`, `reviewers` | Login/username strings. |
| `head_sha` | Review head commit SHA or null. |
| `raw_paths` | Paths relative to the snapshot root containing detailed evidence. |

The index is intentionally small. Approval state, unresolved discussions,
pipeline/check results, diffs, comment bodies, and review chronology remain in
the raw files referenced by `raw_paths`.

## Common top-level files

- `manifest.json`: synchronization state and counts.
- `index.json`: normalized current open-item discovery index.
- `open-issues.json`: raw current open issues.
- `changed-items.json`: raw items changed during the successful incremental
  window; it may include items that are now closed or merged.
- `README-PRIVATE.txt`: warning that the cache may contain private material.
- `.forge-sync.lock`: synchronization lock; consumers must ignore it.

Files such as `github-bootstrap.json`, `github-delta.json`, and
`github-reconciliation.json` are resumable internal progress. They are not
complete snapshots and must not be used as briefing evidence.

## GitHub layout

Additional top-level file:

- `open-pulls.json`: raw current open pull requests as returned through
  GitHub's issue representation.

Issue details:

```text
issues/<number>/
  issue.json
  comments.json
  events.json
  sync-meta.json
```

Pull-request details:

```text
pulls/<number>/
  pull.json
  issue-comments.json
  review-comments.json
  reviews.json
  commits.json
  files.json
  check-runs.json
  sync-meta.json
```

GitHub `kind` values are `issue` and `pull`. GitHub Discussions are absent
because they require authenticated GraphQL; this is recorded in
`unsupported_resources`.

## GitLab layout

Additional top-level files:

- `project.json`: raw project metadata.
- `open-merge-requests.json`: raw current open merge requests.
- `todos.json`: pending user To-Dos filtered to the selected project.

Issue details:

```text
issues/<iid>/
  issue.json
  discussions.json
  resource-state-events.json
  resource-label-events.json
```

Merge-request details:

```text
merge-requests/<iid>/
  merge-request.json
  approvals.json
  discussions.json
  commits.json
  changes.json
  pipeline.json
```

GitLab `kind` values are `issue` and `merge-request`. `pipeline.json` is null
when an MR has no head pipeline. If a referenced pipeline has disappeared or
is inaccessible with a 404, it contains an explicit marker such as:

```json
{"id": 491567, "unavailable": "not_found"}
```

## Compatibility rule

Consumers may rely on the version-1 fields and paths documented above. New
optional fields or files may be added without changing `schema_version`.
Removing or renaming documented fields or files, changing their meaning, or
changing normalized `kind` values requires a new schema version and updated
consumer documentation.
