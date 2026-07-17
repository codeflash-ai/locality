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

The initial projection groups issues by team:

```text
Engineering/
  ENG-1/
    page.md
```

Team directories are metadata containers. Issue `page.md` files are canonical
Markdown with Linear reference fields in frontmatter and the Linear issue
description as the body. Rendered references use the `Label <id>` shape so local
diff can ignore label-only refreshes while preserving the stable Linear UUID.

The connector currently supports:

- full issue enumeration;
- lazy root/team child listing;
- single-issue observation and batch observation;
- issue fetch/render;
- whole-entity body updates mapped to Linear issue descriptions;
- property updates for title, status, project, and assignee.

Unsupported properties fail closed before remote mutation. Undo and create/move
operations remain future work for the native connector.

## Daemon Integration

Linear is registered in the daemon source registry, so source resolution,
hydration, lazy child listing, single observation, batch observation, and push
apply all route through the native connector. Mount guidance is Linear-specific
and tells agents to preserve UUID references in editable frontmatter.

The descriptor uses whole-entity body diffs because Linear issue descriptions
are updated as a single remote field. Local creates are rejected by the source
policy for now; edits to existing issue files remain writable unless the mount
itself is read-only.

Background connector sync schedules Linear discovery every five minutes. Batch
observation uses the connector checkpoint's `updated_after` timestamp to refresh
changed issues and repair dependent metadata when referenced Linear UUIDs are
renamed remotely.
