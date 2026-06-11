use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use afs_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity,
};
use afs_core::canonical::render_canonical_markdown;
use afs_core::journal::JournalStatus;
use afs_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use afs_core::planner::PushOperationKind;
use afs_core::push::PushExecutionAction;
use afs_core::shadow::ShadowDocument;
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, ShadowRepository,
};
use afsd::execution::{DaemonExecutor, PushJob};
use afsd::hydration::{HydratedEntity, HydrationQueue, HydrationSource};
use afsd::push::PushJobAction;
use afsd::scheduler::PullScheduler;
use afsd::supervisor::DaemonSupervisor;
use afsd::watcher::FileWatcher;

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
    let source = FakePushSource::with_remote(rendered_entity("page-1", "New body."));

    let report = supervisor
        .execute_push(fixture.push_job(true), &source)
        .expect("execute push");

    assert_eq!(report.action, PushJobAction::Reconciled);
    assert_eq!(
        report.execution.as_ref().expect("execution").journal_status,
        Some(JournalStatus::Reconciled)
    );
    assert_eq!(source.applied_count(), 1);

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
        afs_core::push::PushPipelineAction::unsupported_operations(vec![
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
        afs_core::push::PushPipelineAction::FixValidation
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
            std::env::temp_dir().join(format!("afs-daemon-push-{}-{unique}", std::process::id()));
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

        DaemonSupervisor::new(
            store,
            RecordingWatcher::default(),
            HydrationQueue::new(),
            PullScheduler::new(Default::default()),
        )
    }

    fn push_job(&self, assume_yes: bool) -> PushJob {
        PushJob {
            target_path: self.root.join("Roadmap.md"),
            assume_yes,
            confirm_dangerous: false,
        }
    }

    fn write_page(&self, body: &str) {
        let document = CanonicalDocument::new(
            "afs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            markdown_body(body),
        );
        fs::write(
            self.root.join("Roadmap.md"),
            render_canonical_markdown(&document),
        )
        .expect("write page");
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
    fn watch_mount(&mut self, root: PathBuf) -> AfsResult<()> {
        self.watched.push(root);
        Ok(())
    }
}

#[derive(Default)]
struct FakePushSource {
    remote: Option<HydratedEntity>,
    applied: std::cell::Cell<usize>,
    supported_operations: Option<BTreeSet<PushOperationKind>>,
}

impl FakePushSource {
    fn with_remote(remote: HydratedEntity) -> Self {
        Self {
            remote: Some(remote),
            applied: std::cell::Cell::new(0),
            supported_operations: None,
        }
    }

    fn applied_count(&self) -> usize {
        self.applied.get()
    }

    fn with_supported_operations(
        mut self,
        supported_operations: BTreeSet<PushOperationKind>,
    ) -> Self {
        self.supported_operations = Some(supported_operations);
        self
    }
}

impl HydrationSource for FakePushSource {
    fn fetch_render(
        &self,
        request: &afs_core::hydration::HydrationRequest,
    ) -> AfsResult<HydratedEntity> {
        if request.remote_id != RemoteId::new("page-1") {
            return Err(AfsError::InvalidState("unexpected remote id".to_string()));
        }

        self.remote
            .clone()
            .ok_or_else(|| AfsError::InvalidState("missing remote fixture".to_string()))
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
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        self.supported_operations
            .clone()
            .unwrap_or_else(|| PushOperationKind::all().into_iter().collect())
    }

    fn enumerate(&self, _request: EnumerateRequest) -> AfsResult<Vec<TreeEntry>> {
        Err(AfsError::NotImplemented("fake enumerate"))
    }

    fn fetch(&self, _request: FetchRequest) -> AfsResult<NativeEntity> {
        Err(AfsError::NotImplemented("fake fetch"))
    }

    fn render(&self, _entity: &NativeEntity) -> AfsResult<CanonicalDocument> {
        Err(AfsError::NotImplemented("fake render"))
    }

    fn parse(&self, _document: &CanonicalDocument) -> AfsResult<ParsedEntity> {
        Err(AfsError::NotImplemented("fake parse"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> AfsResult<()> {
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> AfsResult<ApplyPlanResult> {
        self.applied.set(self.applied.get() + 1);
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: Vec::new(),
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> AfsResult<ApplyUndoResult> {
        Err(AfsError::NotImplemented("fake undo"))
    }
}

fn rendered_entity(remote_id: &str, plain_body: &str) -> HydratedEntity {
    let body = markdown_body(plain_body);
    let document = CanonicalDocument::new(
        "afs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
        body.clone(),
    );
    HydratedEntity {
        document,
        shadow: shadow(remote_id, plain_body),
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
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
