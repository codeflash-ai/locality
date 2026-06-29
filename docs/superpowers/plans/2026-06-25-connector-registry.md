# Connector Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a first-party connector registry used by daemon source resolution and CLI-visible connector descriptors without adding non-Notion connector behavior.

**Architecture:** Keep `afs-connector` as the trait boundary and implement a small first-party registry inside `afsd::source`. The registry contains one supported runtime entry, Notion, and descriptor lookup continues to return generic metadata for unknown connector IDs where the product already supports generic guidance.

**Tech Stack:** Rust workspace, `afsd`, `afs-cli`, existing `afs-store` in-memory test utilities, existing Notion connector crate.

---

### Task 1: Add Registry Behavior Tests

**Files:**
- Modify: `crates/afsd/tests/source_descriptor.rs`

- [ ] **Step 1: Write failing tests**

Add tests that require a supported runtime registry and unsupported resolution behavior:

```rust
#[test]
fn supported_source_connectors_lists_runtime_registered_connectors() {
    assert_eq!(supported_source_connectors(), vec!["notion"]);
}

#[test]
fn resolving_unregistered_connector_reports_unsupported_connector() {
    let store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let mount = MountConfig::new(MountId::new("custom-main"), "custom", "/tmp/afs/custom");

    let error = resolve_source_for_mount(&store, &credentials, &mount).expect_err("unsupported");

    assert_eq!(error.code(), "unsupported_connector");
    assert_eq!(
        error.message(),
        "connector `custom` is not supported by this build"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p afsd --test source_descriptor
```

Expected: fails because `supported_source_connectors` is not exported or implemented.

- [ ] **Step 3: Implement minimal registry API**

Add a registry entry type and `supported_source_connectors()` in `crates/afsd/src/source.rs`. Route `source_descriptor` and `resolve_source_for_mount` through registry lookup.

- [ ] **Step 4: Run test to verify it passes**

Run:

```bash
cargo test -p afsd --test source_descriptor
```

Expected: all tests pass.

### Task 2: Centralize Guidance Descriptor Usage

**Files:**
- Modify: `crates/afsd/src/virtual_fs.rs`
- Test: `crates/afsd/tests/source_descriptor.rs`
- Test: existing virtual FS tests in `crates/afsd/src/virtual_fs.rs`

- [ ] **Step 1: Write failing test if needed**

If no existing test proves generic guidance remains unchanged, add a descriptor test that compares `source_descriptor("custom").mount_guidance()` with virtual guidance output through the public behavior that materializes `AGENTS.md`.

- [ ] **Step 2: Refactor virtual FS guidance**

Use `crate::source::source_descriptor(connector).mount_guidance()` instead of duplicating generic guidance generation inside `virtual_fs.rs`.

- [ ] **Step 3: Run focused tests**

Run:

```bash
cargo test -p afsd --test source_descriptor
cargo test -p afsd virtual_fs::tests::virtual_fs_source_root_includes_guidance_files
```

Expected: tests pass.

### Task 3: Keep Notion-Specific Paths Explicit

**Files:**
- Modify: `crates/afsd/src/source.rs`
- Modify only if required: `crates/afsd/src/reconcile.rs`
- Modify only if required: `crates/afs-cli/src/commands.rs`

- [ ] **Step 1: Inspect remaining Notion checks**

Run:

```bash
rg -n 'connector == "notion"|connector != "notion"|source_descriptor\("notion"\)|ResolvedNotionSource|NotionConnector' crates/afsd/src crates/afs-cli/src
```

Expected: remaining matches are either Notion-only product behavior or tests.

- [ ] **Step 2: Rename or isolate generic-looking Notion coupling**

Move only generic-looking registry coupling behind registry helpers. Leave Notion URL search, OAuth connect, database schema repair, and push semantics Notion-named.

- [ ] **Step 3: Run focused CLI/daemon tests**

Run:

```bash
cargo test -p afsd --test source_descriptor
cargo test -p afs-cli --test mount
```

Expected: tests pass.

### Task 4: Update Connector SDK Docs

**Files:**
- Modify: `docs/connector-sdk.md`

- [ ] **Step 1: Add registry documentation**

Document that first-party connectors are registered in the daemon source registry and that CLI descriptor metadata is consumed from that same registry.

- [ ] **Step 2: Verify docs are consistent**

Run:

```bash
rg -n 'registry|descriptor|first-party' docs/connector-sdk.md
```

Expected: docs mention the registry and descriptor path.

### Task 5: Final Verification

**Files:**
- No new files.

- [ ] **Step 1: Run targeted tests**

Run:

```bash
cargo test -p afsd --test source_descriptor
cargo test -p afs-cli --test mount
```

Expected: all targeted tests pass.

- [ ] **Step 2: Run broader compile check**

Run:

```bash
cargo test -p afsd --lib
cargo test -p afs-cli --lib
```

Expected: both library test suites pass.

