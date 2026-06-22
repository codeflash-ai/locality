use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::planner::PushOperation;
use afs_core::push::PushPipelineAction;
use afs_core::shadow::ShadowDocument;
use afs_core::validation::ValidationReport;
use afs_notion::media::sha256_hex;
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, StoreError, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use afsd::execution::PushJob;
use afsd::push::{PushPrepareError, prepare_push};
use afsd::source::{LocalSourceValidator, SourcePushValidator, SourceValidationContext};
use afsd::virtual_fs::{virtual_fs_content_path, virtual_fs_content_root};
use serde_json::json;

#[test]
fn prepare_push_blocks_notion_schema_violation_for_existing_database_row() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("notion");
    fixture.save_database(&mut store);
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

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::FixValidation);
    assert!(prepared.pipeline.plan.is_none());
    assert_eq!(
        prepared.pipeline.validation.issues[0].code,
        "notion_schema_option_unknown"
    );
}

#[test]
fn prepare_push_leaves_non_notion_database_schema_validation_clean() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.store("fake");
    fixture.save_database(&mut store);
    let path = fixture.write_raw(
        "Tasks/new-task.md",
        "---\ntitle: New task\nUnexpected: value\n---\n# Notes\n",
    );

    let prepared =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect("prepare push");

    assert!(prepared.pipeline.validation.is_clean());
    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    let plan = prepared.pipeline.plan.expect("plan");
    assert!(matches!(
        plan.operations[0],
        PushOperation::CreateEntity { .. }
    ));
}

#[test]
fn prepare_push_plans_content_cache_absolute_media_byte_update() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
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
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "![Image](../.afs/media/Roadmap/image-1.png)",
                8,
                [RemoteId::new("image-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let content_root = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id);
    let media_path = PathBuf::from(".afs/media/Roadmap/image-1.png");
    fixture.write_virtual_media_manifest(&media_path, "image-1", b"original image bytes");
    fixture.write_virtual_media(&media_path, b"updated image bytes");
    let absolute_media = content_root.join(&media_path);
    fixture.write_virtual_page(
        "Roadmap/page.md",
        &canonical_markdown("page-1", &format!("![Image]({})", absolute_media.display())),
    );

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Roadmap/page.md")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");
    let plan = prepared.pipeline.plan.expect("plan");

    assert_eq!(plan.summary.media_updated, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateMedia {
            block_id: RemoteId::new("image-1"),
            local_path: media_path,
            caption: "Image".to_string(),
        }]
    );
}

#[test]
fn prepare_push_plans_content_cache_absolute_file_media_byte_update() {
    let fixture = PrepareFixture::new();
    let mut store = fixture.virtual_store("notion");
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
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "[Demo](../.afs/media/Roadmap/video-1.mp4)",
                8,
                [RemoteId::new("video-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    let content_root = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id);
    let media_path = PathBuf::from(".afs/media/Roadmap/video-1.mp4");
    fixture.write_virtual_media_manifest_with_kind(&media_path, "video", "video-1", b"old video");
    fixture.write_virtual_media(&media_path, b"new video");
    let absolute_media = content_root.join(&media_path);
    fixture.write_virtual_page(
        "Roadmap/page.md",
        &canonical_markdown("page-1", &format!("[Demo]({})", absolute_media.display())),
    );

    let prepared = prepare_push(
        &store,
        &job(fixture.root.join("Roadmap/page.md")),
        Some(&fixture.state_root),
        &LocalSourceValidator,
    )
    .expect("prepare push");
    let plan = prepared.pipeline.plan.expect("plan");

    assert_eq!(plan.summary.media_updated, 1);
    assert_eq!(plan.summary.blocks_updated, 0);
    assert_eq!(
        plan.operations,
        vec![PushOperation::UpdateMedia {
            block_id: RemoteId::new("video-1"),
            local_path: media_path,
            caption: "Demo".to_string(),
        }]
    );
}

#[test]
fn prepare_push_uses_shared_validator_for_direct_and_virtual_creates() {
    let fixture = PrepareFixture::new();
    let validator = RecordingValidator::default();

    let mut direct_store = fixture.store("fake");
    fixture.save_parent_page(&mut direct_store);
    let direct_path = fixture.write_raw("Roadmap/Draft.md", "---\ntitle: Draft\n---\n# Draft\n");
    let direct =
        prepare_push(&direct_store, &job(direct_path), None, &validator).expect("direct prepare");
    assert_eq!(direct.pipeline.action, PushPipelineAction::ConfirmPlan);

    let mut virtual_store = fixture.virtual_store("fake");
    fixture.save_parent_page(&mut virtual_store);
    let cache_path = fixture.write_cache("Draft.md", "---\ntitle: Draft\n---\n# Draft\n");
    virtual_store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            Some(RemoteId::new("page-parent")),
            "Roadmap/Draft.md",
            cache_path,
        ))
        .expect("save mutation");
    let virtual_prepared = prepare_push(
        &virtual_store,
        &job(fixture.root.join("Roadmap/Draft.md")),
        None,
        &validator,
    )
    .expect("virtual prepare");
    assert_eq!(
        virtual_prepared.pipeline.action,
        PushPipelineAction::ConfirmPlan
    );

    assert_eq!(validator.create_count.get(), 2);
    assert_eq!(
        validator.paths.borrow().as_slice(),
        &[
            PathBuf::from("Roadmap/Draft.md"),
            PathBuf::from("Roadmap/Draft.md")
        ]
    );
    assert_eq!(
        validator.parents.borrow().as_slice(),
        &[RemoteId::new("page-parent"), RemoteId::new("page-parent")]
    );
}

#[test]
fn prepare_push_uses_page_directory_parent_for_new_page_document() {
    let fixture = PrepareFixture::new();
    let validator = RecordingValidator::default();
    let mut store = fixture.store("fake");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("page-parent"),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
        ))
        .expect("save page directory parent");
    let path = fixture.write_raw("Roadmap/Draft/page.md", "---\ntitle: Draft\n---\n# Draft\n");

    let prepared = prepare_push(&store, &job(path), None, &validator).expect("prepare push");

    assert_eq!(prepared.pipeline.action, PushPipelineAction::ConfirmPlan);
    assert_eq!(
        validator.parents.borrow().as_slice(),
        &[RemoteId::new("page-parent")]
    );
    let plan = prepared.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity {
            parent_id,
            parent_kind,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, &RemoteId::new("page-parent"));
            assert_eq!(parent_kind, &Some(EntityKind::Page));
            assert_eq!(source_path, &PathBuf::from("Roadmap/Draft/page.md"));
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn prepare_push_preserves_structured_missing_shadow_error() {
    let fixture = PrepareFixture::new();
    let store = fixture.store("fake");
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");

    let error =
        prepare_push(&store, &job(path), None, &LocalSourceValidator).expect_err("missing shadow");

    assert_eq!(
        error,
        PushPrepareError::Store(StoreError::ShadowMissing {
            mount_id: fixture.mount_id.clone(),
            entity_id: RemoteId::new("page-1"),
        })
    );
}

#[derive(Default)]
struct RecordingValidator {
    create_count: Cell<usize>,
    paths: RefCell<Vec<PathBuf>>,
    parents: RefCell<Vec<RemoteId>>,
}

impl SourcePushValidator for RecordingValidator {
    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> afs_core::AfsResult<ValidationReport> {
        self.create_count.set(self.create_count.get() + 1);
        self.paths
            .borrow_mut()
            .push(context.relative_path.to_path_buf());
        if let Some(parent) = context.parent {
            self.parents.borrow_mut().push(parent.remote_id.clone());
        }
        Ok(ValidationReport::clean())
    }
}

struct PrepareFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
}

impl PrepareFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afsd-push-preparation-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = std::env::temp_dir().join(format!(
            "afsd-push-preparation-state-{}-{unique}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        fs::create_dir_all(&state_root).expect("fixture state root");
        Self {
            root,
            state_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self, connector: &str) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                self.mount_id.clone(),
                connector,
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

    fn virtual_store(&self, connector: &str) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(self.mount_id.clone(), connector, self.root.clone())
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store
    }

    fn save_database(&self, store: &mut InMemoryStateStore) {
        store
            .save_entity(EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .expect("save database");
    }

    fn save_parent_page(&self, store: &mut InMemoryStateStore) {
        store
            .save_entity(EntityRecord::new(
                self.mount_id.clone(),
                RemoteId::new("page-parent"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap",
            ))
            .expect("save parent page");
    }

    fn write_page(&self, relative_path: &str, body: &str) -> PathBuf {
        self.write_raw(relative_path, &canonical_markdown("page-1", body))
    }

    fn write_raw(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, contents).expect("fixture file");
        path
    }

    fn write_cache(&self, relative_path: &str, contents: &str) -> PathBuf {
        self.write_raw(&format!(".content/{relative_path}"), contents)
    }

    fn write_virtual_page(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path =
            virtual_fs_content_path(&self.state_root, &self.mount_id, Path::new(relative_path))
                .expect("content path");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("content parent");
        }
        fs::write(&path, contents).expect("write virtual page");
        path
    }

    fn write_virtual_media(&self, relative_path: &Path, contents: &[u8]) -> PathBuf {
        let path = virtual_fs_content_root(&self.state_root, &self.mount_id).join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("media parent");
        }
        fs::write(&path, contents).expect("write virtual media");
        path
    }

    fn write_virtual_media_manifest(&self, local_path: &Path, block_id: &str, bytes: &[u8]) {
        self.write_virtual_media_manifest_with_kind(local_path, "image", block_id, bytes);
    }

    fn write_virtual_media_manifest_with_kind(
        &self,
        local_path: &Path,
        kind: &str,
        block_id: &str,
        bytes: &[u8],
    ) {
        let manifest_path = virtual_fs_content_root(&self.state_root, &self.mount_id)
            .join(".afs/media/manifest.json");
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
                        "source_url": format!("https://example.com/{kind}"),
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

    fn write_tasks_schema(&self) {
        self.write_raw("Tasks/_schema.yaml", tasks_schema());
    }
}

impl Drop for PrepareFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

fn job(target_path: PathBuf) -> PushJob {
    PushJob {
        target_path,
        assume_yes: false,
        confirm_dangerous: false,
    }
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nafs:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}

fn row_frontmatter(status: &str) -> String {
    format!(
        "afs:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: {status}\n"
    )
}

fn tasks_schema() -> &'static str {
    r#"afs:
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
"#
}

fn virtual_mutation(
    mount_id: &MountId,
    local_id: &str,
    parent_remote_id: Option<RemoteId>,
    path: &str,
    content_path: PathBuf,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind: VirtualMutationKind::Create,
        target_remote_id: None,
        parent_remote_id,
        original_path: None,
        projected_path: PathBuf::from(path),
        title: "Draft".to_string(),
        content_path: Some(content_path),
        created_at: "2026-06-12T00:00:00Z".to_string(),
        updated_at: "2026-06-12T00:00:00Z".to_string(),
    }
}
