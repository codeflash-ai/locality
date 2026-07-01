# Simulation Harness Reliability Test Plan

## Summary

The simulation harness is intended to make Locality sync reliability measurable
under long, replayable interleavings rather than only isolated scenario tests.
It complements the existing e2e suite by modeling local state, remote state,
synced shadows, hydration state, validation failures, journal transitions,
push fault points, and final recovery/convergence.

Primary target: extreme reliability. Fast deterministic smoke tests should run
in PRs, while ignored nightly/live profiles should stress longer sequences and
real Notion behavior.

## Implemented Coverage

### Deterministic Core Simulation

Implemented in `crates/locality-core/tests/simulation_harness.rs`:

- `simulation_smoke_replays_seeded_sequence_to_convergence`
  - Replays a seeded scenario through local edits, remote edits, hydration,
    validation blocking, push attempts, retries, and final convergence.
- `simulation_replays_interrupted_pushes_without_losing_content`
  - Exercises interrupted push points and verifies accepted local content is not
    lost.
- `simulation_nightly_profile_runs_many_seeded_sequences`
  - Ignored heavy profile for many seeded sequences.

The model in `crates/locality-core/src/simulation_harness.rs` tracks:

- local body, remote body, and synced body;
- local dirty/conflicted state;
- hydration state;
- validation blocking;
- journal terminal states;
- injected push faults before apply, after mutation, and during reconcile;
- final recovery after transient failures are disabled.

### Daemon-Level Simulation

Implemented in `crates/localityd/tests/simulation.rs`:

- `simulation_smoke_exercises_core_reliability_harness`
  - Daemon test target wrapper for the core reliability harness.
- `simulation_nightly_reliability_profile`
  - Ignored nightly profile for heavier seeded runs.

### Live Notion E2E Equivalents

Implemented in `crates/loc-cli/tests/e2e_push_workflow.rs`:

- `live_seeded_reliability_sequence_push_drift_conflict_converges_notion`
  - Creates disposable scratch pages in real Notion.
  - Verifies local push convergence.
  - Verifies remote drift blocks overwrite before apply.
  - Materializes dirty-pull conflicts, resolves them, pushes the resolution, and
    checks journal terminal states.
- `live_multi_seed_reliability_sequences_converge_notion`
  - Runs independent live seeded reliability sequences.
- `live_stress_repeated_push_reopen_status_noop_converges_notion`
  - Repeated live edit, push, journal reconciliation, SQLite reopen, clean
    status, no-op pull, and remote-render verification.
- `live_stress_repeated_drift_conflict_recovery_converges_notion`
  - Repeated local-dirty plus remote-drift cycles.
  - Verifies blocked push, reverted journal, conflict materialization,
    resolution push, reconciled journal, SQLite reopen, and clean status before
    the next cycle.
- `live_page_directory_create_then_move_pushes_under_final_parent`
  - Creates a new page directory under one mounted Notion parent, moves it to a
    different mounted parent before push, and verifies the remote Notion page is
    created under the final parent only.
- `live_validation_failure_blocks_before_journal_and_remote_write`
  - Invalid Locality frontmatter stops before journal creation or remote write.
- `live_sqlite_restart_preserves_reconciled_journal_and_clean_status`
  - Pushes to live Notion, reopens SQLite state, and verifies journal/status and
    remote content remain correct.
- `live_remote_fast_forward_updates_clean_file_and_preserves_pending_file`
  - Verifies clean local content can fast-forward from remote while pending
    local content is not overwritten.

## Reliability Invariants

The implemented and planned tests should enforce these invariants:

- No content loss: accepted local edits are either present remotely after
  convergence, preserved locally as dirty/conflicted content, or explicitly
  rejected before mutation.
- Remote safety: remote writes do not happen before validation, confirmation,
  concurrency checks, and journal preparation.
- Journal correctness: failed, reverted, and reconciled terminal states remain
  durable, explicit, and replayable.
- Convergence: when transient failures stop and conflicts are resolved or absent,
  local, remote, and synced state converge to clean canonical content.
- Conflict safety: dirty local content is never overwritten by pull, hydration,
  scheduled refresh, or virtual projection reads.
- Idempotency: interrupted pushes, retries, hydration jobs, and scheduled pulls
  do not duplicate blocks, lose IDs, grow queues without bound, or create
  duplicate journals.
- Explainability: non-converged endings must be expected review states such as
  validation blocked, confirmation required, remote drift review needed,
  unresolved conflict, missing credential, or read-only blocked.

## Commands

Fast deterministic simulation smoke:

```sh
make test-simulation
```

Equivalent direct commands:

```sh
cargo test -p locality-core --test simulation_harness
cargo test -p localityd --test simulation -- --test-threads=1
```

Ignored nightly profile:

```sh
LOCALITY_SIMULATION_SEEDS=128 make test-simulation-nightly
```

Live Notion reliability equivalent:

```sh
export LOCALITY_NOTION_LIVE_PARENT_PAGE=...
export NOTION_TOKEN=...
make test-simulation-live-notion
```

## CI

Implemented CI wiring:

- `.github/workflows/simulation-nightly.yml`
  - Runs the ignored `localityd` simulation profile with
    `LOCALITY_SIMULATION_SEEDS=128`.

Implemented Make targets:

- `test-simulation`
- `test-simulation-nightly`
- `test-simulation-live-notion`

## Replay And Regression Workflow

Simulation failures should print:

- seed;
- profile;
- step index;
- action name;
- operation trace.

When a random or nightly failure is found:

1. Replay with the reported seed and at least the failing step count.
2. Reduce the trace if practical.
3. Add a named deterministic regression test in
   `crates/locality-core/tests/simulation_harness.rs` or
   `crates/localityd/tests/simulation.rs`.
4. Keep the original seed in the test name or failure message so the incident is
   reproducible.

## Remaining Recommended Coverage

- Broaden the simulation model beyond body strings into structured block trees,
  child pages, databases, and media-bearing blocks.
- Add generated create, rename, move, delete, restore, and undo actions to the
  local simulation model.
- Add durable SQLite reopen at more simulated fault points, not only selected
  live e2e paths.
- Add scheduler/freshness queue simulation with dedupe, priority, and budget
  assertions.
- Add OS adapter stress equivalents for macOS File Provider, Linux FUSE, and
  Windows Cloud Files semantics.
- Expand connector fault injection to include retryable network errors, rate
  limits, stale remote versions, unsupported operations, and missing credentials.
- Promote every discovered nightly failure into a committed deterministic
  regression seed.

## Assumptions

- Fast PR coverage remains intentionally small and deterministic.
- Nightly reliability coverage should be heavy and replayable.
- Live Notion scratch tests remain the connector canary layer and should clean
  up disposable remote content.
- Production APIs should stay stable unless a small testability hook is needed
  to make a reliability invariant observable.
