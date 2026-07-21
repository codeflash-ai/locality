use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::diff::run_diff;
use loc_cli::mv::{MvError, MvOptions, run_mv};
use loc_cli::status::{StatusOptions, StatusState, run_status};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, SqliteStateStore, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use localityd::virtual_fs::virtual_fs_content_root;
use serde_json::Value;

#[test]
fn mv_plain_files_renames_locally_and_reports_next_steps() {
    let fixture = MvFixture::new("loc-mv-plain");
    let mut store = fixture.store(ProjectionMode::PlainFiles, false);
    let source = fixture.write("Roadmap.md", "body");
    let destination = fixture.root.join("Renamed.md");

    let report = run_mv(
        &mut store,
        MvOptions {
            source: source.clone(),
            destination: destination.clone(),
            state_root: None,
        },
    )
    .expect("move file");

    assert_eq!(report.command, "mv");
    assert_eq!(report.mode, "plain_files");
    assert_eq!(Path::new(&report.destination), destination);
    assert!(!report.pushed);
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(&destination).expect("dest"), "body");
    assert!(
        report
            .next
            .iter()
            .any(|step| step.starts_with("loc diff") && step.contains("Renamed.md"))
    );
}

#[test]
fn mv_plain_files_destination_directory_appends_source_filename() {
    let fixture = MvFixture::new("loc-mv-dest-dir");
    let mut store = fixture.store(ProjectionMode::PlainFiles, false);
    let source = fixture.write("Roadmap.md", "body");
    let archive = fixture.root.join("Archive");
    fs::create_dir_all(&archive).expect("archive");

    let report = run_mv(
        &mut store,
        MvOptions {
            source: source.clone(),
            destination: archive.clone(),
            state_root: None,
        },
    )
    .expect("move into directory");

    let destination = archive.join("Roadmap.md");
    assert_eq!(Path::new(&report.destination), destination);
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(destination).expect("dest"), "body");
}

#[test]
fn mv_plain_files_rejects_overwrite_before_mutating() {
    let fixture = MvFixture::new("loc-mv-overwrite");
    let mut store = fixture.store(ProjectionMode::PlainFiles, false);
    let source = fixture.write("Roadmap.md", "source");
    let destination = fixture.write("Renamed.md", "existing");

    let error = run_mv(
        &mut store,
        MvOptions {
            source: source.clone(),
            destination: destination.clone(),
            state_root: None,
        },
    )
    .expect_err("overwrite rejected");

    assert!(matches!(error, MvError::DestinationExists(path) if path == destination));
    assert_eq!(fs::read_to_string(source).expect("source"), "source");
    assert_eq!(fs::read_to_string(destination).expect("dest"), "existing");
}

#[test]
fn mv_rejects_read_only_mount_without_changes() {
    let fixture = MvFixture::new("loc-mv-read-only");
    let mut store = fixture.store(ProjectionMode::PlainFiles, true);
    let source = fixture.write("Roadmap.md", "source");
    let destination = fixture.root.join("Renamed.md");

    let error = run_mv(
        &mut store,
        MvOptions {
            source: source.clone(),
            destination: destination.clone(),
            state_root: None,
        },
    )
    .expect_err("read-only rejected");

    assert!(matches!(error, MvError::ReadOnlyMount { mount_id } if mount_id == "notion-main"));
    assert!(source.exists());
    assert!(!destination.exists());
}

#[test]
fn mv_rejects_cross_mount_destination_without_changes() {
    let fixture = MvFixture::new("loc-mv-cross-mount");
    let other = MvFixture::new("loc-mv-cross-mount-other");
    let mut store = fixture.store(ProjectionMode::PlainFiles, false);
    store
        .save_mount(MountConfig::new(
            MountId::new("notion-other"),
            "notion",
            other.root.clone(),
        ))
        .expect("save other mount");
    let source = fixture.write("Roadmap.md", "source");
    let destination = other.root.join("Roadmap.md");

    let error = run_mv(
        &mut store,
        MvOptions {
            source: source.clone(),
            destination,
            state_root: None,
        },
    )
    .expect_err("cross mount rejected");

    assert!(
        matches!(error, MvError::CrossMount { source_mount_id, destination_mount_id }
            if source_mount_id == "notion-main" && destination_mount_id == "notion-other")
    );
    assert!(source.exists());
}

#[test]
fn mv_rejects_mount_root_and_missing_parent() {
    let fixture = MvFixture::new("loc-mv-validation");
    let mut store = fixture.store(ProjectionMode::PlainFiles, false);
    let source = fixture.write("Roadmap.md", "source");

    let root_error = run_mv(
        &mut store,
        MvOptions {
            source: fixture.root.clone(),
            destination: fixture.root.join("Moved"),
            state_root: None,
        },
    )
    .expect_err("mount root rejected");
    assert!(matches!(root_error, MvError::MountRootMove { .. }));

    let parent_error = run_mv(
        &mut store,
        MvOptions {
            source: source.clone(),
            destination: fixture.root.join("Missing/Roadmap.md"),
            state_root: None,
        },
    )
    .expect_err("missing parent rejected");
    assert!(
        matches!(parent_error, MvError::MissingDestinationParent(path) if path.ends_with("Missing"))
    );
    assert!(source.exists());
}

#[test]
fn cli_mv_emits_json_report() {
    let fixture = MvFixture::new("loc-mv-cli-json");
    let state_root = fixture.temp.path("state");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("sqlite");
    store
        .save_mount(fixture.mount(ProjectionMode::PlainFiles, false))
        .expect("mount");
    let source = fixture.write("Roadmap.md", "body");
    let destination = fixture.root.join("Renamed.md");

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .env("LOCALITY_STATE_DIR", &state_root)
        .env("LOCALITY_DAEMON_DISABLE", "1")
        .args([
            "mv",
            source.to_str().expect("source"),
            destination.to_str().expect("destination"),
            "--json",
        ])
        .output()
        .expect("loc mv");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(json["ok"], true);
    assert_eq!(json["command"], "mv");
    assert_eq!(json["mode"], "plain_files");
    assert_eq!(json["pushed"], false);
    assert_eq!(json["destination"], destination.display().to_string());
}

#[test]
fn cli_mv_usage_errors_exit_with_usage_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .args(["mv", "only-source"])
        .output()
        .expect("loc mv usage");

    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn mv_plain_files_move_is_visible_to_status_and_diff() {
    let fixture = MvFixture::new("loc-mv-plain-workflow");
    let mut store = fixture.store(ProjectionMode::PlainFiles, false);
    seed_page(
        &mut store,
        "page-parent",
        "Folder",
        "Folder/page.md",
        HydrationState::Hydrated,
    );
    seed_page(
        &mut store,
        "page-1",
        "Roadmap",
        "Folder/Roadmap/page.md",
        HydrationState::Hydrated,
    );
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("page-1"),
                "# Roadmap\n\nBody.",
                9,
                [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
            )
            .expect("shadow"),
        )
        .expect("save shadow");
    fixture.write(
        "Folder/Roadmap/page.md",
        "---\nloc:\n  id: page-1\n  type: page\ntitle: \"Roadmap\"\n---\n# Roadmap\n\nBody.",
    );
    let source_dir = fixture.root.join("Folder/Roadmap");
    let destination = fixture.root.join("Folder/Renamed");

    run_mv(
        &mut store,
        MvOptions {
            source: source_dir,
            destination: destination.clone(),
            state_root: None,
        },
    )
    .expect("move");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status");
    assert!(!status.clean, "{status:#?}");
    assert_eq!(
        entry_state(&status, "Folder/Roadmap/page.md"),
        StatusState::Missing
    );

    let diff = run_diff(&store, destination.join("page.md")).expect("diff destination");
    assert!(!diff.ok, "{diff:#?}");
}

#[test]
fn mv_virtual_page_directory_rename_records_move_and_status_issue() {
    let fixture = MvFixture::new("loc-mv-virtual-rename");
    let state_root = fixture.temp.path("state");
    let mut store = fixture.store(ProjectionMode::LinuxFuse, false);
    seed_page(
        &mut store,
        "page-home",
        "Home",
        "Home/page.md",
        HydrationState::Hydrated,
    );
    seed_page(
        &mut store,
        "page-child",
        "Child",
        "Home/Child/page.md",
        HydrationState::Hydrated,
    );
    write_cache(
        &state_root,
        "Home/Child/page.md",
        "---\nloc:\n  id: page-child\n  type: page\ntitle: \"Child\"\n---\nBody",
    );

    let report = run_mv(
        &mut store,
        MvOptions {
            source: fixture.root.join("Home/Child"),
            destination: fixture.root.join("Home/Renamed Child"),
            state_root: Some(state_root.clone()),
        },
    )
    .expect("virtual rename");

    assert_eq!(report.mode, "virtual_fs");
    assert_eq!(
        report.item_identifier.as_deref(),
        Some("children:page-child")
    );
    assert!(!cache_path(&state_root, "Home/Child/page.md").exists());
    let renamed_cache = fs::read_to_string(cache_path(&state_root, "Home/Renamed Child/page.md"))
        .expect("renamed cache");
    assert!(renamed_cache.contains("title: \"Renamed Child\""));
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-child"))
        .expect("entity")
        .expect("entity");
    assert_eq!(entity.path, PathBuf::from("Home/Renamed Child/page.md"));
    assert_eq!(entity.title, "Renamed Child");
    assert_eq!(entity.hydration, HydrationState::Dirty);
    let mutation = store
        .get_virtual_mutation(&fixture.mount_id, "move:page-child")
        .expect("mutation")
        .expect("mutation");
    assert_eq!(mutation.mutation_kind, VirtualMutationKind::Move);

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.join("Home/Renamed Child")),
            state_root: Some(state_root),
            ..StatusOptions::default()
        },
    )
    .expect("status");
    assert_eq!(
        entry_issue(&status, "Home/Renamed Child/page.md"),
        "pending_virtual_rename"
    );
}

#[test]
fn mv_virtual_page_directory_move_records_new_parent_remote_id() {
    let fixture = MvFixture::new("loc-mv-virtual-move-parent");
    let state_root = fixture.temp.path("state");
    let mut store = fixture.store(ProjectionMode::LinuxFuse, false);
    seed_page(
        &mut store,
        "page-home",
        "Home",
        "Home/page.md",
        HydrationState::Hydrated,
    );
    seed_page(
        &mut store,
        "page-archive",
        "Archive",
        "Archive/page.md",
        HydrationState::Hydrated,
    );
    seed_page(
        &mut store,
        "page-child",
        "Child",
        "Home/Child/page.md",
        HydrationState::Hydrated,
    );
    write_cache(
        &state_root,
        "Home/Child/page.md",
        "---\ntitle: \"Child\"\n---\nBody",
    );

    run_mv(
        &mut store,
        MvOptions {
            source: fixture.root.join("Home/Child"),
            destination: fixture.root.join("Archive/Moved Child"),
            state_root: Some(state_root),
        },
    )
    .expect("virtual move");

    let mutation = store
        .get_virtual_mutation(&fixture.mount_id, "move:page-child")
        .expect("mutation")
        .expect("mutation");
    assert_eq!(
        mutation.parent_remote_id.as_ref().map(RemoteId::as_str),
        Some("page-archive")
    );
    assert_eq!(
        mutation.projected_path,
        PathBuf::from("Archive/Moved Child/page.md")
    );
}

#[test]
fn mv_virtual_pending_created_page_directory_can_be_moved() {
    let fixture = MvFixture::new("loc-mv-virtual-pending");
    let state_root = fixture.temp.path("state");
    let mut store = fixture.store(ProjectionMode::LinuxFuse, false);
    seed_page(
        &mut store,
        "page-home",
        "Home",
        "Home/page.md",
        HydrationState::Hydrated,
    );
    seed_pending_page(
        &mut store,
        "local:draft",
        "Home/Draft/page.md",
        "Draft",
        Some("page-home"),
    );
    write_cache(
        &state_root,
        "Home/Draft/page.md",
        "---\ntitle: \"Draft\"\n---\nBody",
    );

    run_mv(
        &mut store,
        MvOptions {
            source: fixture.root.join("Home/Draft"),
            destination: fixture.root.join("Home/Published"),
            state_root: Some(state_root.clone()),
        },
    )
    .expect("move pending page");

    assert!(!cache_path(&state_root, "Home/Draft/page.md").exists());
    assert!(cache_path(&state_root, "Home/Published/page.md").exists());
    let mutation = store
        .get_virtual_mutation(&fixture.mount_id, "local:draft")
        .expect("mutation")
        .expect("mutation");
    assert_eq!(mutation.mutation_kind, VirtualMutationKind::Create);
    assert_eq!(
        mutation.projected_path,
        PathBuf::from("Home/Published/page.md")
    );
    assert_eq!(mutation.title, "Published");
}

#[test]
fn mv_virtual_collision_leaves_store_and_cache_unchanged() {
    let fixture = MvFixture::new("loc-mv-virtual-collision");
    let state_root = fixture.temp.path("state");
    let mut store = fixture.store(ProjectionMode::LinuxFuse, false);
    seed_page(
        &mut store,
        "page-home",
        "Home",
        "Home/page.md",
        HydrationState::Hydrated,
    );
    seed_page(
        &mut store,
        "page-child",
        "Child",
        "Home/Child/page.md",
        HydrationState::Hydrated,
    );
    write_cache(
        &state_root,
        "Home/Child/page.md",
        "---\ntitle: \"Child\"\n---\nBody",
    );
    let before = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-child"))
        .expect("entity")
        .expect("entity");

    let error = run_mv(
        &mut store,
        MvOptions {
            source: fixture.root.join("Home/Child"),
            destination: fixture.root.join("Home"),
            state_root: Some(state_root.clone()),
        },
    )
    .expect_err("collision rejected");

    assert!(matches!(error, MvError::DestinationExists(path) if path.ends_with("Home/Child")));
    assert_eq!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-child"))
            .expect("entity"),
        Some(before)
    );
    assert!(cache_path(&state_root, "Home/Child/page.md").exists());
}

struct MvFixture {
    temp: TestTempDir,
    root: PathBuf,
    mount_id: MountId,
}

impl MvFixture {
    fn new(name: &str) -> Self {
        let temp = TestTempDir::new(name);
        let root = temp.path("notion");
        fs::create_dir_all(&root).expect("mount root");
        Self {
            temp,
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn mount(&self, projection: ProjectionMode, read_only: bool) -> MountConfig {
        MountConfig {
            mount_id: self.mount_id.clone(),
            connector: "notion".to_string(),
            root: self.root.clone(),
            remote_root_id: Some(RemoteId::new("root-page")),
            connection_id: None,
            read_only,
            projection,
            settings_json: "{}".to_string(),
        }
    }

    fn store(&self, projection: ProjectionMode, read_only: bool) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(self.mount(projection, read_only))
            .expect("save mount");
        store
    }

    fn write(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(&path, contents).expect("write");
        path
    }
}

struct TestTempDir {
    root: PathBuf,
}

impl TestTempDir {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "{prefix}-{}-{now}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("temp root");
        Self { root }
    }

    fn path(&self, child: &str) -> PathBuf {
        self.root.join(child)
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn seed_page(
    store: &mut InMemoryStateStore,
    remote_id: &str,
    title: &str,
    path: &str,
    hydration: HydrationState,
) {
    store
        .save_entity(
            EntityRecord::new(
                MountId::new("notion-main"),
                RemoteId::new(remote_id),
                EntityKind::Page,
                title,
                path,
            )
            .with_hydration(hydration),
        )
        .expect("save entity");
}

fn seed_pending_page(
    store: &mut InMemoryStateStore,
    local_id: &str,
    path: &str,
    title: &str,
    parent_remote_id: Option<&str>,
) {
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: MountId::new("notion-main"),
            local_id: local_id.to_string(),
            mutation_kind: VirtualMutationKind::Create,
            target_remote_id: None,
            parent_remote_id: parent_remote_id.map(RemoteId::new),
            original_path: None,
            projected_path: PathBuf::from(path),
            title: title.to_string(),
            content_path: None,
            created_at: "1".to_string(),
            updated_at: "1".to_string(),
        })
        .expect("save mutation");
}

fn write_cache(state_root: &Path, relative_path: &str, contents: &str) {
    let path = cache_path(state_root, relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("cache parent");
    }
    fs::write(path, contents).expect("cache");
}

fn cache_path(state_root: &Path, relative_path: &str) -> PathBuf {
    virtual_fs_content_root(state_root, &MountId::new("notion-main")).join(relative_path)
}

fn entry_state(report: &loc_cli::status::StatusReport, path: &str) -> StatusState {
    report
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("missing status entry {path}: {report:#?}"))
        .state
        .clone()
}

fn entry_issue(report: &loc_cli::status::StatusReport, path: &str) -> String {
    report
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .find(|entry| entry.path == path)
        .and_then(|entry| entry.issues.first())
        .map(|issue| issue.code.clone())
        .unwrap_or_else(|| panic!("missing status issue {path}: {report:#?}"))
}
