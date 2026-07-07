# Page Directory Pull Hydration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `loc pull <page-directory>` hydrate that directory's own `page.md` consistently across plain-files, Linux FUSE, Windows Cloud Files, and macOS File Provider mounts.

**Architecture:** Refactor pull target dispatch in `crates/localityd/src/pull.rs` so page directories are classified explicitly instead of falling through mount-root or virtual-only branches. Reuse the existing entity hydration path for the target page itself, then preserve the current child enumeration and recursive descendant hydration flow.

**Tech Stack:** Rust, Cargo integration tests, Locality pull orchestration, virtual filesystem projections, File Provider projection refresh.

---

### Task 1: Write The Failing Regressions

**Files:**
- Modify: `crates/loc-cli/tests/pull.rs`
- Modify: `crates/loc-cli/tests/e2e_push_workflow.rs`
- Test: `crates/loc-cli/tests/pull.rs`
- Test: `crates/loc-cli/tests/e2e_push_workflow.rs`

- [ ] **Step 1: Update the virtual page-directory regression to expect target hydration**

```rust
assert_eq!(report.enumerated, 3);
assert_eq!(report.hydrated, 3);

let target = store
    .find_entity_by_path(&fixture.mount_id, &PathBuf::from("roadmap/page.md"))?
    .expect("target entity");
assert_eq!(target.hydration, HydrationState::Hydrated);
assert!(content_root.join("roadmap/page.md").exists());
```

- [ ] **Step 2: Run the focused virtual regression and confirm it fails for the current behavior**

Run: `cargo test -p loc-cli --test pull pull_virtual_page_directory_recursively_hydrates_child_pages -- --exact`

Expected: `FAIL` because `report.hydrated` is still `2` and/or `roadmap/page.md` is not hydrated.

- [ ] **Step 3: Update the visible File Provider regression to expect the target page body**

```rust
assert_eq!(report.hydrated, 3);

let visible_target = fixture.mount_point_root().join("roadmap").join("page.md");
assert!(visible_target.exists());
assert!(fs::read_to_string(&visible_target)?.contains("Root body."));
```

- [ ] **Step 4: Add a plain-files regression for `loc pull <page-directory>`**

```rust
#[test]
fn pull_plain_files_page_directory_hydrates_target_and_descendants() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector_with_nested_child("Roadmap");
    run_pull(&mut store, &connector, &fixture.root).expect("initial plain-files pull");

    let report = run_pull(&mut store, &connector, fixture.root.join("roadmap"))
        .expect("pull plain-files page directory");

    assert!(report.ok);
    assert_eq!(report.hydrated, 3);
    assert!(fs::read_to_string(fixture.root.join("roadmap/page.md"))
        .expect("target page")
        .contains("Root body."));
}
```

- [ ] **Step 5: Update the cross-projection e2e regression to expect the target page itself to hydrate**

```rust
assert_eq!(recursive_pull.hydrated, 3);

let target_entity = store
    .find_entity_by_path(&fixture.mount_id, &PathBuf::from("Roadmap/Design Notes/page.md"))?
    .expect("target entity");
assert_eq!(target_entity.hydration, HydrationState::Hydrated);
assert!(target_page_path.exists());
assert!(fs::read_to_string(&target_page_path)?.contains("Design notes body."));
```

- [ ] **Step 6: Run the focused regression set and confirm it fails before production changes**

Run: `cargo test -p loc-cli --test pull pull_virtual_page_directory_recursively_hydrates_child_pages pull_file_provider_page_directory_materializes_visible_child_pages pull_plain_files_page_directory_hydrates_target_and_descendants`

Run: `cargo test -p loc-cli --test e2e_push_workflow virtual_projection_modes_pull_page_directory_hydrates_descendants_but_not_target_page_body -- --exact`

Expected:
- `pull_virtual_page_directory_recursively_hydrates_child_pages` fails because target `roadmap/page.md` stays unhydrated.
- `pull_file_provider_page_directory_materializes_visible_child_pages` fails because visible `roadmap/page.md` is absent or still a stub.
- `pull_plain_files_page_directory_hydrates_target_and_descendants` fails because plain-files directory pull is treated as a mount-root path.
- the e2e test fails because the target `Roadmap/Design Notes/page.md` remains stubbed.

- [ ] **Step 7: Commit the failing test changes**

```bash
git add crates/loc-cli/tests/pull.rs crates/loc-cli/tests/e2e_push_workflow.rs
git commit -m "test: capture page-directory pull hydration regression"
```

### Task 2: Implement Shared Page-Directory Pull Dispatch

**Files:**
- Modify: `crates/localityd/src/pull.rs`
- Test: `crates/loc-cli/tests/pull.rs`
- Test: `crates/loc-cli/tests/e2e_push_workflow.rs`

- [ ] **Step 1: Add explicit pull target classification in `crates/localityd/src/pull.rs`**

```rust
enum PullTarget {
    MountRoot,
    Entity(PathBuf),
    PageDirectory {
        page: EntityRecord,
        directory_path: PathBuf,
    },
    DatabaseDirectory(VirtualDirectoryTarget),
}
```

```rust
fn classify_pull_target<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
    target_path: &Path,
) -> Result<PullTarget, PullError>
where
    S: EntityRepository,
{
    if should_pull_mount_root(mount, relative_path, target_path) {
        return Ok(PullTarget::MountRoot);
    }

    if let Some(page) = store
        .find_entity_by_path(&mount.mount_id, &relative_path.join("page.md"))
        .map_err(PullError::Store)?
        .filter(|entity| entity.kind == EntityKind::Page)
    {
        return Ok(PullTarget::PageDirectory {
            page,
            directory_path: relative_path.to_path_buf(),
        });
    }

    Ok(PullTarget::Entity(relative_path.to_path_buf()))
}
```

- [ ] **Step 2: Route `run_pull_with_state_root()` through the classifier**

```rust
let report = match classify_pull_target(store, &mount, &relative_path, &target_path)? {
    PullTarget::MountRoot => pull_mount_root(...),
    PullTarget::PageDirectory { page, directory_path } => {
        pull_page_directory_path(store, &source, &mount, page, directory_path, target_path.clone(), state_root)
    }
    PullTarget::DatabaseDirectory(target) => pull_virtual_database_directory_path(...),
    PullTarget::Entity(path) => pull_entity_path(store, &source, &mount, &path, target_path.clone(), state_root),
}?;    
```

- [ ] **Step 3: Implement `pull_page_directory_path()` with target-first hydration**

```rust
fn pull_page_directory_path<S, Source>(
    store: &mut S,
    source: &Source,
    mount: &MountConfig,
    page: EntityRecord,
    directory_path: PathBuf,
    target_path: PathBuf,
    state_root: Option<&Path>,
) -> Result<PullReport, PullError>
where
    S: EntityRepository + ShadowRepository + locality_store::FreshnessStateRepository + locality_store::RemoteObservationRepository,
    Source: SourceAdapter,
{
    let target_outcome = hydrate_entity(store, source, mount, page.clone(), state_root)?;
    let mut report = pull_outcome_report(&target_outcome);
    let child_report = enumerate_and_hydrate_page_descendants(
        store,
        source,
        mount,
        page,
        &directory_path,
        state_root,
    )?;
    report.enumerated += child_report.enumerated;
    report.hydrated += child_report.hydrated;
    report.skipped_dirty += child_report.skipped_dirty;
    report.conflicts.extend(child_report.conflicts);
    Ok(finalize_pull_report(report, mount, target_path))
}
```

- [ ] **Step 4: Keep database-directory behavior intact by narrowing the virtual directory helper**

```rust
fn classify_virtual_directory_target(...) -> Result<Option<VirtualDirectoryTarget>, PullError> { ... }

if let Some(target) = classify_virtual_directory_target(...) {
    return pull_virtual_database_directory_path(..., target, ...);
}
```

The important constraint is that page directories no longer enter the old
virtual-directory branch that skips the target page body.

- [ ] **Step 5: Reuse recursive child hydration instead of re-implementing it**

```rust
let result = source.list_children(ListChildrenRequest {
    mount_id: mount.mount_id.clone(),
    container: ChildContainer::PageChildren(page.remote_id.clone()),
    parent_path: directory_path.clone(),
})?;
```

```rust
let recursive = hydrate_page_descendants(
    store,
    source,
    mount,
    child_page_ids,
    state_root,
    &mut visited,
)?;
```

- [ ] **Step 6: Run the focused regression set and confirm it passes**

Run: `cargo test -p loc-cli --test pull pull_virtual_page_directory_recursively_hydrates_child_pages -- --exact`

Run: `cargo test -p loc-cli --test pull pull_file_provider_page_directory_materializes_visible_child_pages -- --exact`

Run: `cargo test -p loc-cli --test pull pull_plain_files_page_directory_hydrates_target_and_descendants -- --exact`

Run: `cargo test -p loc-cli --test e2e_push_workflow virtual_projection_modes_pull_page_directory_hydrates_descendants_but_not_target_page_body -- --exact`

Expected: all four tests `PASS`, with the target page counted in `hydrated`.

- [ ] **Step 7: Commit the production pull fix**

```bash
git add crates/localityd/src/pull.rs crates/loc-cli/tests/pull.rs crates/loc-cli/tests/e2e_push_workflow.rs
git commit -m "fix: hydrate target page when pulling page directories"
```

### Task 3: Update Docs And Verify End-To-End

**Files:**
- Modify: `docs/cli.md`
- Test: `crates/loc-cli/tests/pull.rs`
- Test: `crates/loc-cli/tests/e2e_push_workflow.rs`

- [ ] **Step 1: Update the CLI docs to make page-directory semantics explicit**

```md
For page directories, `loc pull <page-directory>` hydrates that directory's own
`page.md` and recursively refreshes child page directories below it. Database
directories keep their existing row-listing and small-database row hydration
behavior.
```

- [ ] **Step 2: Run formatting and the focused regression suite**

Run: `cargo fmt --all`

Run: `cargo test -p loc-cli --test pull`

Run: `cargo test -p loc-cli --test e2e_push_workflow virtual_projection_modes_pull_page_directory_hydrates_descendants_but_not_target_page_body -- --exact`

Expected:
- `cargo fmt --all` exits `0`
- `cargo test -p loc-cli --test pull` exits `0`
- the focused e2e test exits `0`

- [ ] **Step 3: Verify the installed macOS File Provider behavior**

Run:

```bash
loc pull '/Users/aseemsaxena/Library/CloudStorage/Locality/notion/Workspace/teamspace-home/recursive-pull-e2e-20260630t151630z' --json
loc status '/Users/aseemsaxena/Library/CloudStorage/Locality/notion/Workspace/teamspace-home/recursive-pull-e2e-20260630t151630z' --json
```

Expected:
- the pull report includes the target page in `hydrated`
- `loc status` shows the target directory's own `page.md` as `clean` rather than `stub`
- opening the visible `page.md` yields the remote body

- [ ] **Step 4: Commit docs and verification-ready changes**

```bash
git add docs/cli.md
git commit -m "docs: clarify page-directory pull hydration"
```

## Self-Review

Spec coverage:
- Target page hydration across mount types is covered by Task 1 and Task 2.
- Database-directory behavior staying unchanged is covered by Task 2 and Task 3 test runs.
- Docs clarification is covered by Task 3.
- Live macOS File Provider verification is covered by Task 3.

Placeholder scan:
- No `TODO`, `TBD`, or deferred "handle later" steps remain.
- Every run step includes an exact command and expected result.

Type consistency:
- The plan consistently refers to a new page-directory dispatch helper and uses
  the existing `hydrate_entity` and `hydrate_page_descendants` helpers rather
  than inventing conflicting paths.
