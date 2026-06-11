# Architecture

AgentFS v1 is a local-first Rust system with six implementation surfaces:

1. `afs` CLI: stable command and exit-code surface for humans and agents.
2. `afsd` daemon: one per user, supervising many mounts.
3. Sync core: connector-agnostic correctness layer.
4. Connector SDK: trait boundary for source-specific APIs.
5. Notion connector: first first-party connector.
6. State store: SQLite-backed source of truth under `~/.afs/`.

The optional cloud relay is deliberately not implemented in v1. The local remote-truth boundary should still remain swappable so a future relay can replace direct source polling without changing the sync model.

## Connector and Auth Model

AgentFS keeps connector capability, authentication policy, account credentials,
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
| `afs-core` | Three-tree model, hydration ladder, validation, diff planning, guardrails, conflicts, journal abstractions. |
| `afs-connector` | Connector trait and data transfer types for enumerate, fetch, render, parse, apply, and reverse apply. |
| `afs-notion` | Notion API client, DTOs, block mapping, root-page projection, OAuth/API integration, Markdown/frontmatter conversion. |
| `afs-store` | SQLite schema, snapshots, journal, mount config, hydration state, tree persistence. |
| `afs-cli` | Commands: `connect`, `mount`, `status`, `pull`, `push`, `diff`, `undo`, `log`, `resolve`, `config`. |
| `afsd` | File watcher, hydration engine, pull scheduler, push queue, daemon lifecycle. |

## Data-flow sketch

```text
agent/editor/grep
      |
      v
real files under ~/afs/<mount>
      |
      v
afsd watcher and hydration engine
      |
      v
afs-core sync model <-> afs-store SQLite state
      |
      v
afs-connector trait
      |
      v
afs-notion direct API now, relay later
```
