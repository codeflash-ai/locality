# Simulation Harness Placeholder

The randomized sync simulation should eventually drive the deterministic `locality-core` state machine with:

- local edits
- remote edits
- hydration transitions
- validation failures
- crashes before, during, and after push apply
- connector retries and rate limits

The harness should assert that content is not lost, journals replay cleanly, and convergent states are reached after failures are removed.

