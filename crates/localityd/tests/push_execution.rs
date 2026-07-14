use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind, PushPlan};
use locality_core::push::PushExecutionAction;
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use locality_notion::client::NotionApi;
use locality_notion::dto::{
    BlockDto, BlockListDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto,
    RichTextBlockDto, RichTextDto, TextRichTextDto,
};
use locality_notion::{NotionConfig, NotionConnector};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, EntityRecord,
    EntityRepository, InMemoryStateStore, JournalRepository, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use localityd::execution::{DaemonExecutor, PushJob};
use localityd::hydration::{HydratedEntity, HydrationQueue, HydrationSource};
use localityd::push::{
    PushJobAction, execute_auto_save_push_job_with_content_root, execute_push_job_with_content_root,
};
use localityd::scheduler::PullScheduler;
use localityd::supervisor::DaemonSupervisor;
use localityd::virtual_fs::{virtual_fs_content_path, virtual_fs_content_root};
use localityd::watcher::FileWatcher;
use serde_json::Value;

#[test]
fn daemon_push_job_reports_not_ready_for_noop_without_touching_journal() {
    let fixture = PushFixture::new();
    let mut supervisor = fixture.supervisor("Same body.");
    fixture.write_page("Same body.");
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_push(fixture.push_job(true), &FakePushSource::default())
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert!(matches!(
        report.execution.expect("execution").action,
        PushExecutionAction::NotReady { .. }
    ));
    assert!(
        supervisor
            .store()
            .list_journal()
            .expect("journal")
            .is_empty()
    );
}

#[test]
fn daemon_push_job_applies_and_reconciles_through_single_store_owner() {
    let fixture = PushFixture::new();
    let mut supervisor = fixture.supervisor("Old body.");
    fixture.write_page("New body.");
    supervisor.start().expect("start supervisor");
    let source = FakePushSource::with_remote_transition(
        rendered_entity("page-1", "Old body."),
        rendered_entity("page-1", "New body."),
    );

    let report = supervisor
        .execute_push(fixture.push_job(true), &source)
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert_eq!(
        report.execution.as_ref().expect("execution").journal_status,
        Some(JournalStatus::Reconciled)
    );
    assert_eq!(source.applied_count(), 1);
    assert_eq!(
        source.requested_paths(),
        vec![PathBuf::from("Roadmap.md"), PathBuf::from("Roadmap.md")]
    );

    let entity = supervisor
        .store()
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    assert_eq!(
        entity.remote_edited_at,
        Some("2026-06-11T00:00:00Z".to_string())
    );
    let shadow = supervisor
        .store()
        .load_shadow(&fixture.mount_id, &fixture.remote_id)
        .expect("load shadow");
    assert!(shadow.rendered_body.contains("New body."));
    let journal = supervisor.store().list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].status, JournalStatus::Reconciled);
}

#[test]
fn auto_save_push_applies_safe_update_and_keeps_enrollment_active() {
    let fixture = PushFixture::new();
    let mut store = fixture.store("Old body.");
    store
        .save_auto_save_enrollment(
            AutoSaveEnrollmentRecord::new(
                fixture.mount_id.clone(),
                "Roadmap.md",
                AutoSaveOrigin::LocalityCreated,
                "now",
            )
            .active("now"),
        )
        .expect("save enrollment");
    fixture.write_page("New body.");
    let source = FakePushSource::with_remote_transition(
        rendered_entity("page-1", "Old body."),
        rendered_entity("page-1", "New body."),
    );

    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        fixture.push_job(false),
        &source,
        None,
    )
    .expect("auto-save push");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert_eq!(source.applied_count(), 1);
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, Path::new("Roadmap.md"))
        .expect("get enrollment")
        .expect("enrollment");
    assert!(enrollment.enabled);
    assert_eq!(enrollment.state, AutoSaveState::Active);
    assert_eq!(enrollment.remote_id, Some(fixture.remote_id.clone()));
    assert!(enrollment.last_push_id.is_some());
}

#[test]
fn manual_push_reactivates_paused_file_live_mode_after_conflict_resolution() {
    let fixture = PushFixture::new();
    let mut store = fixture.store("Old body.");
    let mut enrollment = AutoSaveEnrollmentRecord::new(
        fixture.mount_id.clone(),
        "Roadmap.md",
        AutoSaveOrigin::UserEnabled,
        "1",
    )
    .paused_remote_changed("Notion changed externally", "2");
    enrollment.remote_id = Some(fixture.remote_id.clone());
    store
        .save_auto_save_enrollment(enrollment)
        .expect("save enrollment");
    fixture.write_page("New body.");
    let source = FakePushSource::with_remote_transition(
        rendered_entity("page-1", "Old body."),
        rendered_entity("page-1", "New body."),
    );

    let report =
        execute_push_job_with_content_root(&mut store, fixture.push_job(true), &source, None)
            .expect("execute push");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, Path::new("Roadmap.md"))
        .expect("get enrollment")
        .expect("enrollment");
    assert_eq!(enrollment.state, AutoSaveState::Active);
    assert_eq!(enrollment.last_reason, None);
    assert_eq!(enrollment.remote_id, Some(fixture.remote_id.clone()));
}

#[test]
fn auto_save_push_pauses_when_remote_changed_before_apply() {
    let fixture = PushFixture::new();
    let mut store = fixture.store("Old body.");
    store
        .save_auto_save_enrollment(
            AutoSaveEnrollmentRecord::new(
                fixture.mount_id.clone(),
                "Roadmap.md",
                AutoSaveOrigin::LocalityCreated,
                "now",
            )
            .active("now"),
        )
        .expect("save enrollment");
    fixture.write_page("New body.");
    let source = FakePushSource::with_remote(rendered_entity("page-1", "Remote body."));

    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        fixture.push_job(false),
        &source,
        None,
    )
    .expect("auto-save push");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(source.applied_count(), 0);
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, Path::new("Roadmap.md"))
        .expect("get enrollment")
        .expect("enrollment");
    assert_eq!(enrollment.state, AutoSaveState::PausedRemoteChanged);
    assert!(
        enrollment
            .last_reason
            .as_deref()
            .unwrap_or_default()
            .contains("changed since")
    );
}

#[test]
fn daemon_push_job_blocks_when_remote_tree_content_changed_before_apply() {
    let fixture = PushFixture::new();
    let mut supervisor = fixture.supervisor("Old body.");
    fixture.write_page("New body.");
    supervisor.start().expect("start supervisor");
    let source = FakePushSource::with_remote(rendered_entity("page-1", "Remote body."));

    let report = supervisor
        .execute_push(fixture.push_job(true), &source)
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(report.journal_status, Some(JournalStatus::Reverted));
    assert_eq!(source.applied_count(), 0);
    assert_eq!(report.error.as_ref().expect("error").code, "guardrail");
    assert!(
        report
            .error
            .as_ref()
            .expect("error")
            .message
            .contains("changed since the Synced Tree shadow")
    );
    let journal = supervisor.store().list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].status, JournalStatus::Reverted);
}

#[test]
fn daemon_push_job_rejects_second_state_push_after_first_state_wins_race() {
    let first = PushFixture::new();
    let second = PushFixture::new();
    let mut first_store = first.store("Old body.");
    let mut second_store = second.store("Old body.");
    first_store
        .save_shadow(
            &first.mount_id,
            notion_shadow("page-1", "Old body.", "2026-06-10T00:00:00Z"),
        )
        .expect("save first notion shadow");
    second_store
        .save_shadow(
            &second.mount_id,
            notion_shadow("page-1", "Old body.", "2026-06-10T00:00:00Z"),
        )
        .expect("save second notion shadow");
    first.write_page("First client body.");
    second.write_page("Second client body.");
    let api = Arc::new(RacyNotionApi::new("Old body.", "2026-06-10T00:00:00Z"));
    let second_api = api.clone();
    let second_job = second.push_job(true);

    let second_push = std::thread::spawn(move || {
        let connector = NotionConnector::with_api(NotionConfig::default(), second_api);
        execute_push_job_with_content_root(&mut second_store, second_job, &connector, None)
    });
    api.wait_until_second_state_preflight_read();

    let first_connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let first_report = execute_push_job_with_content_root(
        &mut first_store,
        first.push_job(true),
        &first_connector,
        None,
    )
    .expect("first push");
    assert_eq!(first_report.action, PushJobAction::Reconciled);
    assert_eq!(api.remote_body(), "First client body.");

    api.release_second_state_preflight_read();
    let second_report = second_push
        .join()
        .expect("second push thread")
        .expect("second push");

    assert_eq!(second_report.action, PushJobAction::Failed);
    assert_eq!(api.write_count(), 1);
    assert_eq!(api.remote_body(), "First client body.");
    let error = second_report.error.as_ref().expect("error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("changed since last sync"));
}

#[test]
fn daemon_push_job_preflights_unsupported_operations_before_journal() {
    let fixture = PushFixture::new();
    let mut supervisor = fixture.supervisor("Old body.");
    fixture.write_page("New body.");
    supervisor.start().expect("start supervisor");
    let source = FakePushSource::with_remote(rendered_entity("page-1", "New body."))
        .with_supported_operations(BTreeSet::new());

    let report = supervisor
        .execute_push(fixture.push_job(true), &source)
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert_eq!(
        report.pipeline.action,
        locality_core::push::PushPipelineAction::unsupported_operations(vec![
            "update_block".to_string()
        ])
    );
    assert_eq!(source.applied_count(), 0);
    assert!(
        supervisor
            .store()
            .list_journal()
            .expect("journal")
            .is_empty()
    );
}

#[test]
fn daemon_push_job_blocks_database_row_schema_violation_before_apply() {
    let fixture = PushFixture::new();
    let mut store = InMemoryStateStore::new();
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
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00Z"),
        )
        .expect("save row");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                "# Notes\n\nExisting body.\n",
                9,
                [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
            )
            .expect("shadow")
            .with_frontmatter(row_frontmatter("Todo")),
        )
        .expect("save shadow");
    fs::create_dir_all(fixture.root.join("Tasks")).expect("tasks dir");
    fs::write(fixture.root.join("Tasks/_schema.yaml"), tasks_schema()).expect("schema");
    fs::write(
        fixture.root.join("Tasks/existing-task.md"),
        format!(
            "---\n{}---\n# Notes\n\nExisting body.\n",
            row_frontmatter("Blocked")
        ),
    )
    .expect("row file");
    let mut supervisor = DaemonSupervisor::new(
        store,
        RecordingWatcher::default(),
        HydrationQueue::new(),
        PullScheduler::new(Default::default()),
    );
    supervisor.start().expect("start supervisor");
    let source = FakePushSource::default();

    let report = supervisor
        .execute_push(
            PushJob {
                target_path: fixture.root.join("Tasks/existing-task.md"),
                assume_yes: true,
                confirm_dangerous: false,
            },
            &source,
        )
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert_eq!(
        report.pipeline.action,
        locality_core::push::PushPipelineAction::FixValidation
    );
    assert_eq!(
        report.pipeline.validation.issues[0].code,
        "notion_schema_option_unknown"
    );
    assert_eq!(source.applied_count(), 0);
    assert!(
        supervisor
            .store()
            .list_journal()
            .expect("journal")
            .is_empty()
    );
}

#[test]
fn daemon_push_job_plans_pending_virtual_create() {
    let fixture = PushFixture::new();
    let cache_path = fixture.root.join(".content/Draft.md");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, "---\ntitle: Draft\n---\n# Draft\n\nBody.\n").expect("cache file");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            fixture.remote_id.clone(),
            EntityKind::Page,
            "Roadmap",
            "Roadmap.md",
        ))
        .expect("save parent page");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            VirtualMutationKind::Create,
            None,
            Some(fixture.remote_id.clone()),
            "Roadmap/Draft.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let mut supervisor = DaemonSupervisor::new(
        store,
        RecordingWatcher::default(),
        HydrationQueue::new(),
        PullScheduler::new(Default::default()),
    );
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_push(
            PushJob {
                target_path: fixture.root.join("Roadmap/Draft.md"),
                assume_yes: false,
                confirm_dangerous: false,
            },
            &FakePushSource::default(),
        )
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    let plan = report.pipeline.plan.expect("plan");
    assert_eq!(plan.operations.len(), 1);
    match &plan.operations[0] {
        PushOperation::CreateEntity {
            parent_id,
            parent_kind,
            title,
            source_path,
            ..
        } => {
            assert_eq!(parent_id, &fixture.remote_id);
            assert_eq!(parent_kind, &Some(EntityKind::Page));
            assert_eq!(title, "Draft");
            assert_eq!(source_path, &PathBuf::from("Roadmap/Draft.md"));
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn daemon_push_job_reads_explicit_pending_virtual_create_from_projected_path() {
    let fixture = PushFixture::new();
    let cache_path = fixture.root.join(".content/Roadmap/Draft/page.md");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, "").expect("stale cache file");
    let projected_path = fixture.root.join("Roadmap/Draft/page.md");
    fs::create_dir_all(projected_path.parent().expect("projected parent"))
        .expect("projected parent");
    fs::write(
        &projected_path,
        "---\ntitle: Fresh Draft\n---\n# Fresh Draft\n\nProjected body.\n",
    )
    .expect("projected file");

    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::WindowsCloudFiles),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            fixture.remote_id.clone(),
            EntityKind::Page,
            "Roadmap",
            "Roadmap.md",
        ))
        .expect("save parent page");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            VirtualMutationKind::Create,
            None,
            Some(fixture.remote_id.clone()),
            "Roadmap/Draft/page.md",
            Some(cache_path),
        ))
        .expect("save mutation");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: projected_path,
            assume_yes: false,
            confirm_dangerous: false,
        },
        &FakePushSource::default(),
        None,
    )
    .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert!(report.pipeline.validation.issues.is_empty());
    let plan = report.pipeline.plan.expect("plan");
    match &plan.operations[0] {
        PushOperation::CreateEntity {
            title,
            body,
            source_path,
            ..
        } => {
            assert_eq!(title, "Fresh Draft");
            assert_eq!(body, "# Fresh Draft\n\nProjected body.\n");
            assert_eq!(source_path, &PathBuf::from("Roadmap/Draft/page.md"));
        }
        operation => panic!("unexpected operation: {operation:?}"),
    }
}

#[test]
fn daemon_push_reconciles_redundant_pending_create_before_planning_existing_page() {
    let fixture = PushFixture::new();
    let source_path = PathBuf::from("Roadmap/Draft/page.md");
    let page_path = fixture.root.join(&source_path);
    fs::create_dir_all(page_path.parent().expect("page parent")).expect("page parent");
    let document = CanonicalDocument::new(
        "loc:\n  id: page-2\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Draft\n",
        markdown_body("New body."),
    );
    fs::write(&page_path, render_canonical_markdown(&document)).expect("write page");

    let mut store = InMemoryStateStore::new();
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
            fixture.remote_id.clone(),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
        ))
        .expect("save parent page");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-2"),
                EntityKind::Page,
                "Draft",
                source_path.clone(),
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00Z"),
        )
        .expect("save tracked page");
    store
        .save_shadow(&fixture.mount_id, shadow("page-2", "Old body."))
        .expect("save shadow");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:stale-create",
            VirtualMutationKind::Create,
            None,
            Some(fixture.remote_id.clone()),
            "Roadmap/Draft/page.md",
            Some(page_path.clone()),
        ))
        .expect("save stale pending create");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: page_path,
            assume_yes: false,
            confirm_dangerous: false,
        },
        &FakePushSource::default(),
        None,
    )
    .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    let plan = report.pipeline.plan.expect("plan");
    assert!(matches!(
        plan.operations.as_slice(),
        [PushOperation::UpdateBlock { block_id, content }]
            if block_id == &RemoteId::new("paragraph-1") && content == "New body."
    ));
    assert!(
        store
            .get_virtual_mutation(&fixture.mount_id, "local:stale-create")
            .expect("load stale mutation")
            .is_none(),
        "reconciliation should clear the redundant pending create"
    );
}

#[test]
fn auto_save_push_applies_pending_virtual_create_and_tracks_created_remote() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("Roadmap/Draft.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, "---\ntitle: Draft\n---\n# Draft\n\nBody.\n").expect("cache file");

    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            fixture.remote_id.clone(),
            EntityKind::Page,
            "Roadmap",
            "Roadmap.md",
        ))
        .expect("save parent page");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            VirtualMutationKind::Create,
            None,
            Some(fixture.remote_id.clone()),
            "Roadmap/Draft.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    store
        .save_auto_save_enrollment(AutoSaveEnrollmentRecord::new(
            fixture.mount_id.clone(),
            source_path,
            AutoSaveOrigin::LocalityCreated,
            "now",
        ))
        .expect("save enrollment");
    let created_remote_id = RemoteId::new("page-2");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_entity("page-2", "Body."),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-draft".to_string()),
            operation_index: 0,
            parent_id: fixture.remote_id.clone(),
            entity_id: created_remote_id.clone(),
        }]);

    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("auto-save create");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert_eq!(source.applied_count(), 1);
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, source_path)
        .expect("get enrollment")
        .expect("enrollment");
    assert_eq!(enrollment.state, AutoSaveState::Active);
    assert_eq!(enrollment.remote_id, Some(created_remote_id.clone()));
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
            .expect("find mutation")
            .is_none()
    );
}

#[test]
fn daemon_push_reconciles_direct_database_row_create_to_page_document_path() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let projection_root = fixture.root.join("loc");
    let source_path = Path::new("Tasks/new-task.md");
    let target_path = projection_root.join(source_path);
    fs::create_dir_all(target_path.parent().expect("target parent")).expect("target parent");
    fs::write(
        &target_path,
        "---\ntitle: New task\nStatus: Todo\n---\n# New task\n\nBody.\n",
    )
    .expect("write direct row file");

    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    fs::create_dir_all(content_root.join("Tasks")).expect("schema parent");
    fs::write(content_root.join("Tasks/_schema.yaml"), tasks_schema()).expect("write schema");

    let database_id = RemoteId::new("database-1");
    let created_remote_id = RemoteId::new("row-1");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &projection_root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            database_id.clone(),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database");
    let source = FakePushSource::default()
        .with_created_entity(created_remote_id.clone(), rendered_entity("row-1", "Body."))
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-row".to_string()),
            operation_index: 0,
            parent_id: database_id,
            entity_id: created_remote_id.clone(),
        }]);

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path,
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("push direct database row");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let row = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get row")
        .expect("row entity");
    assert_eq!(row.path, PathBuf::from("Tasks/new-task/page.md"));
    assert_eq!(source.requested_paths(), vec![row.path.clone()]);
    assert!(content_root.join("Tasks/new-task/page.md").exists());
    assert!(!content_root.join(source_path).exists());
}

#[test]
fn daemon_push_reconciles_sent_gmail_draft_create_to_sent_folder() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let sent_folder_id = RemoteId::new("gmail-folder:sent");
    let created_remote_id = RemoteId::new("gmail-message:sent-1");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            draft_folder_id.clone(),
            EntityKind::Directory,
            "draft",
            "draft",
        ))
        .expect("save draft folder");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            sent_folder_id.clone(),
            EntityKind::Directory,
            "sent",
            "sent",
        ))
        .expect("save sent folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_entity("gmail-message:sent-1", "Body."),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-gmail-draft".to_string()),
            operation_index: 0,
            parent_id: sent_folder_id,
            entity_id: created_remote_id.clone(),
        }]);

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("push gmail draft");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let message = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get sent message")
        .expect("sent message entity");
    assert_eq!(message.path, PathBuf::from("sent/reply.md"));
    assert_eq!(source.requested_paths(), vec![message.path.clone()]);
    assert!(content_root.join("sent/reply.md").exists());
    assert!(!content_root.join(source_path).exists());
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
            .expect("find mutation")
            .is_none()
    );
}

#[test]
fn auto_save_push_blocks_gmail_draft_send_without_applying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/reply.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let sent_folder_id = RemoteId::new("gmail-folder:sent");
    let created_remote_id = RemoteId::new("gmail-message:sent-1");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            draft_folder_id.clone(),
            EntityKind::Directory,
            "draft",
            "draft",
        ))
        .expect("save draft folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    store
        .save_auto_save_enrollment(AutoSaveEnrollmentRecord::new(
            fixture.mount_id.clone(),
            source_path,
            AutoSaveOrigin::LocalityCreated,
            "now",
        ))
        .expect("save enrollment");
    let source =
        FakePushSource::default().with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-gmail-draft".to_string()),
            operation_index: 0,
            parent_id: sent_folder_id,
            entity_id: created_remote_id,
        }]);

    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("auto-save gmail draft");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert_eq!(source.applied_count(), 0, "auto-save must not send Gmail");
    assert_eq!(
        report.error.as_ref().expect("error").code,
        "auto_save_blocked"
    );
    assert_eq!(
        report.error.as_ref().expect("error").message,
        "Gmail draft sends require review"
    );
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, source_path)
        .expect("get enrollment")
        .expect("enrollment");
    assert_eq!(enrollment.state, AutoSaveState::Blocked);
    assert_eq!(
        enrollment.last_reason.as_deref(),
        Some("Gmail draft sends require review")
    );
    assert!(store.list_journal().expect("journal").is_empty());
}

#[test]
fn daemon_push_resumes_failed_gmail_send_reconciliation_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let sent_folder_id = RemoteId::new("gmail-folder:sent");
    let created_remote_id = RemoteId::new("gmail-message:sent-1");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            draft_folder_id.clone(),
            EntityKind::Directory,
            "draft",
            "draft",
        ))
        .expect("save draft folder");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            sent_folder_id.clone(),
            EntityKind::Directory,
            "sent",
            "sent",
        ))
        .expect("save sent folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_entity("gmail-message:sent-1", "Body."),
        )
        .with_created_fetch_failures(created_remote_id.clone(), 1)
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-gmail-draft".to_string()),
            operation_index: 0,
            parent_id: sent_folder_id,
            entity_id: created_remote_id.clone(),
        }]);
    let job = || PushJob {
        target_path: fixture.root.join(source_path),
        assume_yes: true,
        confirm_dangerous: false,
    };

    let first = execute_push_job_with_content_root(&mut store, job(), &source, Some(&state_root))
        .expect("first push");

    assert_eq!(first.action, PushJobAction::Failed);
    assert_eq!(source.applied_count(), 1);
    let first_push_id = first.push_id.expect("first push id");
    let journal = store.list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    assert!(matches!(journal[0].status, JournalStatus::Failed(_)));
    assert_eq!(journal[0].apply_effects.len(), 1);
    let edited_cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::write(
        &edited_cache_path,
        "---\ntitle: Edited reply\nto: [\"user@example.com\"]\nsubject: Edited reply\n---\nChanged body.\n",
    )
    .expect("edit stale draft");

    let second = execute_push_job_with_content_root(&mut store, job(), &source, Some(&state_root))
        .expect("retry push");

    assert_eq!(second.action, PushJobAction::Reconciled);
    assert_eq!(source.applied_count(), 1, "retry must not resend Gmail");
    assert_eq!(second.push_id.as_ref(), Some(&first_push_id));
    let journal = store.list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].status, JournalStatus::Reconciled);
    let message = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get sent message")
        .expect("sent message entity");
    assert_eq!(message.path, PathBuf::from("sent/reply.md"));
    assert!(content_root.join("sent/reply.md").exists());
    assert!(content_root.join(source_path).exists());
    assert_eq!(
        fs::read_to_string(content_root.join(source_path)).expect("preserved edited draft"),
        "---\ntitle: Edited reply\nto: [\"user@example.com\"]\nsubject: Edited reply\n---\nChanged body.\n"
    );
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
            .expect("find mutation")
            .is_some()
    );
}

#[test]
fn daemon_push_resumes_applied_gmail_send_reconciliation_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let sent_folder_id = RemoteId::new("gmail-folder:sent");
    let created_remote_id = RemoteId::new("gmail-message:sent-1");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            draft_folder_id.clone(),
            EntityKind::Directory,
            "draft",
            "draft",
        ))
        .expect("save draft folder");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            sent_folder_id.clone(),
            EntityKind::Directory,
            "sent",
            "sent",
        ))
        .expect("save sent folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id.clone()),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");

    let mut properties = BTreeMap::new();
    properties.insert(
        "subject".to_string(),
        PropertyValue::String("Reply".to_string()),
    );
    properties.insert(
        "to".to_string(),
        PropertyValue::List(vec!["user@example.com".to_string()]),
    );
    let plan = PushPlan::new(
        vec![draft_folder_id],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("gmail-folder:draft"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Reply".to_string(),
            properties,
            body: "Body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-already-applied-gmail-draft".to_string());
    let effect = JournalApplyEffect::CreatedEntity {
        operation_id: PushOperationId("create-gmail-draft".to_string()),
        operation_index: 0,
        parent_id: sent_folder_id.clone(),
        entity_id: created_remote_id.clone(),
    };
    store
        .append_journal(
            JournalEntry::new(
                push_id.clone(),
                fixture.mount_id.clone(),
                plan.affected_entities.clone(),
                plan,
                JournalStatus::Applied,
            )
            .with_apply_effects(vec![effect.clone()]),
        )
        .expect("append applied journal");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_entity("gmail-message:sent-1", "Body."),
        )
        .with_apply_effects(vec![effect]);

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("retry applied gmail push");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert_eq!(source.applied_count(), 0, "retry must not resend Gmail");
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    let journal = store.list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].status, JournalStatus::Reconciled);
    let message = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get sent message")
        .expect("sent message entity");
    assert_eq!(message.path, PathBuf::from("sent/reply.md"));
    assert!(content_root.join("sent/reply.md").exists());
    assert!(!content_root.join(source_path).exists());
}

#[test]
fn daemon_push_blocks_ambiguous_gmail_send_journal_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/reply.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let sent_folder_id = RemoteId::new("gmail-folder:sent");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            draft_folder_id.clone(),
            EntityKind::Directory,
            "draft",
            "draft",
        ))
        .expect("save draft folder");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            sent_folder_id,
            EntityKind::Directory,
            "sent",
            "sent",
        ))
        .expect("save sent folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id.clone()),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");

    let mut properties = BTreeMap::new();
    properties.insert(
        "subject".to_string(),
        PropertyValue::String("Reply".to_string()),
    );
    properties.insert(
        "to".to_string(),
        PropertyValue::List(vec!["user@example.com".to_string()]),
    );
    let plan = PushPlan::new(
        vec![draft_folder_id],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("gmail-folder:draft"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Reply".to_string(),
            properties,
            body: "Body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-ambiguous-gmail-draft".to_string());
    store
        .append_journal(JournalEntry::new(
            push_id.clone(),
            fixture.mount_id.clone(),
            plan.affected_entities.clone(),
            plan,
            JournalStatus::Applying,
        ))
        .expect("append applying journal");
    let source = FakePushSource::default();

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("retry ambiguous gmail push");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(source.applied_count(), 0, "retry must not resend Gmail");
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    assert_eq!(report.journal_status, Some(JournalStatus::Applying));
    let error = report.error.expect("guardrail error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("ambiguous result"));
}

#[test]
fn daemon_push_blocks_failed_gmail_send_recovery_lookup_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/reply.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let sent_folder_id = RemoteId::new("gmail-folder:sent");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            draft_folder_id.clone(),
            EntityKind::Directory,
            "draft",
            "draft",
        ))
        .expect("save draft folder");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            sent_folder_id,
            EntityKind::Directory,
            "sent",
            "sent",
        ))
        .expect("save sent folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id.clone()),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");

    let mut properties = BTreeMap::new();
    properties.insert(
        "subject".to_string(),
        PropertyValue::String("Reply".to_string()),
    );
    properties.insert(
        "to".to_string(),
        PropertyValue::List(vec!["user@example.com".to_string()]),
    );
    let plan = PushPlan::new(
        vec![draft_folder_id],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("gmail-folder:draft"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Reply".to_string(),
            properties,
            body: "Body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-failed-gmail-send-lookup".to_string());
    store
        .append_journal(JournalEntry::new(
            push_id.clone(),
            fixture.mount_id.clone(),
            plan.affected_entities.clone(),
            plan,
            JournalStatus::Failed(
                "io error: gmail draft send ambiguous after send failure; sent lookup failed: sent search timed out"
                    .to_string(),
            ),
        ))
        .expect("append failed journal");
    let source = FakePushSource::default();

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("retry failed gmail push");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(source.applied_count(), 0, "retry must not resend Gmail");
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    let error = report.error.expect("guardrail error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("ambiguous result"));
}

#[test]
fn daemon_push_reconciles_repeated_gmail_draft_filename_to_unique_sent_paths() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody one.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let sent_folder_id = RemoteId::new("gmail-folder:sent");
    let first_remote_id = RemoteId::new("gmail-message:sent-1");
    let second_remote_id = RemoteId::new("gmail-message:sent-2");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            draft_folder_id.clone(),
            EntityKind::Directory,
            "draft",
            "draft",
        ))
        .expect("save draft folder");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            sent_folder_id.clone(),
            EntityKind::Directory,
            "sent",
            "sent",
        ))
        .expect("save sent folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft-1",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id.clone()),
            "draft/reply.md",
            Some(cache_path.clone()),
        ))
        .expect("save first mutation");
    let first_source = FakePushSource::default()
        .with_created_entity(
            first_remote_id.clone(),
            rendered_gmail_entity(
                "gmail-message:sent-1",
                "Reply",
                "1720900000000",
                "Body one.",
            ),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-gmail-draft-1".to_string()),
            operation_index: 0,
            parent_id: sent_folder_id.clone(),
            entity_id: first_remote_id.clone(),
        }]);

    let first = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &first_source,
        Some(&state_root),
    )
    .expect("first push");

    assert_eq!(first.action, PushJobAction::Reconciled);
    let first_message = store
        .get_entity(&fixture.mount_id, &first_remote_id)
        .expect("get first sent message")
        .expect("first sent message");
    assert_eq!(
        first_message.path,
        PathBuf::from("sent/1720900000000-reply-gmail-message-sent-1.md")
    );

    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody two.\n",
    )
    .expect("second cache file");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-draft-2",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save second mutation");
    let second_source = FakePushSource::default()
        .with_created_entity(
            second_remote_id.clone(),
            rendered_gmail_entity(
                "gmail-message:sent-2",
                "Reply",
                "1720900001000",
                "Body two.",
            ),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-gmail-draft-2".to_string()),
            operation_index: 0,
            parent_id: sent_folder_id,
            entity_id: second_remote_id.clone(),
        }]);

    let second = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &second_source,
        Some(&state_root),
    )
    .expect("second push");

    assert_eq!(second.action, PushJobAction::Reconciled);
    let second_message = store
        .get_entity(&fixture.mount_id, &second_remote_id)
        .expect("get second sent message")
        .expect("second sent message");
    assert_eq!(
        second_message.path,
        PathBuf::from("sent/1720900001000-reply-gmail-message-sent-2.md")
    );
    assert!(content_root.join(first_message.path).exists());
    assert!(content_root.join(second_message.path).exists());
}

#[test]
fn daemon_push_job_plans_pending_virtual_delete_from_scope() {
    let fixture = PushFixture::new();
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save page");
    store
        .save_shadow(&fixture.mount_id, shadow("page-1", "Old body."))
        .expect("save shadow");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "delete:page-1",
            VirtualMutationKind::Delete,
            Some(fixture.remote_id.clone()),
            None,
            "Roadmap.md",
            None,
        ))
        .expect("save mutation");
    let mut supervisor = DaemonSupervisor::new(
        store,
        RecordingWatcher::default(),
        HydrationQueue::new(),
        PullScheduler::new(Default::default()),
    );
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_push(
            PushJob {
                target_path: fixture.root.clone(),
                assume_yes: false,
                confirm_dangerous: false,
            },
            &FakePushSource::default(),
        )
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    let plan = report.pipeline.plan.expect("plan");
    assert_eq!(
        plan.operations,
        vec![PushOperation::ArchiveEntity {
            entity_id: fixture.remote_id.clone()
        }]
    );
}

#[test]
fn daemon_push_job_plans_pending_virtual_delete_from_file_path() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save page");
    store
        .save_shadow(&fixture.mount_id, shadow("page-1", "Old body."))
        .expect("save shadow");
    let cached_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, Path::new("Roadmap.md"))
            .expect("cache path");
    fs::create_dir_all(cached_path.parent().expect("cache parent")).expect("cache parent");
    fixture.write_page_to(&cached_path, "Old body.");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "delete:page-1",
            VirtualMutationKind::Delete,
            Some(fixture.remote_id.clone()),
            None,
            "Roadmap.md",
            None,
        ))
        .expect("save mutation");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join("Roadmap.md"),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &FakePushSource::default(),
        Some(&state_root),
    )
    .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    let plan = report.pipeline.plan.expect("plan");
    assert_eq!(
        plan.operations,
        vec![PushOperation::ArchiveEntity {
            entity_id: fixture.remote_id.clone()
        }]
    );
}

#[test]
fn auto_save_push_blocks_pending_virtual_delete_without_applying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save page");
    store
        .save_shadow(&fixture.mount_id, shadow("page-1", "Old body."))
        .expect("save shadow");
    let cached_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, Path::new("Roadmap.md"))
            .expect("cache path");
    fs::create_dir_all(cached_path.parent().expect("cache parent")).expect("cache parent");
    fixture.write_page_to(&cached_path, "Old body.");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "delete:page-1",
            VirtualMutationKind::Delete,
            Some(fixture.remote_id.clone()),
            None,
            "Roadmap.md",
            None,
        ))
        .expect("save mutation");
    store
        .save_auto_save_enrollment(AutoSaveEnrollmentRecord::new(
            fixture.mount_id.clone(),
            "Roadmap.md",
            AutoSaveOrigin::LocalityCreated,
            "now",
        ))
        .expect("save enrollment");
    let source = FakePushSource::default();

    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join("Roadmap.md"),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("auto-save delete");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert_eq!(source.applied_count(), 0);
    assert_eq!(
        report.error.as_ref().expect("error").code,
        "auto_save_blocked"
    );
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, Path::new("Roadmap.md"))
        .expect("get enrollment")
        .expect("enrollment");
    assert_eq!(enrollment.state, AutoSaveState::Blocked);
    assert_eq!(
        enrollment.last_reason.as_deref(),
        Some("deletions require review")
    );
    assert!(store.list_journal().expect("journal").is_empty());
}

#[test]
fn daemon_push_job_plans_normal_update_for_pending_virtual_rename_path() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let renamed_path = Path::new("Roadmap-renamed.md");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                EntityKind::Page,
                "Roadmap renamed",
                renamed_path,
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save renamed page");
    store
        .save_shadow(&fixture.mount_id, shadow("page-1", "Old body."))
        .expect("save shadow");
    let cached_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, renamed_path).expect("cache path");
    fs::create_dir_all(cached_path.parent().expect("cache parent")).expect("cache parent");
    fixture.write_page_to(&cached_path, "New body.");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "rename:page-1",
            VirtualMutationKind::Rename,
            Some(fixture.remote_id.clone()),
            None,
            "Roadmap-renamed.md",
            Some(cached_path),
        ))
        .expect("save mutation");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(renamed_path),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &FakePushSource::default(),
        Some(&state_root),
    )
    .expect("execute push");

    assert_eq!(report.action, PushJobAction::NotReady);
    let plan = report.pipeline.plan.expect("plan");
    assert!(matches!(
        plan.operations.as_slice(),
        [PushOperation::UpdateBlock { block_id, content }]
            if block_id == &RemoteId::new("paragraph-1") && content == "New body."
    ));
}

struct PushFixture {
    root: PathBuf,
    mount_id: MountId,
    remote_id: RemoteId,
}

impl PushFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("loc-daemon-push-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("fixture root");

        Self {
            root,
            mount_id: MountId::new("notion-main"),
            remote_id: RemoteId::new("page-1"),
        }
    }

    fn supervisor(
        &self,
        synced_body: &str,
    ) -> DaemonSupervisor<InMemoryStateStore, RecordingWatcher, HydrationQueue> {
        let store = self.store(synced_body);

        DaemonSupervisor::new(
            store,
            RecordingWatcher::default(),
            HydrationQueue::new(),
            PullScheduler::new(Default::default()),
        )
    }

    fn store(&self, synced_body: &str) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(self.mount_id.clone(), "notion", self.root.clone());
        store.save_mount(mount).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    self.remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Hydrated)
                .with_remote_edited_at("2026-06-10T00:00:00Z"),
            )
            .expect("save entity");
        store
            .save_shadow(&self.mount_id, shadow("page-1", synced_body))
            .expect("save shadow");
        store
    }

    fn push_job(&self, assume_yes: bool) -> PushJob {
        PushJob {
            target_path: self.root.join("Roadmap.md"),
            assume_yes,
            confirm_dangerous: false,
        }
    }

    fn write_page(&self, body: &str) {
        self.write_page_to(&self.root.join("Roadmap.md"), body);
    }

    fn write_page_to(&self, path: &Path, body: &str) {
        let document = CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            markdown_body(body),
        );
        fs::write(path, render_canonical_markdown(&document)).expect("write page");
    }
}

impl Drop for PushFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RecordingWatcher {
    watched: Vec<PathBuf>,
}

impl FileWatcher for RecordingWatcher {
    fn watch_mount(&mut self, root: PathBuf) -> LocalityResult<()> {
        self.watched.push(root);
        Ok(())
    }
}

#[derive(Default)]
struct FakePushSource {
    remote_before_apply: Option<HydratedEntity>,
    remote_after_apply: Option<HydratedEntity>,
    applied: std::cell::Cell<usize>,
    requested_paths: std::cell::RefCell<Vec<PathBuf>>,
    supported_operations: Option<BTreeSet<PushOperationKind>>,
    created_entities: BTreeMap<RemoteId, HydratedEntity>,
    created_fetch_failures: std::cell::RefCell<BTreeMap<RemoteId, usize>>,
    apply_effects: Vec<JournalApplyEffect>,
}

impl FakePushSource {
    fn with_remote(remote: HydratedEntity) -> Self {
        Self {
            remote_before_apply: Some(remote.clone()),
            remote_after_apply: Some(remote),
            ..Self::default()
        }
    }

    fn with_remote_transition(
        remote_before_apply: HydratedEntity,
        remote_after_apply: HydratedEntity,
    ) -> Self {
        Self {
            remote_before_apply: Some(remote_before_apply),
            remote_after_apply: Some(remote_after_apply),
            ..Self::default()
        }
    }

    fn applied_count(&self) -> usize {
        self.applied.get()
    }

    fn requested_paths(&self) -> Vec<PathBuf> {
        self.requested_paths.borrow().clone()
    }

    fn with_supported_operations(
        mut self,
        supported_operations: BTreeSet<PushOperationKind>,
    ) -> Self {
        self.supported_operations = Some(supported_operations);
        self
    }

    fn with_created_entity(mut self, remote_id: RemoteId, rendered: HydratedEntity) -> Self {
        self.created_entities.insert(remote_id, rendered);
        self
    }

    fn with_created_fetch_failures(mut self, remote_id: RemoteId, failures: usize) -> Self {
        self.created_fetch_failures
            .get_mut()
            .insert(remote_id, failures);
        self
    }

    fn with_apply_effects(mut self, effects: Vec<JournalApplyEffect>) -> Self {
        self.apply_effects = effects;
        self
    }
}

impl HydrationSource for FakePushSource {
    fn fetch_render(
        &self,
        request: &locality_core::hydration::HydrationRequest,
    ) -> LocalityResult<HydratedEntity> {
        self.requested_paths.borrow_mut().push(request.path.clone());
        if let Some(remaining) = self
            .created_fetch_failures
            .borrow_mut()
            .get_mut(&request.remote_id)
            && *remaining > 0
        {
            *remaining -= 1;
            return Err(LocalityError::InvalidState(
                "injected created entity fetch failure".to_string(),
            ));
        }
        if let Some(rendered) = self.created_entities.get(&request.remote_id) {
            return Ok(rendered.clone());
        }
        if request.remote_id != RemoteId::new("page-1") {
            return Err(LocalityError::InvalidState(
                "unexpected remote id".to_string(),
            ));
        }

        let remote = if self.applied.get() == 0 {
            self.remote_before_apply.clone()
        } else {
            self.remote_after_apply.clone()
        };
        remote.ok_or_else(|| LocalityError::InvalidState("missing remote fixture".to_string()))
    }
}

impl Connector for FakePushSource {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("fake")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: false,
            supports_oauth: false,
            ..ConnectorCapabilities::default()
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        self.supported_operations
            .clone()
            .unwrap_or_else(|| PushOperationKind::all().into_iter().collect())
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Err(LocalityError::NotImplemented("fake enumerate"))
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        Err(LocalityError::NotImplemented("fake fetch"))
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        Err(LocalityError::NotImplemented("fake render"))
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::NotImplemented("fake parse"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        self.applied.set(self.applied.get() + 1);
        let changed_remote_ids = if self.apply_effects.is_empty() {
            request.plan.affected_entities.clone()
        } else {
            Vec::new()
        };
        Ok(ApplyPlanResult {
            changed_remote_ids,
            effects: self.apply_effects.clone(),
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::NotImplemented("fake undo"))
    }
}

fn rendered_entity(remote_id: &str, plain_body: &str) -> HydratedEntity {
    let body = markdown_body(plain_body);
    let document = CanonicalDocument::new(
        format!(
            "loc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n"
        ),
        body.clone(),
    );
    HydratedEntity {
        document,
        shadow: shadow(remote_id, plain_body),
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}

fn rendered_gmail_entity(
    remote_id: &str,
    subject: &str,
    internal_date: &str,
    plain_body: &str,
) -> HydratedEntity {
    let body = markdown_body(plain_body);
    let remote_version = format!("gmail:{remote_id}:{internal_date}:SENT");
    let document = CanonicalDocument::new(
        format!(
            "loc:\n  id: {remote_id}\n  type: page\n  connector: gmail\n  synced_at: {remote_version}\n  remote_edited_at: {remote_version}\ntitle: {subject}\ngmail:\n  mailbox: sent\n  message_id: {remote_id}\n  thread_id: thread-{remote_id}\n  labels: [SENT]\nfrom: sender@example.com\nto: [user@example.com]\ncc: []\nbcc: []\nsubject: {subject}\ndate: Tue, 14 Jul 2026 10:00:00 +0000\n"
        ),
        body.clone(),
    );
    HydratedEntity {
        document,
        shadow: shadow(remote_id, plain_body),
        remote_edited_at: Some(remote_version),
        assets: Vec::new(),
    }
}

#[derive(Debug)]
struct RacyNotionApi {
    remote: Mutex<RacyNotionRemote>,
    writes: Mutex<Vec<String>>,
    preflight_gate: PreflightGate,
}

#[derive(Debug)]
struct RacyNotionRemote {
    body: String,
    version: String,
}

impl RacyNotionApi {
    fn new(body: &str, version: &str) -> Self {
        Self {
            remote: Mutex::new(RacyNotionRemote {
                body: body.to_string(),
                version: version.to_string(),
            }),
            writes: Mutex::new(Vec::new()),
            preflight_gate: PreflightGate::new(),
        }
    }

    fn write_count(&self) -> usize {
        self.writes.lock().expect("writes").len()
    }

    fn remote_body(&self) -> String {
        self.remote.lock().expect("remote").body.clone()
    }

    fn wait_until_second_state_preflight_read(&self) {
        self.preflight_gate.wait_until_blocked();
    }

    fn release_second_state_preflight_read(&self) {
        self.preflight_gate.release();
    }
}

#[derive(Debug)]
struct PreflightGate {
    state: Mutex<PreflightGateState>,
    changed: Condvar,
}

#[derive(Debug)]
struct PreflightGateState {
    should_block_next_children_read: bool,
    blocked: bool,
    released: bool,
}

impl PreflightGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(PreflightGateState {
                should_block_next_children_read: true,
                blocked: false,
                released: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn block_if_first_preflight_read(&self) {
        let mut state = self.state.lock().expect("preflight gate");
        if !state.should_block_next_children_read {
            return;
        }
        state.should_block_next_children_read = false;
        state.blocked = true;
        self.changed.notify_all();
        while !state.released {
            let (next, timeout) = self
                .changed
                .wait_timeout(state, Duration::from_secs(5))
                .expect("preflight gate wait");
            assert!(
                !timeout.timed_out(),
                "timed out waiting to release preflight gate"
            );
            state = next;
        }
    }

    fn wait_until_blocked(&self) {
        let mut state = self.state.lock().expect("preflight gate");
        while !state.blocked {
            let (next, timeout) = self
                .changed
                .wait_timeout(state, Duration::from_secs(5))
                .expect("preflight gate wait");
            assert!(
                !timeout.timed_out(),
                "timed out waiting for second state preflight"
            );
            state = next;
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("preflight gate");
        state.released = true;
        self.changed.notify_all();
    }
}

impl NotionApi for RacyNotionApi {
    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
        if page_id != "page-1" {
            return Err(LocalityError::InvalidState(format!(
                "missing page {page_id}"
            )));
        }
        let remote = self.remote.lock().expect("remote");
        Ok(notion_page(&remote.version))
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto> {
        if block_id != "page-1" || start_cursor.is_some() {
            return Ok(PaginatedListDto::default());
        }
        let remote = self.remote.lock().expect("remote");
        let body = remote.body.clone();
        let results = vec![
            notion_heading_block("heading-1", "Roadmap"),
            notion_paragraph_block("paragraph-1", &body),
        ];
        drop(remote);
        self.preflight_gate.block_if_first_preflight_read();
        Ok(PaginatedListDto {
            results,
            next_cursor: None,
            has_more: false,
        })
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
        let remote = self.remote.lock().expect("remote");
        Ok(PaginatedListDto {
            results: vec![notion_page(&remote.version)],
            next_cursor: None,
            has_more: false,
        })
    }

    fn update_block(&self, block_id: &str, body: Value) -> LocalityResult<BlockDto> {
        self.writes
            .lock()
            .expect("writes")
            .push(block_id.to_string());
        let text = body
            .pointer("/paragraph/rich_text/0/text/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut remote = self.remote.lock().expect("remote");
        remote.body = text.clone();
        remote.version = "2026-06-10T00:00:01Z".to_string();
        Ok(notion_paragraph_block(block_id, &text))
    }

    fn append_block_children(&self, _block_id: &str, _body: Value) -> LocalityResult<BlockListDto> {
        Err(LocalityError::InvalidState(
            "unexpected append in racy Notion fixture".to_string(),
        ))
    }

    fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
        Err(LocalityError::InvalidState(
            "unexpected delete in racy Notion fixture".to_string(),
        ))
    }
}

fn notion_shadow(remote_id: &str, body: &str, remote_edited_at: &str) -> ShadowDocument {
    shadow(remote_id, body).with_frontmatter(format!(
        "loc:\n  id: {remote_id}\n  type: page\n  synced_at: {remote_edited_at}\n  remote_edited_at: {remote_edited_at}\ntitle: Roadmap\n"
    ))
}

fn notion_page(version: &str) -> PageDto {
    PageDto {
        id: "page-1".to_string(),
        parent: None,
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some(version.to_string()),
        archived: false,
        in_trash: false,
        properties: BTreeMap::from([(
            "Name".to_string(),
            PagePropertyDto {
                kind: "title".to_string(),
                title: notion_rich_text("Roadmap"),
                ..Default::default()
            },
        )]),
    }
}

fn notion_heading_block(id: &str, text: &str) -> BlockDto {
    let mut block = notion_block(id, "heading_1");
    block.heading_1 = Some(notion_rich_text_block(text));
    block
}

fn notion_paragraph_block(id: &str, text: &str) -> BlockDto {
    let mut block = notion_block(id, "paragraph");
    block.paragraph = Some(notion_rich_text_block(text));
    block
}

fn notion_block(id: &str, kind: &str) -> BlockDto {
    BlockDto {
        id: id.to_string(),
        kind: kind.to_string(),
        ..Default::default()
    }
}

fn notion_rich_text_block(text: &str) -> RichTextBlockDto {
    RichTextBlockDto {
        rich_text: notion_rich_text(text),
        color: None,
    }
}

fn notion_rich_text(text: &str) -> Vec<RichTextDto> {
    vec![RichTextDto {
        kind: "text".to_string(),
        text: Some(TextRichTextDto {
            content: text.to_string(),
            link: None,
        }),
        plain_text: text.to_string(),
        ..Default::default()
    }]
}

fn shadow(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        markdown_body(body),
        7,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

fn markdown_body(body: &str) -> String {
    format!("# Roadmap\n\n{body}\n")
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
"#
}

fn virtual_mutation(
    mount_id: &MountId,
    local_id: &str,
    kind: VirtualMutationKind,
    target_remote_id: Option<RemoteId>,
    parent_remote_id: Option<RemoteId>,
    path: &str,
    content_path: Option<PathBuf>,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind: kind,
        target_remote_id,
        parent_remote_id,
        original_path: None,
        projected_path: PathBuf::from(path),
        title: "Draft".to_string(),
        content_path,
        created_at: "2026-06-12T00:00:00Z".to_string(),
        updated_at: "2026-06-12T00:00:00Z".to_string(),
    }
}
