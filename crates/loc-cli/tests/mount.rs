use std::fs;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::mount::{GuidanceFileAction, MountOptions, run_mount};
use locality_connector::ConnectorCapabilities;
use locality_core::model::{MountId, RemoteId};
use locality_gmail::{GMAIL_OAUTH_SCOPES, gmail_capabilities_json};
use locality_google_calendar::oauth::google_calendar_capabilities_json;
use locality_google_calendar::{GOOGLE_CALENDAR_OAUTH_SCOPES, StoredGoogleCalendarCredential};
use locality_platform::{capabilities::projection_cli_value, mount_cli_capabilities};
use locality_slack::{
    SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE, SLACK_CONNECTOR_ID, SLACK_OAUTH_SCOPES,
    slack_capabilities_json,
};
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
    assert!(agents.contains("Common Locality CLI workflow:"));
    assert!(agents.contains("loc search <query>"));
    assert!(agents.contains("Locality hydrates online-only files on open"));
    assert!(agents.contains("loc status <path>"));
    assert!(agents.contains("loc inspect <path>"));
    assert!(agents.contains("loc diff <path>"));
    assert!(agents.contains("loc push <path> -y"));
    assert!(agents.contains("loc live-mode status <file>"));
    assert!(agents.contains("loc mv <source> <dest>"));
    assert!(agents.contains("Push intentional changes with `loc push <path>`"));
    assert!(agents.contains("If desktop Live Mode is on"));
    assert!(agents.contains("Do not run routine `loc pull` or `loc push`"));
    assert!(agents.contains("remote changed since last sync"));
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
    assert!(agents.lines().count() <= 70);
    assert!(agents.split_whitespace().count() <= 850);

    let mounts = store.load_mounts().expect("load mounts");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].root, fixture.root);
}

#[test]
fn mount_writes_linear_source_guidance() {
    let fixture = MountFixture::new("loc-cli-mount-generic-guidance");
    let mut store = InMemoryStateStore::new();

    fixture.mount_with_connector(&mut store, "linear");

    let agents = read_to_string(fixture.agents_file());
    assert!(agents.contains("# Locality Linear Mount"));
    assert!(agents.contains("projects Linear as local Markdown"));
    assert!(agents.contains("Linear facts:"));
    assert!(agents.contains("read-only lifecycle/date metadata"));
    assert!(agents.contains("Supported writes are only issue description body edits"));
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
            settings_json: "{}".to_string(),
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
            settings_json: "{}".to_string(),
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
            settings_json: "{}".to_string(),
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
            settings_json: "{}".to_string(),
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
            settings_json: "{}".to_string(),
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
            settings_json: "{}".to_string(),
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
fn slack_mount_is_read_only_by_default() {
    let fixture = MountFixture::new("loc-cli-mount-slack-read-only");
    let mut store = InMemoryStateStore::new();
    let settings_json = r#"{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"],"auto_join_public_channels":true}}"#;

    let report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("slack-main"),
            connector: SLACK_CONNECTOR_ID.to_string(),
            root: fixture.root.clone(),
            remote_root_id: None,
            connection_id: Some(ConnectionId::new("slack-default")),
            read_only: true,
            projection: ProjectionMode::PlainFiles,
            settings_json: settings_json.to_string(),
        },
    )
    .expect("mount slack");

    assert_eq!(report.connector, SLACK_CONNECTOR_ID);
    assert!(report.read_only);
    assert_eq!(report.settings_json, settings_json);

    let mount = store
        .get_mount(&MountId::new("slack-main"))
        .expect("get mount")
        .expect("mount");
    assert!(mount.read_only);
    assert_eq!(mount.settings_json, settings_json);
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
            settings_json: "{}".to_string(),
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

    let mut doctor_command = loc_command(loc, &state_root);
    #[cfg(target_os = "macos")]
    {
        let helper = fixture.root.join("fake-locality-file-providerctl");
        fs::write(
            &helper,
            "#!/bin/sh\nprintf '%s\\n' '{\"ok\":true,\"action\":\"list\",\"domains\":[],\"message\":\"listed 0 domain(s)\"}'\n",
        )
        .expect("write fake file provider helper");
        let mut permissions = fs::metadata(&helper)
            .expect("stat fake file provider helper")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions)
            .expect("make fake file provider helper executable");
        doctor_command.env("LOCALITY_FILE_PROVIDERCTL", helper);
    }
    let doctor = loc_json_with_exit(doctor_command.args(["doctor", "--json"]), 3);
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
fn cli_mount_gmail_persists_requested_registration() {
    let fixture = MountFixture::new("loc-cli-gmail-mount-registration");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_gmail_connection(&state_root, "gmail-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("gmail");
    let mount_root_arg = mount_root.display().to_string();

    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "gmail",
        mount_root_arg.as_str(),
        "--connection",
        "gmail-work",
        "--mount-id",
        "gmail-main",
        "--projection",
        "plain-files",
        "--read-only",
        "--json",
    ]));

    assert_eq!(report["connector"], "gmail", "{report:#?}");
    assert_eq!(report["remote_root_id"], Value::Null, "{report:#?}");
    assert_eq!(report["connection_id"], "gmail-work", "{report:#?}");
    assert_eq!(report["mount_id"], "gmail-main", "{report:#?}");
    assert_eq!(report["projection"], "plain_files", "{report:#?}");
    assert_eq!(report["read_only"], true, "{report:#?}");

    let store = SqliteStateStore::open(state_root).expect("open state");
    let mount = store
        .get_mount(&MountId::new("gmail-main"))
        .expect("load mount")
        .expect("mount exists");
    assert_eq!(mount.connector, "gmail");
    assert_eq!(mount.remote_root_id, None);
    assert_eq!(mount.connection_id, Some(ConnectionId::new("gmail-work")));
    assert_eq!(mount.mount_id, MountId::new("gmail-main"));
    assert_eq!(mount.projection, ProjectionMode::PlainFiles);
    assert!(mount.read_only);
}

#[test]
fn cli_mount_slack_persists_requested_read_only_registration() {
    let fixture = MountFixture::new("loc-cli-slack-mount-registration");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_slack_connection(&state_root, "slack-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("slack");
    let mount_root_arg = mount_root.display().to_string();
    let settings_json = r#"{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"],"auto_join_public_channels":true}}"#;

    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "slack",
        mount_root_arg.as_str(),
        "--connection",
        "slack-work",
        "--mount-id",
        "slack-main",
        "--projection",
        "plain-files",
        "--history-limit",
        "15",
        "--types",
        "public_channel,private_channel,im,mpim",
        "--json",
    ]));

    assert_eq!(report["connector"], SLACK_CONNECTOR_ID, "{report:#?}");
    assert_eq!(report["remote_root_id"], Value::Null, "{report:#?}");
    assert_eq!(report["connection_id"], "slack-work", "{report:#?}");
    assert_eq!(report["mount_id"], "slack-main", "{report:#?}");
    assert_eq!(report["projection"], "plain_files", "{report:#?}");
    assert_eq!(report["read_only"], true, "{report:#?}");
    assert_eq!(report["settings_json"], settings_json, "{report:#?}");

    let store = SqliteStateStore::open(state_root).expect("open state");
    let mount = store
        .get_mount(&MountId::new("slack-main"))
        .expect("load mount")
        .expect("mount exists");
    assert_eq!(mount.connector, SLACK_CONNECTOR_ID);
    assert_eq!(mount.remote_root_id, None);
    assert_eq!(mount.connection_id, Some(ConnectionId::new("slack-work")));
    assert_eq!(mount.mount_id, MountId::new("slack-main"));
    assert_eq!(mount.projection, ProjectionMode::PlainFiles);
    assert!(mount.read_only);
    assert_eq!(mount.settings_json, settings_json);
}

#[test]
fn cli_mount_slack_rejects_public_channel_mount_without_channels_join_scope() {
    let fixture = MountFixture::new("loc-cli-slack-auto-join-missing-scope");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_slack_connection_with_scopes(
        &state_root,
        "slack-work",
        slack_scopes_without_auto_join(),
    );

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("slack");
    let mount_root_arg = mount_root.display().to_string();
    let body = loc_json_with_exit(
        loc_command(loc, &state_root).args([
            "mount",
            "slack",
            mount_root_arg.as_str(),
            "--connection",
            "slack-work",
            "--projection",
            "plain-files",
            "--json",
        ]),
        2,
    );

    assert_eq!(body["code"], "slack_auto_join_scope_missing", "{body:#?}");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("channels:join"),
        "{body:#?}"
    );
}

#[test]
fn cli_mount_slack_persists_auto_join_public_channels_setting() {
    let fixture = MountFixture::new("loc-cli-slack-auto-join-setting");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_slack_connection_with_scopes(&state_root, "slack-work", slack_scopes_with_auto_join());

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("slack");
    let mount_root_arg = mount_root.display().to_string();
    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "slack",
        mount_root_arg.as_str(),
        "--connection",
        "slack-work",
        "--mount-id",
        "slack-main",
        "--projection",
        "plain-files",
        "--json",
    ]));

    assert_eq!(
        report["settings_json"],
        r#"{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"],"auto_join_public_channels":true}}"#
    );

    let store = SqliteStateStore::open(state_root).expect("open state");
    let mount = store
        .get_mount(&MountId::new("slack-main"))
        .expect("get mount")
        .expect("mount");
    assert_eq!(
        mount.settings_json,
        r#"{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"],"auto_join_public_channels":true}}"#
    );
}

#[test]
fn cli_mount_slack_omits_auto_join_when_public_channel_type_is_excluded() {
    let fixture = MountFixture::new("loc-cli-slack-auto-join-public-excluded");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_slack_connection_with_scopes(&state_root, "slack-work", slack_scopes_with_auto_join());

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("slack");
    let mount_root_arg = mount_root.display().to_string();
    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "slack",
        mount_root_arg.as_str(),
        "--connection",
        "slack-work",
        "--mount-id",
        "slack-main",
        "--projection",
        "plain-files",
        "--types",
        "im,mpim",
        "--json",
    ]));

    assert_eq!(
        report["settings_json"],
        r#"{"slack":{"history_limit":15,"types":["im","mpim"]}}"#
    );
}

#[test]
fn cli_mount_slack_rejects_out_of_range_history_limit_before_state_open() {
    for history_limit in ["0", "999"] {
        let fixture = MountFixture::new("loc-cli-slack-invalid-history-limit");
        let state_root = fixture.root.join("state");

        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("slack");
        let mount_root_arg = mount_root.display().to_string();

        let body = loc_json_with_exit(
            loc_command(loc, &state_root).args([
                "mount",
                "slack",
                mount_root_arg.as_str(),
                "--connection",
                "slack-work",
                "--projection",
                "plain-files",
                "--history-limit",
                history_limit,
                "--json",
            ]),
            2,
        );

        assert_eq!(body["code"], "slack_history_limit_invalid", "{body:#?}");
        assert!(
            !state_root.exists(),
            "invalid Slack history limit {history_limit} should fail before opening state"
        );
    }
}

#[test]
fn cli_mount_slack_rejects_non_integer_history_limit_before_state_open() {
    let fixture = MountFixture::new("loc-cli-slack-non-integer-history-limit");
    let state_root = fixture.root.join("state");

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("slack");
    let mount_root_arg = mount_root.display().to_string();

    let body = loc_json_with_exit(
        loc_command(loc, &state_root).args([
            "mount",
            "slack",
            mount_root_arg.as_str(),
            "--connection",
            "slack-work",
            "--projection",
            "plain-files",
            "--history-limit",
            "abc",
            "--json",
        ]),
        2,
    );

    assert_eq!(body["code"], "slack_history_limit_invalid", "{body:#?}");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("integer from 1 to 15"),
        "{body:#?}"
    );
    assert!(
        !state_root.exists(),
        "non-integer Slack history limit should fail before opening state"
    );
}

#[test]
fn cli_mount_slack_rejects_invalid_types_before_state_open() {
    for (types, expected_message) in [
        (
            "bogus",
            "unsupported Slack conversation type `bogus`; supported values:",
        ),
        ("", "Slack conversation types must be non-empty"),
    ] {
        let fixture = MountFixture::new("loc-cli-slack-invalid-types");
        let state_root = fixture.root.join("state");

        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("slack");
        let mount_root_arg = mount_root.display().to_string();

        let body = loc_json_with_exit(
            loc_command(loc, &state_root).args([
                "mount",
                "slack",
                mount_root_arg.as_str(),
                "--connection",
                "slack-work",
                "--projection",
                "plain-files",
                "--types",
                types,
                "--json",
            ]),
            2,
        );

        assert_eq!(body["code"], "slack_types_invalid", "{body:#?}");
        assert!(
            body["message"]
                .as_str()
                .expect("message")
                .contains(expected_message),
            "{body:#?}"
        );
        assert!(
            body["message"]
                .as_str()
                .expect("message")
                .contains("public_channel,private_channel,im,mpim"),
            "{body:#?}"
        );
        assert!(
            !state_root.exists(),
            "invalid Slack types `{types}` should fail before opening state"
        );
    }
}

#[test]
fn cli_mount_slack_rejects_remote_root_selectors() {
    let cases: &[&[&str]] = &[
        &["--workspace"],
        &["--root-page", "root-page"],
        &["--workspace-folder", "workspace-folder"],
    ];

    for forbidden_args in cases {
        let fixture = MountFixture::new("loc-cli-slack-mount-root-rejection");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_slack_connection(&state_root, "slack-work");

        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("slack");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "slack",
            mount_root_arg.as_str(),
            "--connection",
            "slack-work",
            "--mount-id",
            "slack-main",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(*forbidden_args);

        let output = command.output().expect("run loc mount slack");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "Slack mount accepted forbidden args {forbidden_args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let store = SqliteStateStore::open(state_root).expect("open state");
        assert!(
            store.load_mounts().expect("load mounts").is_empty(),
            "Slack mount with forbidden args {forbidden_args:?} must not be persisted"
        );
    }
}

#[test]
fn cli_mount_gmail_persists_date_window_and_thread_view() {
    let fixture = MountFixture::new("loc-cli-gmail-mount-settings");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_gmail_connection(&state_root, "gmail-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("gmail");
    let mount_root_arg = mount_root.display().to_string();

    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "gmail",
        mount_root_arg.as_str(),
        "--connection",
        "gmail-work",
        "--mount-id",
        "gmail-main",
        "--projection",
        "plain-files",
        "--after",
        "2026-07-01",
        "--before",
        "2026-07-15",
        "--view",
        "threads",
        "--json",
    ]));

    assert_eq!(report["connector"], "gmail", "{report:#?}");
    assert_eq!(
        report["settings_json"],
        r#"{"gmail":{"date_window":{"after":"2026-07-01","before":"2026-07-15"},"view":"threads"}}"#,
        "{report:#?}"
    );

    let store = SqliteStateStore::open(state_root).expect("open state");
    let mount = store
        .get_mount(&MountId::new("gmail-main"))
        .expect("load mount")
        .expect("mount exists");
    assert_eq!(
        mount.settings_json,
        r#"{"gmail":{"date_window":{"after":"2026-07-01","before":"2026-07-15"},"view":"threads"}}"#
    );
}

#[test]
fn cli_mount_google_calendar_persists_explicit_date_window_settings() {
    let fixture = MountFixture::new("loc-cli-google-calendar-mount-settings");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_google_calendar_connection(&state_root, "calendar-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("calendar");
    let mount_root_arg = mount_root.display().to_string();

    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "google-calendar",
        mount_root_arg.as_str(),
        "--connection",
        "calendar-work",
        "--mount-id",
        "calendar-main",
        "--projection",
        "plain-files",
        "--after",
        "2026-07-01",
        "--before",
        "2026-07-31",
        "--json",
    ]));

    assert_eq!(report["connector"], "google-calendar", "{report:#?}");
    assert_eq!(
        report["settings_json"],
        r#"{"google_calendar":{"date_window":{"after":"2026-07-01","before":"2026-07-31"}}}"#,
        "{report:#?}"
    );

    let store = SqliteStateStore::open(state_root).expect("open state");
    let mount = store
        .get_mount(&MountId::new("calendar-main"))
        .expect("load mount")
        .expect("mount exists");
    assert_eq!(mount.connector, "google-calendar");
    assert_eq!(mount.remote_root_id, None);
    assert_eq!(
        mount.connection_id,
        Some(ConnectionId::new("calendar-work"))
    );
    assert_eq!(
        mount.settings_json,
        r#"{"google_calendar":{"date_window":{"after":"2026-07-01","before":"2026-07-31"}}}"#
    );
}

#[test]
fn cli_mount_google_calendar_rejects_partial_date_window() {
    for args in [
        vec!["--after", "2026-07-01"],
        vec!["--before", "2026-07-31"],
    ] {
        let fixture = MountFixture::new("loc-cli-google-calendar-partial-date-window");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_google_calendar_connection(&state_root, "calendar-work");
        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("calendar");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "google-calendar",
            mount_root_arg.as_str(),
            "--connection",
            "calendar-work",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(args);

        let output = command.output().expect("run loc mount google-calendar");
        assert!(!output.status.success());
        let body: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("json error response");
        assert_eq!(
            body["code"],
            "google_calendar_date_window_requires_after_and_before"
        );
    }
}

#[test]
fn cli_mount_gmail_rejects_partial_date_window() {
    for args in [
        vec!["--after", "2026-07-01"],
        vec!["--before", "2026-07-15"],
    ] {
        let fixture = MountFixture::new("loc-cli-gmail-partial-date-window");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_gmail_connection(&state_root, "gmail-work");
        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("gmail");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "gmail",
            mount_root_arg.as_str(),
            "--connection",
            "gmail-work",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(args);

        let output = command.output().expect("run loc mount gmail");
        assert!(!output.status.success());
        let body: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("json error response");
        assert_eq!(body["code"], "gmail_date_window_requires_after_and_before");
    }
}

#[test]
fn cli_mount_gmail_rejects_invalid_view_before_connection_resolution() {
    let fixture = MountFixture::new("loc-cli-gmail-invalid-view-no-connection");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("gmail");
    let mount_root_arg = mount_root.display().to_string();

    let body = loc_json_with_exit(
        loc_command(loc, &state_root).args([
            "mount",
            "gmail",
            mount_root_arg.as_str(),
            "--projection",
            "plain-files",
            "--view",
            "bogus",
            "--json",
        ]),
        2,
    );

    assert_eq!(body["code"], "gmail_view_invalid", "{body:#?}");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .contains("unsupported Gmail view `bogus`"),
        "{body:#?}"
    );
}

#[test]
fn cli_mount_gmail_invalid_date_window_errors_include_validation_detail() {
    for (args, expected_message) in [
        (
            vec!["--after", "2026/07/01", "--before", "2026-07-15"],
            "must use YYYY-MM-DD",
        ),
        (
            vec!["--after", "2026-02-30", "--before", "2026-07-15"],
            "is not a calendar date",
        ),
    ] {
        let fixture = MountFixture::new("loc-cli-gmail-invalid-date-window");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_gmail_connection(&state_root, "gmail-work");

        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("gmail");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "gmail",
            mount_root_arg.as_str(),
            "--connection",
            "gmail-work",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(args);

        let body = loc_json_with_exit(&mut command, 2);

        assert_eq!(body["code"], "gmail_date_window_invalid", "{body:#?}");
        assert!(
            body["message"]
                .as_str()
                .expect("message")
                .contains(expected_message),
            "{body:#?}"
        );
    }
}

#[test]
fn cli_mount_gmail_rejects_reversed_or_equal_date_windows_with_detail() {
    for args in [
        vec!["--after", "2026-07-15", "--before", "2026-07-01"],
        vec!["--after", "2026-07-15", "--before", "2026-07-15"],
    ] {
        let fixture = MountFixture::new("loc-cli-gmail-reversed-date-window");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_gmail_connection(&state_root, "gmail-work");

        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("gmail");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "gmail",
            mount_root_arg.as_str(),
            "--connection",
            "gmail-work",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(args);

        let body = loc_json_with_exit(&mut command, 2);

        assert_eq!(body["code"], "gmail_date_window_invalid", "{body:#?}");
        assert!(
            body["message"]
                .as_str()
                .expect("message")
                .contains("`--before` must be later than `--after`"),
            "{body:#?}"
        );
    }
}

#[test]
fn cli_mount_gmail_default_settings_are_suppressed() {
    let fixture = MountFixture::new("loc-cli-gmail-default-settings");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_gmail_connection(&state_root, "gmail-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let json_mount_root = fixture.root.join("gmail-json");
    let json_mount_root_arg = json_mount_root.display().to_string();

    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "gmail",
        json_mount_root_arg.as_str(),
        "--connection",
        "gmail-work",
        "--mount-id",
        "gmail-json",
        "--projection",
        "plain-files",
        "--json",
    ]));

    assert_eq!(report["settings_json"], "{}", "{report:#?}");

    let human_mount_root = fixture.root.join("gmail-human");
    let human_mount_root_arg = human_mount_root.display().to_string();
    let output = loc_command(loc, &state_root)
        .args([
            "mount",
            "gmail",
            human_mount_root_arg.as_str(),
            "--connection",
            "gmail-work",
            "--mount-id",
            "gmail-human",
            "--projection",
            "plain-files",
        ])
        .output()
        .expect("run loc mount gmail");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("settings:"), "{stdout}");

    let store = SqliteStateStore::open(state_root).expect("open state");
    let json_mount = store
        .get_mount(&MountId::new("gmail-json"))
        .expect("load JSON mount")
        .expect("JSON mount exists");
    let human_mount = store
        .get_mount(&MountId::new("gmail-human"))
        .expect("load human mount")
        .expect("human mount exists");
    assert_eq!(json_mount.settings_json, "{}");
    assert_eq!(human_mount.settings_json, "{}");
}

#[test]
fn cli_mount_gmail_rejects_remote_root_selectors() {
    let cases: &[&[&str]] = &[
        &["--workspace"],
        &["--root-page", "root-page"],
        &["--workspace-folder", "workspace-folder"],
    ];

    for forbidden_args in cases {
        let fixture = MountFixture::new("loc-cli-gmail-mount-root-rejection");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_gmail_connection(&state_root, "gmail-work");

        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("gmail");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "gmail",
            mount_root_arg.as_str(),
            "--connection",
            "gmail-work",
            "--mount-id",
            "gmail-main",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(*forbidden_args);

        let output = command.output().expect("run loc mount gmail");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "Gmail mount accepted forbidden args {forbidden_args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let store = SqliteStateStore::open(state_root).expect("open state");
        assert!(
            store.load_mounts().expect("load mounts").is_empty(),
            "Gmail mount with forbidden args {forbidden_args:?} must not be persisted"
        );
    }
}

#[test]
fn cli_mount_google_calendar_rejects_remote_root_selectors() {
    let cases: &[&[&str]] = &[
        &["--workspace"],
        &["--root-page", "root-page"],
        &["--workspace-folder", "workspace-folder"],
    ];

    for forbidden_args in cases {
        let fixture = MountFixture::new("loc-cli-google-calendar-mount-root-rejection");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_google_calendar_connection(&state_root, "calendar-work");

        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("calendar");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "google-calendar",
            mount_root_arg.as_str(),
            "--connection",
            "calendar-work",
            "--mount-id",
            "calendar-main",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(*forbidden_args);

        let output = command.output().expect("run loc mount google-calendar");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "Google Calendar mount accepted forbidden args {forbidden_args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let store = SqliteStateStore::open(state_root).expect("open state");
        assert!(
            store.load_mounts().expect("load mounts").is_empty(),
            "Google Calendar mount with forbidden args {forbidden_args:?} must not be persisted"
        );
    }
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
            settings_json: "{}".to_string(),
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
            settings_json: "{}".to_string(),
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
                settings_json: "{}".to_string(),
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
        supports_entity_body_updates: false,
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

fn seed_cli_gmail_connection(state_root: &Path, connection_id: &str) {
    fs::create_dir_all(state_root).expect("create state root");
    let profile_id = ConnectorProfileId::new("gmail-oauth-default");
    let secret_ref = format!("connection:{connection_id}");
    let credentials = FileCredentialStore::new(state_root);
    credentials
        .put(
            &secret_ref,
            "{\"connector\":\"gmail\",\"refresh_handle\":\"refresh-handle\"}",
        )
        .expect("seed credential");

    let now = "2026-07-03T00:00:00Z".to_string();
    let capabilities_json = gmail_capabilities_json().expect("gmail capabilities");
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: "gmail".to_string(),
            display_name: "Gmail OAuth".to_string(),
            auth_kind: "oauth".to_string(),
            scopes: GMAIL_OAUTH_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
            capabilities_json: capabilities_json.clone(),
            enabled_actions_json: "[\"read\",\"send\"]".to_string(),
            connector_version: "gmail.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed Gmail profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: ConnectionId::new(connection_id),
            profile_id: Some(profile_id),
            connector: "gmail".to_string(),
            display_name: "Gmail".to_string(),
            account_label: Some(format!("{connection_id}@example.com")),
            workspace_id: Some("gmail".to_string()),
            workspace_name: Some("Gmail".to_string()),
            auth_kind: "oauth".to_string(),
            secret_ref,
            scopes: GMAIL_OAUTH_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
            capabilities_json,
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed Gmail connection");
}

fn seed_cli_google_calendar_connection(state_root: &Path, connection_id: &str) {
    fs::create_dir_all(state_root).expect("create state root");
    let profile_id = ConnectorProfileId::new("google-calendar-oauth-default");
    let secret_ref = format!("connection:{connection_id}");
    let credentials = FileCredentialStore::new(state_root);
    let credential = StoredGoogleCalendarCredential {
        kind: "oauth".to_string(),
        connector: "google-calendar".to_string(),
        access_token: "access-token".to_string(),
        token_type: Some("Bearer".to_string()),
        oauth_client_id: Some("google-client-id".to_string()),
        oauth_broker_url: Some("https://auth.example.test".to_string()),
        account_id: Some("acct-1".to_string()),
        account_label: Some(format!("{connection_id}@example.com")),
        workspace_id: Some("primary".to_string()),
        workspace_name: Some("Primary Calendar".to_string()),
        scopes: GOOGLE_CALENDAR_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect(),
        refresh_token_handle: Some("refresh-handle".to_string()),
        acquired_at: 1_783_036_800,
        expires_at: None,
    };
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&credential).expect("credential json"),
        )
        .expect("seed credential");

    let now = "2026-07-03T00:00:00Z".to_string();
    let capabilities_json =
        google_calendar_capabilities_json().expect("google calendar capabilities");
    let scopes = GOOGLE_CALENDAR_OAUTH_SCOPES
        .iter()
        .map(|scope| scope.to_string())
        .collect::<Vec<_>>();
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: "google-calendar".to_string(),
            display_name: "Google Calendar OAuth".to_string(),
            auth_kind: "oauth".to_string(),
            scopes: scopes.clone(),
            capabilities_json: capabilities_json.clone(),
            enabled_actions_json: "[\"read\",\"create\"]".to_string(),
            connector_version: "google-calendar.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed Google Calendar profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: ConnectionId::new(connection_id),
            profile_id: Some(profile_id),
            connector: "google-calendar".to_string(),
            display_name: "Google Calendar".to_string(),
            account_label: Some(format!("{connection_id}@example.com")),
            workspace_id: Some("primary".to_string()),
            workspace_name: Some("Primary Calendar".to_string()),
            auth_kind: "oauth".to_string(),
            secret_ref,
            scopes,
            capabilities_json,
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed Google Calendar connection");
}

fn seed_cli_slack_connection(state_root: &Path, connection_id: &str) {
    seed_cli_slack_connection_with_scopes(
        state_root,
        connection_id,
        SLACK_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect(),
    );
}

fn seed_cli_slack_connection_with_scopes(
    state_root: &Path,
    connection_id: &str,
    scopes: Vec<String>,
) {
    fs::create_dir_all(state_root).expect("create state root");
    let profile_id = ConnectorProfileId::new("slack-oauth-default");
    let secret_ref = format!("connection:{connection_id}");
    let credentials = FileCredentialStore::new(state_root);
    credentials
        .put(
            &secret_ref,
            "{\"connector\":\"slack\",\"access_token\":\"xoxb-test\"}",
        )
        .expect("seed credential");

    let now = "2026-07-03T00:00:00Z".to_string();
    let capabilities_json = slack_capabilities_json().expect("slack capabilities");
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: SLACK_CONNECTOR_ID.to_string(),
            display_name: "Slack OAuth".to_string(),
            auth_kind: "oauth".to_string(),
            scopes: scopes.clone(),
            capabilities_json: capabilities_json.clone(),
            enabled_actions_json: "[\"read\"]".to_string(),
            connector_version: "slack.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed Slack profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: ConnectionId::new(connection_id),
            profile_id: Some(profile_id),
            connector: SLACK_CONNECTOR_ID.to_string(),
            display_name: "Slack".to_string(),
            account_label: Some(format!("{connection_id}@example.com")),
            workspace_id: Some("slack-workspace".to_string()),
            workspace_name: Some("Slack".to_string()),
            auth_kind: "oauth".to_string(),
            secret_ref,
            scopes,
            capabilities_json,
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed Slack connection");
}

fn slack_scopes_with_auto_join() -> Vec<String> {
    let mut scopes = SLACK_OAUTH_SCOPES
        .iter()
        .map(|scope| scope.to_string())
        .collect::<Vec<_>>();
    if !scopes
        .iter()
        .any(|scope| scope == SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE)
    {
        scopes.push(SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE.to_string());
    }
    scopes
}

fn slack_scopes_without_auto_join() -> Vec<String> {
    SLACK_OAUTH_SCOPES
        .iter()
        .copied()
        .filter(|scope| *scope != SLACK_AUTO_JOIN_PUBLIC_CHANNELS_SCOPE)
        .map(str::to_string)
        .collect()
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
