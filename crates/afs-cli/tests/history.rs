use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::history::{HistoryError, LogOptions, run_log, run_undo, undo_report_exit_code};
use afs_core::journal::{JournalEntry, JournalStatus, PushId};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::planner::{PushOperation, PushPlan};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, SqliteStateStore,
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
fn undo_reconciled_journal_entry_stops_at_remote_undo_boundary() {
    let fixture = HistoryFixture::new();
    let mut store = fixture.store();
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Reconciled))
        .expect("append journal");

    let report = run_undo(&mut store, "push-1").expect("undo report");

    assert!(!report.ok);
    assert_eq!(report.action, "undo_not_implemented");
    assert_eq!(report.status, "reconciled");
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
            "afs-cli-history-{}-{unique}-{suffix}",
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
    JournalEntry {
        push_id: PushId(push_id.to_string()),
        mount_id: MountId::new("notion-main"),
        remote_ids: vec![RemoteId::new(remote_id)],
        plan: PushPlan::new(
            vec![RemoteId::new(remote_id)],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new(format!("{remote_id}-paragraph-1")),
                content: "Updated paragraph.".to_string(),
            }],
        ),
        status,
    }
}
