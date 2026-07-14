# macOS File Provider Native Approval Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a notarized Developer ID build request File Provider approval from the foreground Locality desktop app and continue mount creation after macOS reports the existing `loc` domain as user-enabled.

**Architecture:** Add a macOS-only Rust boundary around the public FileProvider Objective-C API, schedule `addDomain` on Tauri's main thread, and perform bounded state polling on a worker. Keep the Swift helper as the idempotent maintenance and URL-resolution path, preserve all existing identifiers and durable state, and correct the embedded extension's app-extension compilation flags and bundle metadata.

**Tech Stack:** Rust 2024, Tauri 2, `objc2`/`objc2-file-provider`, macOS FileProvider and Foundation frameworks, Swift 6, Bash, Vitest, Cargo, Developer ID signing, and Apple notarization.

---

## File Map

- Create `apps/desktop/src-tauri/src/macos_file_provider.rs`: isolate all unsafe Objective-C FileProvider calls, main-thread registration scheduling, callback timeouts, state refetching, and bounded polling.
- Modify `apps/desktop/src-tauri/src/main.rs`: invoke foreground registration from explicit desktop actions, map the native result into existing onboarding reports, and remove the Finder-first approval launcher.
- Modify `apps/desktop/src-tauri/Cargo.toml`: add target-specific Objective-C dependencies so non-macOS builds do not acquire Apple framework dependencies.
- Modify `Cargo.lock`: lock the verified `objc2-file-provider` crate and its feature graph.
- Modify `platform/macos/LocalityFileProvider/Package.swift` and create `DomainReplacementPolicy.swift` plus its Swift Testing suite: keep transient user-visible URL failures from removing an approved matching domain.
- Modify `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift`: route existing-domain replacement through the tested conservative policy.
- Modify `apps/desktop/src/onboarding-mount.ts`: replace Finder-first progress and recovery copy with native approval copy.
- Modify `apps/desktop/src/onboarding-mount.test.ts`: pin the approval labels, native prompt recovery path, and System Settings fallback.
- Modify `apps/desktop/src/App.tsx`: show macOS-owned approval progress while the native call is pending.
- Modify `docs/desktop-app.md`: document foreground native approval as the normal path and Finder/System Settings as recovery.
- Modify `platform/macos/LocalityFileProvider/App/LocalityFileProvider.Info.plist`: declare `MacOSX` as the supported platform.
- Modify `platform/macos/LocalityFileProvider/scripts/build-dev-bundle.sh`: add app-extension compiler flags, support an isolated test build root, and stage truthful SDK/platform metadata.
- Create `platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh`: build an isolated ad-hoc bundle and assert metadata, containment, entitlements, and nested signatures.
- Modify `scripts/publish-macos.sh`: reject malformed File Provider containment/metadata and any testing-mode entitlement before notarization.
- Modify `tests/macos_publish_config.sh`: prove the release guards reject malformed metadata, unrelated containment, unreadable entitlements, and testing mode.

### Task 1: Add The Native File Provider Boundary

**Files:**
- Create: `apps/desktop/src-tauri/src/macos_file_provider.rs`
- Modify: `apps/desktop/src-tauri/src/main.rs:1-80`
- Modify: `apps/desktop/src-tauri/Cargo.toml`
- Modify: `Cargo.lock`

- [ ] **Step 1: Declare the macOS-only module and add failing state-machine tests**

Add this declaration with the other module declarations in `apps/desktop/src-tauri/src/main.rs`:

```rust
#[cfg(target_os = "macos")]
mod macos_file_provider;
```

Create `apps/desktop/src-tauri/src/macos_file_provider.rs` with the public result types and tests first. The tests deliberately reference `register_domain_and_wait_with` before its implementation exists:

```rust
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DomainActivation {
    Enabled,
    ApprovalRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DomainState {
    user_enabled: bool,
    disconnected: bool,
    hidden: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::sync::mpsc::SyncSender;

    fn successful_add(sender: SyncSender<Result<(), String>>) -> Result<(), String> {
        sender.send(Ok(())).expect("send add completion");
        Ok(())
    }

    #[test]
    fn enabled_domain_finishes_without_sleeping() {
        let clock = Cell::new(Duration::ZERO);
        let sleeps = RefCell::new(Vec::new());
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(3),
            Duration::from_millis(1),
            |_| Ok(Some(DomainState {
                user_enabled: true,
                disconnected: false,
                hidden: false,
            })),
            |duration| {
                sleeps.borrow_mut().push(duration);
                clock.set(clock.get() + duration);
            },
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::Enabled));
        assert!(sleeps.into_inner().is_empty());
    }

    #[test]
    fn polling_waits_for_domain_to_become_enabled() {
        let clock = Cell::new(Duration::ZERO);
        let states = RefCell::new(VecDeque::from([
            None,
            Some(DomainState {
                user_enabled: false,
                disconnected: false,
                hidden: false,
            }),
            Some(DomainState {
                user_enabled: true,
                disconnected: false,
                hidden: false,
            }),
        ]));
        let sleeps = RefCell::new(Vec::new());

        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(3),
            Duration::from_millis(1),
            |_| Ok(states.borrow_mut().pop_front().expect("poll state")),
            |duration| {
                sleeps.borrow_mut().push(duration);
                clock.set(clock.get() + duration);
            },
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::Enabled));
        assert_eq!(sleeps.into_inner(), vec![Duration::from_millis(1); 2]);
    }

    #[test]
    fn disabled_domain_reaches_approval_required_at_the_bound() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(2),
            Duration::from_millis(1),
            |_| Ok(Some(DomainState {
                user_enabled: false,
                disconnected: false,
                hidden: false,
            })),
            |duration| clock.set(clock.get() + duration),
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::ApprovalRequired));
    }

    #[test]
    fn scheduling_failure_is_distinct_from_framework_failure() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            |_| Err("main thread unavailable".to_string()),
            Duration::from_millis(10),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| Ok(None),
            |_| {},
            || clock.get(),
        );

        assert_eq!(
            result,
            Err("Could not schedule File Provider registration on the main thread: main thread unavailable".to_string())
        );
    }

    #[test]
    fn framework_failure_is_returned_without_polling() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            |sender| {
                sender
                    .send(add_completion_result(
                        "NSFileProviderErrorDomain",
                        -1,
                        "rejected",
                    ))
                    .expect("send add failure");
                Ok(())
            },
            Duration::from_millis(10),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| panic!("framework failure must not poll"),
            |_| {},
            || clock.get(),
        );

        assert_eq!(
            result,
            Err("NSFileProviderErrorDomain (-1): rejected".to_string())
        );
    }

    #[test]
    fn registration_callback_timeout_is_bounded() {
        let clock = Cell::new(Duration::ZERO);
        let pending_sender = RefCell::new(None);
        let result = register_domain_and_wait_with(
            |sender| {
                pending_sender.replace(Some(sender));
                Ok(())
            },
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| panic!("timed out registration must not poll"),
            |_| {},
            || clock.get(),
        );

        assert!(
            result
                .expect_err("registration must time out")
                .starts_with("File Provider registration callback timed out:")
        );
    }

    #[test]
    fn state_query_failure_is_returned() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| Err("domain query failed".to_string()),
            |_| {},
            || clock.get(),
        );

        assert_eq!(result, Err("domain query failed".to_string()));
    }
}
```

- [ ] **Step 2: Run the focused test and confirm the expected red state**

Run:

```bash
cargo test -p locality-desktop macos_file_provider::tests -- --nocapture
```

Expected: compilation fails because `register_domain_and_wait_with` and `add_completion_result` are not defined. A framework-link error or unrelated pre-existing test failure is not the expected red state and must be investigated before continuing.

- [ ] **Step 3: Add the verified target-specific dependencies**

Append this section to `apps/desktop/src-tauri/Cargo.toml`:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
block2 = "0.6.2"
objc2 = "0.6.4"
objc2-file-provider = { version = "0.3.2", default-features = false, features = [
  "std",
  "Extension",
  "NSFileProviderDomain",
  "block2",
] }
objc2-foundation = { version = "0.3.2", default-features = false, features = [
  "std",
  "FoundationErrors",
  "NSArray",
  "NSError",
  "NSEnumerator",
  "NSString",
] }
```

Run `cargo check -p locality-desktop` once so Cargo updates `Cargo.lock`. Keep the resolved crate at the registry-verified `objc2-file-provider 0.3.2`; do not add a git dependency. `FoundationErrors` supplies the typed `NSFileWriteFileExistsError` constant and `NSEnumerator` enables `NSArray::iter()`.

- [ ] **Step 4: Add failing compatibility tests for native domain construction and add errors**

Append these tests to `macos_file_provider.rs` after the dependencies are available:

```rust
#[test]
fn configured_domain_preserves_non_syncing_trash_semantics() {
    objc2::rc::autoreleasepool(|_| {
        let domain = unsafe { new_domain("loc", "") };
        assert!(!unsafe { domain.supportsSyncingTrash() });
    });
}

#[test]
fn cocoa_file_exists_error_is_an_idempotent_add() {
    assert_eq!(
        add_completion_result(
            "NSCocoaErrorDomain",
            objc2_foundation::NSFileWriteFileExistsError,
            "A file with the same name already exists.",
        ),
        Ok(())
    );
}
```

Run:

```bash
cargo test -p locality-desktop macos_file_provider::tests -- --nocapture
```

Expected: compilation still fails, now also naming the missing `new_domain` and `add_completion_result` functions. These tests preserve the Swift helper's existing trash and idempotency semantics when the foreground app becomes the first domain creator.

- [ ] **Step 5: Implement the bounded worker logic**

Replace the initial `Duration` import with these imports, then add the constants and generic implementation above the tests in `macos_file_provider.rs`:

```rust
use std::sync::mpsc::{self, SyncSender};
use std::time::Duration;

const ADD_CALLBACK_TIMEOUT: Duration = Duration::from_secs(15);
const DOMAIN_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const DOMAIN_POLL_TIMEOUT: Duration = Duration::from_secs(30);
const DOMAIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn deliver_callback<T>(sender: &SyncSender<T>, value: T) {
    let _ = sender.send(value);
}

fn register_domain_and_wait_with<Schedule, Query, Sleep, Now>(
    schedule_add: Schedule,
    callback_timeout: Duration,
    poll_timeout: Duration,
    poll_interval: Duration,
    mut query: Query,
    mut sleep: Sleep,
    mut now: Now,
) -> Result<DomainActivation, String>
where
    Schedule: FnOnce(SyncSender<Result<(), String>>) -> Result<(), String>,
    Query: FnMut(Duration) -> Result<Option<DomainState>, String>,
    Sleep: FnMut(Duration),
    Now: FnMut() -> Duration,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    schedule_add(sender).map_err(|error| {
        format!("Could not schedule File Provider registration on the main thread: {error}")
    })?;
    receiver
        .recv_timeout(callback_timeout)
        .map_err(|error| format!("File Provider registration callback timed out: {error}"))??;

    let poll_started = now();
    loop {
        let elapsed = now().saturating_sub(poll_started);
        if elapsed >= poll_timeout {
            return Ok(DomainActivation::ApprovalRequired);
        }
        let remaining = poll_timeout - elapsed;
        if let Some(state) = query(remaining)? {
            let (user_enabled, _disconnected, _hidden) =
                (state.user_enabled, state.disconnected, state.hidden);
            if user_enabled {
                return Ok(DomainActivation::Enabled);
            }
        }

        let elapsed = now().saturating_sub(poll_started);
        if elapsed >= poll_timeout {
            return Ok(DomainActivation::ApprovalRequired);
        }
        sleep(poll_interval.min(poll_timeout - elapsed));
    }
}
```

The query reads `disconnected` and `hidden` as part of the authoritative native state, while `userEnabled` remains the approval gate. Existing URL resolution will continue to reject a domain whose visible root is unavailable. Both Objective-C callbacks must use `deliver_callback`; a late callback after either receiver timeout is ignored rather than panicking or unwinding across Objective-C.

- [ ] **Step 6: Implement the Objective-C bridge and production entry point**

Add a `register_domain_and_wait` entry point with this signature:

```rust
pub(crate) fn register_domain_and_wait(
    app: &tauri::AppHandle,
    identifier: &str,
    display_name: &str,
) -> Result<DomainActivation, String>
```

Its body must call `register_domain_and_wait_with` exactly once with:

```rust
register_domain_and_wait_with(
    |sender| schedule_domain_add(app, identifier, display_name, sender),
    ADD_CALLBACK_TIMEOUT,
    DOMAIN_POLL_TIMEOUT,
    DOMAIN_POLL_INTERVAL,
    |remaining| query_domain_state(identifier, remaining.min(DOMAIN_QUERY_TIMEOUT)),
    std::thread::sleep,
    {
        let started = std::time::Instant::now();
        move || started.elapsed()
    },
)
```

Implement three private native helpers in the same file:

```rust
fn schedule_domain_add(
    app: &tauri::AppHandle,
    identifier: &str,
    display_name: &str,
    completion: SyncSender<Result<(), String>>,
) -> Result<(), String>

unsafe fn add_domain(
    identifier: &str,
    display_name: &str,
    completion: SyncSender<Result<(), String>>,
)

fn query_domain_state(
    identifier: &str,
    timeout: Duration,
) -> Result<Option<DomainState>, String>

unsafe fn new_domain(
    identifier: &str,
    display_name: &str,
) -> objc2::rc::Retained<objc2_file_provider::NSFileProviderDomain>

fn add_completion_result(
    domain: &str,
    code: isize,
    description: &str,
) -> Result<(), String>
```

Use `tauri::AppHandle::run_on_main_thread` only for `add_domain`. Inside the unsafe boundary:

1. Copy the borrowed identifier and display name into owned Rust `String` values before moving them into the `'static` main-thread closure, then construct `NSString` values there.
2. Allocate `NSFileProviderDomain` with `initWithIdentifier_displayName` through `new_domain` and call `setSupportsSyncingTrash(false)` before returning it. The deployment target is macOS 14, where that property is available.
3. Create a copied `block2::RcBlock` callback.
4. Call `NSFileProviderManager::addDomain_completionHandler`.
5. Inside an autorelease pool in the callback, copy a non-null `NSError` into owned domain, code, and localized-description values. `add_completion_result` returns `Ok(())` only for `NSCocoaErrorDomain` plus the typed `NSFileWriteFileExistsError`; every other error uses the exact diagnostic shape `domain (code): localized description`.
6. Deliver the owned result with `deliver_callback` and never `unwrap` or `expect` a callback send.

Implement `query_domain_state` with `NSFileProviderManager::getDomainsWithCompletionHandler`, its own bounded channel receive, and an autorelease pool inside the callback. Copy `identifier` into an owned Rust string before constructing the retained callback because the bounded receive can return before FileProvider invokes it. Iterate the returned `NSArray<NSFileProviderDomain>`, compare `domain.identifier()` with the owned requested identifier, and copy `userEnabled`, `isDisconnected`, and `isHidden` into `DomainState`. Deliver only owned Rust values with `deliver_callback`; a late send must be harmless. Do not call private selectors, `pluginkit`, testing-mode APIs, or `removeDomain`.

Extend `registration_callback_timeout_is_bounded` after its timeout assertion so it proves the shared callback delivery helper tolerates a late result:

```rust
let late_sender = pending_sender
    .take()
    .expect("pending callback sender retained");
deliver_callback(&late_sender, Ok::<(), String>(()));
```

- [ ] **Step 7: Run formatting, tests, and static checks**

Run:

```bash
cargo fmt --all
cargo test -p locality-desktop macos_file_provider::tests -- --nocapture
cargo check -p locality-desktop
cargo clippy -p locality-desktop --all-targets -- -D warnings
```

Expected: all four commands exit successfully. The test output must include nine passing `macos_file_provider::tests` cases.

- [ ] **Step 8: Commit the native boundary**

```bash
git add apps/desktop/src-tauri/src/macos_file_provider.rs \
  apps/desktop/src-tauri/src/main.rs \
  apps/desktop/src-tauri/Cargo.toml \
  Cargo.lock
git commit -m "feat(macos): add foreground File Provider registrar"
```

### Task 2: Route User-Driven Mount Creation Through Native Approval

**Files:**
- Create: `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/DomainReplacementPolicy.swift`
- Create: `platform/macos/LocalityFileProvider/Tests/LocalityFileProviderCtlTests/DomainReplacementPolicyTests.swift`
- Modify: `platform/macos/LocalityFileProvider/Package.swift`
- Modify: `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift:80-110`
- Modify: `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift:340-355`
- Modify: `apps/desktop/src-tauri/src/main.rs:1290-1410`
- Modify: `apps/desktop/src-tauri/src/main.rs:9330-9480`
- Test: `apps/desktop/src-tauri/src/main.rs:12620-12830`

- [ ] **Step 1: Add a failing Swift regression for delayed user-visible URLs**

Add a second test target to `platform/macos/LocalityFileProvider/Package.swift`:

```swift
.testTarget(
    name: "LocalityFileProviderCtlTests",
    dependencies: ["LocalityFileProviderCtl"],
    path: "Tests/LocalityFileProviderCtlTests"
),
```

Create `platform/macos/LocalityFileProvider/Tests/LocalityFileProviderCtlTests/DomainReplacementPolicyTests.swift`:

```swift
// SPDX-License-Identifier: Apache-2.0

import Foundation
import Testing
@testable import LocalityFileProviderCtl

@Test func unavailableURLDoesNotReplaceAnExistingDomain() {
    #expect(!domainNeedsReplacement(.unavailable, expectedDirectoryName: "Locality"))
}

@Test func absentURLDoesNotReplaceAnExistingDomain() {
    #expect(!domainNeedsReplacement(.available(nil), expectedDirectoryName: "Locality"))
}

@Test func matchingURLKeepsTheExistingDomain() {
    let url = URL(fileURLWithPath: "/Users/test/Library/CloudStorage/Locality")
    #expect(!domainNeedsReplacement(.available(url), expectedDirectoryName: "Locality"))
}

@Test func mismatchedURLReplacesTheExistingDomain() {
    let url = URL(fileURLWithPath: "/Users/test/Library/CloudStorage/Locality-Old")
    #expect(domainNeedsReplacement(.available(url), expectedDirectoryName: "Locality"))
}
```

Run:

```bash
swift test --package-path platform/macos/LocalityFileProvider
```

Expected: compilation fails because `UserVisibleDomainURLState` and `domainNeedsReplacement` do not exist.

- [ ] **Step 2: Preserve native approval when URL resolution is temporarily unavailable**

Create `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/DomainReplacementPolicy.swift`:

```swift
// SPDX-License-Identifier: Apache-2.0

import Foundation

enum UserVisibleDomainURLState {
  case available(URL?)
  case unavailable
}

func domainNeedsReplacement(
  _ state: UserVisibleDomainURLState,
  expectedDirectoryName: String
) -> Bool {
  switch state {
  case .unavailable, .available(nil):
    return false
  case .available(let url?):
    return url.lastPathComponent != expectedDirectoryName
  }
}
```

Rewrite `shouldReplaceExistingDomain` in `main.swift` to map the real API result into the tested policy:

```swift
private func shouldReplaceExistingDomain(_ domain: NSFileProviderDomain, displayName: String) -> Bool {
  let expectedName = fileProviderDirectoryName(for: displayName)
  let state: UserVisibleDomainURLState
  do {
    state = .available(try userVisibleDomainURLFromManager(for: domain))
  } catch {
    state = .unavailable
  }
  return domainNeedsReplacement(state, expectedDirectoryName: expectedName)
}
```

This retains genuine wrong-root repair while ensuring a transient URL error cannot remove and recreate the domain that the foreground app just registered and the user just approved.

Run `swift test --package-path platform/macos/LocalityFileProvider` and expect all package tests to pass.

- [ ] **Step 3: Add failing command-level orchestration tests with injected registrar and mount futures**

Add the following cases to the existing `#[cfg(test)]` module in `main.rs`:

```rust
#[test]
fn onboarding_registers_before_creating_the_mount() {
    let events = std::cell::RefCell::new(Vec::new());
    let report = tauri::async_runtime::block_on(super::run_workspace_mount_onboarding_with(
        super::WorkspaceMountOnboardingRequest {
            path: "/tmp/locality".to_string(),
            action: "start".to_string(),
        },
        |_| {
            events.borrow_mut().push("activate");
            std::future::ready(Ok(super::MacosFileProviderActivation::Enabled))
        },
        |_| {
            events.borrow_mut().push("mount");
            std::future::ready(Ok("mount created".to_string()))
        },
    ));

    assert_eq!(events.into_inner(), vec!["activate", "mount"]);
    assert_eq!(report.state, "created");
}

#[test]
fn onboarding_does_not_create_mount_while_native_approval_is_pending() {
    let events = std::cell::RefCell::new(Vec::new());
    let report = tauri::async_runtime::block_on(super::run_workspace_mount_onboarding_with(
        super::WorkspaceMountOnboardingRequest {
            path: "/tmp/locality".to_string(),
            action: "start".to_string(),
        },
        |_| {
            events.borrow_mut().push("activate");
            std::future::ready(Ok(super::MacosFileProviderActivation::ApprovalRequired))
        },
        |_| {
            events.borrow_mut().push("mount");
            std::future::ready(Ok("mount created".to_string()))
        },
    ));

    assert_eq!(events.into_inner(), vec!["activate"]);
    assert_eq!(report.state, "approval_required");
    assert_eq!(report.primary_action, "allow_in_macos");
}

#[test]
fn onboarding_reports_native_registration_failure_without_creating_mount() {
    let events = std::cell::RefCell::new(Vec::new());
    let report = tauri::async_runtime::block_on(super::run_workspace_mount_onboarding_with(
        super::WorkspaceMountOnboardingRequest {
            path: "/tmp/locality".to_string(),
            action: "start".to_string(),
        },
        |_| {
            events.borrow_mut().push("activate");
            std::future::ready(Err("main thread unavailable".to_string()))
        },
        |_| {
            events.borrow_mut().push("mount");
            std::future::ready(Ok("mount created".to_string()))
        },
    ));

    assert_eq!(events.into_inner(), vec!["activate"]);
    assert_eq!(report.state, "failed");
    assert_eq!(report.message, "main thread unavailable");
}

#[cfg(target_os = "macos")]
#[test]
fn check_again_queries_the_existing_domain_without_native_registration() {
    let events = std::cell::RefCell::new(Vec::new());
    let report = tauri::async_runtime::block_on(super::run_workspace_mount_onboarding_with(
        super::WorkspaceMountOnboardingRequest {
            path: "/tmp/locality".to_string(),
            action: "check_again".to_string(),
        },
        |_| {
            events.borrow_mut().push("activate");
            std::future::ready(Ok(super::MacosFileProviderActivation::Enabled))
        },
        |_| {
            events.borrow_mut().push("mount");
            std::future::ready(Err("registered but not enabled".to_string()))
        },
    ));

    assert_eq!(events.into_inner(), vec!["mount"]);
    assert_eq!(report.state, "approval_required");
}

#[test]
fn allow_in_macos_retries_native_activation() {
    let events = std::cell::RefCell::new(Vec::new());
    let report = tauri::async_runtime::block_on(super::run_workspace_mount_onboarding_with(
        super::WorkspaceMountOnboardingRequest {
            path: "/tmp/locality".to_string(),
            action: "allow_in_macos".to_string(),
        },
        |_| {
            events.borrow_mut().push("activate");
            std::future::ready(Ok(super::MacosFileProviderActivation::ApprovalRequired))
        },
        |_| {
            events.borrow_mut().push("mount");
            std::future::ready(Ok("mount created".to_string()))
        },
    ));

    assert_eq!(events.into_inner(), vec!["activate"]);
    assert_eq!(report.state, "approval_required");
}

#[test]
fn source_manager_mount_uses_the_same_activation_gate() {
    let events = std::cell::RefCell::new(Vec::new());
    let report = tauri::async_runtime::block_on(super::create_desktop_mount_command_with(
        super::CreateDesktopMountRequest {
            connector: "notion".to_string(),
            path: "/tmp/locality".to_string(),
            mount_id: "notion-main".to_string(),
            connection_id: None,
            read_only: false,
            notion_root_page: None,
            google_docs_workspace_folder: None,
        },
        |_| {
            events.borrow_mut().push("activate");
            std::future::ready(Ok(super::MacosFileProviderActivation::Enabled))
        },
        |_| {
            events.borrow_mut().push("mount");
            std::future::ready(Ok("mount created".to_string()))
        },
    ));

    assert_eq!(events.into_inner(), vec!["activate", "mount"]);
    assert!(report.ok);
}
```

- [ ] **Step 4: Run the focused Rust tests and confirm the expected red state**

Run:

```bash
cargo test -p locality-desktop onboarding_ -- --nocapture
```

Expected: compilation fails because `MacosFileProviderActivation`, `run_workspace_mount_onboarding_with`, and `create_desktop_mount_command_with` do not exist.

- [ ] **Step 5: Add the shared async registrar-before-mount seam**

Add this enum near the existing onboarding enums:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacosFileProviderActivation {
    NotRequested,
    Enabled,
    ApprovalRequired,
}
```

Add the private outcome and implement the generic async seam using `std::future::Future`:

```rust
enum DesktopMountCreationOutcome {
    Created(String),
    ApprovalRequired,
    Failed(String),
}

async fn create_desktop_mount_with_activation<Activate, ActivationFuture, CreateMount, MountFuture>(
    action: WorkspaceMountOnboardingAction,
    request: CreateDesktopMountRequest,
    activate: Activate,
    create_mount: CreateMount,
) -> DesktopMountCreationOutcome
where
    Activate: FnOnce(WorkspaceMountOnboardingAction) -> ActivationFuture,
    ActivationFuture: Future<Output = Result<MacosFileProviderActivation, String>>,
    CreateMount: FnOnce(CreateDesktopMountRequest) -> MountFuture,
    MountFuture: Future<Output = Result<String, String>>,
{
    let activation = if matches!(
        action,
        WorkspaceMountOnboardingAction::Start | WorkspaceMountOnboardingAction::AllowInMacos
    ) {
        activate(action).await
    } else {
        Ok(MacosFileProviderActivation::NotRequested)
    };

    match activation {
        Err(message) => return DesktopMountCreationOutcome::Failed(message),
        Ok(MacosFileProviderActivation::ApprovalRequired) => {
            return DesktopMountCreationOutcome::ApprovalRequired;
        }
        Ok(MacosFileProviderActivation::NotRequested)
        | Ok(MacosFileProviderActivation::Enabled) => {}
    }

    match create_mount(request).await {
        Ok(message) => DesktopMountCreationOutcome::Created(message),
        Err(message) => DesktopMountCreationOutcome::Failed(message),
    }
}
```

Build `run_workspace_mount_onboarding_with` on this seam with the following signature and mapping:

```rust
async fn run_workspace_mount_onboarding_with<Activate, ActivationFuture, CreateMount, MountFuture>(
    request: WorkspaceMountOnboardingRequest,
    activate: Activate,
    create_mount: CreateMount,
) -> WorkspaceMountOnboardingReport
where
    Activate: FnOnce(WorkspaceMountOnboardingAction) -> ActivationFuture,
    ActivationFuture: Future<Output = Result<MacosFileProviderActivation, String>>,
    CreateMount: FnOnce(CreateDesktopMountRequest) -> MountFuture,
    MountFuture: Future<Output = Result<String, String>>,
{
    let action = match WorkspaceMountOnboardingAction::parse(request.action.trim()) {
        Ok(action) => action,
        Err(message) => {
            return workspace_mount_onboarding_report(
                MacosWorkspaceMountOnboardingState::Failed,
                message,
                WorkspaceMountOnboardingPrimaryAction::RetrySetup,
                WorkspaceMountOnboardingLaunchStrategy::None,
            );
        }
    };
    let mount_request = CreateDesktopMountRequest {
        connector: "notion".to_string(),
        path: request.path,
        mount_id: "notion-main".to_string(),
        connection_id: None,
        read_only: false,
        notion_root_page: None,
        google_docs_workspace_folder: None,
    };

    match create_desktop_mount_with_activation(action, mount_request, activate, create_mount).await {
        DesktopMountCreationOutcome::Created(message) => workspace_mount_onboarding_report(
            MacosWorkspaceMountOnboardingState::Created,
            message,
            WorkspaceMountOnboardingPrimaryAction::RetrySetup,
            WorkspaceMountOnboardingLaunchStrategy::None,
        ),
        DesktopMountCreationOutcome::ApprovalRequired => workspace_mount_onboarding_report(
            MacosWorkspaceMountOnboardingState::ApprovalRequired,
            workspace_mount_onboarding_curated_message(
                MacosWorkspaceMountOnboardingState::ApprovalRequired,
            )
            .expect("approval_required message"),
            WorkspaceMountOnboardingPrimaryAction::AllowInMacos,
            WorkspaceMountOnboardingLaunchStrategy::InstructionsOnly,
        ),
        DesktopMountCreationOutcome::Failed(message) => {
            classify_workspace_mount_onboarding_failure(&message)
        }
    }
}
```

Build the source-manager wrapper with the same shared seam and explicit mapping:

```rust
async fn create_desktop_mount_command_with<Activate, ActivationFuture, CreateMount, MountFuture>(
    request: CreateDesktopMountRequest,
    activate: Activate,
    create_mount: CreateMount,
) -> ActionReport
where
    Activate: FnOnce(WorkspaceMountOnboardingAction) -> ActivationFuture,
    ActivationFuture: Future<Output = Result<MacosFileProviderActivation, String>>,
    CreateMount: FnOnce(CreateDesktopMountRequest) -> MountFuture,
    MountFuture: Future<Output = Result<String, String>>,
{
    match create_desktop_mount_with_activation(
        WorkspaceMountOnboardingAction::Start,
        request,
        activate,
        create_mount,
    )
    .await
    {
        DesktopMountCreationOutcome::Created(message) => ActionReport { ok: true, message },
        DesktopMountCreationOutcome::ApprovalRequired => ActionReport {
            ok: false,
            message: workspace_mount_onboarding_curated_message(
                MacosWorkspaceMountOnboardingState::ApprovalRequired,
            )
            .expect("approval_required message")
            .to_string(),
        },
        DesktopMountCreationOutcome::Failed(message) => ActionReport { ok: false, message },
    }
}
```

- [ ] **Step 6: Add the production activation adapter and non-macOS policy test**

Add an async function that takes the parsed action:

```rust
async fn activate_macos_file_provider_for_user_action(
    app: AppHandle,
    action: WorkspaceMountOnboardingAction,
) -> Result<MacosFileProviderActivation, String>
```

On non-macOS, factor and test this exact policy:

```rust
#[cfg(not(target_os = "macos"))]
fn non_macos_file_provider_activation(
    action: WorkspaceMountOnboardingAction,
) -> Result<MacosFileProviderActivation, String> {
    match action {
        WorkspaceMountOnboardingAction::AllowInMacos => {
            Err("macOS File Provider approval is only available on macOS.".to_string())
        }
        WorkspaceMountOnboardingAction::Start | WorkspaceMountOnboardingAction::CheckAgain => {
            Ok(MacosFileProviderActivation::NotRequested)
        }
    }
}
```

Add a `#[cfg(not(target_os = "macos"))]` unit test asserting all three branches exactly. Keep the existing `macos_desktop_mount_rejects_paths_outside_cloudstorage` regression unchanged on macOS.

Implement its branches as follows:

- `CheckAgain` returns `NotRequested` without calling `addDomain`.
- On non-macOS, `Start` returns `NotRequested` and `AllowInMacos` returns `macOS File Provider approval is only available on macOS.`
- On macOS, `Start` and `AllowInMacos` call `tauri::async_runtime::spawn_blocking` and then `macos_file_provider::register_domain_and_wait` with `MACOS_FILE_PROVIDER_DOMAIN_ID` and `MACOS_FILE_PROVIDER_DISPLAY_NAME`.
- Map `DomainActivation::Enabled` to `MacosFileProviderActivation::Enabled` and `DomainActivation::ApprovalRequired` to `MacosFileProviderActivation::ApprovalRequired`.
- Map a join failure to `File Provider activation worker failed: {error}` and retain native API error text unchanged.

Do not call the Swift helper from this adapter. The existing `create_desktop_mount_blocking` path retains its helper call as an idempotent fallback and URL resolver; the tested Swift policy from Steps 1-2 ensures this fallback never removes a matching domain merely because its URL is delayed.

- [ ] **Step 7: Wire both production commands through the tested async seams**

Rewrite `run_workspace_mount_onboarding` so it:

1. Calls `run_workspace_mount_onboarding_with`, which parses `request.action` and returns the existing failed report for a parse error before invoking either injected future.
2. Passes the production activation adapter and a mount future that invokes `create_desktop_mount_blocking` through `spawn_blocking`.
3. Awaits the returned report without blocking the Tauri main thread.
4. Refreshes desktop surfaces only when the returned report is `created`.

Delete the special `AllowInMacos` branch from `run_workspace_mount_onboarding_blocking`, then remove `run_workspace_mount_onboarding_blocking` if it has no remaining callers.

Rewrite `create_desktop_mount_command` to call `create_desktop_mount_command_with` with the same production activation adapter and blocking mount future. The generic seam performs activation first and never invokes the mount future for pending approval or a native error. Startup repair continues to call the noninteractive blocking/helper path directly and therefore never presents approval UI.

- [ ] **Step 8: Remove the Finder-first launcher and dead launch strategy**

Delete:

```rust
macos_file_provider_approval_surface_path
launch_macos_file_provider_approval_surface
```

Delete `macos_file_provider_approval_surface_path_uses_first_existing_candidate`. Keep `open_in_file_manager` and CloudStorage candidate functions because normal reveal and recovery code still uses them.

Remove `WorkspaceMountOnboardingLaunchStrategy::OpenFinder` and its `as_str` arm because native approval no longer constructs it. `InstructionsOnly` remains the recovery state and `None` remains the success/failure state.

Update the curated approval message assertion to the exact new backend message:

```rust
Some(
    "Approve Locality in the macOS File Provider prompt. If the prompt was dismissed, use Allow in macOS to try again."
)
```

Keep `classify_workspace_mount_onboarding_failure` for helper URL errors and `waiting_for_cloudstorage_root`. A user-enabled domain with a delayed root must still map to `CheckAgain`.

- [ ] **Step 9: Run Swift, Rust, and path-validation checks**

Run:

```bash
cargo fmt --all
cargo test -p locality-desktop onboarding_ -- --nocapture
cargo test -p locality-desktop macos_file_provider::tests -- --nocapture
cargo test -p locality-desktop macos_desktop_mount_rejects_paths_outside_cloudstorage -- --nocapture
cargo check -p locality-desktop
cargo clippy -p locality-desktop --all-targets -- -D warnings
swift test --package-path platform/macos/LocalityFileProvider
```

Expected: all commands exit successfully. Confirm from the focused output that activation precedes mount creation, pending/error outcomes do not invoke the mount future, `Allow in macOS` retries activation, Check again skips activation, the source-manager wrapper uses the same gate, path validation is unchanged, and unavailable URL resolution keeps the existing Swift domain.

- [ ] **Step 10: Commit the orchestration change**

```bash
git add apps/desktop/src-tauri/src/main.rs \
  platform/macos/LocalityFileProvider/Package.swift \
  platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift \
  platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/DomainReplacementPolicy.swift \
  platform/macos/LocalityFileProvider/Tests/LocalityFileProviderCtlTests/DomainReplacementPolicyTests.swift
git commit -m "fix(desktop): use native File Provider approval flow"
```

### Task 3: Align Onboarding Copy And Product Documentation

**Files:**
- Modify: `apps/desktop/src/onboarding-mount.test.ts`
- Modify: `apps/desktop/src/onboarding-mount.ts`
- Modify: `apps/desktop/src/App.tsx:1950-2030`
- Modify: `docs/desktop-app.md:160-180`

- [ ] **Step 1: Change the frontend tests first**

Update the approval fixture message in `onboarding-mount.test.ts` to the backend string from Task 2. Replace the progress, headline, and recovery expectations with:

```typescript
expect(mountOnboardingPrimaryLabel(report({ primaryAction: "allow_in_macos" }), true)).toBe(
  "Waiting for macOS",
);

expect(mountOnboardingHeadline(report({ state: "approval_required" }))).toBe(
  "Allow Locality to sync.",
);

expect(
  mountOnboardingInstructions?.(report({ launchStrategy: "instructions_only" })) ?? null,
).toBe(
  "Approve the macOS File Provider prompt. If it no longer appears, choose Locality under " +
    "Locations in Finder and enable it, or enable Locality under Extensions or File Providers " +
    "in System Settings, then return here and click Allow in macOS.",
);
```

Replace the old `open_finder` instruction assertion with:

```typescript
expect(mountOnboardingNeedsInstructions(report({ launchStrategy: "instructions_only" }))).toBe(
  true,
);
expect(mountOnboardingNeedsInstructions(report({ launchStrategy: "none" }))).toBe(false);
```

Rename the tests so they describe native approval and recovery instead of Finder opening. Keep the test proving that no approval instructions appear for `waiting_for_cloudstorage_root`.

- [ ] **Step 2: Run the frontend test and confirm the expected red state**

Run:

```bash
npm --prefix apps/desktop test -- --run src/onboarding-mount.test.ts
```

Expected: three exact-string assertions fail against `Opening Finder`, `Allow Locality in Finder.`, and the Finder-first instructions.

- [ ] **Step 3: Implement the approved copy**

In `apps/desktop/src/onboarding-mount.ts`:

- Remove `"open_finder"` from `WorkspaceMountOnboardingLaunchStrategy`; the backend no longer emits it.
- Return `Waiting for macOS` when an `allow_in_macos` action is busy.
- Return `Allow Locality to sync.` for `approval_required`.
- Return the exact recovery instruction string asserted above only for `approval_required` plus `instructions_only`.
- Keep `Allow in macOS`, `Check again`, and `Retry setup` as the stable action labels.

In `apps/desktop/src/App.tsx`, replace the busy sync-note text `Checking File Provider approval` with `Waiting for macOS approval`. Do not add an explanatory card or expose polling intervals.

- [ ] **Step 4: Update the durable product documentation**

Replace the Finder-first paragraph in `docs/desktop-app.md` with:

```markdown
On macOS, this step is a blocked File Provider gate. A user-driven mount action
asks the foreground Locality app to register the File Provider domain, allowing
macOS to present its native approval. Locality waits for the refetched domain to
become user-enabled before continuing. If approval is denied, dismissed, or
remains pending, the screen stays on step 4 and offers `Allow in macOS` again;
Finder and System Settings are recovery paths rather than the first-install
path. If approval succeeds but macOS has not yet materialized the CloudStorage
root, the screen remains blocked with `Check again` until the folder exists and
the mount root passes verification.
```

- [ ] **Step 5: Run frontend and Rust verification**

Run:

```bash
npm --prefix apps/desktop test -- --run src/onboarding-mount.test.ts
npm --prefix apps/desktop test -- --run
npm --prefix apps/desktop run build
cargo test -p locality-desktop workspace_mount_onboarding_curated_message_matches_recoverable_state -- --nocapture
```

Expected: all commands exit successfully and the TypeScript build reports no type errors.

- [ ] **Step 6: Commit the UX and docs change**

```bash
git add apps/desktop/src/onboarding-mount.test.ts \
  apps/desktop/src/onboarding-mount.ts \
  apps/desktop/src/App.tsx \
  docs/desktop-app.md
git commit -m "docs(desktop): describe native File Provider approval"
```

### Task 4: Build A Correctly Contained macOS App Extension

**Files:**
- Create: `platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh`
- Modify: `platform/macos/LocalityFileProvider/scripts/build-dev-bundle.sh`
- Modify: `platform/macos/LocalityFileProvider/App/LocalityFileProvider.Info.plist`
- Modify: `scripts/publish-macos.sh`
- Modify: `tests/macos_publish_config.sh`

- [ ] **Step 1: Add the failing bundle-construction test**

Create an executable `platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh` with this structure:

```bash
#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="$(mktemp -d)"
trap 'rm -rf "${BUILD_ROOT}"' EXIT

fail() {
  printf 'build-dev-bundle test failed: %s\n' "$*" >&2
  exit 1
}

SCRIPT="${ROOT}/scripts/build-dev-bundle.sh"
grep -Fq -- '-application-extension' "${SCRIPT}" \
  || fail 'Swift application-extension flag is missing'
grep -Fq -- '-Xcc -fapplication-extension' "${SCRIPT}" \
  || fail 'C application-extension flag is missing'

APP="$(
  LOCALITY_FILE_PROVIDER_BUILD_ROOT="${BUILD_ROOT}" \
    APPLE_SIGNING_IDENTITY=- \
    bash "${SCRIPT}"
)"
APPEX="${APP}/Contents/PlugIns/LocalityFileProvider.appex"
PLIST="${APPEX}/Contents/Info.plist"

[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleSupportedPlatforms:0' "${PLIST}")" == "MacOSX" ]] \
  || fail 'CFBundleSupportedPlatforms does not contain MacOSX'
[[ "$(/usr/libexec/PlistBuddy -c 'Print :DTPlatformName' "${PLIST}")" == "macosx" ]] \
  || fail 'DTPlatformName is not macosx'
[[ "$(/usr/libexec/PlistBuddy -c 'Print :DTSDKName' "${PLIST}")" == macosx* ]] \
  || fail 'DTSDKName does not identify the macOS SDK'
[[ -n "$(/usr/libexec/PlistBuddy -c 'Print :DTSDKBuild' "${PLIST}")" ]] \
  || fail 'DTSDKBuild is empty'
for key in BuildMachineOSBuild DTCompiler DTPlatformBuild DTPlatformVersion; do
  [[ -n "$(/usr/libexec/PlistBuddy -c "Print :${key}" "${PLIST}")" ]] \
    || fail "${key} is empty"
done

HOST_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "${APP}/Contents/Info.plist")"
APPEX_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "${PLIST}")"
[[ "${APPEX_ID}" == "${HOST_ID}."* ]] \
  || fail "${APPEX_ID} is not contained by ${HOST_ID}"

codesign --verify --deep --strict --verbose=2 "${APP}"
if codesign -d --entitlements - "${APPEX}" 2>/dev/null \
  | grep -Fq 'com.apple.developer.fileprovider.testing-mode'; then
  fail 'testing-mode entitlement is present'
fi

printf 'build-dev-bundle tests passed\n'
```

Make the file executable with `chmod +x platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh`.

- [ ] **Step 2: Run the script and confirm the expected red state**

Run:

```bash
bash platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh
```

Expected: failure before compilation with `Swift application-extension flag is missing`.

- [ ] **Step 3: Add failing negative tests for the release guards**

Before sourcing the publish script, require a source-safe main guard so the red test cannot accidentally start a build:

```bash
grep -Fq 'if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must be safe to source for guard tests"
source "${PUBLISH_SCRIPT}"
```

Append these fixtures after the existing `tmp_root` setup in `tests/macos_publish_config.sh`:

```bash
fixture_app="${tmp_root}/Locality.app"
fixture_appex="${fixture_app}/Contents/PlugIns/LocalityFileProvider.appex"
mkdir -p "${fixture_appex}/Contents"
cp "${FILE_PROVIDER_HOST_PLIST}" "${fixture_app}/Contents/Info.plist"
cp "${FILE_PROVIDER_EXTENSION_PLIST}" "${fixture_appex}/Contents/Info.plist"
/usr/libexec/PlistBuddy -c 'Add :DTPlatformName string macosx' "${fixture_appex}/Contents/Info.plist"
/usr/libexec/PlistBuddy -c 'Add :DTSDKName string macosx26.5' "${fixture_appex}/Contents/Info.plist"
/usr/libexec/PlistBuddy -c 'Add :DTSDKBuild string TESTSDK' "${fixture_appex}/Contents/Info.plist"

assert_file_provider_bundle_metadata "${fixture_app}"

bad_containment="${tmp_root}/BadContainment.app"
cp -R "${fixture_app}" "${bad_containment}"
/usr/libexec/PlistBuddy -c \
  'Set :CFBundleIdentifier unrelated.FileProvider' \
  "${bad_containment}/Contents/PlugIns/LocalityFileProvider.appex/Contents/Info.plist"
if (assert_file_provider_bundle_metadata "${bad_containment}"); then
  fail "publish guard accepted an unrelated extension identifier"
fi

missing_sdk="${tmp_root}/MissingSDK.app"
cp -R "${fixture_app}" "${missing_sdk}"
/usr/libexec/PlistBuddy -c \
  'Delete :DTSDKBuild' \
  "${missing_sdk}/Contents/PlugIns/LocalityFileProvider.appex/Contents/Info.plist"
if (assert_file_provider_bundle_metadata "${missing_sdk}"); then
  fail "publish guard accepted missing SDK metadata"
fi

entitlement_source="${tmp_root}/entitlement-probe.c"
entitlement_binary="${tmp_root}/entitlement-probe"
printf 'int main(void) { return 0; }\n' >"${entitlement_source}"
xcrun clang "${entitlement_source}" -o "${entitlement_binary}"
codesign --remove-signature "${entitlement_binary}" >/dev/null 2>&1 || true
if (assert_no_file_provider_testing_mode "${entitlement_binary}"); then
  fail "publish guard accepted an unreadable entitlement set"
fi

codesign --force --sign - "${entitlement_binary}" >/dev/null
assert_no_file_provider_testing_mode "${entitlement_binary}"

testing_entitlements="${tmp_root}/testing-entitlements.plist"
cat >"${testing_entitlements}" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.developer.fileprovider.testing-mode</key><true/>
</dict></plist>
PLIST
codesign --force --sign - --entitlements "${testing_entitlements}" \
  "${entitlement_binary}" >/dev/null
if (assert_no_file_provider_testing_mode "${entitlement_binary}"); then
  fail "publish guard accepted the File Provider testing-mode entitlement"
fi
```

Run:

```bash
tests/macos_publish_config.sh
```

Expected: failure with `publish-macos must be safe to source for guard tests`, before the script is sourced or any publish action runs.

- [ ] **Step 4: Add platform metadata to the source plist**

Add this key to `platform/macos/LocalityFileProvider/App/LocalityFileProvider.Info.plist`:

```xml
<key>CFBundleSupportedPlatforms</key>
<array>
  <string>MacOSX</string>
</array>
```

Keep the existing production identifier `ai.codeflash.locality.Locality.FileProvider`, document group, and extension point unchanged.

- [ ] **Step 5: Add isolated build output and truthful staged metadata**

Change the build-root assignment in `build-dev-bundle.sh` to:

```bash
BUILD_ROOT="${LOCALITY_FILE_PROVIDER_BUILD_ROOT:-${ROOT}/.build/dev-bundle}"
```

After copying the extension plist, read build metadata:

```bash
SDK_VERSION="$(xcrun --sdk macosx --show-sdk-version)"
SDK_BUILD="$(xcrun --sdk macosx --show-sdk-build-version)"
BUILD_MACHINE_OS_BUILD="$(sw_vers -buildVersion)"
APPEX_PLIST="${APPEX}/Contents/Info.plist"

/usr/libexec/PlistBuddy -c "Add :BuildMachineOSBuild string ${BUILD_MACHINE_OS_BUILD}" "${APPEX_PLIST}"
/usr/libexec/PlistBuddy -c 'Add :DTCompiler string com.apple.compilers.llvm.clang.1_0' "${APPEX_PLIST}"
/usr/libexec/PlistBuddy -c "Add :DTPlatformBuild string ${SDK_BUILD}" "${APPEX_PLIST}"
/usr/libexec/PlistBuddy -c 'Add :DTPlatformName string macosx' "${APPEX_PLIST}"
/usr/libexec/PlistBuddy -c "Add :DTPlatformVersion string ${SDK_VERSION}" "${APPEX_PLIST}"
/usr/libexec/PlistBuddy -c "Add :DTSDKBuild string ${SDK_BUILD}" "${APPEX_PLIST}"
/usr/libexec/PlistBuddy -c "Add :DTSDKName string macosx${SDK_VERSION}" "${APPEX_PLIST}"
```

The script recreates the staged plist on every run, so `Add` is deterministic. Fail immediately if `xcrun`, `sw_vers`, or `PlistBuddy` cannot provide a value.

- [ ] **Step 6: Compile only the extension as an app extension**

Add these arguments to the `swiftc` command that emits `LocalityFileProvider`, and not to the host or helper commands:

```bash
  -application-extension \
  -Xcc -fapplication-extension \
```

Keep the explicit `-target "${ARCH}-apple-macos14.0"`, FileProvider framework, C entry point, Swift sources, and existing entitlements.

- [ ] **Step 7: Add source-safe, fail-closed release guards**

Add these functions to `scripts/publish-macos.sh`:

```bash
assert_no_file_provider_testing_mode() {
  local path="$1"
  local entitlements
  if ! entitlements="$(codesign -d --entitlements - "${path}" 2>/dev/null)"; then
    fail "could not inspect entitlements for ${path}"
  fi
  [[ "${entitlements}" != *"com.apple.developer.fileprovider.testing-mode"* ]] \
    || fail "${path} contains the File Provider testing-mode entitlement"
}

assert_file_provider_bundle_metadata() {
  local app="$1"
  local appex="${app}/Contents/PlugIns/LocalityFileProvider.appex"
  local host_id appex_id platform sdk_name sdk_build
  host_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "${app}/Contents/Info.plist")"
  appex_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "${appex}/Contents/Info.plist")"
  platform="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleSupportedPlatforms:0' "${appex}/Contents/Info.plist")"
  sdk_name="$(/usr/libexec/PlistBuddy -c 'Print :DTSDKName' "${appex}/Contents/Info.plist")"
  sdk_build="$(/usr/libexec/PlistBuddy -c 'Print :DTSDKBuild' "${appex}/Contents/Info.plist")"

  [[ "${appex_id}" == "${host_id}."* ]] \
    || fail "${appex_id} is not contained by ${host_id}"
  [[ "${platform}" == "MacOSX" ]] \
    || fail "LocalityFileProvider.appex does not declare MacOSX support"
  [[ "${sdk_name}" == macosx* && -n "${sdk_build}" ]] \
    || fail "LocalityFileProvider.appex is missing macOS SDK metadata"

}
```

Make `scripts/publish-macos.sh` safe to source without changing normal execution:

```bash
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
```

Call the metadata guard and the three entitlement guards inside `verify_signed_app_in_dmg` immediately after `codesign --verify --deep --strict`:

```bash
assert_file_provider_bundle_metadata "${app}"
assert_no_file_provider_testing_mode "${app}"
assert_no_file_provider_testing_mode "${app}/Contents/MacOS/locality-file-providerctl"
assert_no_file_provider_testing_mode "${app}/Contents/PlugIns/LocalityFileProvider.appex"
```

Add this prerequisite beside the existing `require_command` calls:

```bash
[[ -x /usr/libexec/PlistBuddy ]] || fail "missing required command: /usr/libexec/PlistBuddy"
```

- [ ] **Step 8: Run positive and negative bundle/publishing verification**

Run:

```bash
bash platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh
bash -n platform/macos/LocalityFileProvider/scripts/build-dev-bundle.sh
bash -n platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh
bash -n scripts/publish-macos.sh
tests/macos_publish_config.sh
```

Expected: the bundle test prints `build-dev-bundle tests passed`, the existing publish-config suite passes all positive and negative fixtures, and all syntax checks exit successfully. Inspect the test-built app before its cleanup during debugging if a nested signature or metadata assertion fails.

- [ ] **Step 9: Commit bundle construction and release guards**

```bash
git add platform/macos/LocalityFileProvider/App/LocalityFileProvider.Info.plist \
  platform/macos/LocalityFileProvider/scripts/build-dev-bundle.sh \
  platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh \
  scripts/publish-macos.sh \
  tests/macos_publish_config.sh
git commit -m "fix(macos): identify File Provider as embedded extension"
```

### Task 5: Review And Verify The Signed Installed Product

**Files:**
- Review: all files changed since `origin/main`
- Verify: generated DMG and `/Applications/Locality.app`

- [ ] **Step 1: Run the complete automated verification matrix from a clean tree**

Run each command independently and preserve its exit status:

```bash
cargo fmt --all -- --check
cargo test -p locality-desktop
cargo clippy -p locality-desktop --all-targets -- -D warnings
npm --prefix apps/desktop test -- --run
npm --prefix apps/desktop run build
swift test --package-path platform/macos/LocalityFileProvider
bash platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh
tests/macos_publish_config.sh
bash scripts/clean-start.test.sh
git diff --check origin/main...HEAD
git status --short
```

Expected: all test/check commands exit successfully and `git status --short` is empty. Record any environmental omission rather than describing an unrun command as passing.

- [ ] **Step 2: Request an independent code review**

Use the `superpowers:requesting-code-review` skill with:

- requirements: `docs/superpowers/specs/2026-07-14-macos-file-provider-native-approval-design.md`;
- implementation plan: this file;
- base: `origin/main`;
- head: `HEAD`; and
- focus: main-thread ownership, callback lifetimes, no UI-thread blocking, bounded waits, non-macOS behavior, stable identifiers, testing-entitlement exclusion, and installed-product verification.

Address every Critical or Important finding. Add a regression test before fixing any behavioral issue found during review, rerun the affected focused test, then rerun the complete matrix from Step 1.

- [ ] **Step 3: Build and notarize with the existing Keychain profile**

Run from a clean committed tree:

```bash
APPLE_NOTARY_KEYCHAIN_PROFILE=loc-notary \
PUBLISH_CHANNEL=pr-validation \
bash scripts/publish-macos.sh
```

Do not place an Apple ID password on the command line or in the environment when the `loc-notary` Keychain profile succeeds. Do not set updater-signing variables; updater keys are unrelated to Developer ID notarization for this validation artifact.

Expected: the script reports a Developer ID identity, successful nested signature checks, accepted notarization, a stapled ticket, Gatekeeper acceptance, and the final DMG path. Save the reported path and SHA-256 for the final handoff.

- [ ] **Step 4: Install the exact notarized app and verify containment**

Mount the DMG path printed in Step 3, quit any running Locality instance, copy the app to `/Applications/Locality.app`, detach the DMG, then run:

```bash
codesign --verify --deep --strict --verbose=2 /Applications/Locality.app
spctl --assess --type execute --verbose=4 /Applications/Locality.app
pluginkit -mAvvv -i ai.codeflash.locality.Locality.FileProvider
fileproviderctl dump | rg -C 8 'ai\.codeflash\.locality\.Locality\.FileProvider|containing bundle identifier'
```

Expected: signatures verify, Gatekeeper accepts the app, PluginKit resolves the extension from `/Applications/Locality.app`, and `fileproviderctl dump` identifies `ai.codeflash.locality` as the containing bundle. If stale registrations make the output ambiguous, prefer a scratch macOS user. Use the repository's clean-start tooling only for state already designated as disposable by the human contributor; do not delete active Locality data or unrelated providers.

- [ ] **Step 5: Exercise the first-install approval path without Finder Enable**

On a clean Locality File Provider identity or a clean macOS user:

1. Launch `/Applications/Locality.app` normally so its setup window is foreground.
2. Connect the scratch Notion account/workspace used for product verification.
3. Start workspace mount creation from onboarding.
4. Confirm macOS presents its File Provider approval owned by the foreground Locality flow.
5. Accept the approval without opening the CloudStorage root in Finder.
6. Confirm the onboarding command continues, `locality-file-providerctl list` reports the `loc` domain with `userEnabled: true`, and the Locality CloudStorage/mount roots appear.
7. Confirm the final ready screen is not reached before both the domain and mount root are available.

Also dismiss the approval once on a clean retryable test identity, confirm the bounded wait returns `approval_required`, and confirm `Allow in macOS` invokes the foreground flow again. If the available machine cannot provide a genuinely clean identity without risking user data, mark these interaction steps unverified and report the exact limitation; do not infer them from unit tests or bundle metadata.

- [ ] **Step 6: Audit the final branch**

Run:

```bash
git status --short --branch
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
git diff --check origin/main...HEAD
```

Expected: a clean feature branch, focused commits, no unrelated files, and no whitespace errors.

### Task 6: Push The Branch And Open The Pull Request

**Files:**
- No source changes expected

- [ ] **Step 1: Push the reviewed branch**

```bash
git push -u origin fix/macos-file-provider-native-approval
```

Expected: the remote branch is created or updated without force-pushing.

- [ ] **Step 2: Create the PR using only verified claims**

Build a concise PR body from the actual diff and recorded command results. It must state:

- the foreground Tauri app now owns public File Provider domain registration for explicit user actions;
- the existing helper remains responsible for maintenance and URL resolution;
- production identifiers and durable state remain unchanged;
- extension packaging now declares macOS/app-extension metadata and release checks reject testing entitlements;
- exact automated commands that actually passed;
- Developer ID/notarization and installed-flow results that were actually observed; and
- an explicit AI-assistance disclosure.

If the clean-identity interaction was not run, say `Fresh-identity native approval interaction: not verified in this environment` rather than claiming the Finder `Enable` step is eliminated.

After Steps 1-5 have produced the stated results, create the temporary body with the verified summary. Keep the conservative fresh-identity line unless that interaction was actually completed:

```bash
PR_BODY="$(mktemp)"
trap 'rm -f "${PR_BODY}"' EXIT
cat >"${PR_BODY}" <<'EOF'
## Summary

- register the stable Locality File Provider domain from the foreground Tauri app during explicit mount actions
- keep the existing helper for idempotent repair, URL resolution, signalling, and CLI operations
- build and validate the existing File Provider identifier as a contained macOS app extension without testing entitlements

## Verification

- `cargo fmt --all -- --check`
- `cargo test -p locality-desktop`
- `cargo clippy -p locality-desktop --all-targets -- -D warnings`
- `npm --prefix apps/desktop test -- --run`
- `npm --prefix apps/desktop run build`
- `swift test --package-path platform/macos/LocalityFileProvider`
- `bash platform/macos/LocalityFileProvider/scripts/build-dev-bundle.test.sh`
- `tests/macos_publish_config.sh`
- `bash scripts/clean-start.test.sh`
- Developer ID build and notarization completed with the repository publishing script.
- Installed nested signatures, Gatekeeper assessment, PluginKit discovery, and containing-bundle attribution were checked.
- Fresh-identity native approval interaction: not verified in this environment.

## AI Assistance

Implementation and test development were assisted by OpenAI Codex.
EOF

gh pr create \
  --base main \
  --head fix/macos-file-provider-native-approval \
  --title "fix(macos): request File Provider approval from desktop app" \
  --body-file "${PR_BODY}"
```

If any listed command, notarization step, or installed check did not succeed, remove that line before invoking `gh pr create` and state the limitation in plain language. If the fresh-identity interaction did succeed, replace only its conservative line with the exact observed result. Do not include credentials, Keychain details, updater private-key paths, scratch account identifiers, or unverified statements.

- [ ] **Step 3: Verify the remote PR state**

Run:

```bash
gh pr view --json url,title,baseRefName,headRefName,state
```

Expected: an open PR targeting `main` from `fix/macos-file-provider-native-approval`. Report its URL, the verification commands, notarization result, installed-flow result, and any explicitly unverified manual step to the user.
