use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::create::{CreateError, CreatePageOptions, run_create_page};
use locality_core::model::{MountId, RemoteId};
use locality_store::{InMemoryStateStore, MountConfig, MountRepository};

#[test]
fn create_page_writes_page_directory_from_title() {
    let fixture = CreateFixture::new("loc-create-page");
    let store = fixture.store(false);
    let parent = fixture.root.join("go-to-market");
    fs::create_dir_all(&parent).expect("parent");

    let report = run_create_page(
        &store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(parent.clone()),
        },
    )
    .expect("create page");

    let page_path = parent.join("Launch Plan/page.md");
    assert_eq!(report.command, "create_page");
    assert_eq!(report.kind, "page");
    assert_eq!(report.title, "Launch Plan");
    assert_eq!(Path::new(&report.path), page_path);
    assert_eq!(report.mount_id, "notion-main");
    assert_eq!(
        fs::read_to_string(page_path).expect("page"),
        "---\ntitle: \"Launch Plan\"\n---\n"
    );
    assert!(
        report
            .next
            .iter()
            .any(|next| next.contains("loc diff") && next.contains("page.md"))
    );
}

#[test]
fn create_page_escapes_yaml_title() {
    let fixture = CreateFixture::new("loc-create-page-escaping");
    let store = fixture.store(false);

    let report = run_create_page(
        &store,
        CreatePageOptions {
            title: "Quote \"Plan\"".to_string(),
            parent: Some(fixture.root.clone()),
        },
    )
    .expect("create page");

    let page_path = fixture.root.join("Quote -Plan-/page.md");
    assert_eq!(Path::new(&report.path), page_path);
    assert_eq!(
        fs::read_to_string(page_path).expect("page"),
        "---\ntitle: \"Quote \\\"Plan\\\"\"\n---\n"
    );
}

#[test]
fn create_page_refuses_existing_page_directory() {
    let fixture = CreateFixture::new("loc-create-page-existing");
    let store = fixture.store(false);
    fs::create_dir_all(fixture.root.join("Launch Plan")).expect("existing");

    let error = run_create_page(
        &store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(fixture.root.clone()),
        },
    )
    .expect_err("existing target");

    assert!(matches!(error, CreateError::TargetExists(path) if path.ends_with("Launch Plan")));
}

#[test]
fn create_page_requires_parent_inside_mount() {
    let fixture = CreateFixture::new("loc-create-page-outside");
    let store = fixture.store(false);
    let outside = fixture.temp.path("outside");
    fs::create_dir_all(&outside).expect("outside");

    let error = run_create_page(
        &store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(outside.clone()),
        },
    )
    .expect_err("outside mount");

    assert!(matches!(error, CreateError::MountNotFound(path) if path == outside));
}

#[test]
fn create_page_refuses_read_only_mount() {
    let fixture = CreateFixture::new("loc-create-page-read-only");
    let store = fixture.store(true);

    let error = run_create_page(
        &store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(fixture.root.clone()),
        },
    )
    .expect_err("read only");

    assert!(matches!(error, CreateError::ReadOnlyMount { mount_id } if mount_id == "notion-main"));
}

#[test]
fn create_page_rejects_titles_that_are_paths() {
    let fixture = CreateFixture::new("loc-create-page-invalid-title");
    let store = fixture.store(false);

    let error = run_create_page(
        &store,
        CreatePageOptions {
            title: "Parent/Child".to_string(),
            parent: Some(fixture.root.clone()),
        },
    )
    .expect_err("invalid title");

    assert!(matches!(error, CreateError::InvalidTitle(_)));
}

struct CreateFixture {
    temp: TestTempDir,
    root: PathBuf,
}

impl CreateFixture {
    fn new(name: &str) -> Self {
        let temp = TestTempDir::new(name);
        let root = temp.path("notion");
        fs::create_dir_all(&root).expect("mount root");
        Self { temp, root }
    }

    fn store(&self, read_only: bool) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig {
                mount_id: MountId::new("notion-main"),
                connector: "notion".to_string(),
                root: self.root.clone(),
                remote_root_id: Some(RemoteId::new("root-page")),
                connection_id: None,
                read_only,
                projection: locality_store::ProjectionMode::PlainFiles,
            })
            .expect("save mount");
        store
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
