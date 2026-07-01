# Simulation Harness

The randomized sync simulation drives the deterministic `locality-core`
reliability model with seeded local edits, remote edits, hydration transitions,
validation failures, push crashes, connector retries, rate limits, and recovery
passes.

The harness asserts the reliability invariants from `docs/reliability.md`:

- accepted local content is never lost;
- remote writes do not happen before journal preparation and concurrency checks;
- failed, reverted, and reconciled journals remain explicit and replayable;
- dirty or conflicted local content is not overwritten by pull/hydration;
- once transient failures stop, remote, local, and synced state converge.

## Fast Smoke

Run the deterministic PR-sized smoke profile:

```sh
make test-simulation
```

Equivalent direct commands:

```sh
cargo test -p locality-core --test simulation_harness
cargo test -p localityd --test simulation -- --test-threads=1
```

## Heavy Reliability Profile

Run the ignored nightly profile locally:

```sh
LOCALITY_SIMULATION_SEEDS=128 make test-simulation-nightly
```

The nightly GitHub Actions workflow runs the same ignored `localityd` simulation
target with `LOCALITY_SIMULATION_SEEDS=128`.

## Live Notion Equivalent

Run the live reliability sequence against disposable Notion scratch pages:

```sh
export LOCALITY_NOTION_LIVE_PARENT_PAGE=...
export NOTION_TOKEN=...
make test-simulation-live-notion
```

The live tests create scratch pages under the configured parent and cover:

- single-seed local push, remote drift blocking, dirty-pull conflict, resolution,
  and convergence;
- two independent seeded live sequences;
- repeated live push/status/no-op-pull cycles across SQLite state reopen;
- repeated live remote-drift conflicts, blocked pushes, conflict materialization,
  manual resolution, and SQLite state reopen;
- local child page creation followed by a directory move before push, verifying
  the created Notion page lands under the final parent;
- validation failure before push journal or remote write;
- SQLite-backed state reopen after a reconciled push;
- remote fast-forward of clean virtual content while preserving pending local
  content.

Scratch content is archived during cleanup.

## Replaying Failures

Simulation failures print the seed, step, action name, and operation trace. To
promote a found failure into a regression, add a deterministic test in
`crates/locality-core/tests/simulation_harness.rs` or
`crates/localityd/tests/simulation.rs` using the reported seed and a step count
at least as large as the failing step.
