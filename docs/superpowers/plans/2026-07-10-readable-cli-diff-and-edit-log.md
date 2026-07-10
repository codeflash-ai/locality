# Readable CLI Diff And Edit Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a readable Git-style Locality diff for CLI review before push, and attach those diffs to a journal-backed edit log with anonymous authors and previous-edit links.

**Architecture:** Add a focused readable-diff renderer in `locality-core`, attach the reviewed diff to `localityd::push::PreparedPush`, and print that value from `loc diff` plus interactive `loc push` confirmation. Persist the same prepared diff and anonymous edit metadata on push journals so `loc log --diff` can show each edit and its previous journal link without re-planning old local files.

**Tech Stack:** Rust 2024, `loc-cli`, `locality-core`, `locality-store` SQLite schema migrations, `localityd` push execution, `similar = "3.1.1"` for unified line diffs, existing `cargo test` integration suites.

---

## Current Context

- `loc diff` already previews the same push plan used by `loc push` through `crates/loc-cli/src/diff.rs`.
- `loc push` first runs a CLI preview through `run_push_with_state_root`, verifies daemon parity, then applies through `localityd`.
- Interactive `loc push` currently prints only the plan summary before `Proceed with push? [y/N]`.
- Push journals live in `locality-core::journal::JournalEntry` and are persisted in `locality-store` table `journals`.
- Journals currently store `push_id`, mount, remote ids, plan, preimages, apply effects, and status. They do not store author metadata, previous journal links, timestamps, or human-readable diffs.
- `loc log [path]` already lists push journal entries, so it is the right CLI surface for the linked edit list.

## File Structure

- Modify `crates/locality-core/Cargo.toml`
  - Add the `similar` dependency for unified line diffs.
- Modify `crates/locality-core/src/lib.rs`
  - Export the new readable diff module.
- Create `crates/locality-core/src/readable_diff.rs`
  - Own the shared readable diff data types and rendering.
- Modify `crates/localityd/src/push.rs`
  - Build `PreparedPush.readable_diff` while the prepared local text and shadow are both in memory.
  - Compute anonymous journal metadata and attach the readable diff snapshot before journal append.
- Modify `crates/loc-cli/src/diff.rs`
  - Attach `readable_diff` to `DiffReport`.
  - Copy `prepared.readable_diff` into the CLI report.
- Modify `crates/loc-cli/src/push.rs`
  - Carry `readable_diff` through `PushReport`.
- Modify `crates/loc-cli/src/commands.rs`
  - Print readable diffs from `loc diff`.
  - Print readable diffs before interactive `loc push` confirmation.
  - Add `loc log --diff` and `loc log --push-id <id>` argument handling.
- Modify `crates/locality-core/src/journal.rs`
  - Add durable anonymous author metadata, previous journal link, created timestamp, and readable diff snapshot fields to `JournalEntry`.
- Modify `crates/locality-store/src/repository.rs`
  - Add a default journal helper for finding the previous journal affecting the same mount/entities.
- Modify `crates/locality-store/src/memory.rs`
  - Preserve new journal metadata and readable diff fields.
- Modify `crates/locality-store/src/sqlite.rs`
  - Bump `SCHEMA_VERSION` from `16` to `17`.
  - Add `metadata_json` and `readable_diff_json` columns to `journals`.
  - Migrate older journals with anonymous metadata and no readable diff.
- Modify `crates/locality-store/tests/sqlite.rs`
  - Cover schema version, migration, and journal round-trip.
- Modify `crates/locality-store/tests/repository.rs`
  - Cover previous-journal lookup in memory store.
- Modify `crates/loc-cli/src/history.rs`
  - Expose author, previous push id, timestamp, and optional readable diff in `loc log`.
- Modify `crates/loc-cli/tests/diff.rs`
  - Cover readable diff output on existing edits and created entities.
- Modify `crates/loc-cli/tests/push.rs`
  - Cover readable diff carried through preview reports.
- Modify `crates/loc-cli/tests/e2e_push_workflow.rs`
  - Cover journaled readable diffs after a real daemon push.
- Modify `docs/cli.md`
  - Document readable `loc diff`, interactive push review, and `loc log --diff`.

## Data Contracts

### Readable Diff Output

Use this shape in `crates/locality-core/src/readable_diff.rs` and reuse it from CLI JSON and journal snapshots:

```rust
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadableDiffOutput {
    pub files: Vec<ReadableDiffFileOutput>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadableDiffFileOutput {
    pub path: String,
    pub old_label: String,
    pub new_label: String,
    pub status: ReadableDiffFileStatus,
    pub patch: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadableDiffFileStatus {
    Modified,
    Added,
    Deleted,
}
```

### Journal Metadata

Use this shape in `crates/locality-core/src/journal.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalMetadata {
    pub author: JournalAuthor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_push_id: Option<PushId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_unix_ms: Option<u128>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalAuthor {
    pub kind: JournalAuthorKind,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalAuthorKind {
    Anonymous,
}
```

Use `display_name: "anonymous"` for now. Do not attempt OS user, Git author, or remote user attribution in this change.

## Tasks

### Task 1: Add The Readable Diff Renderer

**Files:**
- Modify: `crates/locality-core/Cargo.toml`
- Modify: `crates/locality-core/src/lib.rs`
- Create: `crates/locality-core/src/readable_diff.rs`

- [ ] **Step 1: Add the diff dependency**

Edit `crates/locality-core/Cargo.toml`:

```toml
serde = { version = "1.0", features = ["derive"] }
similar = "3.1.1"
```

- [ ] **Step 2: Export the module**

Edit `crates/locality-core/src/lib.rs`:

```rust
pub mod push;
pub mod readable_diff;
pub mod shadow;
```

- [ ] **Step 3: Add focused renderer tests**

Create `crates/locality-core/src/readable_diff.rs` with the data types from "Readable Diff Output" and these tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_modified_file_as_unified_diff() {
        let diff = readable_diff_for_file(
            "Roadmap.md",
            Some("Old paragraph.\n"),
            Some("Changed paragraph.\n"),
        )
        .expect("diff");

        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Modified);
        assert!(diff.text.contains("diff --locality a/Roadmap.md b/Roadmap.md"), "{}", diff.text);
        assert!(diff.text.contains("--- a/Roadmap.md"), "{}", diff.text);
        assert!(diff.text.contains("+++ b/Roadmap.md"), "{}", diff.text);
        assert!(diff.text.contains("-Old paragraph."), "{}", diff.text);
        assert!(diff.text.contains("+Changed paragraph."), "{}", diff.text);
    }

    #[test]
    fn returns_none_when_text_is_unchanged() {
        let diff = readable_diff_for_file("Roadmap.md", Some("Same.\n"), Some("Same.\n"));

        assert_eq!(diff, None);
    }

    #[test]
    fn renders_added_file_against_dev_null() {
        let diff = readable_diff_for_file("Tasks/new.md", None, Some("# New\n")).expect("diff");

        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Added);
        assert!(diff.text.contains("--- /dev/null"), "{}", diff.text);
        assert!(diff.text.contains("+++ b/Tasks/new.md"), "{}", diff.text);
        assert!(diff.text.contains("+# New"), "{}", diff.text);
    }
}
```

- [ ] **Step 4: Run renderer tests and verify they fail**

Run:

```bash
cargo test -p locality-core readable_diff --lib
```

Expected:

```text
error[E0425]: cannot find function `readable_diff_for_file` in this scope
```

- [ ] **Step 5: Implement the renderer**

Add this implementation to `crates/locality-core/src/readable_diff.rs`:

```rust
use similar::TextDiff;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadableDiffOutput {
    pub files: Vec<ReadableDiffFileOutput>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadableDiffFileOutput {
    pub path: String,
    pub old_label: String,
    pub new_label: String,
    pub status: ReadableDiffFileStatus,
    pub patch: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadableDiffFileStatus {
    Modified,
    Added,
    Deleted,
}

pub fn readable_diff_for_file(
    path: impl Into<String>,
    old_text: Option<&str>,
    new_text: Option<&str>,
) -> Option<ReadableDiffOutput> {
    let path = path.into();
    let old_text = old_text.unwrap_or("");
    let new_text = new_text.unwrap_or("");
    if old_text == new_text {
        return None;
    }

    let status = match (old_text.is_empty(), new_text.is_empty()) {
        (true, false) => ReadableDiffFileStatus::Added,
        (false, true) => ReadableDiffFileStatus::Deleted,
        _ => ReadableDiffFileStatus::Modified,
    };
    let old_label = match status {
        ReadableDiffFileStatus::Added => "/dev/null".to_string(),
        ReadableDiffFileStatus::Modified | ReadableDiffFileStatus::Deleted => format!("a/{path}"),
    };
    let new_label = match status {
        ReadableDiffFileStatus::Deleted => "/dev/null".to_string(),
        ReadableDiffFileStatus::Modified | ReadableDiffFileStatus::Added => format!("b/{path}"),
    };
    let patch_body = TextDiff::from_lines(old_text, new_text)
        .unified_diff()
        .header(&old_label, &new_label)
        .context_radius(3)
        .to_string();
    let patch = format!("diff --locality {old_label} {new_label}\n{patch_body}");

    Some(ReadableDiffOutput {
        text: patch.clone(),
        files: vec![ReadableDiffFileOutput {
            path,
            old_label,
            new_label,
            status,
            patch,
        }],
    })
}

pub fn join_readable_diffs(diffs: impl IntoIterator<Item = ReadableDiffOutput>) -> Option<ReadableDiffOutput> {
    let mut files = Vec::new();
    let mut text = String::new();
    for diff in diffs {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&diff.text);
        files.extend(diff.files);
    }

    if files.is_empty() {
        None
    } else {
        Some(ReadableDiffOutput { files, text })
    }
}
```

- [ ] **Step 6: Run renderer tests and verify they pass**

Run:

```bash
cargo test -p locality-core readable_diff --lib
```

Expected:

```text
test result: ok
```

- [ ] **Step 7: Commit**

Run:

```bash
git add crates/locality-core/Cargo.toml crates/locality-core/src/lib.rs crates/locality-core/src/readable_diff.rs Cargo.lock
git commit -m "feat(core): add readable diff renderer"
```

### Task 2: Attach Readable Diff To `loc diff`

**Files:**
- Modify: `crates/loc-cli/src/diff.rs`
- Modify: `crates/loc-cli/tests/diff.rs`

- [ ] **Step 1: Write failing `run_diff` report test**

Add this test after `diff_reports_safe_plan_as_confirmation_needed` in `crates/loc-cli/tests/diff.rs`:

```rust
#[test]
fn diff_report_includes_readable_patch_for_existing_page_edit() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.\n");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph.\n"))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");
    let readable = report.readable_diff.expect("readable diff");

    assert!(readable.text.contains("diff --locality a/Roadmap.md b/Roadmap.md"), "{}", readable.text);
    assert!(readable.text.contains("-Old paragraph."), "{}", readable.text);
    assert!(readable.text.contains("+Changed paragraph."), "{}", readable.text);
}
```

- [ ] **Step 2: Run the failing test**

Run:

```bash
cargo test -p loc-cli --test diff diff_report_includes_readable_patch_for_existing_page_edit
```

Expected:

```text
error[E0609]: no field `readable_diff` on type `DiffReport`
```

- [ ] **Step 3: Add `readable_diff` field to `DiffReport`**

Edit `crates/loc-cli/src/diff.rs`:

```rust
use locality_core::readable_diff::ReadableDiffOutput;
```

Add to `DiffReport`:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readable_diff: Option<ReadableDiffOutput>,
```

Set it to `None` in `DiffReport::from_pipeline`:

```rust
            readable_diff: None,
```

- [ ] **Step 4: Store readable diff on prepared pushes**

Edit `crates/localityd/src/push.rs` and add this field to `PreparedPush`:

```rust
    pub readable_diff: Option<locality_core::readable_diff::ReadableDiffOutput>,
```

Set `readable_diff: None` in validation-only `PreparedPush` initializers. For existing hydrated page edits, compute the field after `augment_notion_media_plan`:

```rust
let readable_diff = readable_diff_for_existing_entity(&relative_path, &shadow, &contents, &pipeline);
Ok(PreparedPush {
    absolute_path,
    mount,
    entity,
    shadows: vec![shadow],
    pipeline,
    readable_diff,
})
```

Add these private helpers in `crates/localityd/src/push.rs`:

```rust
fn readable_diff_for_existing_entity(
    relative_path: &Path,
    shadow: &ShadowDocument,
    local_text: &str,
    pipeline: &PushPipelineResult,
) -> Option<locality_core::readable_diff::ReadableDiffOutput> {
    let plan = pipeline.plan.as_ref()?;
    if plan.operations.is_empty() {
        return None;
    }
    let old = render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    ));
    locality_core::readable_diff::readable_diff_for_file(
        locality_platform::logical_path_display(relative_path),
        Some(&old),
        Some(local_text),
    )
}

fn readable_diff_for_created_entity(
    source_path: &Path,
    body: &str,
    pipeline: &PushPipelineResult,
) -> Option<locality_core::readable_diff::ReadableDiffOutput> {
    let plan = pipeline.plan.as_ref()?;
    if plan.operations.is_empty() {
        return None;
    }
    locality_core::readable_diff::readable_diff_for_file(
        locality_platform::logical_path_display(source_path),
        None,
        Some(body),
    )
}
```

In `prepare_pending_create_from_parsed`, compute:

```rust
let readable_diff = readable_diff_for_created_entity(
    &pending.projected_path,
    &parsed.document.body,
    &pipeline,
);
```

and include `readable_diff` in the returned `PreparedPush`.

- [ ] **Step 5: Copy prepared readable diff into `DiffReport`**

Edit `run_preview_artifacts_with_state_root` in `crates/loc-cli/src/diff.rs` so it creates the report as mutable and populates the diff:

```rust
    let mut report = DiffReport::from_pipeline(
        options.command,
        prepared.absolute_path.clone(),
        &prepared.mount,
        entity_id.clone(),
        pipeline.clone(),
    );
    report.readable_diff = prepared.readable_diff.clone();
```

- [ ] **Step 6: Fix direct struct literals in command tests**

Update the `report(ok: bool) -> DiffReport` helper in `crates/loc-cli/src/commands.rs` tests to include:

```rust
            readable_diff: None,
```

- [ ] **Step 7: Run the diff report test**

Run:

```bash
cargo test -p loc-cli --test diff diff_report_includes_readable_patch_for_existing_page_edit
```

Expected:

```text
test result: ok
```

- [ ] **Step 8: Add created entity readable diff test**

Add this test after `diff_plans_new_database_row_file_as_create_entity`:

```rust
#[test]
fn diff_report_includes_readable_patch_for_created_entity() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database");
    fixture.write_tasks_schema();
    let path = fixture.write_raw(
        "Tasks/new-task.md",
        "---\ntitle: New task\nStatus: Todo\n---\n# Notes\n\nCreated locally.\n",
    );

    let report = run_diff(&store, &path).expect("diff report");
    let readable = report.readable_diff.expect("readable diff");

    assert!(readable.text.contains("diff --locality /dev/null b/Tasks/new-task.md"), "{}", readable.text);
    assert!(readable.text.contains("+# Notes"), "{}", readable.text);
    assert!(readable.text.contains("+Created locally."), "{}", readable.text);
}
```

- [ ] **Step 9: Run created entity test**

Run:

```bash
cargo test -p loc-cli --test diff diff_report_includes_readable_patch_for_created_entity
```

Expected:

```text
test result: ok
```

- [ ] **Step 10: Commit**

Run:

```bash
git add crates/localityd/src/push.rs crates/loc-cli/src/diff.rs crates/loc-cli/src/commands.rs crates/loc-cli/tests/diff.rs
git commit -m "feat(cli): attach readable diff to preview reports"
```

### Task 3: Print Readable Diff From `loc diff` And Push Confirmation

**Files:**
- Modify: `crates/loc-cli/src/commands.rs`
- Modify: `crates/loc-cli/tests/diff.rs`

- [ ] **Step 1: Add CLI integration test for `loc diff` output**

Add this test near `diff_plain_text_summary_includes_entity_creates` in `crates/loc-cli/tests/diff.rs`:

```rust
#[test]
fn diff_plain_text_output_includes_readable_patch() {
    let fixture = DiffFixture::new();
    let state_root = fixture.root.join(".state");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open sqlite");
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.root.clone(),
        ))
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph.\n"))
        .expect("save shadow");
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.\n");

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .env("LOCALITY_STATE_DIR", &state_root)
        .arg("diff")
        .arg(&path)
        .output()
        .expect("run loc diff");

    assert!(
        output.status.code() == Some(4),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 block updated"), "{stdout}");
    assert!(stdout.contains("diff --locality a/Roadmap.md b/Roadmap.md"), "{stdout}");
    assert!(stdout.contains("-Old paragraph."), "{stdout}");
    assert!(stdout.contains("+Changed paragraph."), "{stdout}");
}
```

- [ ] **Step 2: Run the failing CLI output test**

Run:

```bash
cargo test -p loc-cli --test diff diff_plain_text_output_includes_readable_patch
```

Expected:

```text
assertion failed: stdout.contains("diff --locality a/Roadmap.md b/Roadmap.md")
```

- [ ] **Step 3: Print readable diff in human diff output**

Edit `crates/loc-cli/src/commands.rs`:

```rust
fn print_diff_report(report: &crate::diff::DiffReport) {
    print_diff_report_fields(&report.validation, report.plan.as_ref());
    print_readable_diff(report.readable_diff.as_ref());
}

fn print_readable_diff(readable_diff: Option<&locality_core::readable_diff::ReadableDiffOutput>) {
    let Some(readable_diff) = readable_diff else {
        return;
    };
    if readable_diff.text.trim().is_empty() {
        return;
    }
    println!();
    print!("{}", readable_diff.text);
    if !readable_diff.text.ends_with('\n') {
        println!();
    }
}
```

- [ ] **Step 4: Run the CLI output test**

Run:

```bash
cargo test -p loc-cli --test diff diff_plain_text_output_includes_readable_patch
```

Expected:

```text
test result: ok
```

- [ ] **Step 5: Add a unit test for confirmation output helper**

In `crates/loc-cli/src/commands.rs` tests, add:

```rust
#[test]
fn push_confirmation_preview_prints_readable_diff() {
    let mut report = push_report("confirm_plan");
    report.readable_diff = Some(locality_core::readable_diff::ReadableDiffOutput {
        files: Vec::new(),
        text: "diff --locality a/Roadmap.md b/Roadmap.md\n--- a/Roadmap.md\n+++ b/Roadmap.md\n".to_string(),
    });

    let mut output = Vec::new();
    print_push_confirmation_preview(&report, &mut output).expect("preview");
    let rendered = String::from_utf8(output).expect("utf8");

    assert!(rendered.contains("0 blocks updated"), "{rendered}");
    assert!(rendered.contains("diff --locality a/Roadmap.md b/Roadmap.md"), "{rendered}");
}
```

- [ ] **Step 6: Run the failing helper test**

Run:

```bash
cargo test -p loc-cli push_confirmation_preview_prints_readable_diff --lib
```

Expected:

```text
error[E0425]: cannot find function `print_push_confirmation_preview` in this scope
```

- [ ] **Step 7: Add confirmation preview helper and use it**

Edit `crates/loc-cli/src/commands.rs`:

```rust
fn print_push_confirmation_preview<W: Write>(report: &PushReport, output: &mut W) -> io::Result<()> {
    write_diff_report_fields(output, &report.validation, report.plan.as_ref())?;
    write_readable_diff(output, report.readable_diff.as_ref())
}

fn write_diff_report_fields<W: Write>(
    output: &mut W,
    validation: &[crate::diff::ValidationIssueOutput],
    plan: Option<&crate::diff::PushPlanOutput>,
) -> io::Result<()> {
    if !validation.is_empty() {
        for issue in validation {
            match issue.line {
                Some(line) => writeln!(output, "{}:{}: {} ({})", issue.file, line, issue.message, issue.code)?,
                None => writeln!(output, "{}: {} ({})", issue.file, issue.message, issue.code)?,
            }
        }
        return Ok(());
    }

    let Some(plan) = plan else {
        writeln!(output, "no plan")?;
        return Ok(());
    };

    writeln!(
        output,
        "{} block{} updated, {} replaced, {} media updated, {} block{} created, {} entit{} created, {} moved, {} block{} archived, {} entit{} archived",
        plan.summary.blocks_updated,
        plural(plan.summary.blocks_updated),
        plan.summary.blocks_replaced,
        plan.summary.media_updated,
        plan.summary.blocks_created,
        plural(plan.summary.blocks_created),
        plan.summary.entities_created,
        if plan.summary.entities_created == 1 { "y" } else { "ies" },
        plan.summary.blocks_moved,
        plan.summary.blocks_archived,
        plural(plan.summary.blocks_archived),
        plan.summary.entities_archived,
        if plan.summary.entities_archived == 1 { "y" } else { "ies" }
    )
}

fn write_readable_diff<W: Write>(
    output: &mut W,
    readable_diff: Option<&locality_core::readable_diff::ReadableDiffOutput>,
) -> io::Result<()> {
    let Some(readable_diff) = readable_diff else {
        return Ok(());
    };
    if readable_diff.text.trim().is_empty() {
        return Ok(());
    }
    writeln!(output)?;
    write!(output, "{}", readable_diff.text)?;
    if !readable_diff.text.ends_with('\n') {
        writeln!(output)?;
    }
    Ok(())
}
```

Then make `print_diff_report_fields` call `write_diff_report_fields` with `io::stdout()` and replace this line in the push prompt path:

```rust
            print_diff_report_fields(&report.validation, report.plan.as_ref());
```

with:

```rust
            if let Err(error) = print_push_confirmation_preview(&report, &mut io::stdout()) {
                return command_error(
                    json,
                    CommandError::new("push", "stdout_write_failed", error.to_string()),
                    EXIT_INTERNAL,
                );
            }
```

- [ ] **Step 8: Run command unit tests**

Run:

```bash
cargo test -p loc-cli push_confirmation_preview_prints_readable_diff --lib
cargo test -p loc-cli clap_help_includes_expected_commands --lib
```

Expected:

```text
test result: ok
```

- [ ] **Step 9: Commit**

Run:

```bash
git add crates/loc-cli/src/commands.rs crates/loc-cli/tests/diff.rs
git commit -m "feat(cli): print readable diff before push"
```

### Task 4: Add Journal Metadata And Readable Diff Snapshot Types

**Files:**
- Modify: `crates/locality-core/src/journal.rs`
- Modify: `crates/locality-store/tests/repository.rs`

- [ ] **Step 1: Add core unit tests for default anonymous metadata**

Add this test module case in `crates/locality-core/src/journal.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{PlanSummary, PushPlan};

    #[test]
    fn new_journal_entries_default_to_anonymous_metadata() {
        let entry = JournalEntry::new(
            PushId("push-2".to_string()),
            MountId::new("notion-main"),
            vec![RemoteId::new("page-1")],
            PushPlan {
                summary: PlanSummary::default(),
                affected_entities: vec![RemoteId::new("page-1")],
                operations: Vec::new(),
                degradations: Vec::new(),
            },
            JournalStatus::Prepared,
        );

        assert_eq!(entry.metadata.author.kind, JournalAuthorKind::Anonymous);
        assert_eq!(entry.metadata.author.display_name, "anonymous");
        assert_eq!(entry.metadata.previous_push_id, None);
        assert_eq!(entry.metadata.created_at_unix_ms, None);
        assert_eq!(entry.readable_diff, None);
    }
}
```

- [ ] **Step 2: Run the failing core test**

Run:

```bash
cargo test -p locality-core new_journal_entries_default_to_anonymous_metadata
```

Expected:

```text
error[E0609]: no field `metadata` on type `JournalEntry`
```

- [ ] **Step 3: Add metadata and readable diff fields**

Edit `crates/locality-core/src/journal.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub push_id: PushId,
    pub mount_id: MountId,
    pub remote_ids: Vec<RemoteId>,
    pub plan: PushPlan,
    pub preimages: Vec<JournalPreimage>,
    pub apply_effects: Vec<JournalApplyEffect>,
    pub status: JournalStatus,
    #[serde(default)]
    pub metadata: JournalMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readable_diff: Option<ReadableDiffOutput>,
}
```

Add `use crate::readable_diff::ReadableDiffOutput;` plus the `JournalMetadata`, `JournalAuthor`, and `JournalAuthorKind` types from "Data Contracts".

Add defaults and builders:

```rust
impl Default for JournalMetadata {
    fn default() -> Self {
        Self {
            author: JournalAuthor {
                kind: JournalAuthorKind::Anonymous,
                display_name: "anonymous".to_string(),
            },
            previous_push_id: None,
            created_at_unix_ms: None,
        }
    }
}

impl JournalMetadata {
    pub fn anonymous(previous_push_id: Option<PushId>, created_at_unix_ms: Option<u128>) -> Self {
        Self {
            previous_push_id,
            created_at_unix_ms,
            ..Self::default()
        }
    }
}

impl JournalEntry {
    pub fn with_metadata(mut self, metadata: JournalMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_readable_diff(mut self, readable_diff: Option<ReadableDiffOutput>) -> Self {
        self.readable_diff = readable_diff;
        self
    }
}
```

Update `JournalEntry::new` to initialize:

```rust
            metadata: JournalMetadata::default(),
            readable_diff: None,
```

- [ ] **Step 4: Run core tests**

Run:

```bash
cargo test -p locality-core new_journal_entries_default_to_anonymous_metadata
```

Expected:

```text
test result: ok
```

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/locality-core/src/journal.rs
git commit -m "feat(core): add journal edit metadata"
```

### Task 5: Persist Journal Metadata And Diff Snapshots

**Files:**
- Modify: `crates/locality-store/src/sqlite.rs`
- Modify: `crates/locality-store/src/memory.rs`
- Modify: `crates/locality-store/tests/sqlite.rs`
- Modify: `crates/locality-store/tests/repository.rs`

- [ ] **Step 1: Add SQLite round-trip test**

In `crates/locality-store/tests/sqlite.rs`, add imports:

```rust
use locality_core::journal::{JournalAuthorKind, JournalMetadata};
use locality_core::readable_diff::{
    ReadableDiffFileOutput, ReadableDiffFileStatus, ReadableDiffOutput,
};
```

Add this test near existing journal tests:

```rust
#[test]
fn sqlite_store_round_trips_journal_metadata_and_readable_diff() {
    let fixture = SqliteFixture::new();
    let mut store = SqliteStateStore::open(fixture.state_root()).expect("open store");
    store.save_mount(fixture.mount_config()).expect("save mount");

    let entry = journal_entry("push-2", JournalStatus::Prepared)
        .with_metadata(JournalMetadata::anonymous(
            Some(PushId("push-1".to_string())),
            Some(1_783_612_800_000),
        ))
        .with_readable_diff(Some(ReadableDiffOutput {
            files: vec![ReadableDiffFileOutput {
                path: "Roadmap.md".to_string(),
                old_label: "a/Roadmap.md".to_string(),
                new_label: "b/Roadmap.md".to_string(),
                status: ReadableDiffFileStatus::Modified,
                patch: "diff --locality a/Roadmap.md b/Roadmap.md\n".to_string(),
            }],
            text: "diff --locality a/Roadmap.md b/Roadmap.md\n".to_string(),
        }));

    store.append_journal(entry).expect("append journal");
    let loaded = store
        .get_journal(&PushId("push-2".to_string()))
        .expect("get journal")
        .expect("journal");

    assert_eq!(loaded.metadata.author.kind, JournalAuthorKind::Anonymous);
    assert_eq!(loaded.metadata.author.display_name, "anonymous");
    assert_eq!(loaded.metadata.previous_push_id, Some(PushId("push-1".to_string())));
    assert_eq!(loaded.metadata.created_at_unix_ms, Some(1_783_612_800_000));
    assert_eq!(
        loaded.readable_diff.expect("readable diff").text,
        "diff --locality a/Roadmap.md b/Roadmap.md\n"
    );
}
```

- [ ] **Step 2: Run the failing SQLite test**

Run:

```bash
cargo test -p locality-store --test sqlite sqlite_store_round_trips_journal_metadata_and_readable_diff
```

Expected:

```text
no column named metadata_json
```

- [ ] **Step 3: Update schema and row mapping**

Edit `crates/locality-store/src/sqlite.rs`:

```rust
const SCHEMA_VERSION: i64 = 17;
```

Update table DDL:

```sql
            metadata_json TEXT NOT NULL DEFAULT '{}',
            readable_diff_json TEXT
```

Update the `INSERT INTO journals` statement to include `metadata_json` and `readable_diff_json`:

```rust
                to_json(&entry.metadata)?,
                optional_to_json(&entry.readable_diff)?,
```

Add helper:

```rust
fn optional_to_json<T: Serialize>(value: &Option<T>) -> StoreResult<Option<String>> {
    value.as_ref().map(to_json).transpose()
}
```

Update `JournalRow`:

```rust
type JournalRow = (String, String, String, String, String, String, String, String, Option<String>);
```

Update `journal_row` SQL selects everywhere:

```sql
SELECT push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json, metadata_json, readable_diff_json
```

Update `journal_from_row`:

```rust
        metadata: from_json::<JournalMetadata>(&row.7).unwrap_or_default(),
        readable_diff: row.8
            .as_deref()
            .map(from_json::<ReadableDiffOutput>)
            .transpose()?,
```

Add migration:

```rust
    if user_version < 17 {
        if !column_exists(connection, "journals", "metadata_json")? {
            connection.execute_batch(
                "ALTER TABLE journals
                 ADD COLUMN metadata_json TEXT NOT NULL DEFAULT '{}';",
            )?;
        }
        if !column_exists(connection, "journals", "readable_diff_json")? {
            connection.execute_batch(
                "ALTER TABLE journals
                 ADD COLUMN readable_diff_json TEXT;",
            )?;
        }
        if user_version >= 13 {
            record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
        }
    }
```

- [ ] **Step 4: Update schema expectation test**

In `crates/locality-store/tests/sqlite.rs`, update:

```rust
assert_eq!(user_version, 17);
assert_eq!(SqliteStateStore::current_schema_version(), 17);
```

Update schema snapshot string:

```text
journals: push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json, metadata_json, readable_diff_json
```

- [ ] **Step 5: Run SQLite tests**

Run:

```bash
cargo test -p locality-store --test sqlite sqlite_store_round_trips_journal_metadata_and_readable_diff
cargo test -p locality-store --test sqlite sqlite_store_creates_expected_schema
```

Expected:

```text
test result: ok
```

- [ ] **Step 6: Add migration test from schema 16**

Add this test in `crates/locality-store/tests/sqlite.rs`:

```rust
#[test]
fn sqlite_store_migrates_v16_journals_with_empty_edit_metadata() {
    let fixture = SqliteFixture::new();
    let db_path = fixture.state_root().join("state.sqlite3");
    std::fs::create_dir_all(fixture.state_root()).expect("state root");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            r#"
            PRAGMA user_version = 16;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                projection_json TEXT NOT NULL DEFAULT '"PlainFiles"',
                read_only INTEGER NOT NULL DEFAULT 0,
                connection_id TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE journals (
                push_id TEXT PRIMARY KEY,
                mount_id TEXT NOT NULL,
                remote_ids_json TEXT NOT NULL,
                plan_json TEXT NOT NULL,
                preimages_json TEXT NOT NULL DEFAULT '[]',
                apply_effects_json TEXT NOT NULL DEFAULT '[]',
                status_json TEXT NOT NULL
            );
            INSERT INTO mounts (mount_id, connector, root, created_at)
            VALUES ('notion-main', 'notion', '/tmp/notion', '2026-07-10T00:00:00Z');
            "#,
        )
        .expect("seed schema");
    connection
        .execute(
            "INSERT INTO journals (push_id, mount_id, remote_ids_json, plan_json, preimages_json, apply_effects_json, status_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "push-1",
                "notion-main",
                serde_json::to_string(&vec![RemoteId::new("page-1")]).expect("remote ids"),
                serde_json::to_string(&push_plan()).expect("plan"),
                "[]",
                "[]",
                serde_json::to_string(&JournalStatus::Reconciled).expect("status")
            ],
        )
        .expect("seed journal");
    drop(connection);

    let store = SqliteStateStore::open(fixture.state_root()).expect("migrate");
    let entry = store
        .get_journal(&PushId("push-1".to_string()))
        .expect("get journal")
        .expect("journal");

    assert_eq!(entry.metadata.author.display_name, "anonymous");
    assert_eq!(entry.metadata.previous_push_id, None);
    assert_eq!(entry.readable_diff, None);
}
```

- [ ] **Step 7: Run migration test**

Run:

```bash
cargo test -p locality-store --test sqlite sqlite_store_migrates_v16_journals_with_empty_edit_metadata
```

Expected:

```text
test result: ok
```

- [ ] **Step 8: Run repository tests**

Run:

```bash
cargo test -p locality-store --test repository journal
```

Expected:

```text
test result: ok
```

- [ ] **Step 9: Commit**

Run:

```bash
git add crates/locality-store/src/sqlite.rs crates/locality-store/src/memory.rs crates/locality-store/tests/sqlite.rs crates/locality-store/tests/repository.rs
git commit -m "feat(store): persist journal edit metadata"
```

### Task 6: Link Journals As An Edit List

**Files:**
- Modify: `crates/locality-store/src/repository.rs`
- Modify: `crates/localityd/src/push.rs`
- Modify: `crates/locality-store/tests/repository.rs`

- [ ] **Step 1: Add repository test for previous journal lookup**

Add this test in `crates/locality-store/tests/repository.rs` near journal tests:

```rust
#[test]
fn journal_repository_finds_latest_previous_journal_for_entities() {
    let mut store = InMemoryStateStore::new();
    let mount_id = MountId::new("notion-main");
    store
        .append_journal(journal_entry("push-1", JournalStatus::Reconciled))
        .expect("append first");
    store
        .append_journal(journal_entry("push-3", JournalStatus::Reconciled))
        .expect("append third");

    let previous = store
        .latest_journal_for_entities(&mount_id, &[RemoteId::new("page-1")])
        .expect("latest");

    assert_eq!(previous, Some(PushId("push-3".to_string())));
}
```

- [ ] **Step 2: Run failing repository test**

Run:

```bash
cargo test -p locality-store --test repository journal_repository_finds_latest_previous_journal_for_entities
```

Expected:

```text
error[E0599]: no method named `latest_journal_for_entities`
```

- [ ] **Step 3: Add default repository helper**

Edit `crates/locality-store/src/repository.rs` inside `JournalRepository`:

```rust
    fn latest_journal_for_entities(
        &self,
        mount_id: &MountId,
        remote_ids: &[RemoteId],
    ) -> StoreResult<Option<PushId>> {
        let mut latest: Option<PushId> = None;
        for journal in self.list_journal()? {
            if journal.mount_id != *mount_id {
                continue;
            }
            if matches!(journal.status, JournalStatus::Reverted) {
                continue;
            }
            let touches_entity = journal
                .remote_ids
                .iter()
                .any(|id| remote_ids.iter().any(|target| target == id))
                || journal
                    .plan
                    .affected_entities
                    .iter()
                    .any(|id| remote_ids.iter().any(|target| target == id));
            if !touches_entity {
                continue;
            }
            if latest
                .as_ref()
                .is_none_or(|current| journal.push_id.0 > current.0)
            {
                latest = Some(journal.push_id);
            }
        }
        Ok(latest)
    }
```

- [ ] **Step 4: Run repository test**

Run:

```bash
cargo test -p locality-store --test repository journal_repository_finds_latest_previous_journal_for_entities
```

Expected:

```text
test result: ok
```

- [ ] **Step 5: Attach metadata in daemon push execution**

Edit `crates/localityd/src/push.rs` in `execute_prepared_push` after `remote_preconditions`:

```rust
    let remote_ids = prepared
        .pipeline
        .plan
        .as_ref()
        .map(|plan| plan.affected_entities.clone())
        .unwrap_or_else(|| vec![prepared.entity.remote_id.clone()]);
    let previous_push_id = store.latest_journal_for_entities(&prepared.mount.mount_id, &remote_ids)?;
    let created_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis());
```

Then add to the request chain:

```rust
    .with_metadata(JournalMetadata::anonymous(previous_push_id, created_at_unix_ms));
```

Add `JournalMetadata` to the existing journal imports.

- [ ] **Step 6: Add core request metadata support**

Edit `crates/locality-core/src/push.rs`:

```rust
use crate::journal::JournalMetadata;
use crate::readable_diff::ReadableDiffOutput;
```

Add to `PushExecutionRequest`:

```rust
    pub metadata: JournalMetadata,
    pub readable_diff: Option<ReadableDiffOutput>,
```

Initialize in `PushExecutionRequest::new`:

```rust
            metadata: JournalMetadata::default(),
            readable_diff: None,
```

Add builders:

```rust
    pub fn with_metadata(mut self, metadata: JournalMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_readable_diff(mut self, readable_diff: Option<ReadableDiffOutput>) -> Self {
        self.readable_diff = readable_diff;
        self
    }
```

Update journal append:

```rust
        .with_preimages(request.preimages.clone())
        .with_metadata(request.metadata.clone())
        .with_readable_diff(request.readable_diff.clone()),
```

- [ ] **Step 7: Add e2e journal link assertion**

In `crates/loc-cli/tests/e2e_push_workflow.rs`, add this focused test near existing push journal tests:

```rust
#[test]
fn push_journals_link_consecutive_edits_for_same_page() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api);

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        },
    )
    .expect("mount");
    run_pull(&mut store, &connector, &fixture.root).expect("pull");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    fs::write(
        &page_path,
        original.replace("First paragraph.", "First pushed paragraph."),
    )
    .expect("write first edit");
    let first_push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("first push");
    let first_push_id = PushId(first_push.push_id.expect("first push id"));

    let after_first = fs::read_to_string(&page_path).expect("read first reconcile");
    fs::write(
        &page_path,
        after_first.replace("First pushed paragraph.", "Second pushed paragraph."),
    )
    .expect("write second edit");
    let second_push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("second push");
    let second_push_id = PushId(second_push.push_id.expect("second push id"));

    let journals = store.list_journal().expect("journals");
    let second = journals
        .iter()
        .find(|entry| entry.push_id == second_push_id)
        .expect("second journal");
    assert_eq!(second.metadata.previous_push_id, Some(first_push_id));
    assert_eq!(second.metadata.author.display_name, "anonymous");
}
```

- [ ] **Step 8: Run the e2e journal link test**

Run:

```bash
cargo test -p loc-cli --test e2e_push_workflow push_journals_link_consecutive_edits_for_same_page
```

Expected:

```text
test result: ok
```

- [ ] **Step 9: Commit**

Run:

```bash
git add crates/locality-core/src/push.rs crates/locality-store/src/repository.rs crates/locality-store/tests/repository.rs crates/localityd/src/push.rs crates/loc-cli/tests/e2e_push_workflow.rs
git commit -m "feat(journal): link anonymous edit records"
```

### Task 7: Persist Reviewed Diff On Push Journals

**Files:**
- Modify: `crates/localityd/src/push.rs`
- Modify: `crates/loc-cli/tests/e2e_push_workflow.rs`

- [ ] **Step 1: Attach the prepared readable diff to execution requests**

Edit `crates/localityd/src/push.rs` in `execute_prepared_push` after `created_at_unix_ms` is computed:

```rust
    let readable_diff = prepared.readable_diff.clone();
```

Then add it to the `PushExecutionRequest` builder chain:

```rust
    .with_readable_diff(readable_diff);
```

- [ ] **Step 2: Add e2e journal diff assertion**

In `crates/loc-cli/tests/e2e_push_workflow.rs`, add this assertion to the test from Task 6 after the second push:

```rust
let readable = second
    .readable_diff
    .as_ref()
    .expect("journal readable diff");
assert!(readable.text.contains("diff --locality"), "{}", readable.text);
assert!(readable.text.contains("-First pushed paragraph."), "{}", readable.text);
assert!(readable.text.contains("+Second pushed paragraph."), "{}", readable.text);
```

- [ ] **Step 3: Run e2e journal diff test**

Run:

```bash
cargo test -p loc-cli --test e2e_push_workflow push_journals_link_consecutive_edits_for_same_page
```

Expected:

```text
test result: ok
```

- [ ] **Step 4: Commit**

Run:

```bash
git add crates/localityd/src/push.rs crates/loc-cli/tests/e2e_push_workflow.rs
git commit -m "feat(journal): store readable reviewed diffs"
```

### Task 8: Expose Edit List Diffs In `loc log`

**Files:**
- Modify: `crates/loc-cli/src/history.rs`
- Modify: `crates/loc-cli/src/commands.rs`
- Modify: `docs/cli.md`

- [ ] **Step 1: Add history report fields**

Edit `crates/loc-cli/src/history.rs` and extend `LogOptions`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogOptions {
    pub path: Option<PathBuf>,
    pub push_id: Option<PushId>,
    pub include_diff: bool,
}
```

Extend `JournalEntryOutput`:

```rust
    pub author: String,
    pub previous_push_id: Option<String>,
    pub created_at_unix_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readable_diff: Option<locality_core::readable_diff::ReadableDiffOutput>,
```

Update `From<JournalEntry>`:

```rust
            author: value.metadata.author.display_name,
            previous_push_id: value.metadata.previous_push_id.map(|push_id| push_id.0),
            created_at_unix_ms: value.metadata.created_at_unix_ms,
            readable_diff: value.readable_diff,
```

Update `run_log` filtering:

```rust
    if let Some(push_id) = &options.push_id {
        entries.retain(|entry| &entry.push_id == push_id);
    }
```

When `include_diff` is false, clear `entry.readable_diff = None` before output conversion:

```rust
    if !options.include_diff {
        for entry in &mut entries {
            entry.readable_diff = None;
        }
    }
```

- [ ] **Step 2: Add command parser support**

Replace `Log(PathArg)` with:

```rust
Log(LogCliArgs),
```

Add:

```rust
#[derive(Debug, Args)]
struct LogCliArgs {
    #[arg(value_name = "path", help = "Optional path to filter journal entries.")]
    path: Option<String>,
    #[arg(long, value_name = "push-id", help = "Only show one push journal entry.")]
    push_id: Option<String>,
    #[arg(long, help = "Print the readable diff saved with each journal entry.")]
    diff: bool,
}
```

Update `legacy_args_for_command`:

```rust
        LocalityCommand::Log(options) => {
            args.push("log".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
            push_optional_flag_value(&mut args, "--push-id", options.push_id.as_deref());
            push_flag(&mut args, "--diff", options.diff);
        }
```

Update legacy `log(args, json)` parser to read:

```rust
let include_diff = has_flag(args, "--diff");
let push_id = flag_value(args, "--push-id").map(|value| PushId(value.to_string()));
let path = first_positional(args).map(PathBuf::from);
```

Then call:

```rust
run_log(&store, LogOptions { path, push_id, include_diff })
```

- [ ] **Step 3: Print linked edit fields and optional diff**

Edit `print_log_report`:

```rust
        println!("  author: {}", entry.author);
        if let Some(created_at_unix_ms) = entry.created_at_unix_ms {
            println!("  created_at_unix_ms: {created_at_unix_ms}");
        }
        if let Some(previous_push_id) = &entry.previous_push_id {
            println!("  previous: {previous_push_id}");
        }
        if let Some(readable_diff) = &entry.readable_diff {
            println!();
            print!("{}", readable_diff.text);
            if !readable_diff.text.ends_with('\n') {
                println!();
            }
        }
```

- [ ] **Step 4: Add command unit tests**

In `crates/loc-cli/src/commands.rs` tests, update Clap help expectations:

```rust
vec!["Usage: loc log", "List push journal", "path", "--push-id", "--diff", "--json"]
```

Add parse conversion test:

```rust
let cli = parse_cli(["log", "Roadmap.md", "--push-id", "push-1", "--diff"]);
assert_eq!(
    legacy_args_for_command(cli.command.as_ref().expect("command")),
    vec!["log", "Roadmap.md", "--push-id", "push-1", "--diff"]
);
```

- [ ] **Step 5: Add history unit test**

At the end of `crates/loc-cli/src/history.rs`, add this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use locality_core::journal::{JournalEntry, JournalStatus, PushId};
    use locality_core::model::{MountId, RemoteId};
    use locality_core::planner::{PushOperation, PushPlan};
    use locality_core::readable_diff::ReadableDiffOutput;
    use locality_store::{InMemoryStateStore, JournalRepository, MountConfig, MountRepository};

    #[test]
    fn log_report_can_include_readable_diff_for_single_push() {
        let mut store = InMemoryStateStore::new();
        store.save_mount(MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/notion")).expect("mount");
        let plan = PushPlan::new(
            vec![RemoteId::new("page-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("paragraph-1"),
                content: "Updated paragraph.".to_string(),
            }],
        );
        store
            .append_journal(
                JournalEntry::new(
                    PushId("push-1".to_string()),
                    MountId::new("notion-main"),
                    vec![RemoteId::new("page-1")],
                    plan,
                    JournalStatus::Reconciled,
                )
                    .with_readable_diff(Some(ReadableDiffOutput {
                        files: Vec::new(),
                        text: "diff --locality a/Roadmap.md b/Roadmap.md\n".to_string(),
                    })),
            )
            .expect("journal");

        let report = run_log(
            &store,
            LogOptions {
                path: None,
                push_id: Some(PushId("push-1".to_string())),
                include_diff: true,
            },
        )
        .expect("log");

        assert_eq!(report.entries.len(), 1);
        assert_eq!(
            report.entries[0].readable_diff.as_ref().expect("diff").text,
            "diff --locality a/Roadmap.md b/Roadmap.md\n"
        );
    }
}
```

- [ ] **Step 6: Run log tests**

Run:

```bash
cargo test -p loc-cli log_report_can_include_readable_diff_for_single_push
cargo test -p loc-cli clap_parsed_commands_convert_to_legacy_args_for_execution --lib
```

Expected:

```text
test result: ok
```

- [ ] **Step 7: Update CLI docs**

Edit `docs/cli.md` command list:

```markdown
- `loc diff [path] [--json]`
- `loc log [path] [--push-id <push-id>] [--diff] [--json]`
```

Add a section near push/diff docs:

```markdown
## Diff And Edit Log

`loc diff <path>` prints the push-plan summary followed by a readable unified
diff. The diff compares the last synced Locality shadow with the local Markdown
that would be pushed.

Interactive `loc push <path>` prints the same readable diff before asking for
confirmation. `loc push <path> -y` keeps non-interactive behavior and applies the
approved plan without stopping for review.

`loc log [path]` lists journaled pushes as an edit list. Each entry shows an
anonymous author and, when available, the previous push id for the same entity.
Use `loc log --diff` to show the readable diff saved with each journal entry, or
`loc log --push-id <push-id> --diff` to inspect one edit.
```

- [ ] **Step 8: Commit**

Run:

```bash
git add crates/loc-cli/src/history.rs crates/loc-cli/src/commands.rs docs/cli.md
git commit -m "feat(cli): show journaled edit diffs"
```

### Task 9: Full Verification

**Files:**
- No new edits unless tests expose a defect.

- [ ] **Step 1: Run focused suites**

Run:

```bash
cargo test -p loc-cli --test diff
cargo test -p loc-cli --test push
cargo test -p loc-cli --test e2e_push_workflow push_journals_link_consecutive_edits_for_same_page
cargo test -p locality-store --test sqlite
cargo test -p locality-store --test repository
```

Expected:

```text
test result: ok
```

- [ ] **Step 2: Run command unit tests**

Run:

```bash
cargo test -p loc-cli --lib commands
```

Expected:

```text
test result: ok
```

- [ ] **Step 3: Run workspace check**

Run:

```bash
cargo test --workspace --all-targets --no-fail-fast
```

Expected:

```text
test result: ok
```

- [ ] **Step 4: Manual smoke test on scratch content**

Use a scratch mounted page and run:

```bash
./target/debug/loc diff '/path/to/scratch/page.md'
./target/debug/loc push '/path/to/scratch/page.md'
./target/debug/loc log '/path/to/scratch/page.md'
./target/debug/loc log '/path/to/scratch/page.md' --diff
```

Expected:

```text
loc diff prints a summary and a diff --locality patch.
interactive loc push prints the same patch before Proceed with push? [y/N].
loc log shows author: anonymous and previous: <push-id> on the second edit.
loc log --diff shows the saved patch for each journaled edit.
```

- [ ] **Step 5: Commit final fixes**

If verification required code changes, run:

```bash
git add crates/loc-cli crates/locality-core crates/locality-store crates/localityd docs/cli.md Cargo.lock
git commit -m "test: verify readable diff edit log"
```

If verification passed without changes, do not create an empty commit.

## Self-Review

- Spec coverage:
  - Readable Git-style CLI diff before push: Tasks 1-3.
  - Diff generated through Locality CLI: Tasks 2-3.
  - Interactive push review before confirmation: Task 3.
  - Linked list of edits: Tasks 4-6 and Task 8.
  - Anonymous authors for now: Tasks 4, 6, and 8.
  - Each edit can show readable diff: Tasks 7-8.
  - Journal-linked design: Tasks 4-8.
- State compatibility:
  - Schema version bump and v16 migration are included in Task 5.
  - Existing journals load with anonymous metadata and no readable diff.
- Test coverage:
  - Unit renderer tests, CLI diff tests, command tests, SQLite migration tests, repository tests, and one daemon push e2e are included.
- Known implementation choice:
  - New push journals store the reviewed readable diff snapshot. Older journals can still list metadata after migration but will not have retroactive diffs unless a later repair/backfill feature is added.
