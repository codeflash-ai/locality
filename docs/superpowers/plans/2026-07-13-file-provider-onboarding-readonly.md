# File Provider Onboarding Read-Only Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let macOS onboarding recreate a missing `notion-main` mount when its existing File Provider mount-point directory has provider-managed read-only POSIX permissions.

**Architecture:** Split structural mount-path safety from ordinary filesystem writability validation. The macOS File Provider path uses structural checks plus the recognized-provider-root boundary, while other desktop projections retain the current writability checks.

**Tech Stack:** Rust, Tauri desktop backend, macOS File Provider, Cargo unit tests

---

### Task 1: Reproduce The Provider Mount-Point Validation Failure

**Files:**
- Modify: `apps/desktop/src-tauri/src/main.rs:11820-12020`

- [ ] **Step 1: Add the failing macOS regression test**

Add a test using an injected temporary provider root so it never touches the
user's real CloudStorage tree:

```rust
#[cfg(target_os = "macos")]
#[test]
fn macos_desktop_mount_accepts_read_only_file_provider_mount_point() {
    let temp = TestTempDir::new("desktop-read-only-file-provider-root");
    let provider_root = temp.path().join("Locality");
    let root = provider_root.join("notion");
    fs::create_dir_all(&root).expect("create provider mount point");

    let original_permissions = fs::metadata(&root)
        .expect("read provider mount point metadata")
        .permissions();
    let mut read_only_permissions = original_permissions.clone();
    read_only_permissions.set_readonly(true);
    fs::set_permissions(&root, read_only_permissions).expect("make mount point read-only");
    assert!(
        fs::metadata(&root)
            .expect("read updated mount point metadata")
            .permissions()
            .readonly()
    );

    let result = super::validate_macos_file_provider_mount_root(
        &root,
        &temp.path().join(".loc"),
        std::slice::from_ref(&provider_root),
    );

    fs::set_permissions(&root, original_permissions).expect("restore mount point permissions");
    result.expect("provider-owned mount point should not require a POSIX write bit");
}
```

- [ ] **Step 2: Run the test and verify RED**

```bash
cargo test -p locality-desktop macos_desktop_mount_accepts_read_only_file_provider_mount_point -- --nocapture
```

Expected: compilation fails because `validate_macos_file_provider_mount_root`
does not exist. The missing boundary is the behavior under test.

### Task 2: Make Desktop Mount Validation Projection-Aware

**Files:**
- Modify: `apps/desktop/src-tauri/src/main.rs:5702-5820`
- Modify: `apps/desktop/src-tauri/src/main.rs:11820-12020`

- [ ] **Step 1: Extract structural mount-path validation**

```rust
fn validate_mount_root_location(root: &Path, state_root: &Path) -> Result<PathBuf, String> {
    if root.as_os_str().is_empty() {
        return Err("Choose a folder for the Notion mount.".to_string());
    }

    let root = absolute_path(root)?;
    let state_root = absolute_path(state_root)?;
    if root.starts_with(&state_root) {
        return Err("Choose a folder outside the Locality state directory.".to_string());
    }
    Ok(root)
}
```

Update `validate_mount_root` to reuse the structural helper while retaining its
file-type, read-only, and parent-directory checks:

```rust
fn validate_mount_root(root: &Path, state_root: &Path) -> Result<(), String> {
    let root = validate_mount_root_location(root, state_root)?;

    if let Ok(metadata) = fs::metadata(&root) {
        if !metadata.is_dir() {
            return Err(format!("Choose a folder path, not a file: {}", root.display()));
        }
        if metadata.permissions().readonly() {
            return Err(format!("Selected folder is read-only: {}", root.display()));
        }
        return Ok(());
    }

    let parent = root
        .ancestors()
        .skip(1)
        .find(|candidate| candidate.exists())
        .ok_or_else(|| format!("No existing parent folder for {}", root.display()))?;
    let metadata = fs::metadata(parent).map_err(|error| {
        format!(
            "Could not inspect parent folder `{}`: {error}",
            parent.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!("Mount parent is not a folder: {}", parent.display()));
    }
    if metadata.permissions().readonly() {
        return Err(format!(
            "Mount parent folder is read-only: {}",
            parent.display()
        ));
    }
    Ok(())
}
```

- [ ] **Step 2: Add macOS File Provider structural validation**

```rust
#[cfg(target_os = "macos")]
fn validate_macos_file_provider_mount_root(
    root: &Path,
    state_root: &Path,
    provider_roots: &[PathBuf],
) -> Result<(), String> {
    let root = validate_mount_root_location(root, state_root)?;
    let provider_roots = provider_roots
        .iter()
        .filter_map(|provider_root| absolute_path(provider_root).ok())
        .collect::<Vec<_>>();
    let inside_provider_root = provider_roots
        .iter()
        .any(|provider_root| root.starts_with(provider_root) && root != *provider_root);
    if !inside_provider_root {
        return Err(format!(
            "Choose a mount point inside the Locality File Provider root, for example {}.",
            absolute_display_path(&default_notion_mount_root())
        ));
    }
    if let Ok(metadata) = fs::metadata(&root)
        && !metadata.is_dir()
    {
        return Err(format!(
            "Choose a folder path, not a file: {}",
            root.display()
        ));
    }
    Ok(())
}
```

Route only `ProjectionMode::MacosFileProvider` through that helper:

```rust
fn validate_desktop_mount_root(
    root: &Path,
    state_root: &Path,
    projection: &ProjectionMode,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if *projection == ProjectionMode::MacosFileProvider {
        return validate_macos_file_provider_mount_root(
            root,
            state_root,
            &macos_file_provider_cloud_storage_roots(),
        );
    }

    let _ = projection;
    validate_mount_root(root, state_root)
}
```

- [ ] **Step 3: Add an ordinary read-only-directory guard test**

```rust
#[test]
fn mount_validation_rejects_existing_read_only_directory() {
    let temp = TestTempDir::new("read-only-mount-root");
    let root = temp.path().join("Notion");
    fs::create_dir_all(&root).expect("create mount root");

    let original_permissions = fs::metadata(&root).expect("read metadata").permissions();
    let mut read_only_permissions = original_permissions.clone();
    read_only_permissions.set_readonly(true);
    fs::set_permissions(&root, read_only_permissions).expect("make mount read-only");
    let result = validate_mount_root(&root, &temp.path().join(".loc"));
    fs::set_permissions(&root, original_permissions).expect("restore permissions");

    assert_eq!(
        result.expect_err("ordinary read-only mount root rejected"),
        format!("Selected folder is read-only: {}", root.display())
    );
}
```

- [ ] **Step 4: Run focused tests and verify GREEN**

```bash
cargo test -p locality-desktop macos_desktop_mount_accepts_read_only_file_provider_mount_point -- --nocapture
cargo test -p locality-desktop mount_validation -- --nocapture
cargo test -p locality-desktop macos_desktop_mount_rejects_paths_outside_cloudstorage -- --nocapture
```

Expected: all selected tests pass.

- [ ] **Step 5: Commit the regression fix**

```bash
git add apps/desktop/src-tauri/src/main.rs
git commit -m "fix: accept existing File Provider mount points in onboarding"
```

### Task 3: Harden Provider Boundaries From Code Review

**Files:**
- Modify: `apps/desktop/src-tauri/src/main.rs:5741-5900`
- Test: `apps/desktop/src-tauri/src/main.rs:11900-12100`

- [ ] **Step 1: Reproduce provider-root escapes and invalid ancestors**

Add macOS tests that reject:

```rust
let traversing_root = provider_root.join("..").join("outside");
let symlink_root = provider_root.join("escape"); // symlink to outside
let child_below_file = provider_root.join("notion").join("child");
```

Run each focused filter and verify the initial helper accepts these invalid
paths before hardening:

```bash
cargo test -p locality-desktop macos_file_provider_mount_rejects_ -- --nocapture
```

- [ ] **Step 2: Share structural inspection and resolve containment canonically**

Replace the File Provider helper's metadata shortcut with the same structural
inspection used by ordinary mounts, parameterized only by writability:

```rust
let root = validate_mount_root_structure(root, state_root, false)?;
let resolved_root = resolved_mount_validation_path(&root)?;
let resolved_state_root = resolved_mount_validation_path(state_root)?;
```

`validate_mount_root_structure` must reject existing files, reject a file as
the nearest existing ancestor, propagate unexpected metadata errors, and skip
`permissions().readonly()` only when `require_writable` is false.
The File Provider validator must reject `ParentDir` components before path
resolution. `resolved_mount_validation_path` then canonicalizes the nearest
existing ancestor, appends only its missing tail, and compares the result
against canonical state and provider roots so symlinks cannot escape the
boundary, including a symlink followed by `..`.

- [ ] **Step 3: Exercise production projection dispatch**

Route production and tests through an injectable macOS dispatch helper:

```rust
validate_desktop_mount_root_with_macos_provider_roots(
    root,
    state_root,
    &ProjectionMode::MacosFileProvider,
    provider_roots,
)
```

The read-only regression must call this dispatch helper rather than the lower
level File Provider validator.

- [ ] **Step 4: Verify all hardened cases**

```bash
cargo test -p locality-desktop mount_validation -- --nocapture
cargo test -p locality-desktop macos_file_provider_mount_rejects -- --nocapture
cargo test -p locality-desktop macos_desktop_mount_accepts_read_only_file_provider_mount_point -- --nocapture
cargo test -p locality-desktop macos_desktop_mount_rejects_paths_outside_cloudstorage -- --nocapture
```

Expected: ordinary writability checks and all provider containment/type checks
pass.

### Task 4: Verify The Desktop Change And Prepare The PR

**Files:**
- Verify: `apps/desktop/src-tauri/src/main.rs`
- Verify: `apps/desktop/src/onboarding-mount.test.ts`
- Verify: `apps/desktop/src/onboarding-errors.test.ts`
- Verify: `apps/desktop/src/onboarding-flow.test.ts`

- [ ] **Step 1: Check Rust formatting and all desktop Rust tests**

```bash
cargo fmt --all -- --check
cargo test -p locality-desktop
```

Expected: formatting and all desktop Rust tests pass.

- [ ] **Step 2: Run onboarding frontend tests and the production build**

From `apps/desktop`:

```bash
npm test -- --run src/onboarding-mount.test.ts src/onboarding-errors.test.ts src/onboarding-flow.test.ts
npm run build
```

Expected: selected Vitest files and the TypeScript/Vite build pass.

- [ ] **Step 3: Review the final diff**

```bash
git diff origin/main...HEAD --check
git diff origin/main...HEAD --stat
git status --short --branch
```

Confirm only the approved design/plan and validator/tests changed. Confirm no
SQLite schema or persisted component version changed.

- [ ] **Step 4: Commit this implementation plan**

```bash
git add docs/superpowers/plans/2026-07-13-file-provider-onboarding-readonly.md
git commit -m "docs: plan File Provider onboarding validation fix"
```

- [ ] **Step 5: Push and open the pull request**

```bash
git push -u origin fix/onboarding-file-provider-readonly
gh pr create --base main --head fix/onboarding-file-provider-readonly --title "Fix onboarding for existing File Provider mount points" --body "## Summary
- accept provider-owned read-only mount-point directories during macOS onboarding
- retain ordinary local-folder writability checks
- add regression coverage for both paths

## Verification
- cargo fmt --all -- --check
- cargo test -p locality-desktop
- npm test -- --run src/onboarding-mount.test.ts src/onboarding-errors.test.ts src/onboarding-flow.test.ts
- npm run build"
```

The PR body must summarize the reproduced state mismatch, the narrow validator
change, and the exact verification commands run.
