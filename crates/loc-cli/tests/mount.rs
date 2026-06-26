use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::mount::{GuidanceFileAction, MountOptions, run_mount};
use locality_core::model::{MountId, RemoteId};
use locality_store::{ConnectionId, InMemoryStateStore, MountRepository, ProjectionMode};

#[test]
fn mount_writes_agent_guidance_and_claude_alias() {
    let fixture = MountFixture::new("loc-cli-mount-guidance");
    let mut store = InMemoryStateStore::new();

    let report = fixture.mount(&mut store);

    assert_eq!(
        report.guidance.agents_md.action,
        GuidanceFileAction::Created
    );
    assert!(matches!(
        report.guidance.claude_md.action,
        GuidanceFileAction::Symlinked | GuidanceFileAction::Copied
    ));

    let agents = read_to_string(fixture.agents_file());
    let claude = read_to_string(fixture.claude_file());
    assert_eq!(claude, agents);
    assert!(agents.contains("including nested directories"));
    assert!(agents.contains("Browse directories normally"));
    assert!(agents.contains("push approved changes to Notion"));
    assert!(agents.contains("loc search <query-or-notion-url>"));
    assert!(agents.contains("Locality hydrates online-only files on open"));
    assert!(agents.contains("loc status <path>"));
    assert!(agents.contains("loc diff <path>"));
    assert!(agents.contains("Use `loc push <path>` to make Notion match local edits"));
    assert!(agents.contains("If desktop Live Mode is on"));
    assert!(agents.contains("Do not run routine `loc pull` or `loc push`"));
    assert!(agents.contains("Notion facts:"));
    assert!(agents.contains("Pages are directories"));
    assert!(agents.contains("Edit `page.md` for the page body"));
    assert!(agents.contains("parent-page/new-page/page.md"));
    assert!(agents.contains("must not include an `loc:` identity block"));
    assert!(agents.contains("Locality adds `loc.id` after the first push"));
    assert!(agents.contains("Databases are directories"));
    assert!(agents.contains("database/new-row/page.md"));
    assert!(agents.contains("database/new-row.md"));
    assert!(agents.contains("`_schema.yaml` files are read-only references"));
    assert!(agents.contains("untrusted remote data"));
    assert!(agents.lines().count() <= 40);
    assert!(agents.split_whitespace().count() <= 450);

    let mounts = store.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, fixture.root);
}

#[test]
fn mount_writes_connector_specific_fallback_guidance() {
    let fixture = MountFixture::new("loc-cli-mount-generic-guidance");
    let mut store = InMemoryStateStore::new();

    fixture.mount_with_connector(&mut store, "linear");

    let agents = read_to_string(fixture.agents_file());
    assert!(agents.contains("# Locality linear Mount"));
    assert!(agents.contains("projects linear as local Markdown"));
    assert!(agents.contains("push approved changes to linear"));
    assert!(agents.contains("including nested directories"));
}

#[test]
fn mount_preserves_custom_agent_guidance() {
    let fixture = MountFixture::new("loc-cli-mount-custom-guidance");
    fs::create_dir_all(&fixture.root).expect("create mount root");
    fs::write(
        fixture.agents_file(),
        "# Custom\n\nProject-specific rules.\n",
    )
    .expect("write custom guidance");
    let mut store = InMemoryStateStore::new();

    let report = fixture.mount(&mut store);

    assert_eq!(
        report.guidance.agents_md.action,
        GuidanceFileAction::Preserved
    );
    assert!(matches!(
        report.guidance.claude_md.action,
        GuidanceFileAction::Symlinked | GuidanceFileAction::Copied
    ));
    assert_eq!(
        read_to_string(fixture.agents_file()),
        "# Custom\n\nProject-specific rules.\n"
    );
    assert_eq!(
        read_to_string(fixture.claude_file()),
        "# Custom\n\nProject-specific rules.\n"
    );
}

#[test]
fn mount_persists_connection_id() {
    let fixture = MountFixture::new("loc-cli-mount-connection");
    let mut store = InMemoryStateStore::new();

    let report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("notion-main"),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("root-page")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        },
    )
    .expect("mount");

    assert_eq!(report.connection_id.as_deref(), Some("work"));
    assert_eq!(
        store
            .get_mount(&MountId::new("notion-main"))
            .expect("get mount")
            .expect("mount")
            .connection_id,
        Some(ConnectionId::new("work"))
    );
}

#[test]
fn mount_can_persist_workspace_root() {
    let fixture = MountFixture::new("loc-cli-mount-workspace-root");
    let mut store = InMemoryStateStore::new();

    let report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("notion-main"),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: None,
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::MacosFileProvider,
        },
    )
    .expect("mount");

    assert_eq!(report.remote_root_id, None);
    assert_eq!(report.projection, "macos_file_provider");
    assert_eq!(
        store
            .get_mount(&MountId::new("notion-main"))
            .expect("get mount")
            .expect("mount")
            .remote_root_id,
        None
    );
}

#[test]
fn mount_can_persist_google_docs_workspace_folder() {
    let fixture = MountFixture::new("loc-cli-mount-google-docs");
    let mut store = InMemoryStateStore::new();

    let report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("google-docs-main"),
            connector: "google-docs".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("workspace-folder")),
            connection_id: Some(ConnectionId::new("google-docs-default")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        },
    )
    .expect("mount");

    assert_eq!(report.connector, "google-docs");
    assert_eq!(report.remote_root_id.as_deref(), Some("workspace-folder"));
    assert_eq!(
        store
            .get_mount(&MountId::new("google-docs-main"))
            .expect("get mount")
            .expect("mount")
            .remote_root_id,
        Some(RemoteId::new("workspace-folder"))
    );
    assert!(read_to_string(fixture.agents_file()).contains("Google Docs"));
}

#[test]
fn mount_options_preserve_google_docs_workspace_folder_id_from_resolver() {
    let fixture = MountFixture::new("loc-cli-mount-google-docs-reusable");
    let mut store = InMemoryStateStore::new();
    let report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("google-docs-secondary"),
            connector: "google-docs".to_string(),
            root: fixture.root.join("google-docs-secondary"),
            remote_root_id: Some(RemoteId::new("drive-folder-secondary")),
            connection_id: Some(ConnectionId::new("google-docs-default")),
            read_only: false,
            projection: ProjectionMode::LinuxFuse,
        },
    )
    .expect("mount google docs");

    assert_eq!(report.mount_id, "google-docs-secondary");
    assert_eq!(
        report.remote_root_id.as_deref(),
        Some("drive-folder-secondary")
    );
    assert_eq!(
        store
            .get_mount(&MountId::new("google-docs-secondary"))
            .expect("read mount")
            .expect("mount saved")
            .remote_root_id,
        Some(RemoteId::new("drive-folder-secondary"))
    );
}

struct MountFixture {
    root: PathBuf,
}

impl MountFixture {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()));

        Self { root }
    }

    fn mount(&self, store: &mut InMemoryStateStore) -> loc_cli::mount::MountReport {
        self.mount_with_connector(store, "notion")
    }

    fn mount_with_connector(
        &self,
        store: &mut InMemoryStateStore,
        connector: &str,
    ) -> loc_cli::mount::MountReport {
        run_mount(
            store,
            MountOptions {
                mount_id: MountId::new("notion-main"),
                connector: connector.to_string(),
                root: self.root.clone(),
                remote_root_id: Some(RemoteId::new("root-page")),
                connection_id: None,
                read_only: false,
                projection: ProjectionMode::PlainFiles,
            },
        )
        .expect("mount")
    }

    fn agents_file(&self) -> PathBuf {
        self.root.join("AGENTS.md")
    }

    fn claude_file(&self) -> PathBuf {
        self.root.join("CLAUDE.md")
    }
}

impl Drop for MountFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn read_to_string(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).expect("read guidance file")
}
