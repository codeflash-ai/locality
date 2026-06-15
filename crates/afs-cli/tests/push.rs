use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::push::{
    PushOptions, push_report_exit_code, run_push, run_push_with_daemon, select_push_targets,
};
use afs_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity,
};
use afs_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
};
use afs_core::hydration::HydrationRequest;
use afs_core::journal::JournalApplyEffect;
use afs_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use afs_core::shadow::ShadowDocument;
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, ShadowRepository, SqliteStateStore, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
};
use afsd::hydration::{HydratedEntity, HydrationSource};

#[test]
fn push_noop_succeeds_without_apply() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(report.ok);
    assert_eq!(report.action, "noop");
    assert_eq!(push_report_exit_code(&report), 0);
}

#[test]
fn push_safe_plan_requires_yes() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.pipeline_action, "confirm_plan");
    assert_eq!(push_report_exit_code(&report), 4);
}

#[test]
fn push_read_only_mount_blocks_write() {
    let fixture = PushFixture::new();
    let mut store = fixture.read_only_store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: true,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "read_only_blocked");
    assert_eq!(push_report_exit_code(&report), 4);
}

#[test]
fn push_file_with_conflict_markers_requires_manual_resolution_first() {
    let fixture = PushFixture::new();
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
    let conflicted = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity")
        .with_hydration(HydrationState::Conflicted);
    store
        .save_entity(conflicted)
        .expect("save conflicted entity");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert_eq!(report.validation[0].code, "unresolved_conflict_markers");
    assert_eq!(push_report_exit_code(&report), 3);
}

#[test]
fn push_resolved_conflicted_entity_can_plan_normally() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nResolved paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nRemote paragraph."))
        .expect("save shadow");
    let conflicted = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity")
        .with_hydration(HydrationState::Conflicted);
    store
        .save_entity(conflicted)
        .expect("save conflicted entity");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: false,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert!(report.validation.is_empty());
}

#[test]
fn push_safe_plan_with_yes_stops_at_apply_boundary() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.pipeline_action, "proceed_to_apply");
    assert_eq!(report.action, "apply_not_implemented");
    assert_eq!(push_report_exit_code(&report), 5);
}

#[test]
fn push_safe_plan_with_daemon_journals_applies_and_reconciles() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");
    let source = FakePushSource::with_remote(rendered_entity("Changed paragraph."));

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(report.ok);
    assert_eq!(report.action, "reconciled");
    assert!(report.push_id.is_some());
    assert_eq!(report.journal_status.as_deref(), Some("reconciled"));
    assert_eq!(report.changed_remote_ids, vec!["page-1"]);
    assert_eq!(report.reconciled_remote_ids, vec!["page-1"]);
    assert_eq!(report.apply_effect_count, 1);
    assert_eq!(push_report_exit_code(&report), 0);

    let journal = store
        .list_journal()
        .expect("list journal")
        .pop()
        .expect("journal");
    assert_eq!(journal.status, afs_core::journal::JournalStatus::Reconciled);
    assert_eq!(journal.preimages.len(), 1);
    assert_eq!(journal.apply_effects.len(), 1);
    assert_eq!(source.checks.get(), 1);
    assert_eq!(source.applies.get(), 1);
}

#[test]
fn push_daemon_reports_connector_not_implemented_with_failed_journal() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");
    let source = FakePushSource::with_remote(rendered_entity("Changed paragraph."))
        .with_concurrency_failure(AfsError::NotImplemented("fake concurrency"));

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "apply_not_implemented");
    assert_eq!(report.journal_status.as_deref(), Some("failed"));
    assert_eq!(push_report_exit_code(&report), 5);
    assert!(matches!(
        store
            .list_journal()
            .expect("list journal")
            .pop()
            .expect("journal")
            .status,
        afs_core::journal::JournalStatus::Failed(_)
    ));
}

#[test]
fn push_daemon_suggests_pull_when_remote_changed_since_last_sync() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");
    let source = FakePushSource::with_remote(rendered_entity("Changed paragraph."))
        .with_concurrency_failure(AfsError::Guardrail(
            "remote entity `page-1` changed since last sync (expected remote_edited_at `old`, found `new`)"
                .to_string(),
        ));

    let report = run_push_with_daemon(
        &mut store,
        &source,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "apply_failed");
    let expected = format!(
        "run `afs pull {}` to update from remote, resolve any conflicts, then rerun `afs push {} -y`",
        path.display(),
        path.display()
    );
    assert_eq!(report.suggested_fix.as_deref(), Some(expected.as_str()));
}

#[test]
fn push_dangerous_plan_requires_confirm() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "");
    store
        .save_shadow(&fixture.mount_id, large_shadow())
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "confirm_dangerous_plan");
    assert_eq!(report.guardrail.decision, "confirm_required");
    assert_eq!(push_report_exit_code(&report), 4);
}

#[test]
fn push_confirmed_dangerous_plan_stops_at_apply_boundary() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "");
    store
        .save_shadow(&fixture.mount_id, large_shadow())
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: false,
            confirm_dangerous: true,
        },
    )
    .expect("push report");

    assert!(!report.ok);
    assert_eq!(report.pipeline_action, "proceed_to_apply");
    assert_eq!(report.action, "apply_not_implemented");
}

#[test]
fn push_validation_failure_returns_fix_validation() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_raw("Roadmap.md", "---\ntitle: Missing AFS\n---\n# Roadmap\n");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_push(&store, &path, PushOptions::default()).expect("push report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert_eq!(report.validation[0].code, "frontmatter_missing_afs");
    assert_eq!(push_report_exit_code(&report), 3);
}

#[test]
fn push_runner_works_with_sqlite_state_store() {
    let fixture = PushFixture::new();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
    seed_store(&mut store, &fixture, false);
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_push(
        &store,
        &path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push report");

    assert_eq!(report.pipeline_action, "proceed_to_apply");
    assert_eq!(report.action, "apply_not_implemented");
}

#[test]
fn push_directory_targets_only_pending_page_changes_under_scope() {
    let fixture = PushFixture::new();
    let mut store = fixture.store();
    let clean_path = fixture.write_raw(
        "Team/Clean.md",
        &canonical_markdown("page-clean", "# Clean\n\nSame paragraph."),
    );
    let dirty_path = fixture.write_raw(
        "Team/Dirty.md",
        &canonical_markdown("page-dirty", "# Dirty\n\nChanged paragraph."),
    );
    let outside_path = fixture.write_raw(
        "Other.md",
        &canonical_markdown("page-outside", "# Outside\n\nChanged paragraph."),
    );
    let pending_path = fixture.root.join("Team/Draft.md");

    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-clean"),
                EntityKind::Page,
                "Clean",
                "Team/Clean.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save clean entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_for("page-clean", "# Clean\n\nSame paragraph."),
        )
        .expect("save clean shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-dirty"),
                EntityKind::Page,
                "Dirty",
                "Team/Dirty.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save dirty entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_for("page-dirty", "# Dirty\n\nOld paragraph."),
        )
        .expect("save dirty shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-outside"),
                EntityKind::Page,
                "Outside",
                "Other.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save outside entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow_for("page-outside", "# Outside\n\nOld paragraph."),
        )
        .expect("save outside shadow");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "create:draft",
            VirtualMutationKind::Create,
            "Team/Draft.md",
            "Draft",
        ))
        .expect("save pending create");

    let team_scope = fixture.root.join("Team");
    let selection = select_push_targets(&store, &team_scope, None).expect("select scoped targets");

    assert!(selection.scoped);
    assert_eq!(selection.requested_path, team_scope);
    assert_eq!(selection.targets, vec![dirty_path, pending_path]);
    assert!(clean_path.exists());
    assert!(outside_path.exists());
}

struct PushFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl PushFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afs-cli-push-{}-{unique}-{suffix}",
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
        seed_store(&mut store, self, false);
        store
    }

    fn read_only_store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        seed_store(&mut store, self, true);
        store
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
}

impl Drop for PushFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Default)]
struct FakePushSource {
    remote: Option<HydratedEntity>,
    checks: Cell<usize>,
    applies: Cell<usize>,
    concurrency_failure: Option<AfsError>,
}

impl FakePushSource {
    fn with_remote(remote: HydratedEntity) -> Self {
        Self {
            remote: Some(remote),
            checks: Cell::new(0),
            applies: Cell::new(0),
            concurrency_failure: None,
        }
    }

    fn with_concurrency_failure(mut self, failure: AfsError) -> Self {
        self.concurrency_failure = Some(failure);
        self
    }
}

impl HydrationSource for FakePushSource {
    fn fetch_render(&self, request: &HydrationRequest) -> AfsResult<HydratedEntity> {
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
        self.checks.set(self.checks.get() + 1);
        match &self.concurrency_failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> AfsResult<ApplyPlanResult> {
        self.applies.set(self.applies.get() + 1);
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: vec![JournalApplyEffect::UpdatedBlock {
                operation_id: request.operation_ids[0].clone(),
                operation_index: 0,
                block_id: RemoteId::new("paragraph-1"),
            }],
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> AfsResult<ApplyUndoResult> {
        Err(AfsError::NotImplemented("fake undo"))
    }
}

fn rendered_entity(body: &str) -> HydratedEntity {
    let body = format!("# Roadmap\n\n{body}");
    HydratedEntity {
        document: CanonicalDocument::new(
            "afs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            body.clone(),
        ),
        shadow: shadow(&body),
        remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
        assets: Vec::new(),
    }
}

fn seed_store<S>(store: &mut S, fixture: &PushFixture, read_only: bool)
where
    S: MountRepository + EntityRepository,
{
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .read_only(read_only),
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
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nafs:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
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

fn large_shadow() -> ShadowDocument {
    let body = (0..11)
        .map(|index| format!("Paragraph {index}."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let block_ids = (0..11)
        .map(|index| RemoteId::new(format!("block-{index}")))
        .collect::<Vec<_>>();

    ShadowDocument::from_synced_body(RemoteId::new("page-1"), body, 9, block_ids).expect("shadow")
}

fn virtual_mutation(
    mount_id: &MountId,
    local_id: &str,
    kind: VirtualMutationKind,
    path: &str,
    title: &str,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind: kind,
        target_remote_id: None,
        parent_remote_id: Some(RemoteId::new("parent-1")),
        original_path: None,
        projected_path: path.into(),
        title: title.to_string(),
        content_path: None,
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
    }
}
