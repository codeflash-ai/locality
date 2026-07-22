# Linear Connector

The Linear connector is a first-party Locality source connector named `linear`.
It uses Linear's GraphQL API for issue metadata, issue body fetch/render, and
approved issue updates.

## Connection

The daemon resolves Linear mounts from stored `api_key` connections. Create the
default connection with:

```bash
printf '%s' "$LINEAR_API_KEY" | loc connect linear --api-key-stdin
loc mount linear ~/Locality/linear --connection linear-default
```

`loc connect linear` stores the API key in the credential store under the
connection secret reference and persists only non-secret connection metadata in
SQLite. `loc mount linear` validates the stored connection before saving the
mount. The default connection id is `linear-default`; Linear mounts default to
`linear-main`. OAuth is intentionally not advertised by the source descriptor
until a Locality OAuth broker flow exists for Linear.

The desktop **Add Source** dialog and first-run onboarding support the same API
key flow. Pasting a Linear API key creates or reconnects the default
`linear-main` mount under the desktop CloudStorage root. Unlike Granola, the
desktop Linear mount is editable by default because the connector supports
approved issue updates.

The projection groups issues by team and Linear workflow status:

```text
Teams/
  Engineering/
    Issues/
      Todo/
        ENG-1 Improve sync/
          page.md
          comments.md
          attachments.md
          pull-requests.md
          history.md
```

`Teams`, `Issues`, and status directories are metadata containers. Team
directories preserve the Linear team identity; status directories are keyed by
team and Linear state id. Only statuses present in fetched issue metadata are
shown, so empty workflow states are omitted. Issue directories are named by the
Linear issue identifier followed by the issue title, and each issue `page.md` is
canonical Markdown with Linear reference fields in frontmatter and the Linear
issue description as the body. Rendered references use the `Label <id>` shape so
local diff can ignore label-only refreshes while preserving the stable Linear
UUID.

`page.md` remains the only editable issue body. The generated sidecars are
read-only `loc.type: asset` Markdown files with `loc.connector: linear`,
`linear.issue_id`, `linear.issue_identifier`, `linear.context`, and
`linear.read_only: true` frontmatter:

- `comments.md` contains all paginated issue comments in creation order,
  including author, URL, timestamps, parent/resolution metadata when present,
  and the comment Markdown body.
- `attachments.md` contains all paginated issue attachments in creation order,
  including title, URL, source type, timestamps, creator, subtitle, download
  status, and stable pretty-printed raw metadata JSON.
- `pull-requests.md` is derived from GitHub/GitLab-like attachments and
  pull-request-shaped attachment metadata. It also renders Linear's suggested
  `branchName` even when no pull request attachment exists.
- `history.md` contains all paginated issue history entries in creation order,
  not only status changes. It renders actor, state, title, assignee, project,
  team, labels, due date, estimate, priority, description updates, attachment
  changes, and raw `changes` JSON when Linear provides it.

Rendered issue frontmatter also includes read-only lifecycle and date metadata:
`created_at`, `updated_at`, `archived_at`, `started_at`, `completed_at`,
`canceled_at`, `auto_archived_at`, `auto_closed_at`, `started_triage_at`,
`triaged_at`, `snoozed_until_at`, `added_to_cycle_at`, `added_to_project_at`,
`added_to_team_at`, and `due_date`. Missing optional values render as `null`;
present timestamps and dates render as quoted strings. Only `title`, `Status`,
`Project`, and `Assignee` are editable frontmatter fields. The lifecycle/date
fields are generated metadata and are rejected as read-only if edited locally.

The connector currently supports:

- full issue enumeration;
- lazy root/team child listing;
- lazy issue sidecar listing;
- single-issue observation and batch observation;
- issue fetch/render;
- generated comments, attachments, pull request, and history sidecar
  fetch/render;
- whole-entity body updates mapped to Linear issue descriptions;
- property updates for title, status, project, and assignee;
- moving issue folders into another `Teams/<team>/Issues/<status>/` folder to
  update the Linear team and/or status.

Unsupported properties, including lifecycle/date metadata, fail closed before
remote mutation. Undo, issue creates, and deletes remain future work for the
native connector.

Sidecar remote ids use `linear-context:<issue_id>:comments`,
`linear-context:<issue_id>:attachments`,
`linear-context:<issue_id>:pull-requests`, and
`linear-context:<issue_id>:history`. Apply and concurrency checks reject these
ids as read-only before any `issueUpdate` mutation is attempted.

## Attachments

Linear issue attachments are external links. During hydration of
`attachments.md`, Locality best-effort downloads HTTP(S) attachment URLs capped
at 25 MB per attachment into:

```text
.loc/linear/attachments/<issue-id>/<stable-file-name>
```

Download failures and skipped downloads do not fail hydration. The attachment
entry keeps the remote URL and renders `download_status`, `local_path` when a
download succeeds, and `download_error` when a download is skipped or fails.
Linear-hosted and same-GraphQL-host download URLs receive the Linear API key;
third-party URLs are fetched without sending the Linear token.
Stale files in the same issue attachment cache are pruned when a later
hydration writes replacement assets for that issue.

## Daemon Integration

Linear is registered in the daemon source registry, so source resolution,
hydration, lazy child listing, single observation, batch observation, and push
apply all route through the native connector. Mount guidance is Linear-specific
and tells agents to preserve UUID references in editable frontmatter.

The descriptor uses whole-entity body diffs because Linear issue descriptions
are updated as a single remote field. Local creates are rejected by the source
policy for now; edits to existing issue files remain writable unless the mount
itself is read-only. Generated sidecars are read-only even when the mount is
writable. Moves are writable only when the destination parent is a status folder
shaped as `Teams/<team>/Issues/<status>/`; arbitrary creates, renames, and
moves at grouping levels stay read-only.

Folder moves are applied through the same GraphQL `issueUpdate` mutation as
frontmatter updates. The connector parses the destination status-folder remote
id, sends `teamId` and `stateId`, reports a moved-entity journal effect, and
marks the issue remote id as changed. Reconciliation then re-reads Linear and
accepts the refreshed canonical path, including any new identifier Linear
assigns after a cross-team move.

Background connector sync schedules Linear discovery every five minutes. Batch
observation uses the connector checkpoint's `updated_after` timestamp to refresh
changed issues and repair dependent metadata when referenced Linear UUIDs are
renamed remotely.
