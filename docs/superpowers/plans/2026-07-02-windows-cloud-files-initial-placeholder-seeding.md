# Windows Cloud Files Initial Placeholder Seeding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a fresh Windows Cloud Files provider startup create immediate children under the shared `notion` mount-point without requiring a provider restart.

**Architecture:** Keep the fix inside `locality-cloud-files.exe`, the Windows provider adapter that owns Cloud Files placeholder creation. The daemon continues to expose metadata through `context.children`; the provider startup path now treats root-level folder placeholders created during the current seed batch as child-seeding targets, while nested recursion remains limited to directories that already exist.

**Tech Stack:** Rust 2024 workspace, Windows Cloud Files helper, `localityd` virtual file provider metadata APIs, `cargo test`.

---

### Task 1: Add Failing Unit Coverage For Fresh Root Seed Targets

**Files:**
- Modify: `platform/windows/locality-cloud-files/src/main.rs`

- [ ] **Step 1: Add the failing test**

Insert this test in `mod tests` near `existing_child_directory_seed_targets_include_nested_existing_directories`, before `folder_item`:

```rust
    #[cfg(target_os = "windows")]
    #[test]
    fn seeded_child_directory_targets_include_root_folder_items_without_preexisting_directory() {
        let temp = unique_test_state_dir("seeded-root-child-dirs");
        let sync_root = temp.join("Locality");
        std::fs::create_dir_all(&sync_root).expect("create sync root");

        let root_items = vec![folder_item("mount:notion-main", "notion-main")];
        let mut child_map = std::collections::BTreeMap::from([(
            "mount:notion-main".to_string(),
            vec![
                folder_item("children:company", "company"),
                folder_item("children:tech", "tech"),
            ],
        )]);
        let mut targets = Vec::new();

        collect_seeded_child_directory_seed_targets(
            &sync_root,
            &root_items,
            &mut |identifier| {
                Ok(child_map
                    .remove(identifier)
                    .unwrap_or_else(|| panic!("unexpected child listing for {identifier}")))
            },
            &mut targets,
        )
        .expect("collect seeded root targets");

        let target_paths = targets
            .iter()
            .map(|(path, _)| path.strip_prefix(&sync_root).unwrap().to_path_buf())
            .collect::<Vec<_>>();
        assert_eq!(target_paths, vec![PathBuf::from("notion-main")]);
        assert_eq!(
            targets[0]
                .1
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["company", "tech"]
        );

        let _ = std::fs::remove_dir_all(temp);
    }
```

- [ ] **Step 2: Run the focused test and confirm it fails**

Run:

```powershell
cargo test -p locality-cloud-files seeded_child_directory_targets_include_root_folder_items_without_preexisting_directory
```

Expected: failure to compile with `cannot find function collect_seeded_child_directory_seed_targets` or equivalent unresolved-name output.

- [ ] **Step 3: Commit the failing test only if your workflow requires red-test commits**

Default: do not commit the failing state. Keep it local and continue to Task 2.

### Task 2: Seed Root Folder Items In The Same Provider Startup

**Files:**
- Modify: `platform/windows/locality-cloud-files/src/main.rs`

- [ ] **Step 1: Replace the root nested seeding call**

Change `seed_root_placeholders` from:

```rust
    seed_existing_child_directory_placeholders(context, &context.sync_root, &children.children)?;
```

to:

```rust
    seed_child_directory_placeholders(context, &context.sync_root, &children.children)?;
```

- [ ] **Step 2: Replace `seed_existing_child_directory_placeholders` with root-aware seeding**

Replace the whole `seed_existing_child_directory_placeholders` function with:

```rust
#[cfg(target_os = "windows")]
fn seed_child_directory_placeholders(
    context: &ProviderContext,
    directory: &Path,
    items: &[localityd::file_provider::FileProviderItem],
) -> Result<(), HelperError> {
    let mut targets = Vec::new();
    collect_seeded_child_directory_seed_targets(
        directory,
        items,
        &mut |identifier| context.children(identifier).map(|report| report.children),
        &mut targets,
    )?;
    for (child_directory, children) in targets {
        trace_cloud_files(format!(
            "seed child directory placeholders directory=`{}` count={}",
            child_directory.display(),
            children.len()
        ));
        create_placeholders_in_directory(&child_directory, &children)?;
        remember_placeholder_children(context, &child_directory, &children);
    }
    Ok(())
}
```

- [ ] **Step 3: Add a root-aware collection helper**

Insert this function immediately before `collect_existing_child_directory_seed_targets`:

```rust
#[cfg(target_os = "windows")]
fn collect_seeded_child_directory_seed_targets<F>(
    directory: &Path,
    items: &[localityd::file_provider::FileProviderItem],
    children_for: &mut F,
    targets: &mut Vec<(PathBuf, Vec<localityd::file_provider::FileProviderItem>)>,
) -> Result<(), HelperError>
where
    F: FnMut(&str) -> Result<Vec<localityd::file_provider::FileProviderItem>, HelperError>,
{
    for item in child_directory_items(items) {
        let child_directory = directory.join(&item.filename);
        let children = children_for(&item.identifier)?;
        targets.push((child_directory.clone(), children.clone()));
        collect_existing_child_directory_seed_targets(
            &child_directory,
            &children,
            children_for,
            targets,
        )?;
    }
    Ok(())
}
```

- [ ] **Step 4: Add a shared folder-item filter**

Insert this function immediately before `existing_child_directory_items`:

```rust
#[cfg(target_os = "windows")]
fn child_directory_items(
    items: &[localityd::file_provider::FileProviderItem],
) -> Vec<localityd::file_provider::FileProviderItem> {
    items
        .iter()
        .filter(|item| item.kind == localityd::file_provider::FileProviderItemKind::Folder)
        .cloned()
        .collect()
}
```

- [ ] **Step 5: Simplify `existing_child_directory_items` to reuse the filter**

Replace the loop header and folder-kind guard in `existing_child_directory_items`:

```rust
    for item in items {
        if item.kind != localityd::file_provider::FileProviderItemKind::Folder {
            continue;
        }
        let child_path = directory.join(&item.filename);
```

with:

```rust
    for item in child_directory_items(items) {
        let child_path = directory.join(&item.filename);
```

Keep the existing `try_exists`, `is_dir`, and error handling unchanged.

- [ ] **Step 6: Run the focused test and confirm it passes**

Run:

```powershell
cargo test -p locality-cloud-files seeded_child_directory_targets_include_root_folder_items_without_preexisting_directory
```

Expected: test passes.

- [ ] **Step 7: Commit the implementation**

Run:

```powershell
git add platform/windows/locality-cloud-files/src/main.rs
git commit -m "fix: seed Windows Cloud Files mount children on startup"
```

### Task 3: Verify Existing Seeding Behavior Still Works

**Files:**
- Verify: `platform/windows/locality-cloud-files/src/main.rs`

- [ ] **Step 1: Run the existing nested-directory regression test**

Run:

```powershell
cargo test -p locality-cloud-files existing_child_directory_seed_targets_include_nested_existing_directories
```

Expected: test passes. This proves nested recursion still only follows existing visible directories beyond the first root seed level.

- [ ] **Step 2: Run the existing preexisting-folder filter test**

Run:

```powershell
cargo test -p locality-cloud-files existing_child_directory_items_only_returns_preexisting_folders
```

Expected: test passes. This proves `existing_child_directory_items` still ignores missing mount-point folders when used for existing-only recursion.

- [ ] **Step 3: Run the helper test group**

Run:

```powershell
cargo test -p locality-cloud-files child_directory
```

Expected: all tests with `child_directory` in the name pass, including the new seeded-root test and the existing regression tests.

- [ ] **Step 4: Check git status**

Run:

```powershell
git status --short
```

Expected: only unrelated pre-existing desktop files may remain modified:

```text
 M apps/desktop/src-tauri/Cargo.toml
 M apps/desktop/src-tauri/gen/schemas/desktop-schema.json
 M apps/desktop/src-tauri/gen/schemas/windows-schema.json
```

Do not include those desktop files in this fix.

### Task 4: Optional Installed-App Smoke Verification

**Files:**
- Verify installed runtime only; no source edits.

- [ ] **Step 1: Rebuild or reinstall the app-side helper if this branch is being tested in the installed app**

Use the existing project install workflow for Windows. If no install workflow is requested, skip this task and report that source-level tests were run only.

- [ ] **Step 2: Start from a fresh or reset Windows Cloud Files projection**

Use the app reset/uninstall path only if the user explicitly approves state reset. Do not delete `C:\Users\vm-user\Locality` or SQLite state manually.

- [ ] **Step 3: Verify first startup shows mount children without restart**

Run:

```powershell
Get-ChildItem -Force C:\Users\vm-user\Locality\notion | Select-Object -First 20 Name
```

Expected: connector content folders such as `company`, `engineering-wiki`, and `tech` are visible before running any `loc file-provider restart`.

- [ ] **Step 4: Verify a known file is readable and clean**

Run:

```powershell
Get-Content -TotalCount 8 C:\Users\vm-user\Locality\notion\company\page.md
loc status C:\Users\vm-user\Locality\notion\company\page.md --json
```

Expected: `Get-Content` prints Locality frontmatter for `Company`, and `loc status` reports one clean/all-synced entry.
