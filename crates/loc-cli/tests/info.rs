use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::info::{InfoOptions, InfoRole, run_info};
use locality_core::journal::{JournalEntry, JournalStatus, PushId};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, JournalRepository, MountConfig,
    MountRepository, ProjectionMode, SqliteStateStore,
};
use localityd::virtual_fs::virtual_projection_mount_point;

#[test]
fn info_for_page_file_reports_source_children_and_journals() {
    let fixture = InfoFixture::new();
    let mut store = fixture.store();
    fixture.seed_tree(&mut store);
    store
        .append_journal(journal_entry("push-1", "page-1", JournalStatus::Prepared))
        .expect("append pending journal");
    store
        .append_journal(journal_entry(
            "push-2",
            "row-1",
            JournalStatus::Failed("connector failed".to_string()),
        ))
        .expect("append failed journal");

    let report = run_info(
        &store,
        InfoOptions {
            path: Some(fixture.root.join("roadmap/page.md")),
        },
    )
    .expect("info report");

    assert!(report.ok);
    assert_eq!(report.command, "info");
    assert_eq!(report.mount.mount_id, "notion-main");
    assert_eq!(report.mount.remote_root_id.as_deref(), Some("root-page"));
    assert_eq!(report.subject.role, InfoRole::PageFile);
    assert_eq!(report.subject.source, "Notion page");
    assert_eq!(
        report
            .subject
            .entity
            .as_ref()
            .map(|entity| entity.title.as_str()),
        Some("Roadmap")
    );
    assert_eq!(report.children.pages, 1);
    assert_eq!(report.children.databases, 1);
    assert_eq!(report.children.immediate, 2);
    assert_eq!(report.children.subtree, 3);
    assert_eq!(report.journals.pending, 1);
    assert_eq!(report.journals.failed, 1);
}

#[test]
fn info_for_page_workspace_resolves_the_backing_page() {
    let fixture = InfoFixture::new();
    let mut store = fixture.store();
    fixture.seed_tree(&mut store);

    let report = run_info(
        &store,
        InfoOptions {
            path: Some(fixture.root.join("roadmap")),
        },
    )
    .expect("info report");

    assert_eq!(report.subject.role, InfoRole::PageWorkspace);
    assert_eq!(
        report
            .subject
            .entity
            .as_ref()
            .map(|entity| entity.path.as_str()),
        Some("roadmap/page.md")
    );
    assert_eq!(report.children.pages, 1);
    assert_eq!(report.children.databases, 1);
}

#[test]
fn info_for_database_directory_reports_schema_and_rows() {
    let fixture = InfoFixture::new();
    let mut store = fixture.store();
    fixture.seed_tree(&mut store);

    let report = run_info(
        &store,
        InfoOptions {
            path: Some(fixture.root.join("roadmap/tasks")),
        },
    )
    .expect("info report");

    let expected_schema = fixture
        .root
        .join("roadmap/tasks/_schema.yaml")
        .display()
        .to_string();

    assert_eq!(report.subject.role, InfoRole::DatabaseDirectory);
    assert_eq!(report.subject.source, "Notion database");
    assert_eq!(
        report
            .subject
            .entity
            .as_ref()
            .map(|entity| entity.title.as_str()),
        Some("Tasks")
    );
    assert_eq!(
        report
            .subject
            .schema_path
            .as_deref()
            .map(|path| path.replace('\\', "/")),
        Some(expected_schema.replace('\\', "/"))
    );
    assert_eq!(report.children.pages, 1);
    assert_eq!(report.children.immediate, 1);
    assert_eq!(report.children.subtree, 1);
}

#[test]
fn info_without_path_uses_cwd_inside_mount() {
    let fixture = InfoFixture::new();
    let mut store = fixture.store();
    fixture.seed_tree(&mut store);

    let _lock = cwd_lock().lock().expect("cwd lock");
    let _cwd = CurrentDirGuard::enter(fixture.root.join("roadmap/tasks"));
    let report = run_info(&store, InfoOptions::default()).expect("info report");

    assert_eq!(report.subject.role, InfoRole::DatabaseDirectory);
    assert!(report.target.replace('\\', "/").ends_with("roadmap/tasks"));
}

#[test]
fn info_reports_untracked_paths_inside_mount() {
    let fixture = InfoFixture::new();
    let store = fixture.store();
    fs::create_dir_all(fixture.root.join("scratch")).expect("scratch dir");

    let report = run_info(
        &store,
        InfoOptions {
            path: Some(fixture.root.join("scratch")),
        },
    )
    .expect("info report");

    assert_eq!(report.subject.role, InfoRole::UntrackedPath);
    assert!(report.subject.entity.is_none());
}

#[test]
fn info_returns_mount_lookup_error_outside_registered_mounts() {
    let fixture = InfoFixture::new();
    let outside = TempRoot::new("loc-cli-info-outside");
    let store = fixture.store();

    let error = run_info(
        &store,
        InfoOptions {
            path: Some(outside.path.join("Missing.md")),
        },
    )
    .expect_err("missing mount");

    assert_eq!(error.code(), "mount_not_found");
}

#[test]
fn info_runner_works_with_sqlite_state_store() {
    let fixture = InfoFixture::new();
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
    fixture.seed_mount(&mut store);
    fixture.seed_tree(&mut store);

    let report = run_info(
        &store,
        InfoOptions {
            path: Some(fixture.root.join("roadmap/page.md")),
        },
    )
    .expect("info report");

    assert_eq!(report.subject.role, InfoRole::PageFile);
    assert_eq!(report.children.immediate, 2);
}

#[test]
fn info_for_linux_fuse_reports_entity_absolute_path_under_mount_point_root() {
    let fixture = InfoFixture::new();
    let mount = MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
        .projection(ProjectionMode::LinuxFuse);
    let visible_root = virtual_projection_mount_point(&mount);
    let visible_file = visible_root.join("roadmap").join("page.md");
    if let Some(parent) = visible_file.parent() {
        fs::create_dir_all(parent).expect("visible parent");
    }
    fs::write(&visible_file, "").expect("visible page");

    let mut store = InMemoryStateStore::new();
    store.save_mount(mount).expect("save linux fuse mount");
    store
        .save_entity(
            entity_record(
                &fixture.mount_id,
                "page-1",
                EntityKind::Page,
                "Roadmap",
                "roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save page");

    let report = run_info(
        &store,
        InfoOptions {
            path: Some(visible_file.clone()),
        },
    )
    .expect("info report");

    let expected = visible_file.display().to_string();
    assert_eq!(report.subject.absolute_path, expected);
    assert_eq!(
        report
            .subject
            .entity
            .as_ref()
            .map(|entity| entity.absolute_path.as_str()),
        Some(expected.as_str())
    );
}

struct InfoFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl InfoFixture {
    fn new() -> Self {
        let root = unique_temp_path("loc-cli-info");
        fs::create_dir_all(&root).expect("fixture root");

        Self {
            root,
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
            .save_mount(
                MountConfig::new(self.mount_id.clone(), "notion", self.root.clone())
                    .with_remote_root_id(RemoteId::new("root-page")),
            )
            .expect("save mount");
    }

    fn seed_tree<S>(&self, store: &mut S)
    where
        S: EntityRepository,
    {
        self.write_raw("roadmap/page.md");
        fs::create_dir_all(self.root.join("roadmap/tasks")).expect("database dir");
        self.write_raw("roadmap/design/page.md");
        self.write_raw("roadmap/tasks/fix-login/page.md");
        fs::write(
            self.root.join("roadmap/tasks/_schema.yaml"),
            "title: Tasks\n",
        )
        .expect("schema");

        store
            .save_entity(
                entity_record(
                    &self.mount_id,
                    "page-1",
                    EntityKind::Page,
                    "Roadmap",
                    "roadmap/page.md",
                )
                .with_hydration(HydrationState::Hydrated)
                .with_content_hash("hash-page-1")
                .with_remote_edited_at("2026-06-10T00:00:00Z"),
            )
            .expect("save root page");
        store
            .save_entity(entity_record(
                &self.mount_id,
                "page-2",
                EntityKind::Page,
                "Design",
                "roadmap/design/page.md",
            ))
            .expect("save child page");
        store
            .save_entity(entity_record(
                &self.mount_id,
                "database-1",
                EntityKind::Database,
                "Tasks",
                "roadmap/tasks",
            ))
            .expect("save database");
        store
            .save_entity(entity_record(
                &self.mount_id,
                "row-1",
                EntityKind::Page,
                "Fix login",
                "roadmap/tasks/fix-login/page.md",
            ))
            .expect("save row");
    }

    fn write_raw(&self, relative_path: &str) {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(path, "").expect("fixture file");
    }
}

impl Drop for InfoFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
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
    kind: EntityKind,
    title: &str,
    path: &str,
) -> EntityRecord {
    EntityRecord::new(
        mount_id.clone(),
        RemoteId::new(remote_id),
        kind,
        title,
        path,
    )
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
