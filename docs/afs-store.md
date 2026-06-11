# `afs-store` Design

`afs-store` is the durable state boundary under the daemon and CLI. It should persist facts, not decide sync semantics. The sync decisions stay in `afs-core`.

## Design Rules

- Repository errors stay structured so agent-facing commands can produce stable JSON.
- Paths are lookup keys only; remote IDs remain the canonical entity identity.
- Shadow snapshots must round-trip exactly enough for `afs-core` diff planning.
- Journal APIs are the durable spine for remote apply code; every push path must be journal-first.
- SQLite is the production target, but an in-memory implementation comes first to prove the repository contract.

## Modules

| Module | Role |
| --- | --- |
| `records` | Durable connector profile, connection, mount, entity, shadow snapshot, and shadow block record shapes. |
| `repository` | Split repository traits for connector profiles, connections, mounts, entities, shadows, hydration jobs, and journals. |
| `memory` | Deterministic in-memory implementation for tests and early orchestration. |
| `sqlite` | SQLite-backed durable implementation of the repository traits. |
| `error` | Store-specific structured errors and conversion to `afs-core` errors. |

## First Contract Implemented

- Mount configs can be saved and listed.
- Connector profiles persist auth-config metadata separately from connected accounts.
- Connections persist connected-account metadata and `secret_ref` values without storing bearer tokens.
- Entity records can be looked up by remote ID or projected path.
- Duplicate projected paths inside one mount are rejected.
- Shadow documents persist through an explicit record shape and load back into `ShadowDocument`.
- Missing shadows return `StoreError::ShadowMissing`.
- Journal append/status/list operations are present in memory and also satisfy `afs_core::journal::JournalStore`.
- SQLite opens a `state.sqlite3` database under the configured state root and initializes the schema idempotently.
- SQLite persists connector profiles, connections, mounts, entities, shadows, hydration jobs, and journals across reopen.
- SQLite migrates v1 journal rows to v2 by adding empty preimage snapshots.
- SQLite migrates v2 journal rows to v3 by adding empty apply-effect lists.
- SQLite migrates v3 mount rows to v4 by adding optional remote root IDs.
- SQLite migrates v8 connection rows to v9 by adding `profile_id` and seeding the built-in `notion-token-default` profile.
- SQLite enables WAL mode, a busy timeout, foreign keys, and `PRAGMA user_version` schema versioning.

## SQLite Schema

The first schema keeps high-value lookup fields relational and stores complex connector-neutral payloads as JSON:

- `connector_profiles`: profile id, connector, display name, auth kind, scopes, capabilities, enabled action classes, connector version, and status;
- `connections`: connection id, optional profile id, connector, account/workspace labels, auth kind, `secret_ref`, scopes, capabilities, status, and expiry metadata;
- `mounts`: mount id, connector, root path, optional remote root id, optional connection id, read-only flag, and projection mode (`plain_files`, `macos_file_provider`, or `linux_fuse`);
- `entities`: mount id, remote id, kind, title, projected path, hydration, content hash, remote edit time;
- `shadows`: mount id, entity id, body hash, rendered body, JSON shadow blocks;
- `journals`: push id, mount id, JSON remote ids, JSON push plan, JSON preimage snapshots, JSON apply effects, JSON status.

Shadow blocks, journal plans, journal preimages, and journal apply effects are JSON by design for now. They round-trip through typed Rust records with stable snake-case serde names, and the schema can normalize them later if query patterns justify it.
