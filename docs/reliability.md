# Reliability Strategy

Correctness belongs primarily in `afs-core` and `afs-store`.

## Invariants

- Local, remote, and synced tree state are explicit.
- Local file mutations use temp-write-plus-rename.
- Pushes are journaled before remote mutation.
- Push journals include shadow preimages for reverse-plan derivation.
- Push journals record operation-level apply effects, including created remote IDs.
- Remote apply operations are idempotent.
- Remote concurrency is checked after journaling and immediately before apply.
- Apply and reconcile failures mark the journal failed instead of leaving an ambiguous success.
- Dangerous plans require explicit confirmation.
- Conflicts preserve local content inside inline markers and cannot push until markers are removed.
- Unknown or unsupported remote blocks round-trip through shadow state.

## Test layers

- Round-trip tests for render/parse idempotence.
- Property tests for hydration state transitions and push-plan invariants.
- Randomized simulation of local edits, remote edits, crashes, retries, and network failures.
- Canary tests against a scratch Notion workspace before release.
