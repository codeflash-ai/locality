use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::mount::{GuidanceFileAction, MountOptions, run_mount};
use locality_connector::ConnectorCapabilities;
use locality_core::model::{MountId, RemoteId};
use locality_platform::{capabilities::projection_cli_value, mount_cli_capabilities};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorProfileRepository, CredentialStore, FileCredentialStore,
    InMemoryStateStore, MountRepository, ProjectionMode, SqliteStateStore,
};
use serde_json::Value;

const TOKEN_ENV: &str = "NOTION_TOKEN";
const DEFAULT_NOTION_PROFILE_ID: &str = "notion-token";

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
    assert!(agents.contains("loc create page --title"));
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
fn macos_file_provider_mount_keeps_source_root_virtual() {
    let fixture = MountFixture::new("loc-cli-mount-macos-file-provider");
    let mut store = InMemoryStateStore::new();

    let report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("notion-main"),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: None,
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::MacosFileProvider,
        },
    )
    .expect("mount");

    assert_eq!(
        report.guidance.agents_md.action,
        GuidanceFileAction::Virtual
    );
    assert_eq!(
        report.guidance.claude_md.action,
        GuidanceFileAction::Virtual
    );
    assert!(
        !fixture.root.exists(),
        "macOS File Provider source roots are virtual and must not be shadowed by a real directory"
    );

    let mount = store
        .get_mount(&MountId::new("notion-main"))
        .expect("load mount")
        .expect("mount exists");
    assert_eq!(mount.root, fixture.root);
    assert_eq!(mount.projection, ProjectionMode::MacosFileProvider);
}

#[test]
fn linux_fuse_mount_keeps_mount_point_virtual() {
    let fixture = MountFixture::new("loc-cli-mount-linux-fuse");
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
            projection: ProjectionMode::LinuxFuse,
        },
    )
    .expect("mount");

    assert_eq!(
        report.guidance.agents_md.action,
        GuidanceFileAction::Virtual
    );
    assert_eq!(
        report.guidance.claude_md.action,
        GuidanceFileAction::Virtual
    );
    assert!(
        !fixture.root.exists(),
        "Linux FUSE mount-point roots are virtual and must not be created before daemon state is updated"
    );

    let mount = store
        .get_mount(&MountId::new("notion-main"))
        .expect("load mount")
        .expect("mount exists");
    assert_eq!(mount.root, fixture.root);
    assert_eq!(mount.projection, ProjectionMode::LinuxFuse);
}

#[test]
fn virtual_mount_rejects_direct_home_child_mount_point() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let mut store = InMemoryStateStore::new();
    let root = PathBuf::from(home).join("notion-main");

    let error = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("notion-main"),
            connector: "notion".to_string(),
            root,
            remote_root_id: None,
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::LinuxFuse,
        },
    )
    .expect_err("unsafe virtual parent rejected");

    assert_eq!(error.code(), "unsafe_virtual_projection_root");
    assert!(error.message().contains("home directory"));
    assert!(
        store.load_mounts().expect("load mounts").is_empty(),
        "unsafe mount must not be persisted"
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

#[test]
fn cli_keeps_multiple_default_notion_workspace_mounts_with_distinct_connections() {
    let Some(virtual_projection) = mount_cli_capabilities().virtual_registration else {
        return;
    };
    let virtual_projection_arg = projection_cli_value(&virtual_projection);
    let fixture = MountFixture::new("loc-cli-multiple-notion-workspace-mounts");
    fs::create_dir_all(&fixture.root).expect("create shared virtual root");
    let state_root = fixture.root.join("state");
    seed_cli_notion_connection(&state_root, "notion-personal-c", "Personal");
    seed_cli_notion_connection(&state_root, "notion-codeflash", "Codeflash");

    let loc = env!("CARGO_BIN_EXE_loc");
    let personal_root = fixture.root.join("personal-notion");
    let codeflash_root = fixture.root.join("codeflash-notion");
    let personal_root_arg = personal_root.display().to_string();
    let codeflash_root_arg = codeflash_root.display().to_string();

    let personal = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "notion",
        personal_root_arg.as_str(),
        "--connection",
        "notion-personal-c",
        "--workspace",
        "--projection",
        virtual_projection_arg,
        "--json",
    ]));
    let codeflash = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "notion",
        codeflash_root_arg.as_str(),
        "--connection",
        "notion-codeflash",
        "--workspace",
        "--projection",
        virtual_projection_arg,
        "--json",
    ]));

    assert_eq!(personal["mount_id"], "notion-main", "{personal:#?}");
    assert_eq!(codeflash["mount_id"], "notion-codeflash", "{codeflash:#?}");

    let store = SqliteStateStore::open(state_root.clone()).expect("open state");
    let mut mounts = store.load_mounts().expect("load mounts");
    mounts.sort_by(|left, right| left.mount_id.cmp(&right.mount_id));
    assert_eq!(mounts.len(), 2, "{mounts:#?}");
    assert!(
        mounts
            .iter()
            .any(|mount| mount.mount_id == MountId::new("notion-main")
                && mount.connection_id == Some(ConnectionId::new("notion-personal-c"))
                && mount.root == personal_root),
        "{mounts:#?}"
    );
    assert!(
        mounts
            .iter()
            .any(|mount| mount.mount_id == MountId::new("notion-codeflash")
                && mount.connection_id == Some(ConnectionId::new("notion-codeflash"))
                && mount.root == codeflash_root),
        "{mounts:#?}"
    );

    let doctor = loc_json_with_exit(loc_command(loc, &state_root).args(["doctor", "--json"]), 3);
    let doctor_mounts = doctor["mounts"].as_array().expect("doctor mounts");
    assert_eq!(doctor_mounts.len(), 2, "{doctor:#?}");
    assert!(
        doctor_mounts
            .iter()
            .any(|mount| mount["mount_id"] == "notion-main"
                && mount["connection_id"] == "notion-personal-c"
                && mount["mount_point"] == personal_root_arg),
        "{doctor:#?}"
    );
    assert!(
        doctor_mounts
            .iter()
            .any(|mount| mount["mount_id"] == "notion-codeflash"
                && mount["connection_id"] == "notion-codeflash"
                && mount["mount_point"] == codeflash_root_arg),
        "{doctor:#?}"
    );
}

#[test]
fn virtual_mount_rejects_duplicate_mount_point_under_same_root() {
    let fixture = MountFixture::new("loc-cli-duplicate-mount-point");
    let mut store = InMemoryStateStore::new();
    let first_root = fixture.root.join("notion-main");
    let duplicate_root = fixture.root.join("notion-main");

    run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("notion-main"),
            connector: "notion".to_string(),
            root: first_root,
            remote_root_id: None,
            connection_id: Some(ConnectionId::new("work-a")),
            read_only: false,
            projection: ProjectionMode::LinuxFuse,
        },
    )
    .expect("first mount");

    let error = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("notion-my-company"),
            connector: "notion".to_string(),
            root: duplicate_root,
            remote_root_id: None,
            connection_id: Some(ConnectionId::new("work-b")),
            read_only: false,
            projection: ProjectionMode::LinuxFuse,
        },
    )
    .expect_err("duplicate mount point rejected");

    assert_eq!(error.code(), "mount_point_conflict");
    assert!(
        error
            .message()
            .contains("already uses mount point `notion-main` under")
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

fn seed_cli_notion_connection(state_root: &Path, connection_id: &str, workspace_name: &str) {
    fs::create_dir_all(state_root).expect("create state root");
    let profile_id = ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID);
    let secret_ref = format!("connection:{connection_id}");
    let credentials = FileCredentialStore::new(state_root);
    credentials
        .put(&secret_ref, &format!("ntn_dummy_{connection_id}"))
        .expect("seed credential");

    let now = "2026-07-03T00:00:00Z".to_string();
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: "notion".to_string(),
            display_name: "Notion token auth".to_string(),
            auth_kind: "token".to_string(),
            scopes: vec![],
            capabilities_json: notion_capabilities_json(),
            enabled_actions_json: "[\"read\",\"write\"]".to_string(),
            connector_version: "notion.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: ConnectionId::new(connection_id),
            profile_id: Some(profile_id),
            connector: "notion".to_string(),
            display_name: workspace_name.to_string(),
            account_label: Some(format!("{connection_id}@example.com")),
            workspace_id: Some(format!("{connection_id}-workspace")),
            workspace_name: Some(workspace_name.to_string()),
            auth_kind: "token".to_string(),
            secret_ref,
            scopes: vec![],
            capabilities_json: notion_capabilities_json(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed connection");
}

fn notion_capabilities_json() -> String {
    serde_json::to_string(&ConnectorCapabilities {
        supports_block_updates: true,
        supports_databases: true,
        supports_oauth: true,
        supports_remote_observation: true,
        supports_lazy_child_enumeration: true,
        supports_media_download: true,
        supports_undo: true,
        supports_batch_observation: false,
    })
    .expect("serialize Notion capabilities")
}

fn loc_command(loc: &str, state_root: &Path) -> Command {
    let mut command = Command::new(loc);
    command
        .env("LOCALITY_STATE_DIR", state_root)
        .env("LOCALITY_DAEMON_DISABLE", "1")
        .env("LOCALITY_CREDENTIAL_STORE", "file")
        .env_remove(TOKEN_ENV);
    command
}

fn loc_json_ok(command: &mut Command) -> Value {
    loc_json_with_exit(command, 0)
}

fn loc_json_with_exit(command: &mut Command, expected_code: i32) -> Value {
    let output = command.output().expect("run loc command");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == expected_code,
        "loc command exited with {code}, expected {expected_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("failed to parse loc JSON: {error}\n{stdout}\n{stderr}"))
}
