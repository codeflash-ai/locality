# Simulation Harness Test Plan

## Goal

Build reliability coverage for Locality sync behavior at two levels:

- deterministic simulation tests that can run quickly and replay failures by seed;
- live Notion e2e equivalents that exercise the real mounted-file workflow against
  disposable scratch content.

The reliability bar is Dropbox-like behavior: local edits, remote drift,
interrupted pushes, restarts, validation failures, conflict recovery, and
directory moves must converge without losing content or silently mutating the
wrong remote object.

## Current Status

Implemented on `feature/simulation-harness`:

- deterministic `locality-core` simulation harness;
- daemon-level simulation wrapper tests;
- ignored nightly simulation profile and GitHub Actions workflow;
- Make targets for fast, nightly, and live Notion reliability runs;
- live Notion e2e equivalents for simulation invariants;
- live Notion stress loops for repeated push/reopen and conflict recovery;
- live Notion create-then-move page workflow;
- docs describing coverage and replay commands.

Draft PR:

- https://github.com/codeflash-ai/locality/pull/40

## Implemented Deterministic Tests

`crates/locality-core/tests/simulation_harness.rs`

- `simulation_smoke_replays_seeded_sequence_to_convergence`
  - Runs a seeded randomized local/remote sync sequence.
  - Verifies final convergence and invariant preservation.
- `simulation_replays_interrupted_pushes_without_losing_content`
  - Exercises interrupted push phases and retry behavior.
  - Verifies journal status transitions and no content loss.
- `simulation_nightly_profile_runs_many_seeded_sequences`
  - Ignored heavy profile for broad seeded coverage.
  - Intended for nightly or explicit local stress runs.

`crates/localityd/tests/simulation.rs`

- `simulation_smoke_exercises_core_reliability_harness`
  - Runs the core harness through the daemon-facing test surface.
- `simulation_nightly_reliability_profile`
  - Ignored heavy daemon wrapper profile.

## Implemented Live Notion E2E Tests

`crates/loc-cli/tests/e2e_push_workflow.rs`

- `live_seeded_reliability_sequence_push_drift_conflict_converges_notion`
  - Pushes a local edit to a live scratch page.
  - Verifies remote drift blocks overwrite and records a reverted journal.
  - Materializes a dirty-pull conflict, resolves it, pushes the resolution, and
    verifies the reconciled journal state.
- `live_multi_seed_reliability_sequences_converge_notion`
  - Replays the live reliability sequence across multiple deterministic labels.
- `live_stress_repeated_push_reopen_status_noop_converges_notion`
  - Repeats local edit, dirty status, push, reconciled journal, SQLite reopen,
    clean status, remote render verification, no-op pull, and clean status.
- `live_stress_repeated_drift_conflict_recovery_converges_notion`
  - Repeats local dirty edit plus remote drift.
  - Verifies blocked push, reverted journal, conflict materialization, manual
    resolution, reconciled push, SQLite reopen, and clean status.
- `live_page_directory_create_then_move_pushes_under_final_parent`
  - Creates a draft page directory under one mounted Notion parent.
  - Moves the draft directory under another mounted parent before push.
  - Verifies the created remote Notion page is parented by the final target
    parent only, and local status is clean after reconcile.
- `live_validation_failure_blocks_before_journal_and_remote_write`
  - Corrupts Locality frontmatter.
  - Verifies validation stops before journal creation or remote mutation.
- `live_sqlite_restart_preserves_reconciled_journal_and_clean_status`
  - Pushes to Notion, reopens SQLite state, and verifies journal/status/remote
    content are still correct.
- `live_remote_fast_forward_updates_clean_file_and_preserves_pending_file`
  - Verifies clean files can fast-forward from remote while pending local edits
    are protected.

## Commands

Fast deterministic smoke:

```sh
make test-simulation
```

Nightly deterministic stress:

```sh
LOCALITY_SIMULATION_SEEDS=128 make test-simulation-nightly
```

Live Notion reliability gate:

```sh
export LOCALITY_NOTION_LIVE_PARENT_PAGE=...
export NOTION_TOKEN=...
make test-simulation-live-notion
```

## Verified So Far

These checks have passed on the branch:

- `make test-simulation`
- `cargo fmt --all -- --check`
- `make test-simulation-live-notion` with live Notion scratch credentials
- exact live run for
  `live_page_directory_create_then_move_pushes_under_final_parent`

Expected red-phase failures observed during TDD:

- missing helper compile failures before implementation;
- an ignored live test run without required Notion env vars.

No final verification failures remain.

## Remaining Recommended Coverage

These are still worth adding for higher confidence:

- longer live Notion soak profile with more iterations and randomized page-tree
  operations;
- live Notion multi-page concurrent edit scenarios, especially two siblings
  edited and pushed in mixed order;
- live Notion remote deletion or archive racing against local dirty edits;
- durable restart checks in the middle of a pending conflict, before resolution;
- macOS File Provider equivalent of create-then-move page workflow;
- daemon Live Mode e2e where the background loop observes the same
  create-then-move workflow without a manual `loc push`.

## Failure Replay Expectations

Simulation failures should print enough detail to replay:

- seed;
- step count;
- action name;
- operation trace;
- final invariant violation.

When a simulation seed exposes a failure, promote it into a focused regression
test in either `crates/locality-core/tests/simulation_harness.rs` or
`crates/localityd/tests/simulation.rs`.
