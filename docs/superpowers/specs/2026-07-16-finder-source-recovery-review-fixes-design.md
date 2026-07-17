# Finder Source Recovery Review Fixes Design

## Goal

Make later-source File Provider recovery deterministic across dialog close,
automatic mount retry, and React effect cleanup, while ensuring the macOS prompt
test installer honors an explicitly selected DMG.

## Recovery Ownership

`MountsView` owns File Provider recovery. Dialog visibility is presentation
state and does not control the lifetime of a pending recovery operation.

When the user closes the Add Source dialog during recovery, Locality hides the
dialog but retains the pending mount request, File Provider readiness report,
and `creating` setup state. The readiness poll continues in the background, as
promised by the existing “Setup continues if you close this window” copy. If the
dialog is reopened while recovery is pending, it shows the same guided recovery
state. Recovery ends only in a terminal mount result or when `MountsView`
unmounts.

## Automatic Mount Retry

When File Provider readiness reaches `ready`, Locality performs the existing
single delayed mount retry.

- A successful mount clears the pending retry and readiness report, then shows
  the mount success message.
- A mount that still reports the disabled-provider condition re-enters the
  readiness flow through the existing recovery entry point.
- Any other failed mount clears the pending retry and readiness report, changes
  the source dialog to `error`, and displays the mount report message in the
  dialog. This returns source actions to an enabled state instead of leaving the
  dialog indefinitely busy.

The implementation will keep this result handling in one focused helper or
branch so success, repeated provider disablement, and terminal errors remain
mutually exclusive.

## Poller Cancellation

Each active poller run has a generation identifier. `stop()` invalidates the
active generation in addition to clearing its timer. After `probe()` settles,
the poller verifies that the captured generation is still active before calling
`onReport`, calling `onReady`, changing failure backoff, or scheduling work for
that result.

This generation check handles both cleanup without restart and a stop/start
sequence in which an older probe resolves after a newer run has begun. A newer
run may wait for the already in-flight probe to settle, but it never receives
the old run’s report or ready callback.

## Installer Source Precedence

The prompt-test installer resolves sources in this order:

1. Explicit `--source-app` or `LOCALITY_PROMPT_TEST_SOURCE_APP`.
2. Explicit `--dmg` or `LOCALITY_PROMPT_TEST_DMG`.
3. The normal built Tauri app bundle.
4. The newest default `Locality_*.dmg`.

An explicit DMG is therefore mounted even when
`target/release/bundle/macos/Locality.app` exists. Existing path validation,
dry-run behavior, target safety checks, and source-app priority remain
unchanged.

## Testing

Follow red-green TDD for each behavior:

- Add deferred-probe tests proving `stop()` drops a late non-ready report and a
  late ready report, including stop/start generation invalidation.
- Add source recovery contract coverage proving close preserves the pending
  flow and a non-provider mount failure becomes a visible terminal dialog
  error. Use extracted behavior where practical; any source-level UI contract
  assertion must match the complete relevant handler rather than isolated
  substrings.
- Add an installer dry-run test using an isolated temporary repository layout
  containing both a built app directory and an explicit fake DMG. Assert that
  the DMG is attached and the built app is not selected.
- Run the focused Vitest and shell tests after each fix, then the complete
  desktop frontend test suite, installer helper suite, TypeScript production
  build, and formatting or lint checks relevant to the changed files.

## Scope

This change does not alter onboarding recovery, add retry limits, change File
Provider readiness states, redesign the Add Source dialog, or modify installer
signing and registration behavior. It only corrects the four reviewed lifecycle
and source-selection regressions.
