# macOS File Provider Approval Recovery Design

## Problem

During macOS onboarding, Locality currently treats File Provider approval as a
mount error inside the "Creating your local folder" step. This is functional
but weak as a product flow:

- onboarding does not model File Provider approval as a first-class blocked
  state;
- the frontend relies on string-classified backend errors;
- users who dismiss or skip approval are not guided through a deterministic
  recovery path before the final ready screen; and
- the flow does not clearly distinguish "provider disabled" from "approved, but
  CloudStorage has not materialized yet."

Dropbox's setup flow shows the target pattern: present a branded pre-permission
step, then let macOS show the system approval dialog. Locality should optimize
for a more robust recovery path instead of a one-shot first-run prompt.

## Goal

On macOS, onboarding step 4 should become a blocked File Provider approval gate
that appears right before the final ready step. If Locality cannot verify that
the File Provider is enabled and usable, onboarding remains on step 4 until the
user completes approval and Locality verifies the mount end-to-end.

## Non-Goals

- Do not bypass macOS File Provider approval.
- Do not switch the desktop product flow to plain files.
- Do not add a second onboarding branch for macOS.
- Do not advance to the final ready screen while approval or CloudStorage
  materialization is still pending.
- Do not keep parsing File Provider onboarding state from free-form error
  strings in the UI.

## UX Flow

Step 4 remains the "Local folder" stage, but becomes a small state machine
instead of a spinner plus inline error.

States:

- `creating`: Locality is creating the mount and registering the macOS File
  Provider domain.
- `approval_required`: the provider is registered but `userEnabled == false`.
  Onboarding is blocked and the primary action is `Allow in macOS`.
- `waiting_for_cloudstorage_root`: macOS approval appears complete, but the
  Locality CloudStorage root or user-visible domain URL is not ready yet. The
  primary action is `Check again`.
- `verifying`: Locality is rerunning backend verification after the user returns
  from macOS.
- `failed`: unrelated mount/setup failure. The primary action is `Retry setup`.
- `created`: verification succeeded and onboarding may advance to step 5.

Behavior:

- Step 4 auto-starts mount creation once Notion is connected.
- If approval is needed, step 4 changes into a blocked approval screen instead
  of surfacing a generic mount error.
- The blocked screen attempts to open the macOS approval surface directly when
  possible. If Locality cannot do that reliably, it shows explicit Finder/System
  Settings instructions and a `Retry`/`Check again` action.
- If the user approved but macOS has not yet created
  `~/Library/CloudStorage/Locality`, the blocked step remains visible with a
  specific waiting message instead of advancing or collapsing into a generic
  retry loop.
- If the app is closed or reloaded while step 4 is blocked, onboarding should
  restore into the same blocked approval state instead of forgetting that
  approval is still pending.
- Onboarding must not advance to step 5 until Locality verifies the provider,
  domain URL, CloudStorage root, and mount root successfully.

## Backend Contract

Replace the onboarding use of generic `ActionReport` mount errors with a
structured macOS onboarding result. To avoid breaking existing non-onboarding
callers of `create_workspace_mount`, prefer a macOS-specific onboarding wrapper
command or helper that can return richer state while reusing the existing mount
creation path internally.

Suggested shape:

- `state`: `created | approval_required | waiting_for_cloudstorage_root |
  verifying | failed`
- `message`: user-facing summary
- `primaryAction`: `allow_in_macos | check_again | retry_setup`
- `launchStrategy`: `open_system_settings | open_finder | instructions_only |
  none`

The backend remains authoritative for classification. It should map existing
provider checks into explicit onboarding states:

- provider registered but disabled -> `approval_required`
- provider enabled but user-visible URL or CloudStorage root unavailable ->
  `waiting_for_cloudstorage_root`
- mount root health verification failed -> `failed`
- success -> `created`

The same backend path should power both the initial automatic mount attempt and
subsequent `Check again` retries.

## macOS Approval Launch Strategy

The backend should attempt the most direct approval launch supported by macOS:

1. Open the most specific System Settings approval surface Locality can target
   reliably.
2. If a direct System Settings jump is not available, open Finder at the
   relevant Locality File Provider location so the user can approve there.
3. If neither launch path is available, return `instructions_only` and let the
   frontend show explicit guidance plus `Check again`.

The backend should not claim success merely because it launched a surface. It
must still require a follow-up verification pass.

## Frontend Changes

The onboarding screen in `apps/desktop/src/App.tsx` should treat step 4 as a
typed state machine rather than an error-special-cased progress card.

Required changes:

- Replace `file-provider-disabled` string classification in
  `onboarding-errors.ts` with a typed onboarding result.
- Keep onboarding on step 4 until the backend returns `created`.
- Change the primary action label by state:
  - `Allow in macOS`
  - `Check again`
  - `Retry setup`
- Render state-specific copy for:
  - approval needed;
  - approval accepted but Locality folder not visible yet; and
  - generic setup failure.
- Preserve the current auto-create behavior, but do not auto-retry after a
  blocked approval state. Recovery remains user-driven.

## Diagnostics

Add explicit desktop log events for approval recovery:

- approval launch attempted
- approval launch fallback used
- provider still disabled
- waiting for CloudStorage root
- verification succeeded
- verification failed

These should replace ambiguous onboarding failures with events that explain
whether the blocker was user approval, Finder/System Settings launch fallback,
CloudStorage materialization delay, or mount-root verification.

## Testing

Frontend:

- step 4 auto-starts mount creation after connection is ready;
- `approval_required` keeps onboarding on step 4;
- `waiting_for_cloudstorage_root` keeps onboarding on step 4 with `Check again`;
- step 5 is not reachable until `created`;
- no automatic retry occurs after a blocked approval state.

Backend:

- disabled provider returns `approval_required`;
- enabled provider with missing user-visible domain URL or CloudStorage root
  returns `waiting_for_cloudstorage_root`;
- successful re-check after approval returns `created`;
- mount-root health errors return `failed`;
- unrelated mount failures remain generic failures.

Manual verification:

- dismiss macOS approval on first run and verify step 4 blocks with approval
  recovery UI;
- approve Locality and verify `Check again` advances only after the Locality
  CloudStorage root exists;
- verify a slow materialization path remains on step 4 with specific waiting
  copy rather than jumping to ready.

## Compatibility

No SQLite schema change is required. This is a desktop onboarding and command
contract change over existing macOS File Provider registration and verification
behavior.
