# Durable Metadata Discovery Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make virtual workspace metadata discovery survive daemon restarts and refresh failures so workspace mounts eventually enumerate all reachable containers.

**Architecture:** Persist virtual child-refresh work in SQLite as metadata-discovery jobs keyed by mount and container identifier. The daemon keeps its existing bounded in-memory scheduler, but every queued child refresh is also durable; startup reloads pending jobs, success deletes them, failure records attempts and leaves the job retryable.

**Tech Stack:** Rust workspace, `locality-store` repository traits plus SQLite schema migration, `localityd` runtime child-refresh scheduler, focused Cargo tests.

---

### Task 1: Durable Store Contract

**Files:**
- Modify: `crates/locality-store/src/records.rs`
- Modify: `crates/locality-store/src/repository.rs`
- Modify: `crates/locality-store/src/memory.rs`
- Modify: `crates/locality-store/src/lib.rs`
- Test: `crates/locality-store/tests/repository.rs`
- Test: `crates/locality-store/tests/sqlite.rs`

- [ ] **Step 1: Write failing repository tests**

Add tests that upsert two metadata discovery jobs, promote a duplicate from background to interactive while preserving attempts, list jobs in priority/depth/attempt order, record a failure, delete a completed job, and verify jobs are deleted when mount source state is cleared.

Run: `cargo test -p locality-store metadata_discovery -- --nocapture`
Expected: compile failure because `MetadataDiscoveryJobRecord` and `MetadataDiscoveryJobRepository` do not exist.

- [ ] **Step 2: Add record and trait**

Add `MetadataDiscoveryPriority` and `MetadataDiscoveryJobRecord` in `records.rs`, plus a `MetadataDiscoveryJobRepository` trait with `upsert_metadata_discovery_job`, `list_metadata_discovery_jobs`, `delete_metadata_discovery_job`, and `record_metadata_discovery_job_failure`.

- [ ] **Step 3: Implement memory store**

Add an in-memory `BTreeMap<(MountId, String), MetadataDiscoveryJobRecord>` and implement the trait. `save_mount` source-identity clearing must also clear metadata discovery jobs for the mount.

- [ ] **Step 4: Implement SQLite schema v16**

Bump `SCHEMA_VERSION` to 16. Add `metadata_discovery_jobs` with `mount_id`, `container_identifier`, `priority_json`, `depth`, `attempts`, `last_error`, `created_at`, and `updated_at`; create it in the base schema and in `user_version < 16` migration. Add snapshot and migration tests.

- [ ] **Step 5: Verify store tests**

Run: `cargo test -p locality-store metadata_discovery -- --nocapture`
Expected: all metadata discovery tests pass.

### Task 2: Runtime Reload And Retry

**Files:**
- Modify: `crates/localityd/src/runtime.rs`
- Test: `crates/localityd/src/runtime.rs`

- [ ] **Step 1: Write failing runtime tests**

Add tests that `RuntimeState::new` reloads persisted metadata discovery jobs into `child_refreshes`, `queue_child_refresh` persists jobs, successful completion deletes the durable job, and failed completion increments attempts but leaves it durable.

Run: `cargo test -p localityd metadata_discovery -- --nocapture`
Expected: compile failure or assertion failure because runtime child refresh jobs are not durable yet.

- [ ] **Step 2: Map runtime priorities to store priorities**

Add conversion helpers between `ChildRefreshPriority` and `MetadataDiscoveryPriority`.

- [ ] **Step 3: Load persisted child refreshes on startup**

Replace `ChildRefreshQueue::default()` initialization with a loader that reads `metadata_discovery_jobs` and queues each job into memory.

- [ ] **Step 4: Persist queue lifecycle**

Update `queue_child_refresh` to upsert the durable job before queueing in memory. Update success handling to delete the durable job before queueing descendants. Update failure handling to record the failure and retry attempts while leaving the job pending.

- [ ] **Step 5: Verify runtime tests**

Run: `cargo test -p localityd metadata_discovery child_refresh_queue -- --nocapture`
Expected: durable queue and existing scheduler tests pass.

### Task 3: Docs And PR Verification

**Files:**
- Modify: `docs/sync-model.md`

- [ ] **Step 1: Document durable discovery behavior**

Update Stage 4/Stage 10 implementation notes to say workspace virtual mounts persist bounded child-enumeration jobs and resume them after daemon restart without hydrating page bodies.

- [ ] **Step 2: Run focused verification**

Run:
`cargo test -p locality-store metadata_discovery -- --nocapture`
`cargo test -p localityd metadata_discovery child_refresh_queue -- --nocapture`
`cargo fmt --check`

- [ ] **Step 3: Commit and create PR**

Commit only branch changes from `.worktrees/durable-metadata-discovery`, push `codex/durable-metadata-discovery`, then run `gh pr create --base main --head codex/durable-metadata-discovery`.
