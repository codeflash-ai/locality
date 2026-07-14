# File Provider Native Start Syncing Approval Design

## Goal

Make macOS onboarding match the native File Provider approval flow used by
Dropbox and Google Drive. When macOS shows the native "Start Syncing" prompt and
the user clicks OK, Locality should treat that as the File Provider location
being enabled. Onboarding must not ask the user to click a separate Finder
"Enable" control after that.

## Problem

Locality currently describes File Provider approval as "Enable Locality in
Finder" and falls back to Finder/System Settings guidance for a disabled File
Provider domain. That copy implies a second enablement step after macOS's native
prompt. For desktop apps such as Dropbox and Google Drive, accepting the native
prompt is the action that enables the File Provider location. Locality should
follow that platform convention.

The mismatch creates an onboarding dead end: after the user clicks OK in the
native macOS prompt, Locality can still show guidance to enable Locality
elsewhere even though the next correct action is simply to verify that macOS has
materialized the CloudStorage root.

## Design

Use the macOS native File Provider prompt as the approval surface.

When setup registers the Locality File Provider domain and receives a
recoverable "registered but not enabled" result, onboarding enters an
`approval_required` state whose copy tells the user to click OK in the macOS
"Start Syncing" prompt. The primary action remains `Allow in macOS`, but its job
is to trigger or foreground the native approval path, not to send users looking
for a Finder Enable control.

After the user takes the `Allow in macOS` action, the backend should immediately
try the workspace mount setup again. If macOS has accepted approval and the
CloudStorage root exists, setup completes. If approval has been accepted but the
CloudStorage root has not appeared yet, onboarding moves to
`waiting_for_cloudstorage_root` with `Check again`. If the domain is still
disabled because the user clicked "Don't allow" or dismissed the native prompt,
onboarding stays in `approval_required` and can offer `Allow in macOS` again.

Finder/System Settings guidance remains only a recovery fallback for cases where
the native prompt cannot be surfaced or the File Provider domain remains
disabled after retry. It should not be presented as the normal next step after
the native prompt.

## Data Flow

1. Onboarding starts mount creation.
2. Desktop backend registers the macOS File Provider domain through the existing
   helper, which uses `NSFileProviderManager.add`.
3. macOS may show the native "Start Syncing" prompt.
4. If the helper reports the domain is registered but not user-enabled,
   onboarding returns `approval_required`.
5. User clicks `Allow in macOS`.
6. Backend triggers or foregrounds the native approval surface and reruns mount
   creation.
7. Outcomes:
   - user clicked OK and CloudStorage is ready: mount creation completes;
   - user clicked OK but CloudStorage is delayed: `waiting_for_cloudstorage_root`;
   - user clicked "Don't allow" or did not approve: `approval_required`;
   - non-File-Provider setup failure: `failed`.

## Components

- `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift`
  should stop telling users that a disabled domain requires a separate Finder or
  System Settings enablement step.
- `apps/desktop/src-tauri/src/main.rs` should classify and report disabled
  domains using native prompt language, and the `allow_in_macos` action should
  retry setup after surfacing approval instead of always returning a manual
  check-again state.
- `apps/desktop/src/onboarding-mount.ts` should update labels, headlines,
  progress copy, instructions, and supplementary notes to reflect the native
  "Start Syncing" prompt.
- Existing path validation and File Provider root safety behavior stays
  unchanged.

## Error Handling

- If `NSFileProviderManager.add` or domain lookup fails because the File Provider
  app or extension is unavailable, onboarding stays in `failed` with the
  existing development/install guidance.
- If the user denies or dismisses the native prompt, `userEnabled` remains false;
  onboarding stays recoverable in `approval_required`.
- If `userEnabled` is true but `getUserVisibleURL` or the CloudStorage root is
  not ready, onboarding uses `waiting_for_cloudstorage_root` and `Check again`.
- Path validation, read-only ordinary directory handling, and provider-root
  containment errors remain unchanged and continue to use path-validation
  messaging.

## Testing

Add focused tests for:

- frontend copy no longer mentioning a Finder/File Provider Enable step in the
  normal approval instructions;
- frontend approval progress and headline copy referencing the native macOS
  prompt;
- backend curated approval message referencing the native "Start Syncing"
  prompt;
- backend `allow_in_macos` behavior retrying setup after approval surfacing,
  using an injectable/testable runner so it does not call real macOS APIs;
- disabled-domain helper/error copy no longer implying a second Finder Enable
  step;
- existing `waiting_for_cloudstorage_root`, generic failure, and path validation
  behavior continuing to work.

## Scope

This change does not alter File Provider identity, projection layout, durable
state schema, mount path validation, daemon sync, push/pull behavior, or
recovery reset logic. It only changes onboarding state transitions and
user-facing guidance around the macOS File Provider approval prompt.
