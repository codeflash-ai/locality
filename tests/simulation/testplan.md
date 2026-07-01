# Simulation Harness Reliability Test Plan

  ## Summary

  Build a model-based randomized simulation harness that complements the existing scenario e2e tests rather than replacing
  them. Existing tests already prove individual workflows across e2e_push_workflow, architecture_behavior,
  projection_contract, daemon runtime tests, and live Notion e2e; the new harness should prove those guarantees survive long
  interleavings of local edits, remote edits, hydration changes, validation failures, retries, rate limits, and crash/
  restart points.

  Primary target: nightly heavy reliability runs, per your preference. Add a small deterministic PR smoke test for fast
  feedback, but optimize the design around long seeded runs with replayable failures.

  ## Key Changes

  - Add a new simulation test target under tests/simulation or crates/localityd/tests/simulation.rs that drives the daemon/
    core product path using fake connectors and durable stores.

  - Use a deterministic scenario runner with:
      - seeded random generation,
      - full operation trace logging,
      - replay by seed plus step count,
      - explicit invariant checks after every step,
      - final convergence checks after failures are disabled.

  - Use the existing correctness boundaries:
      - locality-core for sync classification, push planning, guardrails, journals, undo, and hydration state legality.
      - locality-store SQLite/InMemory stores for durable state and replay behavior.
      - localityd daemon push/pull/hydration paths for product-like orchestration.
      - Existing fake connector patterns from local e2e tests for controlled remote behavior.

  - Prefer adding proptest as a dev-dependency for shrinking generated operation sequences. If dependency churn is rejected
    during implementation, use an internal deterministic PRNG and require seed replay, but keep the same scenario model.

  ## Harness Model

  - Model state must track three explicit trees:
      - remote: connector-owned pages, blocks, versions, archived state, and injected remote failures.
      - local: mounted/cache Markdown content, dirty/conflicted markers, virtual mutations, and visible projection state
        where relevant.

      - synced: Locality shadows, entity records, hydration state, journals, and pending durable work.

  - Supported generated actions:
      - local body edits, frontmatter edits, creates, renames, deletes, restore, diff, push, pull, inspect;
      - remote body edits, title/version changes, creates, archives, moves;
      - hydration transitions: virtual, stub, hydrated, dirty, conflicted;
      - scheduler/freshness ticks and explicit file-open hydration;
      - validation failures: bad frontmatter, unsupported directive edits, schema/property errors, unresolved conflict
        markers;

      - fault injection: crash before journal append, after prepared journal, during apply, after partial apply effects,
        during reconcile, during hydration, and during scheduled pull;

      - connector failures: retryable network error, rate limit, stale remote version, unsupported operation, missing
        credential.

  - Every scenario must end with a recovery phase:
      - disable transient failures,
      - drain pending hydration/push/pull work,
      - retry safe operations,
      - assert convergence or an explicit review-needed/conflict state.

  ## Reliability Invariants

  - No content loss: every accepted local edit is either present remotely after convergence, preserved locally as dirty/
    conflicted content, or explicitly rejected before mutation with a structured validation/guardrail result.

  - Remote safety: remote writes never occur before required validation, confirmation, concurrency checks, and journal
    preparation.

  - Journal correctness: prepared/applying/applied/failed/reconciled/reverted states are durable, replayable, and never
    leave ambiguous success after crash/restart.

  - Convergence: when failures stop and conflicts are resolved or absent, local, remote, and synced shadows converge to the
    same canonical content and clean status.

  - Conflict safety: dirty local content is never overwritten by pull, hydration, scheduled refresh, or virtual projection
    reads.

  - Idempotency: replaying interrupted pushes, retries, hydration jobs, and scheduled pulls does not duplicate blocks, lose
    IDs, grow queues unboundedly, or create duplicate journals.

  - Bounded work: scheduler and hydration queues respect dedupe, priority, and rate-limit budgets across long runs.
  - Explainability: non-converged endings must be one of the expected states: validation blocked, dangerous-plan
    confirmation required, remote drift review needed, unresolved conflict, missing credential, or read-only blocked.

  ## Test Matrix

  - Core model tests: randomized three-tree classification, hydration transition legality, guardrail decisions, directive
    validation, push pipeline actions, and journaled executor failure points.

  - Daemon/store simulation: multi-step scenarios through fake connectors and SQLite state roots, including process-restart
    simulation by reopening the store between steps.

  - Projection scenarios: run selected simulations across mounted plain files plus virtual projection modes below OS
    adapters: macOS File Provider, Linux FUSE, and Windows Cloud Files shared semantics.

  - Crash matrix: deterministic tests for each push stage:
      - before journal,
      - after prepared,
      - after applying,
      - after one or more apply effects,
      - after remote mutation before reconcile,
      - during reconcile,
      - after failed journal.

  - Long-run nightly: hundreds to thousands of generated sequences, with larger trees, repeated scheduler ticks, mixed
    local/remote edits, connector retries, rate limits, and restart points.

  - Regression corpus: when a random failure is found, commit the seed and reduced action trace as a named deterministic
    regression test.

  ## Commands And CI

  - Add fast PR smoke:
      - cargo test -p locality-core simulation_smoke
      - cargo test -p localityd --test simulation simulation_smoke -- --test-threads=1

  - Add nightly/manual heavy workflow:
      - LOCALITY_SIMULATION_PROFILE=nightly cargo test -p localityd --test simulation -- --ignored --test-threads=1

  - Add local exhaustive command:
      - LOCALITY_SIMULATION_PROFILE=soak LOCALITY_SIMULATION_SEEDS=1000 cargo test -p localityd --test simulation --
        --ignored --test-threads=1

  - Update docs:
      - replace tests/simulation/README.md placeholder with how to run, how to replay seeds, invariant definitions, and how
        to promote failures into regressions.

      - update docs/e2e-behavior-coverage.md to mark randomized reliability simulation as local/nightly coverage, not live
        Notion coverage.

  ## Assumptions

  - The harness should not call live Notion; live scratch e2e remains the connector canary layer.
  - Nightly reliability coverage is the primary goal; PR coverage should be intentionally small and deterministic.
  - Production APIs should stay stable unless a small testability hook is unavoidable.
  - Simulation failures must always print seed, profile, step index, action trace, and final model/store summaries so
    another engineer can reproduce the failure directly.
