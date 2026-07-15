# `locality-store` Design

`locality-store` is the durable state boundary under the daemon and CLI. It should persist facts, not decide sync semantics. The sync decisions stay in `locality-core`.

## Design Rules

- Repository errors stay structured so agent-facing commands can produce stable JSON.
- Paths are lookup keys only; remote IDs remain the canonical entity identity.
- Shadow snapshots must round-trip exactly enough for `locality-core` diff planning.
- Journal APIs are the durable spine for remote apply code; every push path must be journal-first.
- SQLite is the production target, but an in-memory implementation comes first to prove the repository contract.

## Modules

| Module | Role |
| --- | --- |
| `records` | Durable connector profile, connection, mount, entity, shadow snapshot, and shadow block record shapes. |
| `repository` | Split repository traits for connector profiles, connections, mounts, entities, shadows, hydration jobs, and journals. |
| `memory` | Deterministic in-memory implementation for tests and early orchestration. |
| `sqlite` | SQLite-backed durable implementation of the repository traits. |
| `error` | Store-specific structured errors and conversion to `locality-core` errors. |

## First Contract Implemented

- Mount configs can be saved and listed.
- Connector profiles persist auth-config metadata separately from connected accounts.
- Connections persist connected-account metadata and `secret_ref` values without storing bearer tokens.
- Entity records can be looked up by remote ID or projected path.
- Entity metadata search can use a derived SQLite FTS index before falling back
  to repository scans in tests and non-SQLite stores.
- Duplicate projected paths inside one mount are rejected.
- Shadow documents persist through an explicit record shape and load back into `ShadowDocument`.
- Missing shadows return `StoreError::ShadowMissing`.
- Journal append/status/list operations are present in memory and also satisfy `locality_core::journal::JournalStore`.
- SQLite opens a `state.sqlite3` database under the configured state root and initializes the schema idempotently.
- SQLite persists connector profiles, connections, mounts, entities, shadows, hydration jobs, and journals across reopen.
- SQLite migrates v1 journal rows to v2 by adding empty preimage snapshots.
- SQLite migrates v2 journal rows to v3 by adding empty apply-effect lists.
- SQLite migrates v3 mount rows to v4 by adding optional remote root IDs.
- SQLite migrates v8 connection rows to v9 by adding `profile_id` and seeding the built-in `notion-token-default` profile with connector capability flags.
- SQLite migrates v11 rows to v12 by creating and rebuilding
  `entity_search_fts` from entity and remote-observation metadata.
- SQLite migrates v12 rows to v13 by adding state compatibility metadata,
  migration history, connector state, and projection state tables.
- SQLite migrates pre-shared-root `linux_fuse` and `windows_cloud_files`
  projection layout state to mount-point roots under the shared projection root.
- SQLite migrates v17 rows to v18 by adding `mounts.settings_json`, a generic
  mount-scoped JSON settings field used by connector-specific mount options.
- SQLite records component versions for durable subsystems so compatibility is
  decided from persisted state contracts instead of desktop build IDs.
- SQLite enables WAL mode, a busy timeout, foreign keys, and `PRAGMA user_version` schema versioning.

## SQLite Schema

The first schema keeps high-value lookup fields relational and stores complex connector-neutral payloads as JSON:

- `connector_profiles`: profile id, connector, display name, auth kind, scopes, capabilities, enabled action classes, connector version, and status;
- `connections`: connection id, optional profile id, connector, account/workspace labels, auth kind, `secret_ref`, scopes, capabilities, status, and expiry metadata;
- `mounts`: mount id, connector, local root, optional remote root id, read-only
  flag, projection mode (`plain_files`, `macos_file_provider`, `linux_fuse`, or
  `windows_cloud_files`), optional connection id, and connector-specific
  `settings_json`;
- `entities`: mount id, remote id, kind, title, projected path, hydration, content hash, remote edit time;
- `entity_search_fts`: derived full-text index over entity titles/paths and
  observed remote titles/paths. It is rebuildable and stores no secrets;
- `shadows`: mount id, entity id, body hash, rendered body, JSON shadow blocks;
- `journals`: push id, mount id, JSON remote ids, JSON push plan, JSON preimage snapshots, JSON apply effects, JSON status;
- `state_components`: current durable/rebuildable component versions, minimum
  reader versions, and whether unknown components must block older binaries;
- `state_migrations`: append-only migration history for state/schema upgrades;
- `connector_state`: connector-owned durable state versioned by connector and
  scope, for future connectors and connector-specific migrations;
- `projection_state`: projection-owned state such as File Provider/FUSE layout
  versions and repair generations.

Shadow blocks, journal plans, journal preimages, and journal apply effects are JSON by design for now. They round-trip through typed Rust records with stable snake-case serde names, and the schema can normalize them later if query patterns justify it.

## Compatibility Rules

- Bump `PRAGMA user_version` when SQLite DDL changes, and add a forward migration.
- Bump a `state_components` version when the stored meaning of JSON, paths,
  shadows, journals, virtual mutations, auth bindings, connector state, or
  projection state changes without a table-shape change.
- Mark rebuildable components as `required = 0` and `rebuildable = 1`; stale
  rebuildable state should be repaired or regenerated instead of forcing a
  reset.
- If a new writer produces state that older readers must not open, raise that
  component's `min_reader_version` so old binaries return `NeedsUpdate`.
- `durable:journals` version 3 adds whole-entity body operations and complete
  entity reverse payloads. Its v2-to-v3 migration updates component metadata
  only, leaves `PRAGMA user_version` and existing journal JSON rows unchanged,
  and raises the minimum reader version to 3.
- Unknown required components block older binaries. Unknown non-required
  rebuildable components are ignored by older binaries.

The SQLite test suite includes a v18 schema snapshot, old-DB migration fixtures,
newer-schema detection, newer-component detection, and unknown-component
compatibility checks. A PR that changes durable state should update these tests
as part of the same change.
