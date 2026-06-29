use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::status::{StatusError, StatusOptions, StatusState, StatusSyncState, run_status};
use locality_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
};
use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::journal::{JournalEntry, JournalStatus, PushId};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::ShadowDocument;
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository,
    InMemoryStateStore, JournalRepository, MountConfig, MountRepository, ProjectionMode,
    RemoteObservationRecord, RemoteObservationRepository, ShadowRepository, SqliteStateStore,
    VirtualMutationKind, VirtualMutationRecord, VirtualMutationRepository,
};
use localityd::virtual_fs::{virtual_fs_content_path, virtual_fs_content_root};

#[test]
fn status_reports_clean_and_dirty_hydrated_files() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    fixture.hydrated_page(
        &mut store,
        "page-2",
        "Notes.md",
        "# Notes\n\nOld paragraph.",
    );
    fixture.write_page("Roadmap.md", "page-1", "# Roadmap\n\nSame paragraph.");
    fixture.write_page("Notes.md", "page-2", "# Notes\n\nChanged paragraph.");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(report.ok);
    assert!(!report.clean);
    assert_eq!(report.summary.total, 2);
    assert_eq!(report.summary.clean, 1);
    assert_eq!(report.summary.dirty, 1);
    assert_eq!(entry_state(&report, "Roadmap.md"), StatusState::Clean);
    assert_eq!(entry_state(&report, "Notes.md"), StatusState::Dirty);
    assert_eq!(entry_issue(&report, "Notes.md"), "local_body_changed");
}

#[test]
fn status_treats_content_cache_absolute_media_href_as_clean() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap/page.md",
        "# Roadmap\n\n![Image](../.loc/media/Roadmap/image-1.png)",
    );
    let absolute_media = virtual_fs_content_root(&fixture.state_root, &fixture.mount_id)
        .join(".loc/media/Roadmap/image-1.png");
    fixture.write_page(
        "Roadmap/page.md",
        "page-1",
        &format!("# Roadmap\n\n![Image]({})", absolute_media.display()),
    );

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("Roadmap/page.md")),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert_eq!(entry_state(&report, "Roadmap/page.md"), StatusState::Clean);
    assert!(report.clean);
}

#[test]
fn status_reports_frontmatter_only_property_edit_as_dirty() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    store
        .save_shadow(
            &fixture.mount_id,
            shadow("page-1", "# Roadmap\n\nSame paragraph.").with_frontmatter(
                "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            ),
        )
        .expect("save shadow with frontmatter");
    fixture.write_raw(
        "Roadmap.md",
        &canonical_markdown_with_title("page-1", "Roadmap v2", "# Roadmap\n\nSame paragraph."),
    );

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("Roadmap.md")),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(!report.clean);
    assert_eq!(report.summary.clean, 0);
    assert_eq!(report.summary.dirty, 1);
    assert_eq!(report.summary.pending_local_changes, 1);
    assert_eq!(entry_state(&report, "Roadmap.md"), StatusState::Dirty);
    assert_eq!(
        entry_sync_state(&report, "Roadmap.md"),
        StatusSyncState::PendingLocalChanges
    );
    assert_eq!(
        entry_issue(&report, "Roadmap.md"),
        "local_frontmatter_changed"
    );
}

#[test]
fn status_reports_clean_when_dirty_hint_has_equivalent_body() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    store
        .save_entity(entity_record(
            &fixture.mount_id,
            "page-1",
            "Roadmap.md",
            HydrationState::Dirty,
        ))
        .expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow("page-1", "- One\n\n- Two"))
        .expect("save shadow");
    fixture.write_page("Roadmap.md", "page-1", "- One\n- Two");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(report.clean, "{report:#?}");
    assert_eq!(entry_state(&report, "Roadmap.md"), StatusState::Clean);
    assert!(status_entry(&report, "Roadmap.md").issues.is_empty());
}

#[test]
fn status_scopes_to_subdirectory_and_reports_stub() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.stub_page(&mut store, "page-1", "Engineering/Design.md");
    fixture.hydrated_page(
        &mut store,
        "page-2",
        "Personal/Journal.md",
        "# Journal\n\nPrivate.",
    );
    fixture.write_stub("Engineering/Design.md", "page-1");
    fixture.write_page("Personal/Journal.md", "page-2", "# Journal\n\nPrivate.");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("Engineering")),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert_eq!(report.summary.total, 1);
    assert_eq!(report.summary.stub, 1);
    assert_eq!(report.mounts[0].entries[0].path, "Engineering/Design.md");
    assert_eq!(report.mounts[0].entries[0].state, StatusState::Stub);
}

#[test]
fn status_without_path_scopes_to_cwd_inside_mount() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.stub_page(&mut store, "page-1", "Engineering/Design.md");
    fixture.hydrated_page(
        &mut store,
        "page-2",
        "Personal/Journal.md",
        "# Journal\n\nPrivate.",
    );
    fixture.write_stub("Engineering/Design.md", "page-1");
    fixture.write_page("Personal/Journal.md", "page-2", "# Journal\n\nPrivate.");

    let _lock = cwd_lock().lock().expect("cwd lock");
    let _cwd = CurrentDirGuard::enter(fixture.root.join("Engineering"));
    let report = run_status(&store, StatusOptions::default()).expect("status report");

    assert_eq!(report.target, None);
    assert_eq!(report.summary.total, 1);
    assert_eq!(report.summary.stub, 1);
    assert_eq!(report.mounts[0].entries[0].path, "Engineering/Design.md");
}

#[test]
fn status_without_path_outside_mount_reports_all_mounts() {
    let fixture = StatusFixture::new();
    let second_root = TempRoot::new("loc-cli-status-second");
    let outside = TempRoot::new("loc-cli-status-outside");
    let mut store = fixture.store();
    store
        .save_mount(MountConfig::new(
            MountId::new("notion-secondary"),
            "notion",
            second_root.path.clone(),
        ))
        .expect("save second mount");

    let _lock = cwd_lock().lock().expect("cwd lock");
    let _cwd = CurrentDirGuard::enter(&outside.path);
    let report = run_status(&store, StatusOptions::default()).expect("status report");
    let mount_ids = report
        .mounts
        .iter()
        .map(|mount| mount.mount_id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(report.target, None);
    assert_eq!(mount_ids, vec!["notion-main", "notion-secondary"]);
}

#[test]
fn status_scopes_to_explicit_mount_id_without_path() {
    let fixture = StatusFixture::new();
    let second_root = TempRoot::new("loc-cli-status-explicit-mount-second");
    let outside = TempRoot::new("loc-cli-status-explicit-mount-outside");
    let mut store = fixture.store();
    fixture.stub_page(&mut store, "page-1", "Selected.md");
    fixture.write_stub("Selected.md", "page-1");
    store
        .save_mount(MountConfig::new(
            MountId::new("google-docs-main"),
            "google-docs",
            second_root.path.clone(),
        ))
        .expect("save second mount");
    store
        .save_entity(
            EntityRecord::new(
                MountId::new("google-docs-main"),
                RemoteId::new("google-docs-page"),
                EntityKind::Page,
                "table-move-guard",
                "table-move-guard/page.md",
            )
            .with_hydration(HydrationState::Dirty),
        )
        .expect("save other mount entity");

    let _lock = cwd_lock().lock().expect("cwd lock");
    let _cwd = CurrentDirGuard::enter(&outside.path);
    let report = run_status(
        &store,
        StatusOptions {
            mount_id: Some(fixture.mount_id.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert_eq!(report.target, None);
    assert_eq!(report.mounts.len(), 1);
    assert_eq!(report.mounts[0].mount_id, "notion-main");
    assert_eq!(report.summary.total, 1);
    assert_eq!(report.mounts[0].entries[0].path, "Selected.md");
}

#[test]
fn status_reports_missing_and_conflicted_entities() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(&mut store, "page-1", "Missing.md", "# Missing\n\nGone.");
    fixture.conflicted_page(&mut store, "page-2", "Conflict.md");
    fixture.write_page(
        "Conflict.md",
        "page-2",
        &format!(
            "{CONFLICT_LOCAL_MARKER}\n# Conflict\n\nLocal.\n{CONFLICT_SEPARATOR_MARKER}\n# Conflict\n\nRemote.\n{CONFLICT_REMOTE_MARKER}\n"
        ),
    );

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert_eq!(report.summary.missing, 1);
    assert_eq!(report.summary.conflicted, 1);
    assert_eq!(report.summary.sync_conflicted, 1);
    assert_eq!(entry_state(&report, "Missing.md"), StatusState::Missing);
    assert_eq!(
        entry_issue(&report, "Missing.md"),
        "local_projection_missing"
    );
    assert_eq!(entry_state(&report, "Conflict.md"), StatusState::Conflicted);
}

#[test]
fn status_reports_pending_and_failed_journals() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    fixture.write_page("Roadmap.md", "page-1", "# Roadmap\n\nSame paragraph.");
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append pending journal");
    store
        .append_journal(journal_entry(
            "push-2",
            "page-1",
            JournalStatus::Failed("connector failed".to_string()),
        ))
        .expect("append failed journal");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("Roadmap.md")),
            ..StatusOptions::default()
        },
    )
    .expect("status report");
    let entry = &report.mounts[0].entries[0];

    assert_eq!(entry.state, StatusState::Clean);
    assert_eq!(entry.pending_journal_count, 1);
    assert_eq!(entry.failed_journal_count, 1);
    assert_eq!(report.summary.pending_journals, 1);
    assert_eq!(report.summary.failed_journals, 1);
    assert!(
        entry
            .issues
            .iter()
            .any(|issue| issue.code == "pending_journal")
    );
    assert!(
        entry
            .issues
            .iter()
            .any(|issue| issue.code == "failed_journal")
    );
    assert!(
        entry
            .issues
            .iter()
            .any(|issue| issue.code == "last_failure" && issue.message == "connector failed")
    );
}

#[test]
fn status_returns_structured_error_for_unknown_path() {
    let fixture = StatusFixture::new();
    let store = fixture.store();

    let error = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("Missing.md")),
            ..StatusOptions::default()
        },
    )
    .expect_err("missing path");

    assert_eq!(error.code(), "entity_path_missing");
}

#[test]
fn status_returns_structured_mount_lookup_error() {
    let fixture = StatusFixture::new();
    let store = InMemoryStateStore::new();

    let error = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("Missing.md")),
            ..StatusOptions::default()
        },
    )
    .expect_err("missing mount");

    assert!(matches!(error, StatusError::MountNotFound(_)));
    assert_eq!(error.code(), "mount_not_found");
}

#[test]
fn status_runner_works_with_sqlite_state_store() {
    let fixture = StatusFixture::new();
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
    fixture.seed_mount(&mut store);
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    fixture.write_page("Roadmap.md", "page-1", "# Roadmap\n\nSame paragraph.");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(report.clean);
    assert_eq!(report.summary.clean, 1);
}

#[test]
fn status_reads_virtual_projection_from_content_cache() {
    let fixture = StatusFixture::new();
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save virtual mount");
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    let cache_path = virtual_fs_content_path(
        &fixture.state_root,
        &fixture.mount_id,
        Path::new("Roadmap.md"),
    )
    .expect("content path");
    fs::create_dir_all(cache_path.parent().expect("content parent")).expect("content parent");
    fs::write(
        cache_path,
        canonical_markdown("page-1", "# Roadmap\n\nSame paragraph."),
    )
    .expect("content cache");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(report.clean);
    assert_eq!(report.summary.clean, 1);
    assert_eq!(entry_state(&report, "Roadmap.md"), StatusState::Clean);
    assert_eq!(
        status_entry(&report, "Roadmap.md").absolute_path,
        fixture
            .root
            .join("notion")
            .join("Roadmap.md")
            .display()
            .to_string()
    );
}

#[test]
fn status_reports_stub_virtual_cache_edits_as_dirty() {
    let fixture = StatusFixture::new();
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save virtual mount");
    fixture.stub_page(&mut store, "page-1", "Roadmap.md");
    fixture.write_virtual_cache(
        "Roadmap.md",
        canonical_markdown("page-1", "# Roadmap\n\nChanged paragraph."),
    );

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(!report.clean);
    assert_eq!(entry_state(&report, "Roadmap.md"), StatusState::Dirty);
    assert_eq!(entry_issue(&report, "Roadmap.md"), "stub_content_changed");
}

#[test]
fn status_reports_stub_virtual_cache_conflicts_as_conflicted() {
    let fixture = StatusFixture::new();
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save virtual mount");
    fixture.stub_page(&mut store, "page-1", "Roadmap.md");
    fixture.write_virtual_cache(
        "Roadmap.md",
        format!(
            "{CONFLICT_LOCAL_MARKER}\nlocal\n{CONFLICT_SEPARATOR_MARKER}\nremote\n{CONFLICT_REMOTE_MARKER}\n"
        ),
    );

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(!report.clean);
    assert_eq!(entry_state(&report, "Roadmap.md"), StatusState::Conflicted);
    assert_eq!(
        entry_issue(&report, "Roadmap.md"),
        "unresolved_conflict_markers"
    );
}

#[test]
fn status_reports_pending_virtual_creates_and_deletes() {
    let fixture = StatusFixture::new();
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save virtual mount");
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "local:draft",
            VirtualMutationKind::Create,
            None,
            "Draft.md",
            "Draft",
        ))
        .expect("save pending create");
    store
        .save_virtual_mutation(virtual_mutation(
            &fixture.mount_id,
            "delete:page-1",
            VirtualMutationKind::Delete,
            Some(RemoteId::new("page-1")),
            "Roadmap.md",
            "Roadmap",
        ))
        .expect("save pending delete");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(!report.clean);
    assert_eq!(report.summary.total, 2);
    assert_eq!(report.summary.dirty, 2);
    assert_eq!(entry_issue(&report, "Draft.md"), "pending_virtual_create");
    assert_eq!(entry_issue(&report, "Roadmap.md"), "pending_virtual_delete");
    assert_eq!(
        status_entry(&report, "Draft.md").absolute_path,
        fixture
            .root
            .join("notion")
            .join("Draft.md")
            .display()
            .to_string()
    );
}

#[test]
fn status_reports_remote_update_available_for_clean_file() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    fixture.write_page("Roadmap.md", "page-1", "# Roadmap\n\nSame paragraph.");
    fixture.remote_observation(&mut store, "page-1", "remote-v2", true);

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(!report.clean);
    assert_eq!(report.summary.remote_update_available, 1);
    assert_eq!(
        entry_sync_state(&report, "Roadmap.md"),
        StatusSyncState::RemoteUpdateAvailable
    );
    assert_eq!(
        status_entry(&report, "Roadmap.md")
            .remote
            .synced_tree_version
            .as_deref(),
        Some("remote-v1")
    );
    assert_eq!(
        status_entry(&report, "Roadmap.md")
            .remote
            .remote_tree_version
            .as_deref(),
        Some("remote-v2")
    );
    assert_eq!(
        status_entry(&report, "Roadmap.md")
            .remote
            .remote_tree_observed_at
            .as_deref(),
        Some("unix_ms:2")
    );
    assert_eq!(entry_issue(&report, "Roadmap.md"), "remote_changed");
}

#[test]
fn status_treats_drive_only_observation_as_same_google_docs_version() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("doc-1"),
                EntityKind::Page,
                "Google Doc",
                "google-doc/page.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("drive:5:2026-06-25T10:18:24.085Z|docs:rev-1"),
        )
        .expect("save entity");
    store
        .save_shadow(
            &fixture.mount_id,
            shadow("doc-1", "# Google Doc\n\nSame body."),
        )
        .expect("save shadow");
    fixture.write_page("google-doc/page.md", "doc-1", "# Google Doc\n\nSame body.");
    store
        .save_remote_observation(
            RemoteObservationRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("doc-1"),
                EntityKind::Page,
                "Google Doc",
                "google-doc/page.md",
                "unix_ms:2",
            )
            .with_remote_version(RemoteVersion::new("drive:5:2026-06-25T10:18:24.085Z")),
        )
        .expect("save remote observation");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("doc-1"),
                FreshnessTier::Warm,
            )
            .checked_at("unix_ms:2"),
        )
        .expect("save freshness state");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("google-doc/page.md")),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(report.clean, "{report:#?}");
    assert_eq!(report.summary.remote_update_available, 0);
    assert_eq!(
        entry_sync_state(&report, "google-doc/page.md"),
        StatusSyncState::AllSynced
    );
}

#[test]
fn status_reports_review_needed_when_local_and_remote_changed() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    fixture.write_page("Roadmap.md", "page-1", "# Roadmap\n\nChanged paragraph.");
    fixture.remote_observation(&mut store, "page-1", "remote-v2", true);

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert_eq!(report.summary.review_needed, 1);
    assert_eq!(
        entry_sync_state(&report, "Roadmap.md"),
        StatusSyncState::ReviewNeeded
    );
    assert!(entry_has_issue(&report, "Roadmap.md", "local_body_changed"));
    assert!(entry_has_issue(
        &report,
        "Roadmap.md",
        "remote_changed_with_local_pending"
    ));
}

#[test]
fn status_reports_checking_freshness_without_attention() {
    let fixture = StatusFixture::new();
    let mut store = fixture.store();
    fixture.hydrated_page(
        &mut store,
        "page-1",
        "Roadmap.md",
        "# Roadmap\n\nSame paragraph.",
    );
    fixture.write_page("Roadmap.md", "page-1", "# Roadmap\n\nSame paragraph.");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                FreshnessTier::Hot,
            )
            .opened_at("unix_ms:1"),
        )
        .expect("save freshness state");

    let report = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status report");

    assert!(report.clean);
    assert_eq!(report.summary.checking_freshness, 1);
    assert_eq!(
        entry_sync_state(&report, "Roadmap.md"),
        StatusSyncState::CheckingFreshness
    );
    assert_eq!(entry_issue(&report, "Roadmap.md"), "checking_freshness");
}

struct StatusFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
}

impl StatusFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "loc-cli-status-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = std::env::temp_dir().join(format!(
            "loc-cli-status-state-{}-{unique}-{suffix}",
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

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        self.seed_mount(&mut store);
        store
    }

    fn seed_mount<S>(&self, store: &mut S)
    where
        S: MountRepository,
    {
        store
            .save_mount(MountConfig::new(
                self.mount_id.clone(),
                "notion",
                self.root.clone(),
            ))
            .expect("save mount");
    }

    fn hydrated_page<S>(&self, store: &mut S, remote_id: &str, path: &str, body: &str)
    where
        S: EntityRepository + ShadowRepository,
    {
        store
            .save_entity(entity_record(
                &self.mount_id,
                remote_id,
                path,
                HydrationState::Hydrated,
            ))
            .expect("save entity");
        store
            .save_shadow(&self.mount_id, shadow(remote_id, body))
            .expect("save shadow");
    }

    fn stub_page<S>(&self, store: &mut S, remote_id: &str, path: &str)
    where
        S: EntityRepository,
    {
        store
            .save_entity(entity_record(
                &self.mount_id,
                remote_id,
                path,
                HydrationState::Stub,
            ))
            .expect("save entity");
    }

    fn conflicted_page<S>(&self, store: &mut S, remote_id: &str, path: &str)
    where
        S: EntityRepository,
    {
        store
            .save_entity(entity_record(
                &self.mount_id,
                remote_id,
                path,
                HydrationState::Conflicted,
            ))
            .expect("save entity");
    }

    fn write_page(&self, relative_path: &str, remote_id: &str, body: &str) -> PathBuf {
        self.write_raw(relative_path, &canonical_markdown(remote_id, body))
    }

    fn write_stub(&self, relative_path: &str, remote_id: &str) -> PathBuf {
        self.write_raw(
            relative_path,
            &canonical_markdown(remote_id, CanonicalDocument::STUB_MARKER),
        )
    }

    fn write_raw(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, contents).expect("fixture file");
        path
    }

    fn write_virtual_cache(&self, relative_path: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path =
            virtual_fs_content_path(&self.state_root, &self.mount_id, Path::new(relative_path))
                .expect("content path");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("content parent");
        }
        fs::write(&path, contents).expect("fixture content cache");
        path
    }

    fn remote_observation<S>(
        &self,
        store: &mut S,
        remote_id: &str,
        remote_version: &str,
        remote_hint_pending: bool,
    ) where
        S: RemoteObservationRepository + FreshnessStateRepository,
    {
        store
            .save_remote_observation(
                RemoteObservationRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new(remote_id),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                    "unix_ms:2",
                )
                .with_remote_version(RemoteVersion::new(remote_version)),
            )
            .expect("save remote observation");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new(remote_id),
                    FreshnessTier::Hot,
                )
                .checked_at("unix_ms:2")
                .remote_hint_pending(remote_hint_pending),
            )
            .expect("save freshness state");
    }
}

impl Drop for StatusFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(prefix: &str) -> Self {
        let path = unique_temp_path(prefix);
        fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct CurrentDirGuard {
    original: PathBuf,
}

impl CurrentDirGuard {
    fn enter(path: impl Into<PathBuf>) -> Self {
        let original = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(path.into()).expect("set current dir");
        Self { original }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

fn entity_record(
    mount_id: &MountId,
    remote_id: &str,
    path: &str,
    hydration: HydrationState,
) -> EntityRecord {
    EntityRecord::new(
        mount_id.clone(),
        RemoteId::new(remote_id),
        EntityKind::Page,
        "Roadmap",
        path,
    )
    .with_hydration(hydration)
    .with_remote_edited_at("remote-v1")
}

fn cwd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    canonical_markdown_with_title(remote_id, "Roadmap", body)
}

fn canonical_markdown_with_title(remote_id: &str, title: &str, body: &str) -> String {
    format!(
        "---\nloc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: {title}\n---\n{body}"
    )
}

fn shadow(remote_id: &str, body: &str) -> ShadowDocument {
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

fn journal_entry(push_id: &str, remote_id: &str, status: JournalStatus) -> JournalEntry {
    JournalEntry::new(
        PushId(push_id.to_string()),
        MountId::new("notion-main"),
        vec![RemoteId::new(remote_id)],
        PushPlan::new(
            vec![RemoteId::new(remote_id)],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new(format!("{remote_id}-paragraph-1")),
                content: "Updated paragraph.".to_string(),
            }],
        ),
        status,
    )
}

fn virtual_mutation(
    mount_id: &MountId,
    local_id: &str,
    kind: VirtualMutationKind,
    target_remote_id: Option<RemoteId>,
    path: &str,
    title: &str,
) -> VirtualMutationRecord {
    VirtualMutationRecord {
        mount_id: mount_id.clone(),
        local_id: local_id.to_string(),
        mutation_kind: kind,
        target_remote_id,
        parent_remote_id: None,
        original_path: None,
        projected_path: PathBuf::from(path),
        title: title.to_string(),
        content_path: None,
        created_at: "2026-06-12T00:00:00Z".to_string(),
        updated_at: "2026-06-12T00:00:00Z".to_string(),
    }
}

fn entry_state(report: &loc_cli::status::StatusReport, path: &str) -> StatusState {
    status_entry(report, path).state
}

fn status_entry<'a>(
    report: &'a loc_cli::status::StatusReport,
    path: &str,
) -> &'a loc_cli::status::StatusEntry {
    report
        .mounts
        .iter()
        .flat_map(|mount| &mount.entries)
        .find(|entry| entry.path == path)
        .expect("status entry")
}

fn entry_sync_state(report: &loc_cli::status::StatusReport, path: &str) -> StatusSyncState {
    status_entry(report, path).sync_state
}

fn entry_issue(report: &loc_cli::status::StatusReport, path: &str) -> String {
    report
        .mounts
        .iter()
        .flat_map(|mount| &mount.entries)
        .find(|entry| entry.path == path)
        .expect("status entry")
        .issues
        .first()
        .expect("issue")
        .code
        .clone()
}

fn entry_has_issue(report: &loc_cli::status::StatusReport, path: &str, code: &str) -> bool {
    report
        .mounts
        .iter()
        .flat_map(|mount| &mount.entries)
        .find(|entry| entry.path == path)
        .expect("status entry")
        .issues
        .iter()
        .any(|issue| issue.code == code)
}
