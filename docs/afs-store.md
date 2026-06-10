# `afs-store` Design

`afs-store` is the durable state boundary under the daemon and CLI. It should persist facts, not decide sync semantics. The sync decisions stay in `afs-core`.

## Design Rules

- Repository errors stay structured so agent-facing commands can produce stable JSON.
- Paths are lookup keys only; remote IDs remain the canonical entity identity.
- Shadow snapshots must round-trip exactly enough for `afs-core` diff planning.
- Journal APIs exist before remote apply code, because every future push path must be journal-first.
- SQLite is the production target, but an in-memory implementation comes first to prove the repository contract.

## Modules

| Module | Role |
| --- | --- |
| `records` | Durable mount, entity, shadow snapshot, and shadow block record shapes. |
| `repository` | Split repository traits for mounts, entities, shadows, and journals. |
| `memory` | Deterministic in-memory implementation for tests and early orchestration. |
| `sqlite` | SQLite-backed durable implementation of the repository traits. |
| `error` | Store-specific structured errors and conversion to `afs-core` errors. |

## First Contract Implemented

- Mount configs can be saved and listed.
- Entity records can be looked up by remote ID or projected path.
- Duplicate projected paths inside one mount are rejected.
- Shadow documents persist through an explicit record shape and load back into `ShadowDocument`.
- Missing shadows return `StoreError::ShadowMissing`.
- Journal append/status/list operations are present in memory and also satisfy `afs_core::journal::JournalStore`.
- SQLite opens a `state.sqlite3` database under the configured state root and initializes the schema idempotently.
- SQLite persists mounts, entities, shadows, and journals across reopen.

## SQLite Schema

The first schema keeps high-value lookup fields relational and stores complex connector-neutral payloads as JSON:

- `mounts`: mount id, connector, root path, read-only flag;
- `entities`: mount id, remote id, kind, title, projected path, hydration, content hash, remote edit time;
- `shadows`: mount id, entity id, body hash, rendered body, JSON shadow blocks;
- `journals`: push id, mount id, JSON remote ids, JSON push plan, JSON status.

Shadow blocks and journal plans are JSON by design for now. They round-trip through typed Rust records, and the schema can normalize them later if query patterns justify it.
