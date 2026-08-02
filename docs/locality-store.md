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
| `discovery` | Connector-neutral atomic batch-discovery commit, validation, and conservative pending-work guards. |
| `generation_delivery` | Observed backend generations, per-path merge bases, and resumable generation-apply journal contracts. |
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
- SQLite migrates v18 rows to v19 by adding durable discovery-projection
  transactions without rewriting or discarding existing mount work.
- SQLite migrates v19 rows to v20 by creating and rebuilding
  `search_documents_fts`, a connector-neutral search cache over entity metadata
  plus connector search metadata and hydrated shadow frontmatter/body.
- SQLite migrates v20 rows to v21 by adding observed generation, per-path merge
  base, terminal receipt, and resumable differential-apply journal tables. The
  additive migration does not rewrite or discard pending push journals, virtual
  mutations, shadows, or local files.
- SQLite migrates v21 rows to v22 by relating each differential-apply journal
  to its mount. Connection, remote-root, and settings changes fail closed while
  that mount has an active journal, preventing source resets from orphaning it.
- SQLite schema v25 / generation-delivery component v4 adds durable pending
  terminal-acknowledgment state without storing a private route or credential.
- SQLite schema v26 / generation-delivery component v5 binds the complete
  authenticated transport selection to each apply journal. Exact retries and
  recovery therefore preserve body-window bounds, acknowledgment state, and
  selected pin-lease policy without renegotiating an in-flight apply.
- SQLite schema v27 / workspace-binding component v2 makes absence of a binding
  the durable layout 0 representation. Because prerelease state did not persist
  the trusted workspace identity/root or a coordinator migration record, upgrade
  discards every prerelease binding and leaves every legacy mount in layout 0;
  it never edits `mounts.root` or user files. `save_mount` also leaves every new
  mount in layout 0: only an owning coordinator may plan against its explicitly
  trusted workspace root and save an accepted binding. Explicit binding inserts
  check every configured mount, including unbound layout 0 roots, so an unbound
  basename cannot later collide with a portable target.
- Workspace-binding component v4 adds stable local workspace identity plus a
  separate trusted host-root/projection record, layout sequence, and durable
  remount-recovery outcome without a schema-version bump. Opening released
  component-v2/v3 state creates or completes these tables transactionally and
  preserves all v1 mount bindings; they keep resolving through their exact
  legacy roots. New layout-1 bindings require an atomic host/mount commit whose
  derived root matches the preserved mount root.
- Generation-delivery component v6 marks whether that selection is fully bound.
  Ambiguous active v25 journals fail migration atomically; completed pre-binding
  journals permit only exact terminal replay and preserve only their recorded
  acknowledgment requirement. Prerelease v26 rows are bound only when their
  stored body-window or pin data proves the complete selection.
- SQLite records component versions for durable subsystems so compatibility is
  decided from persisted state contracts instead of desktop build IDs.
- SQLite enables WAL mode, a busy timeout, foreign keys, and `PRAGMA user_version` schema versioning.

## Durable Discovery Transactions

Discovery publication requires a `TransactionalDiscoveryCommit`; there is no
production API that directly applies a bare `DiscoveryCommit`. The daemon first
captures a mount reservation, prepares its versioned plan, and reserves a
transaction ID. Reservation is idempotent for the same immutable transaction
payload, fails closed for a mismatched retry, and permits only one active
transaction per mount.

The reservation is a canonical snapshot of the mount, mount Live Mode,
mount-scoped connector checkpoint, entities, shadows, hydration and metadata
jobs, virtual mutations, auto-save enrollments, remote observations, freshness,
and unsettled journals. Credentials, connection secrets, derived search rows,
and other discovery transactions are excluded. Reserve and commit both recapture
this state and name only the changed category when rejecting drift; errors do not
echo document contents or stored metadata.

The daemon advances transactions through `reserved`, `applying`, `projected`,
`committed`, and `finalized`, with `repair_pending` and `aborted` recovery paths.
Transitions use compare-and-swap status checks. Only `aborted` and `finalized`
are inactive, and committed transactions cannot move backward.

For plain-file mounts, the executor stores a second versioned envelope inside
the transaction plan and effects fields. It records normalized projection
components, exact create materializations, streamed path fingerprints, recovery
paths, and operation effects. Preparation verifies that the public projection
actions, normalized components, and structural commit changes describe the same
work. Every absent destination ancestor must be supplied by an explicit
directory operation; execution never creates an unjournaled mount ancestor.
The mount root and its recovery parent must also resolve to the same volume
before reservation.

On Unix, new-file writes flush the file before syncing its parent directory,
and no-replace renames and removals sync the affected parent directories. On
Windows, new files are flushed and same-volume no-replace renames request
`MOVEFILE_WRITE_THROUGH`, but there is no portable parent-directory fsync and
power-loss persistence of namespace creation or removal is not guaranteed. In
both cases the effect following a filesystem change is a separate store update,
so repair reconciles the two crash orderings from stored fingerprints. A
temporary create payload is reused only when its exact fingerprint matches;
partial or replaced payloads are preserved for review.

`run_plain_files_discovery_transaction` drives one prepared transaction.
`repair_plain_files_discovery_transaction` aborts an untouched `reserved`
transaction without re-fingerprinting its source paths, resumes or rolls back
later work from recorded effects and fingerprints, and completes committed
hydration publication and recovery cleanup.
`repair_active_plain_files_discovery_transactions` applies that repair API only
to active `plain_files` records and leaves provider-mode records untouched.
These are executor APIs, not a claim about a daemon startup call site. Ambiguous
filesystem state returns `needs_review`; repository and I/O failures propagate.
Raw projection and version fields are checked before typed decoding so future
layouts fail update-required without mutating durable state.

## Generation Delivery Transactions

Backend generation delivery is separate from direct-provider discovery. A mount
records its latest fully observed source generation, inventory digest, exact
workspace layout version/digest, terminal receipt digest, and one merge-base
record per projected path. A delta reservation must match all of those base
facts. Changed retries, newer required readers, incomplete targets, layout
changes, generation gaps, and old-identity mismatches fail before the local tree
is changed.

`generation_apply_journals` stores the owning mount, canonical delta and
terminal receipt, staging root, lifecycle, per-entry outcomes, and whether an
authenticated terminal receipt still requires acknowledgment. Source
identity/settings resets are transactionally blocked while that mount has an
active apply or an unacknowledged required receipt. The daemon replays completed
pending receipts before polling for more delivery, then stages and verifies
incoming bytes before applying clean creates/updates/deletions. Each filesystem
operation is idempotently recognizable after a crash, so recovery can record
the missing outcome and continue. Only after every entry has an applied,
deleted, or conflict outcome does one SQLite transaction advance affected mount
heads and clean path bases. Dirty local bytes stay in place and become explicit
conflicts. A write through a descriptor retained across a clean three-way merge
renames the visible merged inode to a deterministic retained name while keeping
the pre-merge inode retained too. The logical path contains only a small,
deterministic manifest naming both resolvable versions, so two individually
bounded files can never produce an inline conflict larger than the 64 MiB file
limit. Both inode hashes and byte lengths advance atomically in SQLite after
later writes, and both lengths count toward per-mount and global evidence
quotas. Recovery recognizes the visible-inode rename as a durable checkpoint,
including a crash before manifest publication, and idempotently finishes the
manifest and fences without unlinking either inode. Recovery also reconstructs
the pre-merge fence when merged bytes were published before evidence was
persisted. To resolve this conflict, the user closes writers, copies exactly one
named retained file over the manifest, and syncs again. Reconciliation fsyncs
and re-fences that exact choice, then atomically restores the path to `dirty`,
records the apply as `merged`, and marks the dual-inode evidence row resolved.
Both retained files remain as a tombstoned GC journal. Their `captured_sha256`
and `captured_byte_length` values are frozen snapshots of bytes Locality managed
at capture or resolution. Admission treats those captured lengths as a logical
managed-evidence reservation; it is not an actual-disk quota. A foreign file
descriptor opened earlier can grow either inode without Locality's involvement,
so that reachable, user-owned recovery growth is deliberately outside the
managed reservation. Apply, reconciliation, and ordinary startup never open,
fingerprint, require, or unlink resolved tombstones. They account only frozen
captured reservations plus the active update's prospective Locality-managed
preimage. Consequently, an unavailable mount containing only resolved
tombstones cannot block unrelated polling, recovery, or evidence-producing
updates, including deltas with old-identity metadata.

Cleanup is deferred to a future GC that must hold an explicit exclusive
no-active-mount lifecycle gate and must define how external-descriptor growth is
handled; no such GC runs today. Arbitrary custom replacement bytes are preserved
but intentionally do not clear the conflict. Reconciliation retains only
authenticated payloads for current live conflicts and removes successful,
superseded, or orphan payloads. Per-mount and global limits bound Locality's
captured managed-evidence reservations and current conflict payloads, not later
user-owned writes to retained inodes.

The original public `PreparedGenerationApply`, `GenerationApplyJournalRecord`,
and required `GenerationDeliveryRepository` methods remain source-compatible.
Acknowledgment-aware reservation uses the additive
`PreparedGenerationApplyV2`/`reserve_generation_apply_v2` surface. New
acknowledgment methods have safe defaults: legacy repositories report no
pending acknowledgments and reject an acknowledgment-required V2 reservation
instead of silently losing retry state.

V1 deltas are mount-scoped. Empty entry lists are valid when a complete target
generation changes no projected bytes. A logical path may occur in only one
entry, preventing order-dependent delete/create replacement. Per-file and
aggregate content limits are validated before reservation; the journal is
reserved before bounded streaming downloads begin.

Apply mutations are handle-relative beneath a no-follow root and coordinated by
an inter-process mount lock. Unix uses no-follow directory descriptors; Windows
opens and validates a non-reparse root handle and locks a stable root lock file.
Other non-Unix platforms fail closed until equivalent primitives exist.

The public daemon exposes an authenticated transport trait and a deterministic
fake, not a network route. An authenticated private endpoint adapter and the
existing `loc pull`/Live Mode call-site integration remain follow-up work. No
unauthenticated API or new routine sync command is implied by these tables.

`commit_discovery_transaction` loads the stored commit rather than accepting a
caller replacement. One SQLite transaction revalidates the reservation and
shared discovery preflight, applies entity and related state changes, advances
the connector checkpoint, and marks the transaction committed. A checkpoint or
commit-marker failure rolls back all three. The committed transaction remains
active until post-commit work explicitly finalizes it.

The in-memory implementation applies the same transaction contract to a clone
and swaps it into place only after every check succeeds. SQLite uses one transaction and
temporarily stages changing entity paths, allowing swaps and longer path cycles
without violating `UNIQUE(mount_id, path)`. Hydration jobs and auto-save
enrollments follow authoritative entity path changes without resetting failure,
origin, or scheduling state.

Discovery deletion is deliberately conservative. Entity-owned shadows,
hydration, freshness, observations, auto-save state, and FTS rows are removed,
but opaque metadata-job and virtual-mutation identifiers must be supplied
explicitly by the daemon. A move or delete that intersects unlisted pending
virtual work fails atomically. Auto-save ownership is also preflighted: a row at
an affected path may be unbound or bound to that entity, never to a different
remote ID. Explicit auto-save upserts can override an automatic rehome only for
the same owner. A bound upsert must match the remote ID and exact path in the
final entity map; unbound path-addressed enrollments remain supported.

Plan, commit, reservation, and effect payloads use recursively canonicalized
versioned JSON envelopes. The required non-rebuildable
`durable:discovery_projection` component is version 1. Newer row envelopes or
component versions fail with an update-required compatibility result.

## SQLite Schema

The first schema keeps high-value lookup fields relational and stores complex connector-neutral payloads as JSON:

- `connector_profiles`: profile id, connector, display name, auth kind, scopes, capabilities, enabled action classes, connector version, and status;
- `connections`: connection id, optional profile id, connector, account/workspace labels, auth kind, `secret_ref`, scopes, capabilities, status, and expiry metadata;
- `mounts`: mount id, connector, local root, optional remote root id, read-only
  flag, projection mode (`plain_files`, `macos_file_provider`, `linux_fuse`, or
  `windows_cloud_files`), optional connection id, and connector-specific
  `settings_json`;
- `entities`: mount id, remote id, kind, title, projected path, hydration, content hash, remote edit time;
- `entity_search_fts`: legacy derived full-text index over entity titles/paths
  and observed remote titles/paths. It is rebuildable and stores no credential
  material;
- `search_documents_fts`: derived connector-neutral full-text index over entity
  titles, projected paths, observed remote titles/paths, derived breadcrumbs, and
  hydrated shadow frontmatter/body. It also indexes connector-provided
  `loc_search.metadata_text`, `loc_search.aliases`, and `loc_search.source_url`
  from remote observation raw metadata. It is rebuildable and stores no
  credential material, but it can contain indexed user document content from
  hydrated shadows and connector metadata. SQLite returns structured indexed
  fields to callers so UI and CLI search can rank title, alias, URL, metadata,
  and body matches differently;
- `shadows`: mount id, entity id, body hash, rendered body, JSON shadow blocks;
- `journals`: push id, mount id, JSON remote ids, JSON push plan, JSON preimage snapshots, JSON apply effects, JSON status;
- `state_components`: current durable/rebuildable component versions, minimum
  reader versions, and whether unknown components must block older binaries;
- `state_migrations`: append-only migration history for state/schema upgrades;
- `connector_state`: connector-owned durable state versioned by connector and
  scope, for future connectors and connector-specific migrations;
- `discovery_projection_transactions`: immutable discovery plans, commits, and
  reservations plus durable execution effects, status, recovery error, and
  commit/finalization timestamps;
- `observed_generations`: per-mount source generation, inventory and layout
  fence, latest terminal receipt, and update time;
- `generation_paths`: per-projection local merge base, clean/dirty/conflicted
  state, and newest incoming identity;
- `generation_apply_journals` and `generation_apply_outcomes`: immutable delta
  and receipt payloads plus crash-recoverable per-entry apply results;
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

The SQLite test suite includes a v20 schema snapshot, old-DB migration fixtures,
newer-schema detection, newer-component detection, and unknown-component
compatibility checks. A PR that changes durable state should update these tests
as part of the same change.
