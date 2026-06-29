use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::diff::{DiffError, run_diff, run_diff_with_state_root};
use locality_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_notion::media::sha256_hex;
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, SqliteStateStore, StoreError, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
};
use serde_json::json;

#[test]
fn diff_reports_noop_plan() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(report.ok);
    assert_eq!(report.action, "noop");
    assert!(report.validation.is_empty());
    assert_eq!(report.mount_id, "notion-main");
    assert_eq!(report.entity_id, "page-1");
    assert_eq!(report.plan.unwrap().operations.len(), 0);
}

#[test]
fn diff_reports_tight_rendered_lists_as_noop_against_shadow() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let body = concat!(
        "Intro.\n\n",
        "- First bullet\n",
        "- Second bullet\n",
        "- [ ] First task\n",
        "- [x] Done task\n",
        "1. First number\n",
        "1. Second number\n\n",
        "After.",
    );
    let path = fixture.write_page("Roadmap.md", body);
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                body,
                9,
                [
                    RemoteId::new("intro"),
                    RemoteId::new("bullet-1"),
                    RemoteId::new("bullet-2"),
                    RemoteId::new("todo-1"),
                    RemoteId::new("todo-2"),
                    RemoteId::new("number-1"),
                    RemoteId::new("number-2"),
                    RemoteId::new("after"),
                ],
            )
            .expect("shadow"),
        )
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "noop");
    assert!(report.validation.is_empty());
    assert!(plan.operations.is_empty());
    assert_eq!(plan.summary.blocks_created, 0);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(plan.summary.blocks_replaced, 0);
    assert_eq!(plan.summary.blocks_archived, 0);
}

#[test]
fn diff_page_directory_targets_page_document() {
    let fixture = DiffFixture::new();
    let mut store = InMemoryStateStore::new();
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
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");
    fixture.write_page("Roadmap/page.md", "# Roadmap\n\nSame paragraph.");

    let report = run_diff(&store, fixture.root.join("Roadmap")).expect("diff report");

    assert!(report.ok);
    assert_eq!(report.action, "noop");
    assert_eq!(report.entity_id, "page-1");
    assert!(report.plan.unwrap().operations.is_empty());
}

#[test]
fn diff_virtual_projection_reads_daemon_content_cache() {
    let fixture = DiffFixture::new();
    let state_root = fixture.root.join("state");
    let projection_root = fixture.root.join("projection");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &projection_root)
                .projection(ProjectionMode::MacosFileProvider),
        )
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
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");
    let content_path = localityd::virtual_fs::virtual_fs_content_path(
        &state_root,
        &fixture.mount_id,
        "Roadmap.md".as_ref(),
    )
    .expect("content path");
    if let Some(parent) = content_path.parent() {
        fs::create_dir_all(parent).expect("content parent");
    }
    fs::write(
        content_path,
        canonical_markdown("page-1", "# Roadmap\n\nSame paragraph."),
    )
    .expect("content file");

    let report = run_diff_with_state_root(
        &store,
        projection_root.join("Roadmap.md"),
        Some(&state_root),
    )
    .expect("diff report");

    assert!(report.ok);
    assert_eq!(report.action, "noop");
}

#[test]
fn diff_reports_safe_plan_as_confirmation_needed() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.guardrail.decision, "proceed");
    assert_eq!(plan.summary.blocks_updated, 1);
    assert_eq!(plan.operations[0].operation_type(), "update_block");
}

#[test]
fn diff_plans_local_image_media_byte_update_from_manifest() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let media_path = PathBuf::from(".loc/media/Roadmap/image-image1.png");
    let original_bytes = b"original image bytes";
    fixture.write_media(&media_path, original_bytes);
    fixture.write_media_manifest(
        &media_path,
        "image-1",
        "image",
        "https://example.com/original-image.png",
        original_bytes,
    );
    let original_body = "![Original image](.loc/media/Roadmap/image-image1.png)\n";
    let edited_body = original_body;
    let path = fixture.write_page("Roadmap.md", edited_body);
    fs::write(fixture.root.join(&media_path), b"updated image bytes").expect("update media");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                original_body,
                9,
                [RemoteId::new("image-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(plan.summary.media_updated, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    match &plan.operations[0] {
        loc_cli::diff::PushOperationOutput::UpdateMedia {
            block_id,
            local_path,
            caption,
        } => {
            assert_eq!(block_id, "image-1");
            assert_eq!(local_path, ".loc/media/Roadmap/image-image1.png");
            assert_eq!(caption, "Original image");
        }
        operation => panic!("unexpected operation {operation:?}"),
    }
}

#[test]
fn diff_plans_local_image_media_byte_update_with_escaped_parenthesized_href() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let media_path = PathBuf::from(".loc/media/Roadmap/image-(1).png");
    let original_bytes = b"original image bytes";
    fixture.write_media(&media_path, original_bytes);
    fixture.write_media_manifest(
        &media_path,
        "image-1",
        "image",
        "https://example.com/original-image.png",
        original_bytes,
    );
    let original_body = "![Original image](.loc/media/Roadmap/image-\\(1\\).png)\n";
    let path = fixture.write_page("Roadmap.md", original_body);
    fs::write(fixture.root.join(&media_path), b"updated image bytes").expect("update media");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                original_body,
                9,
                [RemoteId::new("image-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(plan.summary.media_updated, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    match &plan.operations[0] {
        loc_cli::diff::PushOperationOutput::UpdateMedia {
            block_id,
            local_path,
            caption,
        } => {
            assert_eq!(block_id, "image-1");
            assert_eq!(local_path, ".loc/media/Roadmap/image-(1).png");
            assert_eq!(caption, "Original image");
        }
        operation => panic!("unexpected operation {operation:?}"),
    }
}

#[test]
fn diff_rejects_unresolved_conflict_markers() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page(
        "Roadmap.md",
        &format!(
            "{CONFLICT_LOCAL_MARKER}\n# Roadmap\n\nLocal paragraph.\n{CONFLICT_SEPARATOR_MARKER}\n# Roadmap\n\nRemote paragraph.\n{CONFLICT_REMOTE_MARKER}\n"
        ),
    );
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nRemote paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert_eq!(report.validation[0].code, "unresolved_conflict_markers");
}

#[test]
fn diff_plans_new_database_row_file_as_create_entity() {
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
        "---\ntitle: New task\nStatus: Todo\nTags:\n  - Backend\nDone: false\nPoints: 5\n---\n# Notes\n\n- [ ] Wire create\n",
    );

    let report = run_diff(&store, &path).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.entity_id, "database-1");
    assert_eq!(plan.summary.entities_created, 1);
    assert_eq!(plan.affected_entities, vec!["database-1"]);
    match &plan.operations[0] {
        loc_cli::diff::PushOperationOutput::CreateEntity {
            parent_id,
            title,
            keys,
            body,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, "database-1");
            assert_eq!(title, "New task");
            assert_eq!(
                keys,
                &vec![
                    "Done".to_string(),
                    "Points".to_string(),
                    "Status".to_string(),
                    "Tags".to_string(),
                ]
            );
            assert_eq!(body, "# Notes\n\n- [ ] Wire create\n");
            assert_eq!(source_path, "Tasks/new-task.md");
        }
        operation => panic!("unexpected operation {operation:?}"),
    }
}

#[test]
fn diff_plans_new_database_row_page_document_as_create_entity() {
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
        "Tasks/new-task/page.md",
        "---\ntitle: New task\nStatus: Todo\nTags:\n  - Backend\nDone: false\nPoints: 5\n---\n# Notes\n\n- [ ] Wire create\n",
    );

    let report = run_diff(&store, &path).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.entity_id, "database-1");
    assert_eq!(plan.summary.entities_created, 1);
    assert_eq!(plan.affected_entities, vec!["database-1"]);
    match &plan.operations[0] {
        loc_cli::diff::PushOperationOutput::CreateEntity {
            parent_id,
            title,
            keys,
            body,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, "database-1");
            assert_eq!(title, "New task");
            assert_eq!(
                keys,
                &vec![
                    "Done".to_string(),
                    "Points".to_string(),
                    "Status".to_string(),
                    "Tags".to_string(),
                ]
            );
            assert_eq!(body, "# Notes\n\n- [ ] Wire create\n");
            assert_eq!(source_path, "Tasks/new-task/page.md");
        }
        operation => panic!("unexpected operation {operation:?}"),
    }
}

#[test]
fn diff_plain_text_summary_includes_entity_creates() {
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

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .env("LOCALITY_STATE_DIR", &state_root)
        .arg("diff")
        .arg(&path)
        .output()
        .expect("run loc diff");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 entity created"), "{stdout}");
}

#[test]
fn diff_database_directory_plans_pending_virtual_delete_under_scope() {
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
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Done task",
                "Tasks/done-task/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save row");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_for("row-1", "# Done\n\nDone body."),
        )
        .expect("save row shadow");
    store
        .save_virtual_mutation(virtual_delete_mutation(
            &fixture.mount_id,
            "delete:row-1",
            "row-1",
            "Tasks/done-task/page.md",
        ))
        .expect("save virtual delete");
    fixture.write_tasks_schema();

    let report = run_diff(&store, fixture.root.join("Tasks")).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.entity_id, "row-1");
    assert_eq!(plan.summary.entities_archived, 1);
    assert_eq!(plan.affected_entities, vec!["row-1"]);
    assert_eq!(plan.operations[0].operation_type(), "archive_entity");
}

#[test]
fn diff_rejects_new_database_row_property_outside_schema() {
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
        "---\ntitle: New task\nStatus: Todo\nUnexpected: value\n---\n# Notes\n",
    );

    let report = run_diff(&store, &path).expect("diff report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert!(report.plan.is_none());
    assert_eq!(report.validation[0].code, "notion_schema_property_unknown");
    assert_eq!(report.validation[0].file, "Tasks/new-task.md");
}

#[test]
fn diff_rejects_existing_database_row_invalid_select_option() {
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
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save row");
    fixture.write_tasks_schema();
    let body = "# Notes\n\nExisting body.";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                body,
                9,
                [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
            )
            .expect("shadow")
            .with_frontmatter(row_frontmatter("Todo")),
        )
        .expect("save shadow");
    let path = fixture.write_raw(
        "Tasks/existing-task.md",
        &format!("---\n{}---\n{body}", row_frontmatter("Blocked")),
    );

    let report = run_diff(&store, &path).expect("diff report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert!(report.plan.is_none());
    assert_eq!(report.validation[0].code, "notion_schema_option_unknown");
    assert_eq!(report.validation[0].line, Some(8));
}

#[test]
fn diff_surfaces_validation_issues_without_plan() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_raw(
        "Roadmap.md",
        "---\ntitle: Missing Locality\n---\n# Roadmap\n",
    );
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert!(report.plan.is_none());
    assert_eq!(report.validation[0].code, "frontmatter_missing_loc");
    assert_eq!(report.completed_stages, vec!["parse_and_validate"]);
}

#[test]
fn diff_rejects_frontmatter_id_mismatch_before_planning() {
    let fixture = DiffFixture::new();
    let store = fixture.store();
    let path = fixture.write_page_with_id("Roadmap.md", "page-2", "# Roadmap\n\nSame paragraph.");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert!(report.plan.is_none());
    assert_eq!(report.validation[0].code, "frontmatter_remote_id_mismatch");
}

#[test]
fn diff_returns_structured_missing_shadow_error() {
    let fixture = DiffFixture::new();
    let store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");

    let error = run_diff(&store, &path).expect_err("missing shadow");

    assert_eq!(error.code(), "shadow_missing");
    assert_eq!(
        error,
        DiffError::Store(StoreError::ShadowMissing {
            mount_id: fixture.mount_id.clone(),
            entity_id: RemoteId::new("page-1"),
        })
    );
}

#[test]
fn diff_returns_structured_mount_lookup_error() {
    let fixture = DiffFixture::new();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");
    let store = InMemoryStateStore::new();

    let error = run_diff(&store, &path).expect_err("missing mount");

    assert_eq!(error.code(), "mount_not_found");
}

#[test]
fn diff_runner_works_with_sqlite_state_store() {
    let fixture = DiffFixture::new();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
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
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.plan.unwrap().summary.blocks_updated, 1);
}

struct DiffFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl DiffFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "loc-cli-diff-{}-{unique}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                self.mount_id.clone(),
                "notion",
                self.root.clone(),
            ))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save entity");
        store
    }

    fn write_page(&self, relative_path: &str, body: &str) -> PathBuf {
        self.write_page_with_id(relative_path, "page-1", body)
    }

    fn write_page_with_id(&self, relative_path: &str, remote_id: &str, body: &str) -> PathBuf {
        self.write_raw(relative_path, &canonical_markdown(remote_id, body))
    }

    fn write_raw(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, contents).expect("fixture file");
        path
    }

    fn write_tasks_schema(&self) {
        self.write_raw("Tasks/_schema.yaml", tasks_schema());
    }

    fn write_media(&self, relative_path: &PathBuf, bytes: &[u8]) {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("media parent");
        }
        fs::write(path, bytes).expect("media bytes");
    }

    fn write_media_manifest(
        &self,
        local_path: &PathBuf,
        block_id: &str,
        kind: &str,
        source_url: &str,
        bytes: &[u8],
    ) {
        let manifest_path = self.root.join(".loc/media/manifest.json");
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent).expect("manifest parent");
        }
        let key = local_path.to_string_lossy().replace('\\', "/");
        fs::write(
            manifest_path,
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "assets": {
                    key: {
                        "block_id": block_id,
                        "kind": kind,
                        "source_url": source_url,
                        "local_path": local_path,
                        "sha256": sha256_hex(bytes),
                        "size": bytes.len(),
                    }
                }
            }))
            .expect("manifest json"),
        )
        .expect("write manifest");
    }
}

impl Drop for DiffFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nloc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}

fn shadow(body: &str) -> ShadowDocument {
    shadow_for("page-1", body)
}

fn shadow_for(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn row_frontmatter(status: &str) -> String {
    format!(
        "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: {status}\n"
    )
}

fn tasks_schema() -> &'static str {
    r#"loc:
  type: notion_database_schema
  database_id: "database-1"
title: "Tasks"
data_sources:
  - id: "source-1"
    name: "Tasks"
    properties:
      Name:
        id: "name-id"
        type: "title"
      Status:
        id: "status-id"
        type: "select"
        options:
          - name: "Todo"
            id: "todo-id"
      Tags:
        id: "tags-id"
        type: "multi_select"
        options:
          - name: "Backend"
            id: "backend-id"
      Done:
        id: "done-id"
        type: "checkbox"
      Points:
        id: "points-id"
        type: "number"
"#
}

fn virtual_delete_mutation(
    mount_id: &MountId,
    local_id: &str,
    remote_id: &str,
    path: &str,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind: VirtualMutationKind::Delete,
        target_remote_id: Some(RemoteId::new(remote_id)),
        parent_remote_id: None,
        original_path: None,
        projected_path: PathBuf::from(path),
        title: "Done task".to_string(),
        content_path: None,
        created_at: "2026-06-23T00:00:00Z".to_string(),
        updated_at: "2026-06-23T00:00:00Z".to_string(),
    }
}

trait OperationOutputExt {
    fn operation_type(&self) -> &'static str;
}

impl OperationOutputExt for loc_cli::diff::PushOperationOutput {
    fn operation_type(&self) -> &'static str {
        match self {
            Self::UpdateBlock { .. } => "update_block",
            Self::ReplaceBlock { .. } => "replace_block",
            Self::AppendBlock { .. } => "append_block",
            Self::MoveBlock { .. } => "move_block",
            Self::ArchiveBlock { .. } => "archive_block",
            Self::ArchiveEntity { .. } => "archive_entity",
            Self::UpdateProperties { .. } => "update_properties",
            Self::CreateEntity { .. } => "create_entity",
            Self::UpdateMedia { .. } => "update_media",
        }
    }
}
