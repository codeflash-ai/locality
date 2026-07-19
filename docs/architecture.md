# Architecture

Locality v1 is a local-first Rust system with seven implementation surfaces:

For a clickable subsystem diagram grounded in code entry points, see
[`architecture-diagram.html`](architecture-diagram.html).

1. `loc` CLI: stable command and exit-code surface for humans and agents.
2. `localityd` daemon: one per user, supervising many mounts.
3. Sync core: connector-agnostic correctness layer.
4. Connector SDK: trait boundary for source-specific APIs.
5. Notion connector: first first-party connector.
6. State store: SQLite-backed source of truth under `~/.loc/`.
7. Platform projections: macOS File Provider now, Linux FUSE next, Windows Cloud
   Files later.

The optional cloud relay is deliberately not implemented in v1. The local remote-truth boundary should still remain swappable so a future relay can replace direct source polling without changing the sync model.

## Target Cloud-Sandbox Architecture

The proposed successor to the optional relay is a continuously maintained
backend data plane, not a thin request proxy. It ingests connected sources once,
maintains immutable versions and current projections in PostgreSQL, runs exact
ACL/profile filtering in one server query, and streams a transient standard tar
with negotiated Zstandard compression. The client concurrently decodes and
materializes ordinary files in a staging tree, then publishes it atomically
before the agent starts. Large opaque attachments and backups use object storage;
v1 has no persistent content-pack/catalog layer. The three high-volume serving
tables use a small identical tenant/source hash layout, monotonic locality keys,
and ordered export reads; broader indexing and physical reorganization remain
measurement-driven.
Read-only content uses a single local tree; three-way state is reserved for
writable resources. The proposed v1 is multi-tenant and multi-connector, uses a
modular backend with PostgreSQL jobs/read serving, and relies on local diff plus
explicit push. A PostgreSQL read replica and then ClickHouse are measured future
scale paths; human-review workflows remain later extensions.

This target does not replace the filesystem path used by the desktop or
headless `loc` CLI. `localityd` remains the shared local-host runtime for
desktop, CLI, and sandbox clients. Direct-source and backend-replica modes are
adapters around the same Rust domain, connector, projection, changeset, and
apply/reconcile workflows; only persistence, credentials, scheduling, and
remote-truth transport differ by host.

See [`cloud-sandbox-data-plane.md`](cloud-sandbox-data-plane.md) for the target
architecture, database decision, data model, replica protocol, permissions,
security model, migration path, and implementation phases. The rest of this
document continues to describe the currently shipped v1 architecture.

## Connector and Auth Model

Locality keeps connector capability, authentication policy, account credentials,
mount projection, and execution separate:

```text
connector implementation
      |
      v
connector profile / auth config
      |
      v
connected account
      |
      v
mount
      |
      v
pull/push execution context
```

- A connector implementation defines provider behavior and supported sync
  operations.
- A connector profile defines how that connector authenticates: auth kind,
  scopes, enabled action classes, connector version, status, and capabilities.
- A connected account stores provider/workspace metadata and a `secret_ref`;
  the bearer token or OAuth secret lives only in the credential store.
- A mount references a connection ID and never stores credentials.
- Pull, push, scheduled hydration, and daemon jobs resolve credentials from the
  mount's connection and profile at execution time. Daemon IPC does not carry
  bearer tokens.

Notion ships with two local profiles: `notion-oauth-default` for the preferred
OAuth connection flow and `notion-token-default` for explicit PAT fallback.
The model is intentionally compatible with connector version pinning, scoped
action sets, health checks, and remote relay execution.

## Crate map

| Crate | Responsibility |
| --- | --- |
| `locality-core` | Three-tree model, hydration ladder, validation, diff planning, guardrails, conflicts, journal abstractions. |
| `locality-connector` | Connector trait and data transfer types for enumerate, fetch, render, parse, apply, and reverse apply. |
| `locality-notion` | Notion API client, DTOs, block mapping, root-page projection, OAuth/API integration, Markdown/frontmatter conversion. |
| `locality-store` | SQLite schema, snapshots, journal, mount config, hydration state, tree persistence. |
| `loc-cli` | Commands: `connect`, `mount`, `status`, `pull`, `push`, `diff`, `undo`, `log`, `resolve`, `config`. |
| `localityd` | Virtual filesystem boundary, file watcher fallback, hydration engine, pull scheduler, push queue, daemon lifecycle. |

## Data-flow sketch

```text
agent/editor/grep
      |
      v
platform projection under ~/Locality/notion-main
      |
      v
localityd virtual_fs boundary, watcher fallback, and hydration engine
      |
      v
locality-core sync model <-> locality-store SQLite state
      |
      v
locality-connector trait
      |
      v
locality-notion direct API now, relay later
```

## Projection path model and safety

Locality exposes one product-level shape across platforms:

```text
Locality/
  notion-main/
  notion-my-company/
  google-docs-main/
```

The physical root is platform-specific. On macOS, File Provider assigns the
user-visible root and Locality reads it from `NSFileProviderManager`; packaged builds
and the local development bundle identify the host app as `Locality`, so new roots
should appear as `~/Library/CloudStorage/Locality`. Windows uses a Cloud Files root
and Linux uses `~/Locality` or the configured shared FUSE root. Command handling
treats the mount-point folder, such as `<root>/notion-main`, as the mount
boundary and normalizes older macOS File Provider root names such as `Locality`
and `Locality-Locality` only so existing local paths continue to resolve during
upgrades.

Every file operation resolves through this boundary:

```text
input path -> canonical mount point -> mount_id -> connection_id -> remote_id
```

Path normalization rejects traversal components and symlink escapes before a
path can resolve to a mount. A local path alone is never enough to write remote
content; push still validates the mounted entity, current connection, remote
freshness, conflict markers, and connector guardrails before apply.
