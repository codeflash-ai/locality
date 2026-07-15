# macOS File Provider Native Approval Design

## Goal

Make the normal first-run desktop setup use the macOS File Provider approval
flow from the foreground Locality app. After the user accepts the system-owned
approval, onboarding should continue without asking them to open the
CloudStorage root and click Finder's `Enable` button.

The production target is a notarized Developer ID build. The design must not
use testing entitlements, private APIs, UI automation, or `pluginkit`
interventions.

## Evidence

Nextcloud registers its account domain by calling the public
`NSFileProviderManager.addDomain` API from the foreground containing app. A
fresh-install trace showed the new domain as disabled immediately after that
call, followed by interaction with a Nextcloud-owned window, and later showed
the same domain as `userEnabled = true`. The domain had no testing modes and the
app had no File Provider bypass entitlement.

Nextcloud also resolves its File Provider service through
`NSFileProviderManager.getServiceWithName` immediately after domain setup and
uses the returned `NSFileProviderService` to connect to an extension-owned XPC
listener. It does not open the CloudStorage location in Finder as its normal
approval mechanism.

Locality currently makes the same API call from the
`locality-file-providerctl` child executable. The desktop then immediately asks
that helper to resolve the CloudStorage URL. When the refetched domain remains
disabled, onboarding can only direct the user to Finder or System Settings.
That call is therefore not associated with the foreground Tauri process and
the user's setup action when macOS decides how to present approval.

The shipped extension also lacks standard app-extension build flags and bundle
metadata. On an installed build, `fileproviderctl dump` attributed the provider
to the extension itself rather than to `ai.codeflash.locality`. A controlled
probe retained the existing `host.Locality.FileProvider` identifier shape but
added current SDK metadata and app-extension compiler flags; macOS then
reported the containing bundle as the host. The bundle identifier therefore
does not need to change.

## Architecture

### Foreground Domain Registrar

Add a focused macOS-only Rust module to the Tauri application. It will use the
public FileProvider Objective-C API through the existing `objc2` ecosystem to:

- construct the `loc` `NSFileProviderDomain`;
- invoke `NSFileProviderManager.addDomain` from the Locality app process;
- resolve the Locality File Provider service and open a short-lived provider
  XPC connection after registration, matching the app-to-extension warm-up
  pattern used by Nextcloud;
- refetch registered domains and read the authoritative `userEnabled`,
  `disconnected`, and `hidden` values; and
- report API failures and bounded wait timeouts without changing the domain's
  testing modes.

The Tauri command will schedule the initial `addDomain` call on the app's main
thread. The call is made only from a user-driven desktop setup action, while the
Locality window is foreground. Completion and state polling must happen away
from the main thread so neither the native approval UI nor the webview is
blocked.

The existing Swift helper remains the maintenance boundary for CLI operations,
URL resolution, enumerator signalling, reimport, and noninteractive startup
repair. It is no longer the normal first creator of the desktop onboarding
domain.

The File Provider extension implements `NSFileProviderServicing` and
`NSFileProviderServiceSource` for a Locality-owned service name. The service
vends an anonymous XPC listener endpoint so the containing app can resolve the
provider service after registration. The current warm-up connection is used only
to exercise the standard app-extension service path; durable credentials and
mount configuration still flow through `localityd`.

### Bundle Construction

Keep the existing production identifiers:

- host: `ai.codeflash.locality`;
- extension: `ai.codeflash.locality.Locality.FileProvider`; and
- helper: `ai.codeflash.locality.Locality.file-providerctl`.

Build the extension with explicit Swift and C application-extension flags.
Populate the staged extension plist with `CFBundleSupportedPlatforms =
[MacOSX]` and truthful SDK/platform build metadata derived from `xcrun` and the
host OS. This keeps LaunchServices and File Provider aware that the appex is an
embedded macOS extension without migrating the provider identity or its
domains.

Build-script tests will require the explicit application-extension compiler
flags. Developer ID release verification will reject missing platform metadata,
mismatched host/extension identifier prefixes, invalid nested signatures, and the
`com.apple.developer.fileprovider.testing-mode` entitlement.

## Desktop Data Flow

1. The user starts mount creation from Locality onboarding.
2. Before the blocking mount worker runs, the Tauri host schedules the native
   domain registration call on its main thread.
3. After `addDomain` completes, the app best-effort resolves the Locality File
   Provider service on the main thread and opens a short-lived provider
   connection. A failure here is logged but does not block activation, because
   older installed extensions or a still-disabled domain may not vend the
   service yet.
4. A worker refetches domain state at a bounded interval for up to 30 seconds.
5. If macOS reports `userEnabled = true`, the existing mount workflow resolves
   the provider URL and continues creating durable mount state.
6. If approval is still pending after the bounded wait, onboarding returns
   `approval_required`. `Allow in macOS` repeats the foreground host call and
   bounded state check without revealing the CloudStorage root in Finder.
7. If the domain is enabled but the CloudStorage root is not ready, onboarding
   returns `waiting_for_cloudstorage_root` and keeps the existing `Check again`
   path.
8. Finder and System Settings instructions remain recovery guidance after a
   denial, dismissal, or persistent disabled state. They are not the normal
   first-install path.

The blocking mount function keeps its existing helper registration as an
idempotent fallback. In the desktop flow the host registration has already
created the domain, so the helper observes the matching domain and does not
become its creator.

## Existing And Exceptional States

- An already enabled domain proceeds without delay after a confirming refetch.
- Re-adding an existing domain may update its display and hidden properties but
  cannot override an explicit user denial. On an explicit `Allow in macOS`
  retry, Locality removes a disabled domain before re-registering so macOS can
  present a fresh approval opportunity.
- Startup repair remains noninteractive. It may restore a missing registration,
  but it must not try to manufacture approval without a foreground user action.
- CLI-driven registration continues to use the helper and may require manual
  approval because there is no foreground desktop interaction to own native UI.
- Existing bundle identifiers, domain identifiers, SQLite state, and
  CloudStorage paths remain unchanged.

## Error Handling

- A native File Provider API error becomes a normal failed onboarding report
  with the macOS error description retained for diagnostics.
- Failure to schedule work on the Tauri main thread is reported separately from
  a File Provider framework failure.
- A completion callback or state query that exceeds its internal timeout fails
  cleanly instead of hanging the desktop command.
- A completed registration whose authoritative state remains disabled is
  recoverable `approval_required`, not a successful mount and not a generic
  crash.
- A user-enabled domain whose visible root is delayed remains
  `waiting_for_cloudstorage_root`.

## Testing

Use test-first coverage for the new orchestration and packaging behavior:

- a fake registrar proves desktop onboarding invokes foreground registration
  before the blocking mount attempt;
- an already enabled domain proceeds immediately;
- a domain that becomes enabled during polling proceeds;
- a domain that remains disabled reaches `approval_required` after the bounded
  wait;
- native registration and main-thread scheduling errors reach `failed`;
- an enabled domain with a delayed root reaches
  `waiting_for_cloudstorage_root`;
- `Allow in macOS` retries foreground registration rather than merely opening
  Finder;
- provider service warm-up runs after registration before polling and a warm-up
  failure does not block activation;
- the extension advertises the Locality File Provider service and can vend an
  XPC listener endpoint;
- non-macOS behavior and existing path validation remain unchanged; and
- build/release checks validate extension platform metadata, application-
  extension compilation, identifier containment, signatures, and absence of
  the testing entitlement.

Verification for the signed artifact must include the installed app, not only
unit tests:

1. build and notarize a Developer ID artifact using the existing Keychain
   notary profile;
2. install the app and verify nested signatures and Gatekeeper assessment;
3. confirm `fileproviderctl dump` reports `ai.codeflash.locality` as the
   extension's containing bundle;
4. on a clean File Provider identity or macOS user, start onboarding from the
   foreground app, accept the system approval, and verify the refetched domain
   becomes user-enabled; and
5. verify the Locality CloudStorage root and mount appear without using Finder's
   `Enable` button.

Any installed-flow step that cannot be exercised in the available environment
must be reported as unverified rather than inferred from unit or packaging
tests.

## Scope

This change affects macOS desktop onboarding and macOS bundle construction. It
does not change projection contents, daemon IPC, connector behavior, mount
schema, updater signing, other platforms, or the Nextcloud source tree.
