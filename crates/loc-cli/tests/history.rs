use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::history::{
    HistoryError, LogOptions, run_log, run_undo, run_undo_with_applier, undo_report_exit_code,
};
use locality_core::journal::{
    JournalApplyEffect, JournalEntry, JournalPreimage, JournalStatus, PushId, PushOperationId,
};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{CreateParentScope, PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_core::undo::{UndoApplier, UndoApplyRequest, UndoApplyResult};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, ShadowRepository, SqliteStateStore,
};

#[test]
fn log_lists_journal_entries_newest_first() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append first journal");
    store
        .append_journal(journal_entry("push-2", "page-1", JournalStatus::Reconciled))
        .expect("append second journal");

    let report = run_log(&store, LogOptions::default()).expect("log report");

    assert!(report.ok);
    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.entries[0].push_id, "push-2");
    assert_eq!(report.entries[0].status, "reconciled");
    assert_eq!(report.entries[0].operation_count, 1);
    assert_eq!(report.entries[0].preimage_count, 1);
    assert_eq!(report.entries[0].apply_effect_count, 0);
    assert_eq!(report.entries[0].plan_summary.blocks_updated, 1);
    assert_eq!(report.entries[1].push_id, "push-1");
}

#[test]
fn log_filters_journal_entries_by_projected_path() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .save_entity(entity_record(&fixture, "page-2", "Notes.md"))
        .expect("save second entity");
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append first journal");
    store
        .append_journal(journal_entry("push-2", "page-2", JournalStatus::Prepared))
        .expect("append second journal");

    let report = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("Roadmap.md")),
        },
    )
    .expect("filtered log");

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].push_id, "push-1");
    assert_eq!(report.entries[0].remote_ids, vec!["page-1"]);
}

#[test]
fn log_page_directory_targets_page_document() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    fs::create_dir_all(fixture.root.join("Roadmap")).expect("create page dir");
    fs::write(fixture.root.join("Roadmap/page.md"), "").expect("write page document");
    store
        .save_entity(entity_record(&fixture, "page-2", "Roadmap/page.md"))
        .expect("save page-directory entity");
    store
        .append_journal(journal_entry("push-1", "page-2", JournalStatus::Prepared))
        .expect("append page-directory journal");

    let report = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("Roadmap")),
        },
    )
    .expect("page-directory log");

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].push_id, "push-1");
    assert_eq!(report.entries[0].remote_ids, vec!["page-2"]);
}

#[test]
fn log_filters_created_entity_journal_by_created_path() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    fs::create_dir_all(fixture.root.join("New child")).expect("create child dir");
    fs::write(fixture.root.join("New child/page.md"), "").expect("write child page");
    store
        .save_entity(entity_record(
            &fixture,
            "created-page-1",
            "New child/page.md",
        ))
        .expect("save created entity");
    let operation = PushOperation::CreateEntity {
        parent_id: RemoteId::new("page-1"),
        parent_kind: Some(EntityKind::Page),
        parent_scope: CreateParentScope::Remote,
        title: "New child".to_string(),
        properties: Default::default(),
        body: "Created child.".to_string(),
        source_path: "New child/page.md".into(),
    };
    let push_id = PushId("push-create".to_string());
    let operation_id = PushOperationId::for_operation(&push_id, 0, &operation);
    let entry = JournalEntry::new(
        push_id.clone(),
        fixture.mount_id.clone(),
        vec![RemoteId::new("page-1")],
        PushPlan::new(vec![RemoteId::new("page-1")], vec![operation]),
        JournalStatus::Reconciled,
    )
    .with_apply_effects(vec![JournalApplyEffect::CreatedEntity {
        operation_id,
        operation_index: 0,
        parent_id: RemoteId::new("page-1"),
        entity_id: RemoteId::new("created-page-1"),
    }]);
    store.append_journal(entry).expect("append create journal");

    let report = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("New child/page.md")),
        },
    )
    .expect("filtered created entity log");

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].push_id, push_id.0);
    assert_eq!(report.entries[0].remote_ids, vec!["page-1"]);
    assert_eq!(report.entries[0].apply_effect_count, 1);
}

#[test]
fn log_reports_structured_error_for_unknown_path() {
    let fixture = HistoryFixture::new();
    let store = fixture.store();

    let error = run_log(
        &store,
        LogOptions {
            path: Some(fixture.root.join("Missing.md")),
        },
    )
    .expect_err("missing path");

    assert_eq!(error.code(), "entity_path_missing");
}

#[test]
fn undo_prepared_journal_entry_marks_it_reverted() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverted_local_journal");
    assert_eq!(report.status, "reverted");
    assert_eq!(undo_report_exit_code(&report), 0);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

#[test]
fn undo_failed_journal_without_apply_effects_marks_it_reverted() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry(
            "push-1",
            "page-1",
            JournalStatus::Failed("remote changed before apply".to_string()),
        ))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverted_empty_failed_journal");
    assert_eq!(report.status, "reverted");
    assert_eq!(undo_report_exit_code(&report), 0);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

#[test]
fn undo_reconciled_journal_entry_derives_reverse_plan_and_stops_before_apply() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(!report.ok);
    assert_eq!(report.action, "reverse_apply_not_implemented");
    assert_eq!(report.status, "reconciled");
    assert_eq!(
        report.undo_plan.as_ref().expect("undo plan").status,
        "complete"
    );
    assert_eq!(
        report
            .undo_plan
            .as_ref()
            .expect("undo plan")
            .operations
            .len(),
        1
    );
    assert_eq!(undo_report_exit_code(&report), 5);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn undo_reports_blocked_plan_for_append_without_created_id() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry_with_operations(
            "push-1",
            "page-1",
            JournalStatus::Reconciled,
            vec![PushOperation::AppendBlock {
                parent_id: RemoteId::new("page-1"),
                after: Some(RemoteId::new("page-1-paragraph-1")),
                content: "New paragraph.".to_string(),
            }],
        ))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(!report.ok);
    assert_eq!(report.action, "undo_plan_blocked");
    let undo_plan = report.undo_plan.expect("undo plan");
    assert_eq!(undo_plan.status, "blocked");
    assert_eq!(
        undo_plan.unsupported[0].code,
        "append_block_missing_created_id"
    );
}

#[test]
fn undo_with_applier_reverses_complete_plan_and_marks_journal_reverted() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");
    let mut applier = FakeUndoApplier::default();

    let report = run_undo_with_applier(&mut store, "push-1", &mut applier).expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverse_applied");
    assert_eq!(report.changed_remote_ids, vec!["page-1"]);
    assert_eq!(applier.applied_push_ids, vec![PushId("push-1".to_string())]);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

#[test]
fn undo_with_applier_restores_local_projection_from_preimage() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    let pushed_body = "# Roadmap\n\nUpdated paragraph.";
    fs::write(
        fixture.root.join("Roadmap.md"),
        canonical_markdown("page-1", pushed_body),
    )
    .expect("write pushed projection");
    store
        .save_shadow(&fixture.mount_id, shadow_with_body("page-1", pushed_body))
        .expect("save pushed shadow");
    store
        .save_entity(
            entity_record(&fixture, "page-1", "Roadmap.md")
                .with_content_hash(shadow_with_body("page-1", pushed_body).body_hash),
        )
        .expect("save pushed entity");
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");
    let mut applier = FakeUndoApplier::default();

    let report = run_undo_with_applier(&mut store, "push-1", &mut applier).expect("undo report");

    assert!(report.ok);
    assert_eq!(report.action, "reverse_applied");
    let restored = fs::read_to_string(fixture.root.join("Roadmap.md")).expect("restored file");
    assert!(restored.contains("# Roadmap\n\nOriginal paragraph."));
    assert!(!restored.contains("Updated paragraph."));
    let shadow = store
        .load_shadow(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("restored shadow");
    assert_eq!(shadow.rendered_body, "# Roadmap\n\nOriginal paragraph.");
}

#[test]
fn undo_with_applier_reports_reverse_apply_failure_without_status_change() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");
    let mut applier = FakeUndoApplier::default()
        .with_failure(LocalityError::NotImplemented("fake reverse apply"));

    let report = run_undo_with_applier(&mut store, "push-1", &mut applier).expect("undo report");

    assert!(!report.ok);
    assert_eq!(report.action, "reverse_apply_not_implemented");
    assert_eq!(report.message, "not implemented: fake reverse apply");
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn undo_reports_missing_journal() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();

    let error = run_undo(&mut store, "missing-push").expect_err("missing journal");

    assert_eq!(
        error,
        HistoryError::JournalNotFound(PushId("missing-push".to_string()))
    );
    assert_eq!(error.code(), "journal_not_found");
}

#[test]
fn log_and_undo_work_with_sqlite_state_store() {
    let fixture = HistoryFixture::new();
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
    seed_store(&mut store, &fixture);
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append journal");

    let log_report = run_log(&store, LogOptions::default()).expect("log report");
    assert_eq!(log_report.entries.len(), 1);
    assert_eq!(log_report.entries[0].push_id, "push-1");

    let undo_report = run_undo(&mut store, "push-1").expect("undo report");
    assert!(undo_report.ok);
    assert_eq!(
        store
            .get_journal(&PushId("push-1".to_string()))
            .expect("get journal")
            .expect("journal")
            .status,
        JournalStatus::Reverted
    );
}

struct HistoryFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl HistoryFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "loc-cli-history-{}-{unique}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        fs::write(root.join("Roadmap.md"), "").expect("roadmap file");

        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        seed_store(&mut store, self);
        store
    }
}

impl Drop for HistoryFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Default)]
struct FakeUndoApplier {
    applied_push_ids: Vec<PushId>,
    failure: Option<LocalityError>,
}

impl FakeUndoApplier {
    fn with_failure(mut self, failure: LocalityError) -> Self {
        self.failure = Some(failure);
        self
    }
}

impl UndoApplier for FakeUndoApplier {
    fn apply_undo(&mut self, request: UndoApplyRequest<'_>) -> LocalityResult<UndoApplyResult> {
        self.applied_push_ids.push(request.target_push_id.clone());

        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(UndoApplyResult {
                changed_remote_ids: request.plan.affected_entities.clone(),
            }),
        }
    }
}

fn seed_store<S>(store: &mut S, fixture: &HistoryFixture)
where
    S: MountRepository + EntityRepository,
{
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.root.clone(),
        ))
        .expect("save mount");
    store
        .save_entity(entity_record(fixture, "page-1", "Roadmap.md"))
        .expect("save entity");
}

fn entity_record(fixture: &HistoryFixture, remote_id: &str, path: &str) -> EntityRecord {
    EntityRecord::new(
        fixture.mount_id.clone(),
        RemoteId::new(remote_id),
        EntityKind::Page,
        "Roadmap",
        path,
    )
    .with_hydration(HydrationState::Hydrated)
}

fn journal_entry(push_id: &str, remote_id: &str, status: JournalStatus) -> JournalEntry {
    journal_entry_with_operations(
        push_id,
        remote_id,
        status,
        vec![PushOperation::UpdateBlock {
            block_id: RemoteId::new(format!("{remote_id}-paragraph-1")),
            content: "Updated paragraph.".to_string(),
        }],
    )
}

fn journal_entry_with_operations(
    push_id: &str,
    remote_id: &str,
    status: JournalStatus,
    operations: Vec<PushOperation>,
) -> JournalEntry {
    JournalEntry::new(
        PushId(push_id.to_string()),
        MountId::new("notion-main"),
        vec![RemoteId::new(remote_id)],
        PushPlan::new(vec![RemoteId::new(remote_id)], operations),
        status,
    )
    .with_preimages(vec![JournalPreimage::from_shadow(shadow(remote_id))])
}

fn shadow(remote_id: &str) -> ShadowDocument {
    shadow_with_body(remote_id, "# Roadmap\n\nOriginal paragraph.")
}

fn shadow_with_body(remote_id: &str, body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body,
        9,
        [
            RemoteId::new(format!("{remote_id}-heading-1")),
            RemoteId::new(format!("{remote_id}-paragraph-1")),
        ],
    )
    .expect("shadow")
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nloc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}
