# File Provider Native Start Syncing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make macOS onboarding treat the native "Start Syncing" OK action as File Provider approval and stop asking users to click a separate Finder/System Settings Enable control.

**Architecture:** Keep durable sync and mount validation unchanged. Update onboarding state/copy in the TypeScript UI and Rust backend, and make the `allow_in_macos` action surface approval then immediately retry mount setup so a native OK can complete onboarding. Keep helper/error copy aligned with the native prompt without broadening File Provider reset behavior.

**Tech Stack:** TypeScript, Vitest, Rust/Tauri, Cargo tests, Swift File Provider helper copy

---

## File Structure

- `apps/desktop/src/onboarding-mount.test.ts`: frontend RED/GREEN coverage for native "Start Syncing" approval copy and absence of normal Finder Enable instructions.
- `apps/desktop/src/onboarding-mount.ts`: frontend labels, headline, instructions, and supplementary note for macOS native prompt approval.
- `apps/desktop/src-tauri/src/main.rs`: backend onboarding state/copy and injectable runner for testing `allow_in_macos` retry behavior without calling real macOS APIs.
- `apps/desktop/src/onboarding-errors.test.ts`: frontend setup-error fixture updated to the new disabled-domain copy.
- `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift`: disabled-domain helper message updated to native prompt language.
- `crates/loc-cli/src/file_provider.rs`: helper error expectation updated so developer guidance does not require a separate Finder Enable action.
- `docs/desktop-app.md`: product doc updated to describe the native prompt as the normal approval path.

## Task 1: Frontend Native Prompt Copy

**Files:**
- Modify: `apps/desktop/src/onboarding-mount.test.ts`
- Modify: `apps/desktop/src/onboarding-mount.ts`

- [ ] **Step 1: Write the failing frontend copy tests**

In `apps/desktop/src/onboarding-mount.test.ts`, update and add tests so approval copy is about the native macOS prompt:

```ts
it("labels and describes the native macOS approval prompt", () => {
  expect(mountOnboardingPrimaryLabel(report({ primaryAction: "allow_in_macos" }), false)).toBe(
    "Allow in macOS",
  );
  expect(mountOnboardingPrimaryLabel(report({ primaryAction: "allow_in_macos" }), true)).toBe(
    "Opening macOS prompt",
  );
  expect(mountOnboardingHeadline(report({ state: "approval_required" }))).toBe(
    "Approve the macOS Start Syncing prompt.",
  );
});
```

Replace the existing instruction expectation with:

```ts
it("explains that OK in the native prompt enables the File Provider location", () => {
  const instructions =
    mountOnboardingInstructions?.(report({ launchStrategy: "instructions_only" })) ?? null;

  expect(instructions).toBe(
    `Click OK in the macOS "Start Syncing" prompt, then Locality will check the folder again. If you clicked Don't allow, choose Allow in macOS to try again.`,
  );
  expect(instructions).not.toContain("enable the File Provider");
  expect(instructions).not.toContain("System Settings");
});
```

- [ ] **Step 2: Run the frontend tests and verify RED**

Run:

```bash
cd apps/desktop
npm install
npm test -- --run src/onboarding-mount.test.ts
```

Expected before implementation: tests fail because existing copy says `Opening Finder`, `Allow Locality in Finder.`, and `enable the File Provider`.

- [ ] **Step 3: Implement minimal frontend copy changes**

In `apps/desktop/src/onboarding-mount.ts`, make these exact copy changes:

```ts
if (busy && report?.primaryAction === "allow_in_macos") {
  return "Opening macOS prompt";
}
```

```ts
case "approval_required":
  return "Approve the macOS Start Syncing prompt.";
```

```ts
return (
  'Click OK in the macOS "Start Syncing" prompt, then Locality will check the folder again. ' +
  "If you clicked Don't allow, choose Allow in macOS to try again."
);
```

- [ ] **Step 4: Run the frontend tests and verify GREEN**

Run:

```bash
cd apps/desktop
npm test -- --run src/onboarding-mount.test.ts
```

Expected: all `onboarding-mount` tests pass.

- [ ] **Step 5: Commit frontend copy**

Run:

```bash
git add apps/desktop/src/onboarding-mount.ts apps/desktop/src/onboarding-mount.test.ts apps/desktop/package-lock.json apps/desktop/package.json
git commit -m "fix: describe native File Provider approval prompt"
```

If `npm install` does not change `package-lock.json` or `package.json`, omit those files from `git add`.

## Task 2: Backend `allow_in_macos` Retries Mount Setup

**Files:**
- Modify: `apps/desktop/src-tauri/src/main.rs`

- [ ] **Step 1: Write the failing backend behavior test**

In the `#[cfg(test)] mod tests` section of `apps/desktop/src-tauri/src/main.rs`, add:

```rust
#[test]
fn workspace_mount_onboarding_allow_action_retries_mount_setup_after_native_prompt() {
    let mut launch_count = 0usize;
    let mut requests = Vec::new();

    let report = super::run_workspace_mount_onboarding_with(
        super::WorkspaceMountOnboardingRequest {
            path: "~/Library/CloudStorage/Locality/notion-main".to_string(),
            action: "allow_in_macos".to_string(),
        },
        |request| {
            requests.push((
                request.connector.clone(),
                request.path.clone(),
                request.mount_id.clone(),
            ));
            Ok("Mounted Notion at /Users/test/Library/CloudStorage/Locality/notion-main with macOS File Provider.".to_string())
        },
        || {
            launch_count += 1;
            super::WorkspaceMountOnboardingLaunchStrategy::InstructionsOnly
        },
    );

    assert_eq!(launch_count, 1);
    assert_eq!(
        requests,
        vec![(
            "notion".to_string(),
            "~/Library/CloudStorage/Locality/notion-main".to_string(),
            "notion-main".to_string(),
        )]
    );
    assert_eq!(report.state, "created");
    assert_eq!(report.message, "Mounted Notion at /Users/test/Library/CloudStorage/Locality/notion-main with macOS File Provider.");
}
```

Add a second test for denial/dismissal staying recoverable:

```rust
#[test]
fn workspace_mount_onboarding_allow_action_stays_approval_required_when_domain_remains_disabled() {
    let mut launch_count = 0usize;

    let report = super::run_workspace_mount_onboarding_with(
        super::WorkspaceMountOnboardingRequest {
            path: "~/Library/CloudStorage/Locality/notion-main".to_string(),
            action: "allow_in_macos".to_string(),
        },
        |_request| {
            Err("Could not open macOS File Provider domain `loc`: The Locality File Provider is registered but not enabled. Click OK in the macOS Start Syncing prompt, then try again.".to_string())
        },
        || {
            launch_count += 1;
            super::WorkspaceMountOnboardingLaunchStrategy::InstructionsOnly
        },
    );

    assert_eq!(launch_count, 1);
    let expected_state = if cfg!(target_os = "macos") {
        "approval_required"
    } else {
        "failed"
    };
    assert_eq!(report.state, expected_state);
    if cfg!(target_os = "macos") {
        assert_eq!(report.primary_action, "allow_in_macos");
        assert!(report.message.contains("Start Syncing"));
    }
}
```

- [ ] **Step 2: Run backend tests and verify RED**

Run:

```bash
cargo test -p locality-desktop workspace_mount_onboarding_allow_action -- --nocapture
```

Expected before implementation: compile fails because `run_workspace_mount_onboarding_with` does not exist.

- [ ] **Step 3: Extract the injectable backend runner**

In `apps/desktop/src-tauri/src/main.rs`, replace the body of `run_workspace_mount_onboarding_blocking` with:

```rust
fn run_workspace_mount_onboarding_blocking(
    request: WorkspaceMountOnboardingRequest,
) -> WorkspaceMountOnboardingReport {
    run_workspace_mount_onboarding_with(
        request,
        create_desktop_mount_blocking,
        launch_macos_file_provider_approval_surface,
    )
}
```

Add this helper below it:

```rust
fn run_workspace_mount_onboarding_with<CreateMount, LaunchApproval>(
    request: WorkspaceMountOnboardingRequest,
    mut create_mount: CreateMount,
    mut launch_approval: LaunchApproval,
) -> WorkspaceMountOnboardingReport
where
    CreateMount: FnMut(CreateDesktopMountRequest) -> Result<String, String>,
    LaunchApproval: FnMut() -> WorkspaceMountOnboardingLaunchStrategy,
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

    if matches!(action, WorkspaceMountOnboardingAction::AllowInMacos) {
        #[cfg(target_os = "macos")]
        {
            let _ = launch_approval();
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = launch_approval;
            return workspace_mount_onboarding_report(
                MacosWorkspaceMountOnboardingState::Failed,
                "macOS File Provider approval is only available on macOS.",
                WorkspaceMountOnboardingPrimaryAction::RetrySetup,
                WorkspaceMountOnboardingLaunchStrategy::None,
            );
        }
    }

    match create_mount(CreateDesktopMountRequest {
        connector: "notion".to_string(),
        path: request.path,
        mount_id: "notion-main".to_string(),
        connection_id: None,
        read_only: false,
        notion_root_page: None,
        google_docs_workspace_folder: None,
    }) {
        Ok(message) => workspace_mount_onboarding_report(
            MacosWorkspaceMountOnboardingState::Created,
            message,
            WorkspaceMountOnboardingPrimaryAction::RetrySetup,
            WorkspaceMountOnboardingLaunchStrategy::None,
        ),
        Err(message) => classify_workspace_mount_onboarding_failure(&message),
    }
}
```

- [ ] **Step 4: Update backend curated approval message**

In `workspace_mount_onboarding_curated_message`, replace the approval message with:

```rust
Some("Click OK in the macOS \"Start Syncing\" prompt. Locality will continue once macOS enables the CloudStorage folder.")
```

Update `workspace_mount_onboarding_curated_message_matches_recoverable_state` to expect the same string.

- [ ] **Step 5: Run backend tests and verify GREEN**

Run:

```bash
cargo test -p locality-desktop workspace_mount_onboarding -- --nocapture
```

Expected: all workspace mount onboarding tests pass.

- [ ] **Step 6: Commit backend behavior**

Run:

```bash
git add apps/desktop/src-tauri/src/main.rs
git commit -m "fix: retry onboarding after native File Provider approval"
```

## Task 3: Disabled-Domain Helper And Error Copy

**Files:**
- Modify: `apps/desktop/src/onboarding-errors.test.ts`
- Modify: `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift`
- Modify: `crates/loc-cli/src/file_provider.rs`

- [ ] **Step 1: Write failing copy expectations**

In `apps/desktop/src/onboarding-errors.test.ts`, update the disabled-provider fixture:

```ts
classifyMountSetupError(
  'Could not open macOS File Provider domain `loc`: The Locality File Provider is registered but not enabled. Click OK in the macOS "Start Syncing" prompt, then try again.',
)
```

In `crates/loc-cli/src/file_provider.rs`, update `macos_file_provider_unavailable_error_is_actionable` to assert native prompt copy:

```rust
assert!(message.contains("make install-macos-file-provider"));
assert!(message.contains("Start Syncing"));
assert!(!message.contains("enable the File Provider"));
assert!(!message.contains("right now.."));
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```bash
cd apps/desktop
npm test -- --run src/onboarding-errors.test.ts
```

Then run:

```bash
cargo test -p loc-cli macos_file_provider_unavailable_error_is_actionable -- --nocapture
```

Expected before implementation: Rust helper-copy test fails because the message still says `enable the File Provider`. The TypeScript test may pass because classification still keys on `registered but not enabled`; keep it as fixture coverage.

- [ ] **Step 3: Implement helper copy changes**

In `platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift`, replace the disabled-domain open error with:

```swift
throw UsageError(
  "The Locality File Provider is registered but not enabled. Click OK in the macOS \"Start Syncing\" prompt, then try again."
)
```

Replace the missing CloudStorage-root error with:

```swift
throw UsageError(
  "File Provider domain \(mountId) exists but macOS has not created \(url.path). Click OK in the macOS \"Start Syncing\" prompt if it is still visible, then try again."
)
```

In `crates/loc-cli/src/file_provider.rs`, update `FileProviderHelperError::message()` for `macos_file_provider_application_unavailable`:

```rust
format!(
    "{message}. The Locality macOS File Provider app or extension is not available to macOS. For local development, run `make install-macos-file-provider`, then reopen Locality and click OK in the macOS \"Start Syncing\" prompt if macOS asks."
)
```

- [ ] **Step 4: Run tests and verify GREEN**

Run:

```bash
cd apps/desktop
npm test -- --run src/onboarding-errors.test.ts
cargo test -p loc-cli macos_file_provider_unavailable_error_is_actionable -- --nocapture
```

Expected: both focused tests pass.

- [ ] **Step 5: Commit helper copy**

Run:

```bash
git add apps/desktop/src/onboarding-errors.test.ts platform/macos/LocalityFileProvider/Sources/LocalityFileProviderCtl/main.swift crates/loc-cli/src/file_provider.rs
git commit -m "fix: align File Provider disabled copy with native prompt"
```

## Task 4: Product Documentation And Focused Verification

**Files:**
- Modify: `docs/desktop-app.md`

- [ ] **Step 1: Update desktop product doc**

In `docs/desktop-app.md`, update the macOS File Provider gate paragraph to say:

```markdown
On macOS, this step is a blocked File Provider gate. If the Locality File
Provider is registered but not yet enabled, the onboarding screen stays on step
4, offers an `Allow in macOS` action, and relies on the native macOS
"Start Syncing" prompt as the approval surface. Clicking OK in that prompt means
the File Provider location is enabled; Locality should then retry setup and move
to folder verification rather than asking the user to click an additional Finder
or System Settings Enable control. If macOS has accepted approval but has not
yet materialized `~/Library/CloudStorage/Locality`, the screen remains blocked
with `Check again` until the folder exists and the mount root passes
verification.
```

- [ ] **Step 2: Run full focused verification**

Run:

```bash
cd apps/desktop
npm test -- --run src/onboarding-mount.test.ts src/onboarding-errors.test.ts
```

Run:

```bash
cargo test -p locality-desktop workspace_mount_onboarding -- --nocapture
cargo test -p loc-cli macos_file_provider_unavailable_error_is_actionable -- --nocapture
```

Expected: all focused frontend and Rust tests pass.

- [ ] **Step 3: Check working tree and commit docs**

Run:

```bash
git status --short
git add docs/desktop-app.md
git commit -m "docs: document native File Provider approval prompt"
```

- [ ] **Step 4: Final status**

Run:

```bash
git status --short
git log --oneline -5
```

Expected: no uncommitted changes except intentionally ignored dependency folders; recent commits include the spec and implementation commits.
