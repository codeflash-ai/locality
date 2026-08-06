use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
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
use locality_gmail::client::GmailApi;
use locality_gmail::dto::{
    GmailDraft, GmailDraftCreateRequest, GmailDraftList, GmailDraftSendRequest,
    GmailDraftUpdateRequest, GmailHeader, GmailMessage, GmailMessageList, GmailMessagePart,
    GmailMessagePartBody, GmailMessageSendRequest, GmailThread, GmailThreadList,
};
use locality_gmail::{GmailConfig, GmailConnector};
use locality_linear::{
    LinearApi, LinearConfig, LinearConnector, LinearIssue, LinearIssuePage, LinearIssuePriority,
    LinearIssueState, LinearIssueUpdateInput, LinearLabel, LinearProject, LinearTeam, LinearUser,
    render_linear_issue,
};
use locality_notion::client::NotionApi;
use locality_notion::dto::{
    BlockDto, BlockListDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto,
    RichTextBlockDto, RichTextDto, TextRichTextDto,
};
use locality_notion::{NotionConfig, NotionConnector};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, EntityRecord,
    EntityRepository, InMemoryStateStore, JournalRepository, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, SqliteStateStore, VirtualMutationKind, VirtualMutationRecord,
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
fn explicit_push_uses_linear_whole_entity_body_policy() {
    let fixture = PushFixture::new();
    let mut store = fixture.store_with_connector("Old body.", "linear");
    fixture.write_page("New body.");
    let source = FakePushSource::with_remote_transition(
        rendered_entity("page-1", "Old body."),
        rendered_entity("page-1", "New body."),
    );

    let report =
        execute_push_job_with_content_root(&mut store, fixture.push_job(true), &source, None)
            .expect("execute push");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert!(matches!(
        report.pipeline.plan.expect("plan").operations.as_slice(),
        [PushOperation::UpdateEntityBody { entity_id, body }]
            if entity_id == &fixture.remote_id
                && body == "# Roadmap\n\nNew body.\n"
    ));
}

#[test]
fn linear_push_repairs_legacy_shadow_missing_lifecycle_frontmatter() {
    let root = std::env::temp_dir().join(format!(
        "loc-linear-legacy-frontmatter-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("fixture root");
    let mount_id = MountId::new("linear-main");
    let issue_id = RemoteId::new("issue-1");
    let issue = linear_push_issue();
    let api = Arc::new(FakeLinearMoveApi::new(issue.clone()));
    let source = LinearConnector::with_api(LinearConfig::new("secret"), api.clone());
    let issue_path = PathBuf::from("Teams/Engineering/Issues/Todo/ENG-1 Improve sync/page.md");
    let rendered = render_linear_issue(&issue).expect("render issue");
    let legacy_frontmatter = legacy_linear_frontmatter_without_lifecycle(&rendered.frontmatter);

    let mut store = InMemoryStateStore::new();
    store
        .save_mount(MountConfig::new(mount_id.clone(), "linear", root.clone()))
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                issue_id.clone(),
                EntityKind::Page,
                "Improve sync",
                issue_path.clone(),
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("linear:issue-1:2026-07-15T12:00:00Z"),
        )
        .expect("save issue");
    store
        .save_shadow(
            &mount_id,
            ShadowDocument::from_synced_body(
                issue_id.clone(),
                rendered.body.clone(),
                1,
                [RemoteId::new("body-1")],
            )
            .expect("shadow")
            .with_frontmatter(legacy_frontmatter.clone()),
        )
        .expect("save legacy shadow");
    let local_path = root.join(&issue_path);
    fs::create_dir_all(local_path.parent().expect("issue parent")).expect("issue parent");
    fs::write(
        &local_path,
        render_canonical_markdown(&CanonicalDocument::new(
            legacy_frontmatter.replace("title: \"Improve sync\"", "title: \"Improve sync v2\""),
            rendered.body.clone(),
        )),
    )
    .expect("write local edit");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: local_path,
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        None,
    )
    .expect("execute Linear push");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert_eq!(
        api.updates.lock().unwrap().as_slice(),
        &[LinearIssueUpdateInput {
            issue_id: "issue-1".to_string(),
            title: Some("Improve sync v2".to_string()),
            description: None,
            team_id: None,
            state_id: None,
            project_id: None,
            assignee_id: None,
        }]
    );
    let repaired_shadow = store
        .load_shadow(&mount_id, &issue_id)
        .expect("load repaired shadow");
    assert!(repaired_shadow.frontmatter.contains("created_at:"));
    assert!(repaired_shadow.frontmatter.contains("due_date:"));
    let _ = fs::remove_dir_all(root);
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
fn auto_save_push_uses_linear_whole_entity_body_policy() {
    let fixture = PushFixture::new();
    let mut store = fixture.store_with_connector("Old body.", "linear");
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
    assert!(matches!(
        report.pipeline.plan.expect("plan").operations.as_slice(),
        [PushOperation::UpdateEntityBody { entity_id, body }]
            if entity_id == &fixture.remote_id
                && body == "# Roadmap\n\nNew body.\n"
    ));
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
fn daemon_push_reconciles_created_database_to_canonical_schema_directory() {
    let fixture = PushFixture::new();
    let projection_root = fixture.root.join("loc");
    let schema_path = projection_root.join("Roadmap/Project Tasks/_schema.yaml");
    fs::create_dir_all(schema_path.parent().expect("schema parent")).expect("schema parent");
    fs::write(
        &schema_path,
        "loc:\n  type: notion_database_schema\ntitle: Project Tasks\ndata_sources:\n  - name: Tasks\n    properties:\n      Name:\n        type: title\n",
    )
    .expect("write draft schema");

    let parent_id = RemoteId::new("roadmap-page");
    let database_id = RemoteId::new("created-database");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            &projection_root,
        ))
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            parent_id.clone(),
            EntityKind::Page,
            "Roadmap",
            "Roadmap/page.md",
        ))
        .expect("save parent page");
    let canonical_schema = "loc:\n  type: notion_database_schema\n  database_id: created-database\ntitle: \"Project Tasks\"\ndata_sources:\n  - id: created-source\n    name: \"Tasks\"\n    properties:\n      \"Name\":\n        id: \"title\"\n        type: \"title\"\n";
    let source = FakePushSource::default()
        .with_database_schema(database_id.clone(), canonical_schema)
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-database".to_string()),
            operation_index: 0,
            parent_id,
            entity_id: database_id.clone(),
        }]);

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: schema_path.clone(),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        None,
    )
    .expect("push database create");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let database = store
        .get_entity(&fixture.mount_id, &database_id)
        .expect("get database")
        .expect("database entity");
    assert_eq!(database.kind, EntityKind::Database);
    assert_eq!(database.path, PathBuf::from("Roadmap/Project Tasks"));
    assert_eq!(database.hydration, HydrationState::Hydrated);
    assert_eq!(
        fs::read_to_string(schema_path).expect("canonical schema"),
        canonical_schema
    );
}

#[test]
fn daemon_push_reconciles_gmail_draft_create_to_draft_folder() {
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
    let created_remote_id = RemoteId::new("gmail-message:draft-1");
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
            Some(draft_folder_id.clone()),
            "draft/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_entity("gmail-message:draft-1", "Body."),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-gmail-draft".to_string()),
            operation_index: 0,
            parent_id: draft_folder_id,
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
        .expect("get draft message")
        .expect("sent message entity");
    assert_eq!(message.path, PathBuf::from("draft/reply.md"));
    assert_eq!(source.requested_paths(), vec![message.path.clone()]);
    assert!(content_root.join(source_path).exists());
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
            .expect("find mutation")
            .is_none()
    );
    let journal = store
        .get_journal(report.push_id.as_ref().expect("push id"))
        .expect("get journal")
        .expect("journal");
    assert_eq!(journal.metadata.local_projection_items.len(), 1);
    assert_eq!(
        journal.metadata.local_projection_items[0].local_id,
        "local:gmail-draft"
    );
    assert_eq!(
        journal.metadata.local_projection_items[0].operation_index,
        0
    );
}

#[test]
fn daemon_push_reconciles_gmail_send_create_to_sent_folder() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
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
            "local:gmail-outbox",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id),
            "outbox/reply.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_entity("gmail-message:sent-1", "Body."),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-gmail-outbox".to_string()),
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
    .expect("push gmail send");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let message = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get sent message")
        .expect("sent message entity");
    assert_eq!(message.path, PathBuf::from("sent/reply.md"));
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
fn daemon_push_reconciles_gmail_draft_update() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/Remote Draft.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
    let api = Arc::new(RecordingGmailApi::new());
    let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
    let mut store = gmail_draft_store(&fixture, &connector, &draft_remote_id, source_path);
    let rendered = store
        .load_shadow(&fixture.mount_id, &draft_remote_id)
        .expect("load draft shadow");
    let edited_frontmatter = rendered
        .frontmatter
        .replace("title: \"Remote Draft\"", "title: \"Updated Draft\"")
        .replace("to: [\"ann@example.com\"]", "to: [\"bob@example.com\"]")
        .replace("subject: \"Remote Draft\"", "subject: \"Updated Draft\"");
    let edited = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter,
        "Updated body.\n".to_string(),
    ));
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, &edited).expect("edit draft");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: true,
        },
        &connector,
        Some(&state_root),
    )
    .expect("push draft update");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let calls = api.calls();
    assert_eq!(calls.updated_drafts.len(), 1);
    assert_eq!(calls.updated_drafts[0].0, "draft-1");
    assert!(decode_raw_mime(&calls.updated_drafts[0].1).contains("Updated body."));
    assert!(calls.sent_drafts.is_empty());
    let entity = store
        .get_entity(&fixture.mount_id, &draft_remote_id)
        .expect("get draft")
        .expect("draft entity");
    assert_eq!(entity.remote_id, draft_remote_id);
    assert_eq!(entity.path, PathBuf::from("draft/Remote Draft.md"));
    assert!(content_root.join(source_path).exists());
    let shadow = store
        .load_shadow(&fixture.mount_id, &draft_remote_id)
        .expect("load reconciled shadow");
    let projected = fs::read_to_string(content_root.join(source_path)).expect("projected draft");
    assert_eq!(
        render_canonical_markdown(&CanonicalDocument::new(
            shadow.frontmatter.clone(),
            shadow.rendered_body.clone(),
        )),
        projected
    );
    assert!(projected.contains("subject: \"Updated Draft\""));
    assert!(projected.contains("to: [\"bob@example.com\"]"));
    assert!(projected.contains("Updated body."));
}

#[test]
fn daemon_push_reconciles_gmail_draft_move_to_outbox_send() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let original_path = Path::new("draft/Remote Draft.md");
    let source_path = Path::new("outbox/Send Remote Draft.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
    let sent_remote_id = RemoteId::new("gmail-message:sent-1");
    let api = Arc::new(RecordingGmailApi::new());
    let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
    let mut store = gmail_draft_store(&fixture, &connector, &draft_remote_id, original_path);
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Send Remote Draft",
                source_path,
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("gmail:draft-message-1:1720900000000:DRAFT"),
        )
        .expect("save moved draft entity");
    let rendered = store
        .load_shadow(&fixture.mount_id, &draft_remote_id)
        .expect("load draft shadow");
    let edited_frontmatter = rendered
        .frontmatter
        .replace("title: \"Remote Draft\"", "title: \"Send Remote Draft\"")
        .replace("to: [\"ann@example.com\"]", "to: [\"bob@example.com\"]")
        .replace(
            "subject: \"Remote Draft\"",
            "subject: \"Send Remote Draft\"",
        );
    let edited = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter,
        "Edited body before sending.\n".to_string(),
    ));
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, &edited).expect("edit moved draft");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(RemoteId::new("gmail-folder:outbox")),
            original_path: Some(original_path.to_path_buf()),
            projected_path: source_path.to_path_buf(),
            title: "Send Remote Draft".to_string(),
            content_path: Some(cache_path),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: true,
        },
        &connector,
        Some(&state_root),
    )
    .expect("push draft send");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let calls = api.calls();
    assert_eq!(calls.updated_drafts.len(), 1);
    assert_eq!(calls.updated_drafts[0].0, "draft-1");
    assert_eq!(calls.sent_drafts, vec!["draft-1"]);
    assert!(
        calls
            .call_log
            .iter()
            .position(|call| call == "update_draft:draft-1")
            < calls
                .call_log
                .iter()
                .position(|call| call == "send_draft:draft-1")
    );
    let sent = store
        .get_entity(&fixture.mount_id, &sent_remote_id)
        .expect("get sent message")
        .expect("sent message entity");
    assert_eq!(sent.remote_id, sent_remote_id);
    assert!(sent.path.starts_with("sent"));
    assert!(content_root.join(&sent.path).exists());
    assert!(!content_root.join(original_path).exists());
    assert!(!content_root.join(source_path).exists());
    assert!(
        store
            .get_entity(&fixture.mount_id, &draft_remote_id)
            .expect("get archived draft")
            .is_none()
    );
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
            .expect("find move mutation")
            .is_none()
    );
}

#[test]
fn daemon_push_resumes_gmail_draft_send_reconciliation_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let original_path = Path::new("draft/Remote Draft.md");
    let source_path = Path::new("outbox/Send Remote Draft.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
    let sent_remote_id = RemoteId::new("gmail-message:sent-1");
    let api = Arc::new(RecordingGmailApi::new().with_sent_fetch_failures(&sent_remote_id, 1));
    let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
    let mut store = gmail_draft_store(&fixture, &connector, &draft_remote_id, original_path);
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Send Remote Draft",
                source_path,
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("gmail:draft-message-1:1720900000000:DRAFT"),
        )
        .expect("save moved draft entity");
    let rendered = store
        .load_shadow(&fixture.mount_id, &draft_remote_id)
        .expect("load draft shadow");
    let edited_frontmatter = rendered
        .frontmatter
        .replace("title: \"Remote Draft\"", "title: \"Send Remote Draft\"")
        .replace("to: [\"ann@example.com\"]", "to: [\"bob@example.com\"]")
        .replace(
            "subject: \"Remote Draft\"",
            "subject: \"Send Remote Draft\"",
        );
    let edited = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter.clone(),
        "Edited body before sending.\n".to_string(),
    ));
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, &edited).expect("edit moved draft");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(RemoteId::new("gmail-folder:outbox")),
            original_path: Some(original_path.to_path_buf()),
            projected_path: source_path.to_path_buf(),
            title: "Send Remote Draft".to_string(),
            content_path: Some(cache_path.clone()),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");
    let job = || PushJob {
        target_path: fixture.root.join(source_path),
        assume_yes: true,
        confirm_dangerous: true,
    };

    let first =
        execute_push_job_with_content_root(&mut store, job(), &connector, Some(&state_root))
            .expect("first draft send push");

    assert_eq!(first.action, PushJobAction::Failed);
    let first_push_id = first.push_id.expect("first push id");
    let calls = api.calls();
    assert_eq!(calls.updated_drafts.len(), 1);
    assert_eq!(calls.sent_drafts, vec!["draft-1"]);
    let stale_edit = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter,
        "Changed after send readback failed.\n".to_string(),
    ));
    fs::write(&cache_path, &stale_edit).expect("write stale local edit");

    let second =
        execute_push_job_with_content_root(&mut store, job(), &connector, Some(&state_root))
            .expect("resume draft send push");

    assert_eq!(second.action, PushJobAction::Reconciled);
    assert_eq!(second.push_id.as_ref(), Some(&first_push_id));
    let calls = api.calls();
    assert_eq!(
        calls.updated_drafts.len(),
        1,
        "retry must not update the sent draft again"
    );
    assert_eq!(
        calls.sent_drafts,
        vec!["draft-1"],
        "retry must not resend Gmail draft"
    );
    let sent = store
        .get_entity(&fixture.mount_id, &sent_remote_id)
        .expect("get sent message")
        .expect("sent message entity");
    assert!(sent.path.starts_with("sent"));
    assert!(content_root.join(&sent.path).exists());
    assert!(
        store
            .get_entity(&fixture.mount_id, &draft_remote_id)
            .expect("get archived draft")
            .is_none()
    );
    assert_eq!(
        fs::read_to_string(content_root.join(source_path)).expect("preserved stale edit"),
        stale_edit
    );
    let mutation = store
        .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
        .expect("find preserved mutation")
        .expect("preserved mutation");
    assert_eq!(mutation.mutation_kind, VirtualMutationKind::Create);
    assert_eq!(mutation.target_remote_id, None);
    assert_eq!(
        mutation.parent_remote_id,
        Some(RemoteId::new("gmail-folder:outbox"))
    );
}

#[test]
fn daemon_push_resumes_gmail_draft_send_after_outbox_rename_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let original_path = Path::new("draft/Remote Draft.md");
    let source_path = Path::new("outbox/Send Remote Draft.md");
    let renamed_path = Path::new("outbox/Renamed Remote Draft.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
    let sent_remote_id = RemoteId::new("gmail-message:sent-1");
    let api = Arc::new(RecordingGmailApi::new().with_sent_fetch_failures(&sent_remote_id, 1));
    let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
    let mut store = gmail_draft_store(&fixture, &connector, &draft_remote_id, original_path);
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Send Remote Draft",
                source_path,
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("gmail:draft-message-1:1720900000000:DRAFT"),
        )
        .expect("save moved draft entity");
    let rendered = store
        .load_shadow(&fixture.mount_id, &draft_remote_id)
        .expect("load draft shadow");
    let edited_frontmatter = rendered
        .frontmatter
        .replace("title: \"Remote Draft\"", "title: \"Send Remote Draft\"")
        .replace("to: [\"ann@example.com\"]", "to: [\"bob@example.com\"]")
        .replace(
            "subject: \"Remote Draft\"",
            "subject: \"Send Remote Draft\"",
        );
    let edited = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter,
        "Edited body before sending.\n".to_string(),
    ));
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, &edited).expect("edit moved draft");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(RemoteId::new("gmail-folder:outbox")),
            original_path: Some(original_path.to_path_buf()),
            projected_path: source_path.to_path_buf(),
            title: "Send Remote Draft".to_string(),
            content_path: Some(cache_path.clone()),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");

    let first = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: true,
        },
        &connector,
        Some(&state_root),
    )
    .expect("first draft send push");

    assert_eq!(first.action, PushJobAction::Failed);
    let first_push_id = first.push_id.expect("first push id");
    let calls = api.calls();
    assert_eq!(calls.updated_drafts.len(), 1);
    assert_eq!(calls.sent_drafts, vec!["draft-1"]);

    let renamed_cache_path = virtual_fs_content_path(&state_root, &fixture.mount_id, renamed_path)
        .expect("renamed cache path");
    fs::create_dir_all(renamed_cache_path.parent().expect("renamed parent"))
        .expect("renamed cache parent");
    let renamed_edit = render_canonical_markdown(&CanonicalDocument::new(
        "loc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: \"Renamed Remote Draft\"\nsubject: \"Renamed Remote Draft\"\nto: [\"bob@example.com\"]\n".to_string(),
        "Changed after send readback failed.\n".to_string(),
    ));
    fs::remove_file(&cache_path).expect("remove old cache path");
    fs::write(&renamed_cache_path, &renamed_edit).expect("write renamed stale edit");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(RemoteId::new("gmail-folder:outbox")),
            original_path: Some(original_path.to_path_buf()),
            projected_path: renamed_path.to_path_buf(),
            title: "Renamed Remote Draft".to_string(),
            content_path: Some(renamed_cache_path.clone()),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:01:00Z".to_string(),
        })
        .expect("save renamed move mutation");

    let second = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(renamed_path),
            assume_yes: true,
            confirm_dangerous: true,
        },
        &connector,
        Some(&state_root),
    )
    .expect("resume renamed draft send push");

    assert_eq!(second.action, PushJobAction::Reconciled);
    assert_eq!(second.push_id.as_ref(), Some(&first_push_id));
    let calls = api.calls();
    assert_eq!(
        calls.updated_drafts.len(),
        1,
        "retry must not update the sent draft again"
    );
    assert_eq!(
        calls.sent_drafts,
        vec!["draft-1"],
        "retry must not resend Gmail draft"
    );
    let sent = store
        .get_entity(&fixture.mount_id, &sent_remote_id)
        .expect("get sent message")
        .expect("sent message entity");
    assert!(sent.path.starts_with("sent"));
    assert!(content_root.join(&sent.path).exists());
    assert_eq!(
        fs::read_to_string(content_root.join(renamed_path)).expect("preserved renamed edit"),
        renamed_edit
    );
    let mutation = store
        .find_virtual_mutation_by_path(&fixture.mount_id, renamed_path)
        .expect("find preserved renamed mutation")
        .expect("preserved renamed mutation");
    assert_eq!(mutation.mutation_kind, VirtualMutationKind::Create);
    assert_eq!(mutation.target_remote_id, None);
    assert_eq!(mutation.content_path, Some(renamed_cache_path));
}

#[test]
fn daemon_push_converts_preserved_gmail_outbox_edit_with_sqlite_store() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let original_path = Path::new("draft/Remote Draft.md");
    let source_path = Path::new("outbox/Send Remote Draft.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
    let sent_remote_id = RemoteId::new("gmail-message:sent-1");
    let api = Arc::new(RecordingGmailApi::new().with_sent_fetch_failures(&sent_remote_id, 1));
    let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open sqlite store");
    seed_gmail_draft_store(
        &mut store,
        &fixture,
        &connector,
        &draft_remote_id,
        original_path,
    );
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Send Remote Draft",
                source_path,
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("gmail:draft-message-1:1720900000000:DRAFT"),
        )
        .expect("save moved draft entity");
    let rendered = store
        .load_shadow(&fixture.mount_id, &draft_remote_id)
        .expect("load draft shadow");
    let edited_frontmatter = rendered
        .frontmatter
        .replace("title: \"Remote Draft\"", "title: \"Send Remote Draft\"")
        .replace("to: [\"ann@example.com\"]", "to: [\"bob@example.com\"]")
        .replace(
            "subject: \"Remote Draft\"",
            "subject: \"Send Remote Draft\"",
        );
    let edited = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter.clone(),
        "Edited body before sending.\n".to_string(),
    ));
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, &edited).expect("edit moved draft");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(RemoteId::new("gmail-folder:outbox")),
            original_path: Some(original_path.to_path_buf()),
            projected_path: source_path.to_path_buf(),
            title: "Send Remote Draft".to_string(),
            content_path: Some(cache_path.clone()),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");
    let job = || PushJob {
        target_path: fixture.root.join(source_path),
        assume_yes: true,
        confirm_dangerous: true,
    };

    let first =
        execute_push_job_with_content_root(&mut store, job(), &connector, Some(&state_root))
            .expect("first draft send push");

    assert_eq!(first.action, PushJobAction::Failed);
    let stale_edit = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter,
        "Changed after send readback failed.\n".to_string(),
    ));
    fs::write(&cache_path, &stale_edit).expect("write stale local edit");

    let second =
        execute_push_job_with_content_root(&mut store, job(), &connector, Some(&state_root))
            .expect("resume draft send push");

    assert_eq!(second.action, PushJobAction::Reconciled);
    let calls = api.calls();
    assert_eq!(
        calls.updated_drafts.len(),
        1,
        "retry must not update the sent draft again"
    );
    assert_eq!(
        calls.sent_drafts,
        vec!["draft-1"],
        "retry must not resend Gmail draft"
    );
    assert_eq!(
        fs::read_to_string(content_root.join(source_path)).expect("preserved stale edit"),
        stale_edit
    );
    let mutation = store
        .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
        .expect("find preserved mutation")
        .expect("preserved mutation");
    assert_eq!(mutation.mutation_kind, VirtualMutationKind::Create);
    assert_eq!(mutation.target_remote_id, None);
    assert_eq!(mutation.content_path, Some(cache_path));
}

#[test]
fn daemon_push_resumes_applying_gmail_draft_send_with_complete_effects() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let original_path = Path::new("draft/Remote Draft.md");
    let source_path = Path::new("outbox/Send Remote Draft.md");
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
    let sent_remote_id = RemoteId::new("gmail-message:sent-1");
    let api = Arc::new(RecordingGmailApi::new().with_sent_fetch_failures(&sent_remote_id, 1));
    let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
    let mut store = gmail_draft_store(&fixture, &connector, &draft_remote_id, original_path);
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Send Remote Draft",
                source_path,
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("gmail:draft-message-1:1720900000000:DRAFT"),
        )
        .expect("save moved draft entity");
    let rendered = store
        .load_shadow(&fixture.mount_id, &draft_remote_id)
        .expect("load draft shadow");
    let edited_frontmatter = rendered
        .frontmatter
        .replace("title: \"Remote Draft\"", "title: \"Send Remote Draft\"")
        .replace("to: [\"ann@example.com\"]", "to: [\"bob@example.com\"]")
        .replace(
            "subject: \"Remote Draft\"",
            "subject: \"Send Remote Draft\"",
        );
    let edited = render_canonical_markdown(&CanonicalDocument::new(
        edited_frontmatter,
        "Edited body before sending.\n".to_string(),
    ));
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(&cache_path, &edited).expect("edit moved draft");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(RemoteId::new("gmail-folder:outbox")),
            original_path: Some(original_path.to_path_buf()),
            projected_path: source_path.to_path_buf(),
            title: "Send Remote Draft".to_string(),
            content_path: Some(cache_path),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");
    let job = || PushJob {
        target_path: fixture.root.join(source_path),
        assume_yes: true,
        confirm_dangerous: true,
    };

    let first =
        execute_push_job_with_content_root(&mut store, job(), &connector, Some(&state_root))
            .expect("first draft send push");

    assert_eq!(first.action, PushJobAction::Failed);
    let first_push_id = first.push_id.expect("first push id");
    store
        .update_journal_status(&first_push_id, JournalStatus::Applying)
        .expect("force applying status");

    let second =
        execute_push_job_with_content_root(&mut store, job(), &connector, Some(&state_root))
            .expect("resume applying draft send push");

    assert_eq!(second.action, PushJobAction::Reconciled);
    assert_eq!(second.push_id.as_ref(), Some(&first_push_id));
    let calls = api.calls();
    assert_eq!(
        calls.updated_drafts.len(),
        1,
        "retry must not update the sent draft again"
    );
    assert_eq!(
        calls.sent_drafts,
        vec!["draft-1"],
        "retry must not resend Gmail draft"
    );
}

#[test]
fn daemon_push_reconciles_google_calendar_draft_create_to_canonical_event_filename() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/design-review.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Design review\nsummary: Design review\nstart:\n  dateTime: \"2026-07-20T10:00:00-07:00\"\nend:\n  dateTime: \"2026-07-20T10:30:00-07:00\"\n---\nAgenda\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("google-calendar-folder:draft");
    let events_folder_id = RemoteId::new("google-calendar-folder:events");
    let created_remote_id = RemoteId::new("google-calendar-event:primary:created-event");
    let expected_path = PathBuf::from("events/20260720-100000-design-review-created-event.md");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "google-calendar", &fixture.root)
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
            events_folder_id.clone(),
            EntityKind::Directory,
            "events",
            "events",
        ))
        .expect("save events folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:calendar-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/design-review.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_google_calendar_entity(
                "google-calendar-event:primary:created-event",
                "Design review",
                "2026-07-20T10:00:00-07:00",
                "Agenda",
            ),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-calendar-draft".to_string()),
            operation_index: 0,
            parent_id: events_folder_id,
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
    .expect("push google calendar draft");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let event = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get created event")
        .expect("created event entity");
    assert_eq!(event.path, expected_path);
    let requested_paths = source.requested_paths();
    assert_eq!(
        requested_paths,
        vec![
            PathBuf::from("events/design-review.md"),
            expected_path.clone()
        ]
    );
    assert_eq!(requested_paths.last(), Some(&expected_path));
    assert!(content_root.join(&expected_path).exists());
    assert!(!content_root.join(source_path).exists());
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
            .expect("find mutation")
            .is_none()
    );
}

#[test]
fn daemon_push_reconciles_long_google_calendar_event_id_to_filesystem_safe_filename() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/design-review.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Design review\nsummary: Design review\nstart:\n  dateTime: \"2026-07-20T10:00:00-07:00\"\nend:\n  dateTime: \"2026-07-20T10:30:00-07:00\"\n---\nAgenda\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("google-calendar-folder:draft");
    let events_folder_id = RemoteId::new("google-calendar-folder:events");
    let long_event_id = format!("loc{}", "a".repeat(1024));
    let event_id_hash = locality_core::shadow::stable_hash(&long_event_id);
    let created_remote_id = RemoteId::new(format!("google-calendar-event:primary:{long_event_id}"));
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "google-calendar", &fixture.root)
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
            events_folder_id.clone(),
            EntityKind::Directory,
            "events",
            "events",
        ))
        .expect("save events folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:calendar-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/design-review.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_google_calendar_entity(
                created_remote_id.as_str(),
                "Design review",
                "2026-07-20T10:00:00-07:00",
                "Agenda",
            ),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-calendar-draft".to_string()),
            operation_index: 0,
            parent_id: events_folder_id,
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
    .expect("push google calendar draft with long event id");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let event = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get created event")
        .expect("created event entity");
    let filename = event
        .path
        .file_name()
        .expect("event filename")
        .to_string_lossy();
    assert!(
        filename.len() <= 255,
        "filename component must fit common filesystem limits: {}",
        filename.len()
    );
    assert!(filename.starts_with("20260720-100000-design-review-"));
    assert!(filename.ends_with(".md"));
    assert!(
        filename.contains(&event_id_hash[..16]),
        "shortened event ids should keep a stable hash suffix"
    );
    assert!(content_root.join(&event.path).exists());
    assert!(!content_root.join(source_path).exists());
}

#[test]
fn daemon_push_accepts_google_calendar_summary_only_draft_create() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/summary-only.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\nsummary: Summary only review\nstart:\n  dateTime: \"2026-07-20T10:00:00-07:00\"\nend:\n  dateTime: \"2026-07-20T10:30:00-07:00\"\n---\nAgenda\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("google-calendar-folder:draft");
    let events_folder_id = RemoteId::new("google-calendar-folder:events");
    let created_remote_id = RemoteId::new("google-calendar-event:primary:summary-only-event");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "google-calendar", &fixture.root)
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
            events_folder_id.clone(),
            EntityKind::Directory,
            "events",
            "events",
        ))
        .expect("save events folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:calendar-summary-only-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/summary-only.md",
            Some(cache_path),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_google_calendar_entity(
                "google-calendar-event:primary:summary-only-event",
                "Summary only review",
                "2026-07-20T10:00:00-07:00",
                "Agenda",
            ),
        )
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-calendar-summary-only-draft".to_string()),
            operation_index: 0,
            parent_id: events_folder_id,
            entity_id: created_remote_id,
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
    .expect("push summary-only google calendar draft");

    assert_eq!(report.action, PushJobAction::Reconciled);
    let journal = store.list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    let PushOperation::CreateEntity { title, .. } = &journal[0].plan.operations[0] else {
        panic!("expected create entity operation");
    };
    assert_eq!(title, "Summary only review");
}

#[test]
fn daemon_push_preserves_edited_google_calendar_draft_after_create_reconcile_retry() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/design-review.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    let edited_draft = "---\nsummary: Follow-up design review\nstart:\n  dateTime: \"2026-07-20T10:00:00-07:00\"\nend:\n  dateTime: \"2026-07-20T10:30:00-07:00\"\n---\nUpdated agenda\n";
    fs::write(
        &cache_path,
        "---\nsummary: Design review\nstart:\n  dateTime: \"2026-07-20T10:00:00-07:00\"\nend:\n  dateTime: \"2026-07-20T10:30:00-07:00\"\n---\nAgenda\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("google-calendar-folder:draft");
    let events_folder_id = RemoteId::new("google-calendar-folder:events");
    let created_remote_id = RemoteId::new("google-calendar-event:primary:created-event");
    let expected_path = PathBuf::from("events/20260720-100000-design-review-created-event.md");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "google-calendar", &fixture.root)
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
            events_folder_id.clone(),
            EntityKind::Directory,
            "events",
            "events",
        ))
        .expect("save events folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:calendar-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/design-review.md",
            Some(cache_path.clone()),
        ))
        .expect("save mutation");
    let source = FakePushSource::default()
        .with_created_entity(
            created_remote_id.clone(),
            rendered_google_calendar_entity(
                "google-calendar-event:primary:created-event",
                "Design review",
                "2026-07-20T10:00:00-07:00",
                "Agenda",
            ),
        )
        .with_created_fetch_failures(created_remote_id.clone(), 1)
        .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-calendar-draft".to_string()),
            operation_index: 0,
            parent_id: events_folder_id,
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
    fs::write(&cache_path, edited_draft).expect("edit stale draft");

    let second = execute_push_job_with_content_root(&mut store, job(), &source, Some(&state_root))
        .expect("retry push");

    assert_eq!(second.action, PushJobAction::Reconciled);
    assert_eq!(
        source.applied_count(),
        1,
        "retry must not recreate Calendar event"
    );
    assert_eq!(second.push_id.as_ref(), Some(&first_push_id));
    let event = store
        .get_entity(&fixture.mount_id, &created_remote_id)
        .expect("get created event")
        .expect("created event entity");
    assert_eq!(event.path, expected_path);
    assert!(content_root.join(&expected_path).exists());
    assert_eq!(
        fs::read_to_string(content_root.join(source_path)).expect("preserved edited draft"),
        edited_draft
    );
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, source_path)
            .expect("find mutation")
            .is_some()
    );
}

#[test]
fn auto_save_push_blocks_google_calendar_draft_create_without_applying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("draft/design-review.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\nsummary: Design review\nstart:\n  dateTime: \"2026-07-20T10:00:00-07:00\"\nend:\n  dateTime: \"2026-07-20T10:30:00-07:00\"\n---\nAgenda\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("google-calendar-folder:draft");
    let created_remote_id = RemoteId::new("google-calendar-event:primary:created-event");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "google-calendar", &fixture.root)
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
            "local:calendar-draft",
            VirtualMutationKind::Create,
            None,
            Some(draft_folder_id),
            "draft/design-review.md",
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
            operation_id: PushOperationId("create-calendar-draft".to_string()),
            operation_index: 0,
            parent_id: RemoteId::new("google-calendar-folder:events"),
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
    .expect("auto-save google calendar draft");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert_eq!(
        source.applied_count(),
        0,
        "auto-save must not create Calendar events"
    );
    assert_eq!(
        report.error.as_ref().expect("error").code,
        "auto_save_blocked"
    );
    assert_eq!(
        report.error.as_ref().expect("error").message,
        "Google Calendar event creates require review"
    );
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, source_path)
        .expect("get enrollment")
        .expect("enrollment");
    assert_eq!(enrollment.state, AutoSaveState::Blocked);
    assert_eq!(
        enrollment.last_reason.as_deref(),
        Some("Google Calendar event creates require review")
    );
    assert!(store.list_journal().expect("journal").is_empty());
}

#[test]
fn auto_save_push_blocks_gmail_direct_send_without_applying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/reply.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:gmail-outbox",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id),
            "outbox/reply.md",
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
            operation_id: PushOperationId("create-gmail-outbox".to_string()),
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
    .expect("auto-save gmail send");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert_eq!(source.applied_count(), 0, "auto-save must not send Gmail");
    assert_eq!(
        report.error.as_ref().expect("error").code,
        "auto_save_blocked"
    );
    assert_eq!(
        report.error.as_ref().expect("error").message,
        "Gmail outbound email creates require review"
    );
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, source_path)
        .expect("get enrollment")
        .expect("enrollment");
    assert_eq!(enrollment.state, AutoSaveState::Blocked);
    assert_eq!(
        enrollment.last_reason.as_deref(),
        Some("Gmail outbound email creates require review")
    );
    assert!(store.list_journal().expect("journal").is_empty());
}

#[test]
fn daemon_push_resumes_failed_gmail_send_reconciliation_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
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
            "local:gmail-outbox",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id),
            "outbox/reply.md",
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
            operation_id: PushOperationId("create-gmail-outbox".to_string()),
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
    .expect("edit stale send");

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
        fs::read_to_string(content_root.join(source_path)).expect("preserved edited send"),
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
    let source_path = Path::new("outbox/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
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
            "local:gmail-outbox",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id.clone()),
            "outbox/reply.md",
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
        vec![outbox_folder_id],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("gmail-folder:outbox"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Reply".to_string(),
            properties,
            body: "Body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-already-applied-gmail-outbox".to_string());
    let effect = JournalApplyEffect::CreatedEntity {
        operation_id: PushOperationId("create-gmail-outbox".to_string()),
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
    let source_path = Path::new("outbox/reply.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
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
            "local:gmail-outbox",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id.clone()),
            "outbox/reply.md",
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
        vec![outbox_folder_id],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("gmail-folder:outbox"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Reply".to_string(),
            properties,
            body: "Body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-ambiguous-gmail-outbox".to_string());
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
fn daemon_push_blocks_ambiguous_gmail_draft_move_send_journal_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/Send Now.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\nloc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Send Now\nsubject: Send now subject\nto: [\"ann@example.com\"]\n---\nEdited body before sending.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Original",
                source_path,
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save moved draft");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                draft_remote_id.clone(),
                "Original body.\n",
                1,
                [RemoteId::new("body-1")],
            )
            .expect("shadow")
            .with_frontmatter("loc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Original\nsubject: Original subject\nto: [\"ann@example.com\"]\n"),
        )
        .expect("save shadow");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(outbox_folder_id.clone()),
            original_path: Some(PathBuf::from("draft/Original.md")),
            projected_path: source_path.to_path_buf(),
            title: "Send Now".to_string(),
            content_path: Some(cache_path),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");

    let plan = PushPlan::new(
        vec![draft_remote_id.clone()],
        vec![
            PushOperation::MoveEntity {
                entity_id: draft_remote_id.clone(),
                new_parent_id: outbox_folder_id,
                new_parent_kind: EntityKind::Directory,
                new_title: "Send Now".to_string(),
                projected_path: source_path.to_path_buf(),
            },
            PushOperation::UpdateProperties {
                entity_id: draft_remote_id.clone(),
                properties: BTreeMap::from([(
                    "subject".to_string(),
                    PropertyValue::String("Send now subject".to_string()),
                )]),
            },
            PushOperation::UpdateEntityBody {
                entity_id: draft_remote_id,
                body: "Edited body before sending.\n".to_string(),
            },
        ],
    );
    let push_id = PushId("push-ambiguous-gmail-draft-send".to_string());
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
    .expect("retry ambiguous gmail draft send");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(
        source.applied_count(),
        0,
        "retry must not resend Gmail draft"
    );
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    assert_eq!(report.journal_status, Some(JournalStatus::Applying));
    let error = report.error.expect("guardrail error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("ambiguous result"));
}

#[test]
fn daemon_push_blocks_ambiguous_gmail_draft_move_send_after_outbox_rename() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/Renamed Send.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\nloc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Renamed Send\nsubject: Send now subject\nto: [\"ann@example.com\"]\n---\nEdited body before sending.\n",
    )
    .expect("cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Original",
                source_path,
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save moved draft");
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                draft_remote_id.clone(),
                "Original body.\n",
                1,
                [RemoteId::new("body-1")],
            )
            .expect("shadow")
            .with_frontmatter("loc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Original\nsubject: Original subject\nto: [\"ann@example.com\"]\n"),
        )
        .expect("save shadow");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:draft-1-to-outbox".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(outbox_folder_id.clone()),
            original_path: Some(PathBuf::from("draft/Original.md")),
            projected_path: source_path.to_path_buf(),
            title: "Renamed Send".to_string(),
            content_path: Some(cache_path),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move mutation");
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");

    let ambiguous_plan = PushPlan::new(
        vec![draft_remote_id.clone()],
        vec![
            PushOperation::MoveEntity {
                entity_id: draft_remote_id.clone(),
                new_parent_id: outbox_folder_id,
                new_parent_kind: EntityKind::Directory,
                new_title: "Send Now".to_string(),
                projected_path: PathBuf::from("outbox/Send Now.md"),
            },
            PushOperation::UpdateProperties {
                entity_id: draft_remote_id.clone(),
                properties: BTreeMap::from([(
                    "subject".to_string(),
                    PropertyValue::String("Send now subject".to_string()),
                )]),
            },
            PushOperation::UpdateEntityBody {
                entity_id: draft_remote_id,
                body: "Edited body before sending.\n".to_string(),
            },
        ],
    );
    let push_id = PushId("push-ambiguous-gmail-draft-send-renamed".to_string());
    store
        .append_journal(JournalEntry::new(
            push_id.clone(),
            fixture.mount_id.clone(),
            ambiguous_plan.affected_entities.clone(),
            ambiguous_plan,
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
    .expect("retry ambiguous renamed gmail draft send");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(
        source.applied_count(),
        0,
        "retry must not resend renamed Gmail draft"
    );
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    let error = report.error.expect("guardrail error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("ambiguous result"));
}

#[test]
fn daemon_push_blocks_ambiguous_gmail_draft_move_send_inside_larger_batch() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let first_path = Path::new("outbox/Send One.md");
    let second_path = Path::new("outbox/Send Two.md");
    let first_cache =
        virtual_fs_content_path(&state_root, &fixture.mount_id, first_path).expect("cache path");
    let second_cache =
        virtual_fs_content_path(&state_root, &fixture.mount_id, second_path).expect("cache path");
    fs::create_dir_all(first_cache.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &first_cache,
        "---\nloc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Send One\nsubject: Send one subject\nto: [\"ann@example.com\"]\n---\nFirst body.\n",
    )
    .expect("first cache file");
    fs::write(
        &second_cache,
        "---\nloc:\n  id: gmail-draft:draft-2\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Send Two\nsubject: Send two subject\nto: [\"bob@example.com\"]\n---\nSecond body.\n",
    )
    .expect("second cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
    let first_remote_id = RemoteId::new("gmail-draft:draft-1");
    let second_remote_id = RemoteId::new("gmail-draft:draft-2");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
    for (remote_id, title, path, body) in [
        (
            first_remote_id.clone(),
            "Original One",
            first_path,
            "Original one body.\n",
        ),
        (
            second_remote_id.clone(),
            "Original Two",
            second_path,
            "Original two body.\n",
        ),
    ] {
        store
            .save_entity(
                EntityRecord::new(
                    fixture.mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    title,
                    path,
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save moved draft");
        store
            .save_shadow(
                &fixture.mount_id,
                ShadowDocument::from_synced_body(remote_id.clone(), body, 1, [RemoteId::new("body-1")])
                    .expect("shadow")
                    .with_frontmatter(format!("loc:\n  id: {}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: {}\nsubject: {}\nto: [\"ann@example.com\"]\n", remote_id.0, title, title)),
            )
            .expect("save shadow");
    }
    for (local_id, remote_id, path, cache, title) in [
        (
            "move:draft-1-to-outbox",
            first_remote_id.clone(),
            first_path,
            first_cache,
            "Send One",
        ),
        (
            "move:draft-2-to-outbox",
            second_remote_id.clone(),
            second_path,
            second_cache,
            "Send Two",
        ),
    ] {
        store
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: fixture.mount_id.clone(),
                local_id: local_id.to_string(),
                mutation_kind: VirtualMutationKind::Move,
                target_remote_id: Some(remote_id),
                parent_remote_id: Some(outbox_folder_id.clone()),
                original_path: Some(PathBuf::from("draft/Original.md")),
                projected_path: path.to_path_buf(),
                title: title.to_string(),
                content_path: Some(cache),
                created_at: "2026-06-12T00:00:00Z".to_string(),
                updated_at: "2026-06-12T00:00:00Z".to_string(),
            })
            .expect("save move mutation");
    }
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");

    let ambiguous_plan = PushPlan::new(
        vec![first_remote_id.clone()],
        vec![PushOperation::MoveEntity {
            entity_id: first_remote_id.clone(),
            new_parent_id: outbox_folder_id,
            new_parent_kind: EntityKind::Directory,
            new_title: "Send One".to_string(),
            projected_path: first_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-ambiguous-gmail-draft-send-batch".to_string());
    store
        .append_journal(JournalEntry::new(
            push_id.clone(),
            fixture.mount_id.clone(),
            ambiguous_plan.affected_entities.clone(),
            ambiguous_plan,
            JournalStatus::Applying,
        ))
        .expect("append applying journal");
    let source = FakePushSource::default();

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join("outbox"),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("retry ambiguous batch gmail draft send");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(
        source.applied_count(),
        0,
        "retry must not resend Gmail draft batch"
    );
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    let error = report.error.expect("guardrail error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("ambiguous result"));
}

#[test]
fn daemon_push_blocks_complete_effect_gmail_draft_send_overlap_inside_larger_batch() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let first_path = Path::new("outbox/Send One.md");
    let second_path = Path::new("outbox/Send Two.md");
    let first_cache =
        virtual_fs_content_path(&state_root, &fixture.mount_id, first_path).expect("cache path");
    let second_cache =
        virtual_fs_content_path(&state_root, &fixture.mount_id, second_path).expect("cache path");
    fs::create_dir_all(first_cache.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &first_cache,
        "---\nloc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Send One\nsubject: Send one subject\nto: [\"ann@example.com\"]\n---\nFirst body.\n",
    )
    .expect("first cache file");
    fs::write(
        &second_cache,
        "---\nloc:\n  id: gmail-draft:draft-2\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Send Two\nsubject: Send two subject\nto: [\"bob@example.com\"]\n---\nSecond body.\n",
    )
    .expect("second cache file");

    let draft_folder_id = RemoteId::new("gmail-folder:draft");
    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
    let first_remote_id = RemoteId::new("gmail-draft:draft-1");
    let second_remote_id = RemoteId::new("gmail-draft:draft-2");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
    for (remote_id, title, path, body) in [
        (
            first_remote_id.clone(),
            "Original One",
            first_path,
            "Original one body.\n",
        ),
        (
            second_remote_id.clone(),
            "Original Two",
            second_path,
            "Original two body.\n",
        ),
    ] {
        store
            .save_entity(
                EntityRecord::new(
                    fixture.mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    title,
                    path,
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save moved draft");
        store
            .save_shadow(
                &fixture.mount_id,
                ShadowDocument::from_synced_body(remote_id.clone(), body, 1, [RemoteId::new("body-1")])
                    .expect("shadow")
                    .with_frontmatter(format!("loc:\n  id: {}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: {}\nsubject: {}\nto: [\"ann@example.com\"]\n", remote_id.0, title, title)),
            )
            .expect("save shadow");
    }
    for (local_id, remote_id, path, cache, title) in [
        (
            "move:draft-1-to-outbox",
            first_remote_id.clone(),
            first_path,
            first_cache,
            "Send One",
        ),
        (
            "move:draft-2-to-outbox",
            second_remote_id.clone(),
            second_path,
            second_cache,
            "Send Two",
        ),
    ] {
        store
            .save_virtual_mutation(VirtualMutationRecord {
                mount_id: fixture.mount_id.clone(),
                local_id: local_id.to_string(),
                mutation_kind: VirtualMutationKind::Move,
                target_remote_id: Some(remote_id),
                parent_remote_id: Some(outbox_folder_id.clone()),
                original_path: Some(PathBuf::from("draft/Original.md")),
                projected_path: path.to_path_buf(),
                title: title.to_string(),
                content_path: Some(cache),
                created_at: "2026-06-12T00:00:00Z".to_string(),
                updated_at: "2026-06-12T00:00:00Z".to_string(),
            })
            .expect("save move mutation");
    }
    fs::create_dir_all(fixture.root.join("outbox")).expect("visible outbox folder");

    let sent_remote_id = RemoteId::new("gmail-message:sent-1");
    let applied_plan = PushPlan::new(
        vec![first_remote_id.clone()],
        vec![PushOperation::MoveEntity {
            entity_id: first_remote_id.clone(),
            new_parent_id: outbox_folder_id,
            new_parent_kind: EntityKind::Directory,
            new_title: "Send One".to_string(),
            projected_path: first_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-complete-gmail-draft-send-batch-overlap".to_string());
    store
        .append_journal(
            JournalEntry::new(
                push_id.clone(),
                fixture.mount_id.clone(),
                applied_plan.affected_entities.clone(),
                applied_plan,
                JournalStatus::Applied,
            )
            .with_apply_effects(vec![
                JournalApplyEffect::ArchivedEntity {
                    operation_id: PushOperationId("send-draft-archive".to_string()),
                    operation_index: 0,
                    entity_id: first_remote_id.clone(),
                },
                JournalApplyEffect::CreatedEntity {
                    operation_id: PushOperationId("send-draft-sent".to_string()),
                    operation_index: 0,
                    parent_id: RemoteId::new("gmail-folder:sent"),
                    entity_id: sent_remote_id,
                },
            ]),
        )
        .expect("append applied journal");
    let source = FakePushSource::default()
        .with_created_entity(
            first_remote_id,
            rendered_entity("gmail-draft:draft-1", "Original one body."),
        )
        .with_created_entity(
            second_remote_id,
            rendered_entity("gmail-draft:draft-2", "Original two body."),
        );

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join("outbox"),
            assume_yes: true,
            confirm_dangerous: true,
        },
        &source,
        Some(&state_root),
    )
    .expect("retry overlapping batch gmail draft send");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(
        source.applied_count(),
        0,
        "retry must not apply a broader batch containing an already-sent Gmail draft"
    );
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    let error = report.error.expect("guardrail error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("already applied"));
}

#[test]
fn daemon_push_blocks_failed_gmail_send_recovery_lookup_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/reply.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
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
            "local:gmail-outbox",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id.clone()),
            "outbox/reply.md",
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
        vec![outbox_folder_id],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("gmail-folder:outbox"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Reply".to_string(),
            properties,
            body: "Body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-failed-gmail-outbox-lookup".to_string());
    store
        .append_journal(JournalEntry::new(
            push_id.clone(),
            fixture.mount_id.clone(),
            plan.affected_entities.clone(),
            plan,
            JournalStatus::Failed(
                "io error: gmail send ambiguous after send failure; sent lookup failed: sent search timed out"
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
fn daemon_push_blocks_failed_gmail_message_send_without_reapplying() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/reply.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
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
            "local:gmail-outbox",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id.clone()),
            "outbox/reply.md",
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
        vec![outbox_folder_id],
        vec![PushOperation::CreateEntity {
            parent_id: RemoteId::new("gmail-folder:outbox"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Reply".to_string(),
            properties,
            body: "Body.\n".to_string(),
            source_path: source_path.to_path_buf(),
        }],
    );
    let push_id = PushId("push-failed-gmail-message-send".to_string());
    store
        .append_journal(JournalEntry::new(
            push_id.clone(),
            fixture.mount_id.clone(),
            plan.affected_entities.clone(),
            plan,
            JournalStatus::Failed("io error: gmail message send timed out".to_string()),
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
    .expect("retry failed gmail message send");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(source.applied_count(), 0, "retry must not resend Gmail");
    assert_eq!(report.push_id.as_ref(), Some(&push_id));
    let error = report.error.expect("guardrail error");
    assert_eq!(error.code, "guardrail");
    assert!(error.message.contains("ambiguous result"));
}

#[test]
fn daemon_push_reconciles_repeated_gmail_send_filename_to_unique_sent_paths() {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let source_path = Path::new("outbox/reply.md");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache_path,
        "---\ntitle: Reply\nto: [\"user@example.com\"]\nsubject: Reply\n---\nBody one.\n",
    )
    .expect("cache file");

    let outbox_folder_id = RemoteId::new("gmail-folder:outbox");
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
            outbox_folder_id.clone(),
            EntityKind::Directory,
            "outbox",
            "outbox",
        ))
        .expect("save outbox folder");
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
            "local:gmail-outbox-1",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id.clone()),
            "outbox/reply.md",
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
            operation_id: PushOperationId("create-gmail-outbox-1".to_string()),
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
            "local:gmail-outbox-2",
            VirtualMutationKind::Create,
            None,
            Some(outbox_folder_id),
            "outbox/reply.md",
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
            operation_id: PushOperationId("create-gmail-outbox-2".to_string()),
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
fn auto_save_push_blocks_slack_recent_edit_before_journaled_apply() {
    let fixture = PushFixture::new();
    let mount_id = MountId::new("slack-main");
    let remote_id = RemoteId::new("slack-recent:C123");
    let relative_path = Path::new("channels/general-C123/recent.md");
    let page_path = fixture.root.join(relative_path);
    if let Some(parent) = page_path.parent() {
        fs::create_dir_all(parent).expect("create Slack conversation directory");
    }
    let document = CanonicalDocument::new(
        format!(
            "loc:\n  id: {}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: recent\n",
            remote_id.0
        ),
        markdown_body("Edited Slack line."),
    );
    fs::write(&page_path, render_canonical_markdown(&document)).expect("write Slack edit");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(MountConfig::new(
            mount_id.clone(),
            "slack",
            fixture.root.clone(),
        ))
        .expect("save Slack mount");
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                "recent",
                relative_path,
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save Slack recent entity");
    store
        .save_shadow(&mount_id, shadow(&remote_id.0, "Original Slack line."))
        .expect("save Slack shadow");
    store
        .save_auto_save_enrollment(
            AutoSaveEnrollmentRecord::new(
                mount_id.clone(),
                relative_path,
                AutoSaveOrigin::UserEnabled,
                "now",
            )
            .active("now"),
        )
        .expect("save Slack auto-save enrollment");
    let source = FakePushSource::default();

    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: page_path,
            assume_yes: false,
            confirm_dangerous: false,
        },
        &source,
        None,
    )
    .expect("auto-save Slack edit");

    assert_eq!(report.action, PushJobAction::NotReady);
    assert_eq!(source.applied_count(), 0);
    assert_eq!(
        report.error.as_ref().expect("error").code,
        "auto_save_blocked"
    );
    assert_eq!(
        report.error.as_ref().expect("error").message,
        "local Markdown needs review before auto-save"
    );
    assert!(report.pipeline.plan.is_none());
    assert_eq!(report.pipeline.validation.issues.len(), 1);
    assert_eq!(report.pipeline.validation.issues[0].code, "slack_read_only");
    assert_eq!(report.push_id, None);
    assert_eq!(report.journal_status, None);
    let enrollment = store
        .get_auto_save_enrollment(&mount_id, relative_path)
        .expect("get Slack auto-save enrollment")
        .expect("Slack auto-save enrollment");
    assert_eq!(enrollment.state, AutoSaveState::Blocked);
    assert_eq!(
        enrollment.last_reason.as_deref(),
        Some("local Markdown needs review before auto-save")
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

#[test]
fn moved_entity_reconciliation_clears_intent_only_after_accepted_readback() {
    let (fixture, state_root, mut store) = pending_move_execution_store();
    let effect = moved_page_effect();
    let source = FakePushSource::with_remote_transition(
        rendered_entity("page-1", "Old body."),
        rendered_entity("page-1", "Remote accepted body."),
    )
    .with_apply_effects(vec![effect])
    .with_changed_remote_ids(vec![fixture.remote_id.clone()]);

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(linear_move_execution_path()),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("execute move");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert!(
        store
            .get_virtual_mutation(&fixture.mount_id, "move:page-1")
            .unwrap()
            .is_none()
    );
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .unwrap()
        .unwrap();
    assert_eq!(entity.path, PathBuf::from(linear_move_execution_path()));
    assert_eq!(entity.title, "Roadmap");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    assert_eq!(
        store
            .load_shadow(&fixture.mount_id, &fixture.remote_id)
            .unwrap()
            .rendered_body,
        markdown_body("Remote accepted body.")
    );
    assert!(
        fs::read_to_string(
            virtual_fs_content_path(
                &state_root,
                &fixture.mount_id,
                Path::new(linear_move_execution_path()),
            )
            .unwrap(),
        )
        .unwrap()
        .contains("Remote accepted body.")
    );
}

#[test]
fn moved_entity_reconciliation_requires_effect_and_changed_id_and_retains_intent() {
    for (name, source) in [
        (
            "missing changed id",
            FakePushSource::with_remote_transition(
                rendered_entity("page-1", "Old body."),
                rendered_entity("page-1", "Remote accepted body."),
            )
            .with_apply_effects(vec![moved_page_effect()])
            .with_changed_remote_ids(Vec::new()),
        ),
        (
            "missing moved effect",
            FakePushSource::with_remote_transition(
                rendered_entity("page-1", "Old body."),
                rendered_entity("page-1", "Remote accepted body."),
            )
            .with_changed_remote_ids(vec![RemoteId::new("page-1")]),
        ),
    ] {
        let (fixture, state_root, mut store) = pending_move_execution_store();
        let report = execute_push_job_with_content_root(
            &mut store,
            PushJob {
                target_path: fixture.root.join(linear_move_execution_path()),
                assume_yes: true,
                confirm_dangerous: false,
            },
            &source,
            Some(&state_root),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(report.action, PushJobAction::Failed, "{name}");
        assert!(
            store
                .get_virtual_mutation(&fixture.mount_id, "move:page-1")
                .unwrap()
                .is_some(),
            "{name}"
        );
        assert!(matches!(
            store.list_journal().unwrap()[0].status,
            JournalStatus::Failed(_)
        ));
    }
}

#[test]
fn non_gmail_move_with_gmail_shaped_ids_still_requires_moved_entity_effect() {
    let (fixture, state_root, mut store) = pending_move_execution_store_for_connector("notion");
    let draft_remote_id = RemoteId::new("gmail-draft:draft-1");
    let source_path = Path::new("Team B/Roadmap.md");
    let cache_path =
        virtual_fs_content_path(&state_root, &fixture.mount_id, source_path).expect("cache path");
    fs::write(
        &cache_path,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: gmail-draft:draft-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            markdown_body("Old body."),
        )),
    )
    .expect("write colliding cache");
    store
        .delete_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("delete default moved entity");
    store
        .delete_virtual_mutation(&fixture.mount_id, "move:page-1")
        .expect("delete default move");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("gmail-folder:outbox"),
            EntityKind::Directory,
            "outbox",
            "Outbox",
        ))
        .expect("save colliding outbox parent");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("gmail-folder:sent"),
            EntityKind::Directory,
            "sent",
            "Sent",
        ))
        .expect("save colliding sent parent");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                source_path,
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("2026-06-10T00:00:00Z"),
        )
        .expect("save gmail-shaped entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow(draft_remote_id.as_str(), "Old body."),
        )
        .expect("save gmail-shaped shadow");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:gmail-draft:draft-1".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(draft_remote_id.clone()),
            parent_remote_id: Some(RemoteId::new("gmail-folder:outbox")),
            original_path: Some(PathBuf::from("Team A/Roadmap.md")),
            projected_path: source_path.to_path_buf(),
            title: "Roadmap".to_string(),
            content_path: None,
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save gmail-shaped move");
    let source = FakePushSource::default()
        .with_created_entity(
            RemoteId::new("gmail-draft:draft-1"),
            rendered_entity("gmail-draft:draft-1", "Old body."),
        )
        .with_created_entity(
            RemoteId::new("gmail-message:sent-1"),
            rendered_entity("gmail-message:sent-1", "Sent body."),
        )
        .with_apply_effects(vec![
            JournalApplyEffect::ArchivedEntity {
                operation_id: PushOperationId("op-move".to_string()),
                operation_index: 0,
                entity_id: draft_remote_id,
            },
            JournalApplyEffect::CreatedEntity {
                operation_id: PushOperationId("op-move".to_string()),
                operation_index: 0,
                parent_id: RemoteId::new("gmail-folder:sent"),
                entity_id: RemoteId::new("gmail-message:sent-1"),
            },
        ])
        .with_changed_remote_ids(Vec::new());

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(source_path),
            assume_yes: true,
            confirm_dangerous: true,
        },
        &source,
        Some(&state_root),
    )
    .expect("execute non-gmail colliding move");

    assert_eq!(report.action, PushJobAction::Failed);
    let journal = store.list_journal().unwrap();
    assert!(matches!(journal[0].status, JournalStatus::Failed(_)));
}

#[test]
fn moved_entity_fetch_failure_resumes_same_journal_without_reapplying() {
    let (fixture, state_root, mut store) = pending_move_execution_store();
    let source = FakePushSource::with_remote_transition(
        rendered_entity("page-1", "Old body."),
        rendered_entity("page-1", "Remote accepted body."),
    )
    .with_apply_effects(vec![moved_page_effect()])
    .with_changed_remote_ids(vec![fixture.remote_id.clone()])
    .with_post_apply_fetch_failures(fixture.remote_id.clone(), 1);

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(linear_move_execution_path()),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("execute move with failed readback");

    assert_eq!(report.action, PushJobAction::Failed);
    assert_eq!(source.applied_count(), 1);
    let push_id = report.push_id.expect("first push id");
    assert!(
        store
            .get_virtual_mutation(&fixture.mount_id, "move:page-1")
            .unwrap()
            .is_some()
    );

    let retried = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: fixture.root.join(linear_move_execution_path()),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("resume move reconciliation");

    assert_eq!(retried.action, PushJobAction::Reconciled);
    assert_eq!(retried.push_id.as_ref(), Some(&push_id));
    assert_eq!(source.applied_count(), 1, "retry must not reapply the move");
    assert!(
        store
            .get_virtual_mutation(&fixture.mount_id, "move:page-1")
            .unwrap()
            .is_none(),
        "move intent clears only after accepted readback"
    );
    let journal = store.list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].push_id, push_id);
    assert_eq!(journal[0].status, JournalStatus::Reconciled);
}

#[test]
fn linear_move_reconciliation_uses_refreshed_canonical_path() {
    let root =
        std::env::temp_dir().join(format!("loc-linear-move-canonical-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("fixture root");
    let state_root = root.join(".state");
    let mount_id = MountId::new("linear-main");
    let issue_id = RemoteId::new("issue-1");
    let issue = linear_push_issue();
    let api = Arc::new(FakeLinearMoveApi::new(issue.clone()));
    let source = LinearConnector::with_api(LinearConfig::new("secret"), api.clone());
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "linear", root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord::new(
            mount_id.clone(),
            RemoteId::new("team-state:team-2:state-2"),
            EntityKind::Directory,
            "Done",
            "Teams/Platform/Issues/Done",
        ))
        .expect("save status");
    let projected_path = PathBuf::from("Teams/Platform/Issues/Done/ENG-1 Improve sync/page.md");
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                issue_id.clone(),
                EntityKind::Page,
                "Improve sync",
                projected_path.clone(),
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("linear:issue-1:2026-07-15T12:00:00Z"),
        )
        .expect("save moved issue");
    let rendered = render_linear_issue(&issue).expect("render issue");
    store
        .save_shadow(
            &mount_id,
            ShadowDocument::from_synced_body(
                issue_id.clone(),
                rendered.body.clone(),
                1,
                [RemoteId::new("body-1")],
            )
            .expect("shadow")
            .with_frontmatter(rendered.frontmatter.clone()),
        )
        .expect("save shadow");
    let cache =
        virtual_fs_content_path(&state_root, &mount_id, &projected_path).expect("cache path");
    fs::create_dir_all(cache.parent().expect("cache parent")).expect("cache parent");
    let edited_frontmatter = rendered
        .frontmatter
        .replace("title: \"Improve sync\"", "title: \"Improve sync renamed\"");
    fs::write(
        &cache,
        render_canonical_markdown(&CanonicalDocument::new(edited_frontmatter, rendered.body)),
    )
    .expect("write cache");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: mount_id.clone(),
            local_id: "move:issue-1".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(issue_id.clone()),
            parent_remote_id: Some(RemoteId::new("team-state:team-2:state-2")),
            original_path: Some(PathBuf::from(
                "Teams/Engineering/Issues/Todo/ENG-1 Improve sync/page.md",
            )),
            projected_path: projected_path.clone(),
            title: "Improve sync".to_string(),
            content_path: Some(cache),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move");
    fs::create_dir_all(root.join(projected_path.parent().expect("projected parent")))
        .expect("visible destination");

    let report = execute_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: root.join(&projected_path),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &source,
        Some(&state_root),
    )
    .expect("execute Linear move");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert_eq!(
        api.updates.lock().unwrap().as_slice(),
        &[LinearIssueUpdateInput {
            issue_id: "issue-1".to_string(),
            title: Some("Improve sync renamed".to_string()),
            description: None,
            team_id: Some("team-2".to_string()),
            state_id: Some("state-2".to_string()),
            project_id: None,
            assignee_id: None,
        }]
    );
    let entity = store
        .get_entity(&mount_id, &issue_id)
        .expect("get issue")
        .expect("issue");
    assert_eq!(
        entity.path,
        PathBuf::from("Teams/Platform/Issues/Done/PLAT-9 Improve sync renamed/page.md")
    );
    assert!(
        store
            .get_virtual_mutation(&mount_id, "move:issue-1")
            .expect("move mutation")
            .is_none()
    );
    assert!(
        fs::read_to_string(
            virtual_fs_content_path(&state_root, &mount_id, &entity.path).expect("final cache")
        )
        .expect("read final cache")
        .contains("Existing description.")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn mixed_create_move_late_fetch_failure_resumes_without_duplicate_create() {
    let (fixture, state_root, mut store) = pending_move_execution_store_for_connector("notion");
    let create_path = Path::new("Team B/ENG-2.md");
    let created_id = RemoteId::new("issue-new");
    let cache = virtual_fs_content_path(&state_root, &fixture.mount_id, create_path)
        .expect("create cache path");
    fs::create_dir_all(cache.parent().expect("cache parent")).expect("cache parent");
    fs::write(
        &cache,
        "---\ntitle: New issue\nstatus: Todo\n---\nNew issue body.\n",
    )
    .expect("create cache");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:new-issue",
            VirtualMutationKind::Create,
            None,
            Some(RemoteId::new("team-b")),
            "Team B/ENG-2.md",
            Some(cache),
        ))
        .expect("save create mutation");
    let source = FakePushSource::with_remote_transition(
        rendered_entity("page-1", "Old body."),
        rendered_entity("page-1", "Remote accepted body."),
    )
    .with_created_entity(
        created_id.clone(),
        rendered_entity("issue-new", "New issue body."),
    )
    .with_post_apply_fetch_failures(fixture.remote_id.clone(), 1)
    .with_changed_remote_ids(vec![created_id.clone(), fixture.remote_id.clone()])
    .with_apply_effects(vec![
        JournalApplyEffect::CreatedEntity {
            operation_id: PushOperationId("create-issue".to_string()),
            operation_index: 0,
            parent_id: RemoteId::new("team-b"),
            entity_id: created_id.clone(),
        },
        JournalApplyEffect::MovedEntity {
            operation_id: PushOperationId("move-page-1".to_string()),
            operation_index: 1,
            entity_id: fixture.remote_id.clone(),
            parent_id: RemoteId::new("team-b"),
        },
    ]);
    let job = || PushJob {
        target_path: fixture.root.clone(),
        assume_yes: true,
        confirm_dangerous: true,
    };

    let first = execute_push_job_with_content_root(&mut store, job(), &source, Some(&state_root))
        .expect("first mixed push");

    assert_eq!(first.action, PushJobAction::Failed);
    assert_eq!(source.applied_count(), 1, "first report: {first:#?}");
    let push_id = first.push_id.expect("first push id");
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, create_path)
            .expect("create mutation")
            .is_some()
    );
    assert!(
        store
            .get_virtual_mutation(&fixture.mount_id, "move:page-1")
            .expect("move mutation")
            .is_some(),
        "no intent may clear before every readback succeeds"
    );

    let second = execute_push_job_with_content_root(&mut store, job(), &source, Some(&state_root))
        .expect("resume mixed reconciliation");

    assert_eq!(second.action, PushJobAction::Reconciled);
    assert_eq!(second.push_id.as_ref(), Some(&push_id));
    assert_eq!(
        source.applied_count(),
        1,
        "retry must not duplicate the create"
    );
    assert!(
        store
            .find_virtual_mutation_by_path(&fixture.mount_id, create_path)
            .expect("create mutation")
            .is_none()
    );
    assert!(
        store
            .get_virtual_mutation(&fixture.mount_id, "move:page-1")
            .expect("move mutation")
            .is_none()
    );
    assert!(
        store
            .get_entity(&fixture.mount_id, &created_id)
            .expect("created entity")
            .is_some()
    );
    assert_eq!(
        store
            .get_entity(&fixture.mount_id, &fixture.remote_id)
            .expect("moved entity")
            .expect("moved entity present")
            .path,
        PathBuf::from("Team B/Roadmap.md")
    );
    let journal = store.list_journal().expect("journal");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].push_id, push_id);
    assert_eq!(journal[0].status, JournalStatus::Reconciled);
}

fn pending_move_execution_store() -> (PushFixture, PathBuf, InMemoryStateStore) {
    pending_move_execution_store_for_connector("linear")
}

fn linear_move_execution_path() -> &'static str {
    "Teams/Team B/Issues/Done/Roadmap/page.md"
}

fn pending_move_execution_store_for_connector(
    connector: &str,
) -> (PushFixture, PathBuf, InMemoryStateStore) {
    let fixture = PushFixture::new();
    let state_root = fixture.root.join(".state");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), connector, fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    let linear = connector == "linear";
    let parent_remote_id = if linear {
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("team-state:team-b:done"),
                EntityKind::Directory,
                "Done",
                "Teams/Team B/Issues/Done",
            ))
            .expect("save Linear status");
        RemoteId::new("team-state:team-b:done")
    } else {
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("team-b"),
                EntityKind::Page,
                "Team B",
                "Team B/page.md",
            ))
            .expect("save team");
        RemoteId::new("team-b")
    };
    let moved_path = if linear {
        PathBuf::from(linear_move_execution_path())
    } else {
        PathBuf::from("Team B/Roadmap.md")
    };
    let original_path = if linear {
        PathBuf::from("Teams/Team A/Issues/Todo/Roadmap/page.md")
    } else {
        PathBuf::from("Team A/Roadmap.md")
    };
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                fixture.remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                moved_path.clone(),
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at("2026-06-10T00:00:00Z"),
        )
        .expect("save moved issue");
    store
        .save_shadow(&fixture.mount_id, shadow("page-1", "Old body."))
        .expect("save shadow");
    let cache =
        virtual_fs_content_path(&state_root, &fixture.mount_id, &moved_path).expect("cache path");
    fs::create_dir_all(cache.parent().unwrap()).expect("cache parent");
    fixture.write_page_to(&cache, "Old body.");
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: fixture.mount_id.clone(),
            local_id: "move:page-1".to_string(),
            mutation_kind: VirtualMutationKind::Move,
            target_remote_id: Some(fixture.remote_id.clone()),
            parent_remote_id: Some(parent_remote_id),
            original_path: Some(original_path),
            projected_path: moved_path.clone(),
            title: "Roadmap".to_string(),
            content_path: Some(cache),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            updated_at: "2026-06-12T00:00:00Z".to_string(),
        })
        .expect("save move");
    fs::create_dir_all(
        fixture
            .root
            .join(moved_path.parent().expect("moved parent")),
    )
    .expect("visible move parent");
    (fixture, state_root, store)
}

fn moved_page_effect() -> JournalApplyEffect {
    JournalApplyEffect::MovedEntity {
        operation_id: PushOperationId("move-page-1".to_string()),
        operation_index: 0,
        entity_id: RemoteId::new("page-1"),
        parent_id: RemoteId::new("team-state:team-b:done"),
    }
}

#[derive(Debug)]
struct FakeLinearMoveApi {
    issue: Mutex<LinearIssue>,
    updates: Mutex<Vec<LinearIssueUpdateInput>>,
}

impl FakeLinearMoveApi {
    fn new(issue: LinearIssue) -> Self {
        Self {
            issue: Mutex::new(issue),
            updates: Mutex::new(Vec::new()),
        }
    }
}

impl LinearApi for FakeLinearMoveApi {
    fn list_issues(
        &self,
        _cursor: Option<&str>,
        _updated_after: Option<&str>,
        team_id: Option<&str>,
    ) -> LocalityResult<LinearIssuePage> {
        let issue = self.issue.lock().unwrap().clone();
        let issues = if team_id.is_none_or(|team_id| issue.team.id == team_id) {
            vec![issue]
        } else {
            Vec::new()
        };
        Ok(LinearIssuePage {
            issues,
            has_next_page: false,
            end_cursor: None,
        })
    }

    fn get_issue(&self, issue_id: &str) -> LocalityResult<LinearIssue> {
        let issue = self.issue.lock().unwrap().clone();
        if issue.id == issue_id {
            Ok(issue)
        } else {
            Err(LocalityError::RemoteNotFound(issue_id.to_string()))
        }
    }

    fn update_issue(&self, input: LinearIssueUpdateInput) -> LocalityResult<LinearIssue> {
        self.updates.lock().unwrap().push(input.clone());
        let mut issue = self.issue.lock().unwrap();
        if let Some(team_id) = &input.team_id {
            issue.team = LinearTeam {
                id: team_id.clone(),
                key: "PLAT".to_string(),
                name: "Platform".to_string(),
            };
            issue.identifier = "PLAT-9".to_string();
            issue.url = "https://linear.app/acme/issue/PLAT-9/improve-sync".to_string();
        }
        if let Some(state_id) = &input.state_id {
            issue.state = LinearIssueState {
                id: state_id.clone(),
                name: "Done".to_string(),
                state_type: Some("completed".to_string()),
            };
        }
        if let Some(title) = &input.title {
            issue.title = title.clone();
        }
        if let Some(description) = &input.description {
            issue.description = Some(description.clone());
        }
        issue.updated_at = "2026-07-16T12:00:00Z".to_string();
        Ok(issue.clone())
    }
}

fn linear_push_issue() -> LinearIssue {
    LinearIssue {
        id: "issue-1".to_string(),
        identifier: "ENG-1".to_string(),
        title: "Improve sync".to_string(),
        description: Some("Existing description.".to_string()),
        url: "https://linear.app/acme/issue/ENG-1/improve-sync".to_string(),
        created_at: "2026-07-14T12:00:00Z".to_string(),
        updated_at: "2026-07-15T12:00:00Z".to_string(),
        archived_at: None,
        started_at: None,
        completed_at: None,
        canceled_at: None,
        auto_archived_at: None,
        auto_closed_at: None,
        started_triage_at: None,
        triaged_at: None,
        snoozed_until_at: None,
        added_to_cycle_at: None,
        added_to_project_at: None,
        added_to_team_at: None,
        due_date: None,
        priority: Some(LinearIssuePriority {
            value: 3,
            label: "High".to_string(),
        }),
        estimate: Some(3.0),
        team: LinearTeam {
            id: "team-1".to_string(),
            key: "ENG".to_string(),
            name: "Engineering".to_string(),
        },
        state: LinearIssueState {
            id: "state-1".to_string(),
            name: "Todo".to_string(),
            state_type: Some("unstarted".to_string()),
        },
        project: Some(LinearProject {
            id: "project-1".to_string(),
            name: "Launch".to_string(),
        }),
        assignee: Some(LinearUser {
            id: "user-1".to_string(),
            name: "Ada".to_string(),
            email: Some("ada@example.com".to_string()),
        }),
        labels: vec![LinearLabel {
            id: "label-1".to_string(),
            name: "Bug".to_string(),
        }],
    }
}

fn legacy_linear_frontmatter_without_lifecycle(frontmatter: &str) -> String {
    const LEGACY_REMOVED_KEYS: &[&str] = &[
        "created_at:",
        "updated_at:",
        "archived_at:",
        "started_at:",
        "completed_at:",
        "canceled_at:",
        "auto_archived_at:",
        "auto_closed_at:",
        "started_triage_at:",
        "triaged_at:",
        "snoozed_until_at:",
        "added_to_cycle_at:",
        "added_to_project_at:",
        "added_to_team_at:",
        "due_date:",
    ];
    let mut legacy = frontmatter
        .lines()
        .filter(|line| !LEGACY_REMOVED_KEYS.iter().any(|key| line.starts_with(key)))
        .collect::<Vec<_>>()
        .join("\n");
    legacy.push('\n');
    legacy
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
        self.store_with_connector(synced_body, "notion")
    }

    fn store_with_connector(&self, synced_body: &str, connector: &str) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(self.mount_id.clone(), connector, self.root.clone());
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
    post_apply_fetch_failures: std::cell::RefCell<BTreeMap<RemoteId, usize>>,
    apply_effects: Vec<JournalApplyEffect>,
    apply_changed_remote_ids: Option<Vec<RemoteId>>,
    database_schemas: BTreeMap<RemoteId, String>,
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

    fn with_post_apply_fetch_failures(mut self, remote_id: RemoteId, failures: usize) -> Self {
        self.post_apply_fetch_failures
            .get_mut()
            .insert(remote_id, failures);
        self
    }

    fn with_apply_effects(mut self, effects: Vec<JournalApplyEffect>) -> Self {
        self.apply_effects = effects;
        self
    }

    fn with_changed_remote_ids(mut self, remote_ids: Vec<RemoteId>) -> Self {
        self.apply_changed_remote_ids = Some(remote_ids);
        self
    }

    fn with_database_schema(mut self, remote_id: RemoteId, schema: &str) -> Self {
        self.database_schemas.insert(remote_id, schema.to_string());
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
        if self.applied.get() > 0
            && let Some(remaining) = self
                .post_apply_fetch_failures
                .borrow_mut()
                .get_mut(&request.remote_id)
            && *remaining > 0
        {
            *remaining -= 1;
            return Err(LocalityError::InvalidState(
                "injected post-apply fetch failure".to_string(),
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

    fn fetch_database_schema_yaml(&self, database_id: &RemoteId) -> LocalityResult<Option<String>> {
        Ok(self.database_schemas.get(database_id).cloned())
    }
}

impl Connector for FakePushSource {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("fake")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_entity_body_updates: true,
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
        let changed_remote_ids = self.apply_changed_remote_ids.clone().unwrap_or_else(|| {
            if self.apply_effects.is_empty() {
                request.plan.affected_entities.clone()
            } else {
                Vec::new()
            }
        });
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

fn gmail_draft_store(
    fixture: &PushFixture,
    connector: &GmailConnector,
    draft_remote_id: &RemoteId,
    source_path: &Path,
) -> InMemoryStateStore {
    let mut store = InMemoryStateStore::new();
    seed_gmail_draft_store(&mut store, fixture, connector, draft_remote_id, source_path);
    store
}

fn seed_gmail_draft_store<S>(
    store: &mut S,
    fixture: &PushFixture,
    connector: &GmailConnector,
    draft_remote_id: &RemoteId,
    source_path: &Path,
) where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "gmail", &fixture.root)
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    for (remote_id, title, path) in [
        ("gmail-folder:draft", "draft", "draft"),
        ("gmail-folder:outbox", "outbox", "outbox"),
        ("gmail-folder:sent", "sent", "sent"),
    ] {
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(remote_id),
                EntityKind::Directory,
                title,
                path,
            ))
            .expect("save gmail folder");
    }
    let rendered = connector
        .fetch_render(&locality_core::hydration::HydrationRequest::new(
            fixture.mount_id.clone(),
            draft_remote_id.clone(),
            source_path.to_path_buf(),
            HydrationState::Hydrated,
            locality_core::hydration::HydrationReason::ExplicitPull,
        ))
        .expect("render remote draft");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                draft_remote_id.clone(),
                EntityKind::Page,
                "Remote Draft",
                source_path,
            )
            .with_hydration(HydrationState::Dirty)
            .with_remote_edited_at(
                rendered
                    .remote_edited_at
                    .as_deref()
                    .expect("draft remote version"),
            ),
        )
        .expect("save draft entity");
    store
        .save_shadow(&fixture.mount_id, rendered.shadow)
        .expect("save draft shadow");
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RecordingGmailCalls {
    call_log: Vec<String>,
    updated_drafts: Vec<(String, String)>,
    sent_drafts: Vec<String>,
}

#[derive(Debug)]
struct RecordingGmailApi {
    state: Mutex<RecordingGmailState>,
}

#[derive(Debug)]
struct RecordingGmailState {
    calls: RecordingGmailCalls,
    drafts: BTreeMap<String, GmailDraft>,
    sent_messages: BTreeMap<String, GmailMessage>,
    sent_fetch_failures: BTreeMap<String, usize>,
}

impl RecordingGmailApi {
    fn new() -> Self {
        Self {
            state: Mutex::new(RecordingGmailState {
                calls: RecordingGmailCalls::default(),
                drafts: BTreeMap::from([(
                    "draft-1".to_string(),
                    GmailDraft {
                        id: "draft-1".to_string(),
                        message: gmail_test_message(
                            "draft-message-1",
                            &["DRAFT"],
                            "Remote Draft",
                            "ann@example.com",
                            "Remote body.\n",
                            "1720900000000",
                        ),
                    },
                )]),
                sent_messages: BTreeMap::new(),
                sent_fetch_failures: BTreeMap::new(),
            }),
        }
    }

    fn with_sent_fetch_failures(mut self, remote_id: &RemoteId, failures: usize) -> Self {
        self.state
            .get_mut()
            .expect("gmail state")
            .sent_fetch_failures
            .insert(remote_id.as_str().to_string(), failures);
        self
    }

    fn calls(&self) -> RecordingGmailCalls {
        self.state.lock().expect("gmail state").calls.clone()
    }
}

impl GmailApi for RecordingGmailApi {
    fn list_messages(
        &self,
        _label_id: &str,
        _max_results: u32,
        _page_token: Option<&str>,
        _query: Option<&str>,
    ) -> LocalityResult<GmailMessageList> {
        Ok(GmailMessageList::default())
    }

    fn list_threads(
        &self,
        _label_id: &str,
        _max_results: u32,
        _page_token: Option<&str>,
        _query: Option<&str>,
    ) -> LocalityResult<GmailThreadList> {
        Ok(GmailThreadList::default())
    }

    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        self.get_message_full(message_id)
    }

    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        let mut state = self.state.lock().expect("gmail state");
        if let Some(remaining) = state.sent_fetch_failures.get_mut(message_id)
            && *remaining > 0
        {
            *remaining -= 1;
            return Err(LocalityError::InvalidState(
                "injected gmail sent readback failure".to_string(),
            ));
        }
        state
            .sent_messages
            .get(message_id)
            .cloned()
            .ok_or_else(|| LocalityError::InvalidState(format!("missing message `{message_id}`")))
    }

    fn get_thread_metadata(&self, thread_id: &str) -> LocalityResult<GmailThread> {
        Ok(GmailThread {
            id: thread_id.to_string(),
            history_id: Some("h1".to_string()),
            messages: Vec::new(),
        })
    }

    fn get_thread_full(&self, thread_id: &str) -> LocalityResult<GmailThread> {
        self.get_thread_metadata(thread_id)
    }

    fn get_attachment(
        &self,
        _message_id: &str,
        _attachment_id: &str,
    ) -> LocalityResult<GmailMessagePartBody> {
        Ok(GmailMessagePartBody::default())
    }

    fn list_drafts(
        &self,
        _max_results: u32,
        _page_token: Option<&str>,
        _query: Option<&str>,
    ) -> LocalityResult<GmailDraftList> {
        Ok(GmailDraftList::default())
    }

    fn get_draft_full(&self, draft_id: &str) -> LocalityResult<GmailDraft> {
        self.state
            .lock()
            .expect("gmail state")
            .drafts
            .get(draft_id)
            .cloned()
            .ok_or_else(|| LocalityError::InvalidState(format!("missing draft `{draft_id}`")))
    }

    fn create_draft(&self, _request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft> {
        Err(LocalityError::InvalidState(
            "unexpected draft create".to_string(),
        ))
    }

    fn update_draft(
        &self,
        draft_id: &str,
        request: GmailDraftUpdateRequest,
    ) -> LocalityResult<GmailDraft> {
        let mut state = self.state.lock().expect("gmail state");
        state
            .calls
            .call_log
            .push(format!("update_draft:{draft_id}"));
        state
            .calls
            .updated_drafts
            .push((draft_id.to_string(), request.message.raw.clone()));
        let updated = GmailDraft {
            id: draft_id.to_string(),
            message: gmail_message_from_raw_mime(
                &format!("updated-draft-message-{draft_id}"),
                &["DRAFT"],
                &request.message.raw,
                "1720900000001",
            ),
        };
        state.drafts.insert(draft_id.to_string(), updated.clone());
        Ok(updated)
    }

    fn send_message(&self, _request: GmailMessageSendRequest) -> LocalityResult<GmailMessage> {
        Err(LocalityError::InvalidState(
            "unexpected message send".to_string(),
        ))
    }

    fn send_draft(&self, request: GmailDraftSendRequest) -> LocalityResult<GmailMessage> {
        let mut state = self.state.lock().expect("gmail state");
        state
            .calls
            .call_log
            .push(format!("send_draft:{}", request.id));
        state.calls.sent_drafts.push(request.id.clone());
        let draft = state.drafts.remove(&request.id).ok_or_else(|| {
            LocalityError::InvalidState(format!("missing draft `{}`", request.id))
        })?;
        let subject = gmail_header(&draft.message, "subject").unwrap_or("Sent Draft");
        let to = gmail_header(&draft.message, "to").unwrap_or("user@example.com");
        let body = gmail_message_body(&draft.message);
        let sent = gmail_test_message(
            "gmail-message:sent-1",
            &["SENT"],
            subject,
            to,
            &body,
            "1720900001000",
        );
        state
            .sent_messages
            .insert("gmail-message:sent-1".to_string(), sent.clone());
        Ok(sent)
    }
}

fn gmail_message_from_raw_mime(
    id: &str,
    labels: &[&str],
    raw: &str,
    internal_date: &str,
) -> GmailMessage {
    let mime = decode_raw_mime(raw);
    let (headers, body) = split_raw_mime(&mime);
    let subject = headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("subject"))
        .map(|header| header.value.as_str())
        .unwrap_or("(no subject)");
    let to = headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("to"))
        .map(|header| header.value.as_str())
        .unwrap_or("");
    gmail_test_message(id, labels, subject, to, &body, internal_date)
}

fn split_raw_mime(mime: &str) -> (Vec<GmailHeader>, String) {
    let normalized = mime.replace("\r\n", "\n");
    let (head, body) = normalized
        .split_once("\n\n")
        .unwrap_or((normalized.as_str(), ""));
    let headers = head
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some(GmailHeader {
                name: name.trim().to_string(),
                value: value.trim().to_string(),
            })
        })
        .collect();
    (headers, body.to_string())
}

fn decode_raw_mime(raw: &str) -> String {
    String::from_utf8(
        URL_SAFE_NO_PAD
            .decode(raw.as_bytes())
            .or_else(|_| URL_SAFE.decode(raw.as_bytes()))
            .expect("decode raw mime"),
    )
    .expect("raw mime utf8")
}

fn gmail_test_message(
    id: &str,
    labels: &[&str],
    subject: &str,
    to: &str,
    body: &str,
    internal_date: &str,
) -> GmailMessage {
    GmailMessage {
        id: id.to_string(),
        thread_id: Some(format!("{id}-thread")),
        label_ids: labels.iter().map(|label| (*label).to_string()).collect(),
        snippet: None,
        internal_date: Some(internal_date.to_string()),
        payload: Some(GmailMessagePart {
            part_id: None,
            mime_type: Some("text/plain".to_string()),
            filename: None,
            headers: vec![
                GmailHeader {
                    name: "From".to_string(),
                    value: "Ann <ann@example.com>".to_string(),
                },
                GmailHeader {
                    name: "To".to_string(),
                    value: to.to_string(),
                },
                GmailHeader {
                    name: "Subject".to_string(),
                    value: subject.to_string(),
                },
                GmailHeader {
                    name: "Date".to_string(),
                    value: "Tue, 14 Jul 2026 09:30:00 +0000".to_string(),
                },
            ],
            body: Some(GmailMessagePartBody {
                size: Some(body.len() as u64),
                data: Some(URL_SAFE_NO_PAD.encode(body.as_bytes())),
                attachment_id: None,
            }),
            parts: Vec::new(),
        }),
        raw: None,
    }
}

fn gmail_header<'a>(message: &'a GmailMessage, name: &str) -> Option<&'a str> {
    message
        .payload
        .as_ref()?
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn gmail_message_body(message: &GmailMessage) -> String {
    let data = message
        .payload
        .as_ref()
        .and_then(|part| part.body.as_ref())
        .and_then(|body| body.data.as_deref())
        .unwrap_or("");
    String::from_utf8(
        URL_SAFE_NO_PAD
            .decode(data.as_bytes())
            .or_else(|_| URL_SAFE.decode(data.as_bytes()))
            .expect("decode gmail body"),
    )
    .expect("gmail body utf8")
}

fn rendered_google_calendar_entity(
    remote_id: &str,
    summary: &str,
    start_date_time: &str,
    plain_body: &str,
) -> HydratedEntity {
    let body = markdown_body(plain_body);
    let event_id = remote_id
        .strip_prefix("google-calendar-event:primary:")
        .expect("primary event remote id");
    let remote_version = format!("google-calendar:created-event:2026-07-20T17:30:00Z:\"etag\"");
    let document = CanonicalDocument::new(
        format!(
            "loc:\n  id: {remote_id}\n  type: page\n  connector: google-calendar\n  synced_at: {remote_version}\n  remote_edited_at: {remote_version}\ntitle: {summary}\nsummary: {summary}\nstart:\n  dateTime: \"{start_date_time}\"\ngoogle_calendar:\n  calendar_id: primary\n  event_id: {event_id}\n"
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
