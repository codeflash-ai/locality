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
        ENG-1/
          page.md
```

`Teams`, `Issues`, and status directories are metadata containers. Team
directories preserve the Linear team identity; status directories are keyed by
team and Linear state id. Only statuses present in fetched issue metadata are
shown, so empty workflow states are omitted. Issue directories are named by the
Linear issue identifier, and each issue `page.md` is canonical Markdown with
Linear reference fields in frontmatter and the Linear issue description as the
body. Rendered references use the `Label <id>` shape so local diff can ignore
label-only refreshes while preserving the stable Linear UUID.

The connector currently supports:

- full issue enumeration;
- lazy root/team child listing;
- single-issue observation and batch observation;
- issue fetch/render;
- whole-entity body updates mapped to Linear issue descriptions;
- property updates for title, status, project, and assignee;
- moving issue folders into another `Teams/<team>/Issues/<status>/` folder to
  update the Linear team and/or status.

Unsupported properties fail closed before remote mutation. Undo, issue creates,
and deletes remain future work for the native connector.

## Daemon Integration

Linear is registered in the daemon source registry, so source resolution,
hydration, lazy child listing, single observation, batch observation, and push
apply all route through the native connector. Mount guidance is Linear-specific
and tells agents to preserve UUID references in editable frontmatter.

The descriptor uses whole-entity body diffs because Linear issue descriptions
are updated as a single remote field. Local creates are rejected by the source
policy for now; edits to existing issue files remain writable unless the mount
itself is read-only. Moves are writable only when the destination parent is a
status folder shaped as `Teams/<team>/Issues/<status>/`; arbitrary creates,
renames, and moves at grouping levels stay read-only.

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
