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
- Search and navigation indexes are derived caches. Losing or rebuilding an
  index must never lose local edits, shadows, journals, credentials, or pending
  virtual mutations.
- Virtual filesystem reads must never overwrite dirty or conflicted content.
  Hydration can prepare missing content, but local user edits remain the higher
  trust source until the user restores or pushes.
- Workspace access changes must preserve local pending changes. The app can
  refresh clean cached projection state, but it must keep dirty/conflicted files
  and explain what still needs review.

## Large Workspace Safety

For 1,000+ page workspaces, reliability depends on separating durable sync state
from derived convenience layers:

- SQLite entity/shadow/journal tables remain the source of truth.
- Local search indexes, recent-activity feeds, and navigation shortcuts are
  disposable and rebuildable.
- Background indexing must be cancellable and resumable so it cannot block push,
  restore, conflict recovery, or explicit file hydration.
- Search results should surface state labels such as online-only, ready, pending
  changes, and conflict, so users do not accidentally hand an agent an unsafe
  target without context.
- Index rebuilds should verify counts and last indexed timestamps per mount, but
  should not require network access unless a normal pull would already do so.

## Test layers

- Round-trip tests for render/parse idempotence.
- Property tests for hydration state transitions and push-plan invariants.
- Randomized simulation of local edits, remote edits, crashes, retries, and network failures.
- Canary tests against a scratch Notion workspace before release.
