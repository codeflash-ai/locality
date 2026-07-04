# macOS In-App Approval Spike Design

## Problem

Locality's current macOS onboarding treats File Provider approval as a blocked
state that can only recover by sending the user to Finder or System Settings.
That is functional, but it does not answer the product question raised by the
Dropbox-style flow: can Locality surface Apple's extension-approval UI from
inside the app and let the existing onboarding recovery path finish from there?

Today the desktop app can:

- register the shared `loc` File Provider domain;
- detect whether `userEnabled == false`;
- keep onboarding blocked on step 4 until approval succeeds; and
- retry verification through the existing `Check again` path.

Today the desktop app does not:

- present any in-app macOS extension browser UI; or
- know whether enabling the Locality File Provider from such a UI is sufficient
  to move the shared domain from `approval_required` to a usable state.

## Goal

Build the smallest possible macOS-only spike that proves or disproves this
hypothesis:

> If Locality launches Apple's extension browser from inside the app, the user
> can enable the Locality File Provider there, and the existing `Check again`
> onboarding path will then advance without a Finder-specific `Enable` click.

## Non-Goals

- Do not bypass macOS approval or attempt to programmatically set
  `userEnabled = true`.
- Do not replace the current Finder/System Settings fallback.
- Do not redesign onboarding copy, button hierarchy, or step sequencing.
- Do not add production-ready polish, telemetry, or release behavior.
- Do not add a second macOS helper binary if the existing helper can host the
  spike.

## Decision

Reuse the existing bundled macOS helper `locality-file-providerctl` instead of
adding a second helper executable.

Why this is the smallest spike:

- it is already bundled into `Locality.app`;
- it is already macOS-only;
- it is already linked as an AppKit executable in the dev bundle;
- the desktop app already knows how to locate and launch it; and
- removing the spike later becomes a small diff instead of a packaging change.

## Proposed Approach

### 1. Extend `locality-file-providerctl` with a GUI spike action

Add a new helper action named `extension-browser-spike`.

Behavior:

- Import `ExtensionKit` and `AppKit` in
  `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift`.
- Add a new `Command` case for `extension-browser-spike`.
- When invoked, create a minimal native window whose
  `contentViewController` is `EXAppExtensionBrowserViewController`.
- Activate the app, bring the window to the foreground, and keep the helper
  alive until the window closes.
- Exit `0` when the window closes normally.
- Return a clear helper error if the browser cannot be created or shown.

This is intentionally a native window, not a Tauri-embedded surface. The spike
only needs to answer whether Apple's browser can manage the Locality File
Provider extension from an app-led flow.

### 2. Add a desktop command that launches the helper action

Add a macOS-only Tauri command in `apps/desktop/src-tauri/src/main.rs` named
`open_macos_extension_browser_spike`.

Behavior:

- Resolve `locality-file-providerctl` using the existing bundled-helper lookup.
- Launch `locality-file-providerctl extension-browser-spike`.
- Return a simple `ActionReport` to the frontend.
- Treat this as a side-effect-only command. It does not mutate onboarding state
  directly.

The backend continues to use the existing `run_workspace_mount_onboarding`
command for state transitions. The spike command only opens the browser.

### 3. Add a dev-only trigger in the blocked onboarding UI

Expose the spike from onboarding step 4 only while approval is blocked.

Behavior:

- Add a dev-only secondary action in `apps/desktop/src/App.tsx` when:
  - platform is macOS; and
  - `mountOnboarding?.state === "approval_required"`.
- Label it `Open Approval Window (Spike)`.
- Clicking it calls `open_macos_extension_browser_spike`.
- After the window closes, the user manually clicks the existing
  `Check again` button.

Keep the spike out of normal release UX by gating it behind development builds
only. The spike is for local feasibility testing, not for product exposure.

### 4. Keep the current onboarding recovery model unchanged

This spike must not change:

- the onboarding state machine;
- the meaning of `Allow in macOS`;
- the `Check again` verification path; or
- Finder/System Settings fallback behavior.

That constraint keeps the result interpretable:

- if the spike works, the only new capability is in-app presentation of the
  approval browser; and
- if the spike fails, the current onboarding flow remains the baseline.

## Expected User Flow

1. The user reaches onboarding step 4 and Locality reports
   `approval_required`.
2. The user clicks `Open Approval Window (Spike)`.
3. Locality opens a native macOS extension browser window from inside the app
   flow.
4. The user enables the Locality File Provider in that window.
5. The user closes the window and returns to Locality.
6. The user clicks `Check again`.
7. Locality reruns the existing mount verification flow.

Expected outcomes:

- success path: onboarding moves to `waiting_for_cloudstorage_root` or
  `created`;
- partial success: `userEnabled` flips to `true`, but CloudStorage is still
  materializing;
- failure path: onboarding stays at `approval_required`.

## Success Criteria

The spike is successful if all of the following are true during manual
verification:

- Locality can launch Apple's extension browser from an app-led action.
- The Locality File Provider extension appears in that browser.
- Enabling the extension there is allowed by macOS.
- After that enable action, Locality's existing `Check again` path no longer
  reports `approval_required`.

## Failure Criteria

The spike disproves the Dropbox-style direction if any of the following happen:

- the extension browser cannot be opened from Locality;
- the Locality File Provider extension does not appear in the browser;
- the extension appears but cannot be enabled there; or
- enabling it there does not change the onboarding outcome from
  `approval_required`.

## Files In Scope

- Modify:
  - `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift`
  - `apps/desktop/src-tauri/src/main.rs`
  - `apps/desktop/src/App.tsx`
  - `apps/desktop/src/onboarding-mount.ts`
  - `apps/desktop/src/onboarding-mount.test.ts`

No packaging changes are expected because `locality-file-providerctl` is already
bundled into the macOS app.

## Error Handling

The spike should fail loudly and locally:

- if the helper cannot create the extension browser, return a helper error with
  a message that explicitly names the spike action;
- if the Tauri command cannot launch the helper, show a normal desktop error
  toast or inline onboarding error;
- do not reinterpret helper-launch failure as approval success; and
- do not auto-run `Check again` after the browser closes.

## Testing

Automated coverage should stay narrow:

- frontend unit tests for the dev-only spike action visibility and label in
  `approval_required`;
- frontend unit tests that the spike action stays hidden for all other
  onboarding states;
- Rust tests for the macOS-only command wrapper returning a launch failure when
  the helper is missing.

Manual verification is the primary proof for this spike because the core
question is about a system-owned macOS UI surface.

## Manual Verification

1. Install or run a local macOS build that includes the Locality File Provider
   helper.
2. Go through onboarding until step 4 blocks on `approval_required`.
3. Click `Open Approval Window (Spike)`.
4. Confirm that a native macOS extension browser window appears.
5. Confirm that the Locality File Provider extension is listed in that window.
6. Enable the Locality File Provider there.
7. Close the spike window and return to Locality.
8. Click `Check again`.
9. Confirm that onboarding advances to either
   `waiting_for_cloudstorage_root` or `created`.
10. If onboarding remains blocked on `approval_required`, record that as a
    failed spike even if the window launched successfully.

## Risks

- `EXAppExtensionBrowserViewController` may not list File Provider extensions in
  the way this spike expects.
- The browser may list the extension but not affect File Provider domain
  approval for Locality's shared `loc` domain.
- Running the browser from `locality-file-providerctl` may not satisfy the host
  assumptions Apple expects, even though the helper lives inside the app bundle.

## Follow-Up If The Spike Works

If the spike succeeds, the next design should replace the Finder-first approval
handoff with a typed `open_extension_browser` launch strategy and promote the
current dev-only spike action into the real `Allow in macOS` path.

## Follow-Up If The Spike Fails

If the spike fails, Locality should keep the current Finder/System Settings
recovery flow and treat Dropbox's UI as a product pattern that does not map
cleanly onto Locality's File Provider setup.
