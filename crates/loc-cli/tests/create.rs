use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::create::{
    CreateDatabaseOptions, CreateError, CreateGmailReplyOptions, CreatePageOptions,
    run_create_database, run_create_gmail_reply, run_create_page,
};
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, SqliteStateStore, VirtualMutationRepository,
};
use serde_json::Value;

#[test]
fn create_page_writes_page_directory_from_title() {
    let fixture = CreateFixture::new("loc-create-page");
    let mut store = fixture.store(false);
    let parent = fixture.root.join("go-to-market");
    fs::create_dir_all(&parent).expect("parent");

    let report = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(parent.clone()),
            private: false,
            state_root: None,
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
fn create_database_writes_editable_schema_draft_inside_notion_page() {
    let fixture = CreateFixture::new("loc-create-database");
    let mut store = fixture.store(false);
    seed_parent_page(&mut store, "parent-page", "Roadmap", "Roadmap/page.md");
    let parent = fixture.root.join("Roadmap");
    fs::create_dir_all(&parent).expect("parent");

    let report = run_create_database(
        &mut store,
        CreateDatabaseOptions {
            title: "Project Tasks".to_string(),
            parent: Some(parent),
            state_root: None,
        },
    )
    .expect("create database draft");

    let schema_path = fixture.root.join("Roadmap/Project Tasks/_schema.yaml");
    assert_eq!(report.command, "create_database");
    assert_eq!(report.kind, "database");
    assert_eq!(Path::new(&report.path), schema_path);
    assert_eq!(
        fs::read_to_string(schema_path).expect("schema"),
        "loc:\n  type: notion_database_schema\ntitle: \"Project Tasks\"\ndata_sources:\n  - name: Rows\n    properties:\n      Name:\n        type: title\n"
    );
}

#[test]
fn create_database_in_virtual_mount_stages_schema_and_parent_identity() {
    let fixture = CreateFixture::new("loc-create-database-virtual");
    let mut store = fixture.store_with_projection(ProjectionMode::MacosFileProvider, false);
    seed_parent_page(&mut store, "parent-page", "Roadmap", "Roadmap/page.md");
    fs::create_dir_all(fixture.root.join("Roadmap")).expect("visible parent");
    let state_root = fixture.temp.path("state");

    let report = run_create_database(
        &mut store,
        CreateDatabaseOptions {
            title: "Project Tasks".to_string(),
            parent: Some(fixture.root.join("Roadmap")),
            state_root: Some(state_root),
        },
    )
    .expect("stage database draft");

    let projected_path = PathBuf::from("Roadmap/Project Tasks/_schema.yaml");
    assert_eq!(Path::new(&report.path), fixture.root.join(&projected_path));
    assert!(!fixture.root.join("Roadmap/Project Tasks").exists());
    let mutation = store
        .find_virtual_mutation_by_path(&MountId::new("notion-main"), &projected_path)
        .expect("find mutation")
        .expect("database mutation");
    assert_eq!(
        mutation.parent_remote_id,
        Some(RemoteId::new("parent-page"))
    );
    assert_eq!(mutation.projected_path, projected_path);
    assert!(
        fs::read_to_string(mutation.content_path.expect("content"))
            .expect("schema")
            .contains("type: notion_database_schema")
    );
}

#[test]
fn create_page_private_writes_notion_private_marker() {
    let fixture = CreateFixture::new("loc-create-page-private");
    let mut store = fixture.store(false);

    let report = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Private Draft".to_string(),
            parent: Some(fixture.root.clone()),
            private: true,
            state_root: None,
        },
    )
    .expect("create private page");

    let page_path = fixture.root.join("Private Draft/page.md");
    assert!(report.private);
    assert_eq!(Path::new(&report.path), page_path);
    assert_eq!(
        fs::read_to_string(page_path).expect("page"),
        "---\nloc:\n  private: true\ntitle: \"Private Draft\"\n---\n"
    );
}

#[test]
fn create_page_private_requires_notion_mount() {
    let fixture = CreateFixture::new("loc-create-page-private-non-notion");
    let mut store = fixture.store_with_connector("google-docs", false);

    let error = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Private Draft".to_string(),
            parent: Some(fixture.root.clone()),
            private: true,
            state_root: None,
        },
    )
    .expect_err("private create is notion-only");

    assert!(
        matches!(error, CreateError::PrivateUnsupported { connector } if connector == "google-docs")
    );
}

#[test]
fn cli_create_page_private_flag_writes_marker() {
    let fixture = CreateFixture::new("loc-create-page-private-cli");
    let state_root = fixture.temp.path("state");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("sqlite");
    store
        .save_mount(MountConfig {
            mount_id: MountId::new("notion-main"),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("root-page")),
            connection_id: None,
            read_only: false,
            projection: locality_store::ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        })
        .expect("save mount");

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .env("LOCALITY_STATE_DIR", &state_root)
        .args([
            "create",
            "page",
            "--title",
            "Private CLI Draft",
            "--parent",
            fixture.root.to_str().expect("root path"),
            "--private",
            "--json",
        ])
        .output()
        .expect("loc create");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(report["private"], true);
    assert_eq!(
        fs::read_to_string(fixture.root.join("Private CLI Draft/page.md")).expect("page"),
        "---\nloc:\n  private: true\ntitle: \"Private CLI Draft\"\n---\n"
    );
}

#[test]
fn create_page_in_virtual_page_directory_stages_pending_create_without_visible_write() {
    let fixture = CreateFixture::new("loc-create-page-virtual-page-dir");
    let mut store = fixture.store_with_projection(ProjectionMode::MacosFileProvider, false);
    seed_parent_page(&mut store, "parent-page", "Launch", "Launch/page.md");
    fs::create_dir_all(fixture.root.join("Launch")).expect("visible parent directory");
    let state_root = fixture.temp.path("state");

    let report = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(fixture.root.join("Launch")),
            private: false,
            state_root: Some(state_root.clone()),
        },
    )
    .expect("create virtual page");

    let projected_path = PathBuf::from("Launch/Launch Plan/page.md");
    assert_eq!(
        Path::new(&report.path),
        fixture.root.join(&projected_path).as_path()
    );
    assert!(
        !fixture.root.join("Launch/Launch Plan").exists(),
        "virtual creates must stage content without writing into the visible projection"
    );
    let mutation = store
        .find_virtual_mutation_by_path(&MountId::new("notion-main"), &projected_path)
        .expect("find mutation")
        .expect("pending create mutation");
    assert_eq!(
        mutation.parent_remote_id,
        Some(RemoteId::new("parent-page"))
    );
    assert_eq!(mutation.projected_path, projected_path);
    assert_eq!(
        fs::read_to_string(mutation.content_path.expect("content path")).expect("content"),
        "---\ntitle: \"Launch Plan\"\n---\n"
    );
}

#[test]
fn create_page_at_virtual_mount_root_requires_parent_for_remote_create() {
    let fixture = CreateFixture::new("loc-create-page-virtual-root-parent");
    let mut store = fixture.store_with_projection(ProjectionMode::MacosFileProvider, false);
    let state_root = fixture.temp.path("state");

    let error = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(fixture.root.clone()),
            private: false,
            state_root: Some(state_root),
        },
    )
    .expect_err("mount root is not a Notion page or database parent");

    assert!(
        matches!(error, CreateError::InvalidParent { ref path, .. } if path == &fixture.root),
        "{error:#?}"
    );
}

#[test]
fn create_page_private_at_virtual_mount_root_stages_pending_create() {
    let fixture = CreateFixture::new("loc-create-page-private-virtual-root");
    let mut store = fixture.store_with_projection(ProjectionMode::LinuxFuse, false);
    let state_root = fixture.temp.path("state");

    let report = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Private Root Draft".to_string(),
            parent: Some(fixture.root.clone()),
            private: true,
            state_root: Some(state_root.clone()),
        },
    )
    .expect("create private virtual root page");

    let projected_path = PathBuf::from("Private Root Draft/page.md");
    assert_eq!(
        Path::new(&report.path),
        fixture.root.join(&projected_path).as_path()
    );
    let mutation = store
        .find_virtual_mutation_by_path(&MountId::new("notion-main"), &projected_path)
        .expect("find mutation")
        .expect("pending create mutation");
    assert_eq!(mutation.parent_remote_id, None);
    assert_eq!(mutation.projected_path, projected_path);
    assert_eq!(
        fs::read_to_string(mutation.content_path.expect("content path")).expect("content"),
        "---\nloc:\n  private: true\ntitle: \"Private Root Draft\"\n---\n"
    );
}

#[test]
fn create_page_escapes_yaml_title() {
    let fixture = CreateFixture::new("loc-create-page-escaping");
    let mut store = fixture.store(false);

    let report = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Quote \"Plan\"".to_string(),
            parent: Some(fixture.root.clone()),
            private: false,
            state_root: None,
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
    let mut store = fixture.store(false);
    fs::create_dir_all(fixture.root.join("Launch Plan")).expect("existing");

    let error = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(fixture.root.clone()),
            private: false,
            state_root: None,
        },
    )
    .expect_err("existing target");

    assert!(matches!(error, CreateError::TargetExists(path) if path.ends_with("Launch Plan")));
}

#[test]
fn create_page_requires_parent_inside_mount() {
    let fixture = CreateFixture::new("loc-create-page-outside");
    let mut store = fixture.store(false);
    let outside = fixture.temp.path("outside");
    fs::create_dir_all(&outside).expect("outside");

    let error = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(outside.clone()),
            private: false,
            state_root: None,
        },
    )
    .expect_err("outside mount");

    assert!(matches!(error, CreateError::MountNotFound(path) if path == outside));
}

#[test]
fn create_page_refuses_read_only_mount() {
    let fixture = CreateFixture::new("loc-create-page-read-only");
    let mut store = fixture.store(true);

    let error = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(fixture.root.clone()),
            private: false,
            state_root: None,
        },
    )
    .expect_err("read only");

    assert!(matches!(error, CreateError::ReadOnlyMount { mount_id } if mount_id == "notion-main"));
}

#[test]
fn create_page_refuses_slack_parent_even_when_mount_flag_allows_writes() {
    let fixture = CreateFixture::new("loc-create-page-slack-read-only");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(MountConfig {
            mount_id: MountId::new("slack-main"),
            connector: "slack".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("slack-root")),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        })
        .expect("save Slack mount");

    let error = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Launch Plan".to_string(),
            parent: Some(fixture.root.clone()),
            private: false,
            state_root: None,
        },
    )
    .expect_err("Slack creates must be blocked by source policy");

    assert_eq!(error.code(), "read_only_source");
    assert_eq!(
        error.message(),
        "Slack mount `slack-main` cannot accept new pages: Slack conversations are read-only"
    );
    assert!(
        !fixture.root.join("Launch Plan").exists(),
        "blocked Slack create must not write a local draft"
    );
}

#[test]
fn create_page_rejects_titles_that_are_paths() {
    let fixture = CreateFixture::new("loc-create-page-invalid-title");
    let mut store = fixture.store(false);

    let error = run_create_page(
        &mut store,
        CreatePageOptions {
            title: "Parent/Child".to_string(),
            parent: Some(fixture.root.clone()),
            private: false,
            state_root: None,
        },
    )
    .expect_err("invalid title");

    assert!(matches!(error, CreateError::InvalidTitle(_)));
}

#[test]
fn create_gmail_reply_writes_a_threaded_draft_from_the_selected_message() {
    let fixture = CreateFixture::new("loc-create-gmail-reply");
    let mut store = fixture.gmail_store(ProjectionMode::PlainFiles, false);
    let thread = fixture.root.join("inbox/Quarterly update");
    fs::create_dir_all(&thread).expect("thread directory");
    fs::write(
        thread.join("page.md"),
        "---\ntitle: Quarterly update\n---\n",
    )
    .expect("thread page");
    fs::write(
        thread.join("2026-07-02-update.md"),
        "---\nfrom: \"Earlier <earlier@example.com>\"\nsubject: \"Quarterly update\"\ngmail:\n  message_id: \"message-1\"\n  thread_id: \"thread-1\"\n  rfc_message_id: \"<message-1@example.com>\"\n  references:\n    - \"<root@example.com>\"\n---\nEarlier message\n",
    )
    .expect("first message");
    fs::write(
        thread.join("2026-07-03-reply.md"),
        "---\nfrom: \"Latest <latest@example.com>\"\nsubject: \"Quarterly update\"\ngmail:\n  message_id: \"message-2\"\n  thread_id: \"thread-1\"\n  rfc_message_id: \"<message-2@example.com>\"\n  reply_to: \"reply@example.com\"\n  references:\n    - \"<root@example.com>\"\n---\nLatest message\n",
    )
    .expect("latest message");

    let report = run_create_gmail_reply(
        &mut store,
        CreateGmailReplyOptions {
            thread: thread.clone(),
            message: Some("2026-07-02-update.md".to_string()),
            state_root: None,
        },
    )
    .expect("create Gmail reply");

    assert_eq!(report.command, "create_gmail_reply");
    assert_eq!(report.thread_id, "thread-1");
    assert_eq!(report.reply_to_message_id, "message-1");
    assert_eq!(report.recipient, "Earlier <earlier@example.com>");
    assert_eq!(report.subject, "Quarterly update");
    let draft = fs::read_to_string(&report.path).expect("reply draft");
    assert_eq!(
        draft,
        "---\nto: \"Earlier <earlier@example.com>\"\nsubject: \"Quarterly update\"\ngmail:\n  thread_id: \"thread-1\"\n  reply_to_message_id: \"message-1\"\n  in_reply_to: \"<message-1@example.com>\"\n  references:\n    - \"<root@example.com>\"\n    - \"<message-1@example.com>\"\n---\n\n"
    );
    assert!(Path::new(&report.path).starts_with(fixture.root.join("draft")));
}

#[test]
fn create_gmail_reply_selects_a_message_by_gmail_message_id() {
    let fixture = CreateFixture::new("loc-create-gmail-reply-message-id");
    let mut store = fixture.gmail_store(ProjectionMode::PlainFiles, false);
    let thread = fixture.root.join("inbox/Quarterly update");
    fs::create_dir_all(&thread).expect("thread directory");
    fs::write(
        thread.join("2026-07-02-update.md"),
        "---\nfrom: \"Earlier <earlier@example.com>\"\nsubject: \"Quarterly update\"\ngmail:\n  message_id: \"gmail-message-1\"\n  thread_id: \"thread-1\"\n  rfc_message_id: \"<message-1@example.com>\"\n---\nEarlier message\n",
    )
    .expect("first message");
    fs::write(
        thread.join("2026-07-03-update.md"),
        "---\nfrom: \"Later <later@example.com>\"\nsubject: \"Quarterly update\"\ngmail:\n  message_id: \"gmail-message-2\"\n  thread_id: \"thread-1\"\n  rfc_message_id: \"<message-2@example.com>\"\n---\nLater message\n",
    )
    .expect("second message");

    let report = run_create_gmail_reply(
        &mut store,
        CreateGmailReplyOptions {
            thread,
            message: Some("gmail-message-1".to_string()),
            state_root: None,
        },
    )
    .expect("create reply from Gmail message ID");

    assert_eq!(report.reply_to_message_id, "gmail-message-1");
    assert_eq!(report.recipient, "Earlier <earlier@example.com>");
}

#[test]
fn create_gmail_reply_uses_unique_bounded_draft_filenames() {
    let fixture = CreateFixture::new("loc-create-gmail-reply-filenames");
    let mut store = fixture.gmail_store(ProjectionMode::PlainFiles, false);
    let thread = fixture.root.join("inbox/Long subject");
    fs::create_dir_all(&thread).expect("thread directory");
    let subject = "📬 A very long subject ".repeat(20);
    fs::write(
        thread.join("message.md"),
        format!(
            "---\nfrom: \"sender@example.com\"\nsubject: {subject:?}\ngmail:\n  message_id: \"message-1\"\n  thread_id: \"thread-1\"\n  rfc_message_id: \"<message-1@example.com>\"\n---\nMessage\n"
        ),
    )
    .expect("message");

    let first = run_create_gmail_reply(
        &mut store,
        CreateGmailReplyOptions {
            thread: thread.clone(),
            message: None,
            state_root: None,
        },
    )
    .expect("first reply");
    let second = run_create_gmail_reply(
        &mut store,
        CreateGmailReplyOptions {
            thread,
            message: None,
            state_root: None,
        },
    )
    .expect("second reply");

    assert_ne!(first.path, second.path);
    for path in [&first.path, &second.path] {
        let filename = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .expect("draft filename");
        assert!(
            filename.len() <= 128,
            "filename was not bounded: {filename}"
        );
        assert!(
            filename.starts_with("reply-"),
            "unexpected filename: {filename}"
        );
    }
}

#[test]
fn cli_create_gmail_reply_defaults_to_the_latest_thread_message() {
    let fixture = CreateFixture::new("loc-create-gmail-reply-cli");
    let state_root = fixture.temp.path("state");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("sqlite");
    store
        .save_mount(MountConfig {
            mount_id: MountId::new("gmail-main"),
            connector: "gmail".to_string(),
            root: fixture.root.clone(),
            remote_root_id: None,
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        })
        .expect("save Gmail mount");
    let thread = fixture.root.join("inbox/Quarterly update");
    fs::create_dir_all(&thread).expect("thread directory");
    fs::write(
        thread.join("2026-07-03-latest.md"),
        "---\nfrom: \"Latest <latest@example.com>\"\nsubject: \"Quarterly update\"\ngmail:\n  message_id: \"message-2\"\n  thread_id: \"thread-1\"\n  rfc_message_id: \"<message-2@example.com>\"\n---\nLatest message\n",
    )
    .expect("message");

    let output = Command::new(env!("CARGO_BIN_EXE_loc"))
        .env("LOCALITY_STATE_DIR", &state_root)
        .args([
            "create",
            "gmail-reply",
            thread.to_str().expect("thread path"),
            "--json",
        ])
        .output()
        .expect("loc create gmail-reply");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    assert_eq!(report["command"], "create_gmail_reply");
    assert_eq!(report["thread_id"], "thread-1");
    assert_eq!(report["reply_to_message_id"], "message-2");
    assert_eq!(report["recipient"], "Latest <latest@example.com>");
    let draft = fs::read_to_string(report["path"].as_str().expect("draft path")).expect("draft");
    assert!(
        draft.contains("in_reply_to: \"<message-2@example.com>\""),
        "{draft}"
    );
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
        self.store_with_connector("notion", read_only)
    }

    fn store_with_connector(&self, connector: &str, read_only: bool) -> InMemoryStateStore {
        self.store_with_connector_and_projection(connector, ProjectionMode::PlainFiles, read_only)
    }

    fn store_with_projection(
        &self,
        projection: ProjectionMode,
        read_only: bool,
    ) -> InMemoryStateStore {
        self.store_with_connector_and_projection("notion", projection, read_only)
    }

    fn store_with_connector_and_projection(
        &self,
        connector: &str,
        projection: ProjectionMode,
        read_only: bool,
    ) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig {
                mount_id: MountId::new("notion-main"),
                connector: connector.to_string(),
                root: self.root.clone(),
                remote_root_id: Some(RemoteId::new("root-page")),
                connection_id: None,
                read_only,
                projection,
                settings_json: "{}".to_string(),
            })
            .expect("save mount");
        store
    }

    fn gmail_store(&self, projection: ProjectionMode, read_only: bool) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig {
                mount_id: MountId::new("gmail-main"),
                connector: "gmail".to_string(),
                root: self.root.clone(),
                remote_root_id: None,
                connection_id: None,
                read_only,
                projection,
                settings_json: "{}".to_string(),
            })
            .expect("save Gmail mount");
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

fn seed_parent_page(store: &mut InMemoryStateStore, remote_id: &str, title: &str, path: &str) {
    store
        .save_entity(EntityRecord::new(
            MountId::new("notion-main"),
            RemoteId::new(remote_id),
            EntityKind::Page,
            title,
            path,
        ))
        .expect("save parent page entity");
}
