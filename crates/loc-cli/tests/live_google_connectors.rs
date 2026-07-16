#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loc_cli::diff::run_diff_with_state_root;
use loc_cli::mount::{MountOptions, run_mount};
use loc_cli::pull::run_pull_with_state_root;
use loc_cli::push::{PushOptions, run_push_with_daemon_at_state_root};
use loc_cli::status::{StatusOptions, run_status};
use locality_core::model::{CanonicalDocument, MountId, RemoteId};
use locality_gmail::{
    GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, GmailConnector, GmailMountSettings,
    GmailProjectionView, StoredGmailCredential, gmail_capabilities_json,
};
use locality_google_docs::client::{GoogleDocsApi, GoogleDriveApi, HttpGoogleApiClient};
use locality_google_docs::docs_dto::{GoogleDocument, StructuralElement};
use locality_google_docs::drive_dto::{DriveCreateFileRequest, DriveUpdateFileRequest};
use locality_google_docs::{
    GOOGLE_DOCS_CONNECTOR_ID, GOOGLE_DOCS_OAUTH_SCOPES, GoogleDocsConnector,
    StoredGoogleDocsCredential, google_docs_capabilities_json,
};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorProfileRepository, CredentialStore, FileCredentialStore,
    MountConfig, MountRepository, ProjectionMode, SqliteStateStore,
};
use localityd::source::{ResolvedSource, resolve_source_for_mount};
use localityd::virtual_fs::{
    VirtualFsItem, VirtualFsItemKind, materialize_virtual_fs_item_with_content_root,
    mount_point_identifier, refresh_virtual_fs_children, rename_virtual_fs_item,
    trash_virtual_fs_item, virtual_fs_children_with_content_root, virtual_fs_content_root,
};

const LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON: &str = "LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON";
const LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_PREFIX: &str =
    "LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_PREFIX";
const LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON: &str = "LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON";
const LOCALITY_GMAIL_LIVE_TEST_RECIPIENT: &str = "LOCALITY_GMAIL_LIVE_TEST_RECIPIENT";
const LOCALITY_GMAIL_LIVE_AFTER: &str = "LOCALITY_GMAIL_LIVE_AFTER";
const LOCALITY_GMAIL_LIVE_BEFORE: &str = "LOCALITY_GMAIL_LIVE_BEFORE";
const LOCALITY_GOOGLE_LIVE_FORCE_REFRESH: &str = "LOCALITY_GOOGLE_LIVE_FORCE_REFRESH";
const KEEP_TMP_ENV: &str = "LOCALITY_GOOGLE_LIVE_KEEP_TMP";
const EXPIRED_SENTINEL_ACCESS_TOKEN: &str = "locality-live-expired-access-token";
const GMAIL_LIVE_BODY: &str = "This message was sent by the Locality live Gmail e2e suite.";

#[derive(Clone, Copy)]
enum GoogleLiveConnector {
    GoogleDocs,
    Gmail,
}

struct LiveFixture {
    state_root: PathBuf,
    mount_root: PathBuf,
}

impl LiveFixture {
    fn new(prefix: &str) -> Self {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_millis()
        );
        let root = std::env::temp_dir().join(format!("{prefix}-{suffix}"));
        let state_root = root.join("state");
        let mount_root = root.join("Locality");
        fs::create_dir_all(&state_root).expect("create state root");
        fs::create_dir_all(&mount_root).expect("create mount root");
        Self {
            state_root,
            mount_root,
        }
    }
}

impl Drop for LiveFixture {
    fn drop(&mut self) {
        if std::env::var(KEEP_TMP_ENV).ok().as_deref() != Some("1") {
            let _ = fs::remove_dir_all(self.state_root.parent().expect("fixture root parent"));
        }
    }
}

struct DriveTrashGuard {
    api: Option<HttpGoogleApiClient>,
    file_id: String,
}

impl DriveTrashGuard {
    fn new(api: HttpGoogleApiClient, file_id: String) -> Self {
        Self {
            api: Some(api),
            file_id,
        }
    }

    fn use_api(&mut self, api: HttpGoogleApiClient) {
        self.api = Some(api);
    }
}

impl Drop for DriveTrashGuard {
    fn drop(&mut self) {
        if let Some(api) = &self.api {
            let _ = api.update_file(&self.file_id, DriveUpdateFileRequest::trash());
        }
    }
}

fn seed_live_connection(
    state_root: &Path,
    connector: GoogleLiveConnector,
    connection_id: &ConnectionId,
) -> String {
    let mut secret = match connector {
        GoogleLiveConnector::GoogleDocs => stored_google_docs_secret(),
        GoogleLiveConnector::Gmail => stored_gmail_secret(),
    };
    if std::env::var(LOCALITY_GOOGLE_LIVE_FORCE_REFRESH)
        .ok()
        .as_deref()
        == Some("1")
    {
        secret = force_expired_secret(connector, &secret);
    }

    let secret_ref = format!("connection:{}", connection_id.as_str());
    FileCredentialStore::new(state_root)
        .put(&secret_ref, &secret)
        .expect("save live credential");

    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state store");
    let profile_id = ConnectorProfileId::new(format!("{}-profile", connection_id.as_str()));
    let timestamp = timestamp_string();
    let connector_id = connector_id(connector);
    let scopes = connector_scopes(connector);
    let capabilities_json = connector_capabilities_json(connector);

    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: connector_id.to_string(),
            display_name: connector_display_name(connector).to_string(),
            auth_kind: "oauth".to_string(),
            scopes: scopes.clone(),
            capabilities_json: capabilities_json.clone(),
            enabled_actions_json: connector_enabled_actions_json(connector).to_string(),
            connector_version: connector_version(connector).to_string(),
            status: "active".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        })
        .expect("save connector profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: connector_id.to_string(),
            display_name: connector_display_name(connector).to_string(),
            account_label: live_account_label(connector, &secret),
            workspace_id: live_workspace_id(connector, &secret),
            workspace_name: live_workspace_name(connector, &secret),
            auth_kind: "oauth".to_string(),
            secret_ref: secret_ref.clone(),
            scopes,
            capabilities_json,
            status: "active".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            expires_at: credential_expires_at(connector, &secret).map(|expires_at| {
                expires_at
                    .duration_since(UNIX_EPOCH)
                    .expect("credential expires after epoch")
                    .as_secs()
                    .to_string()
            }),
        })
        .expect("save connection");

    secret_ref
}

#[test]
#[ignore = "requires LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON with broker refresh handle; creates and trashes scratch Google Drive content"]
fn live_google_docs_workspace_create_edit_move_archive_round_trip() {
    let fixture = LiveFixture::new("loc-live-google-docs");
    let connection_id = ConnectionId::new("google-docs-live");
    seed_live_connection(
        &fixture.state_root,
        GoogleLiveConnector::GoogleDocs,
        &connection_id,
    );
    let bootstrap_connector = resolve_google_docs_from_store(
        &fixture.state_root,
        connection_id.clone(),
        RemoteId::new("bootstrap-workspace"),
    );
    let bootstrap_api = HttpGoogleApiClient::new(bootstrap_connector.config().access_token.clone());
    let workspace_prefix = std::env::var(LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_PREFIX)
        .unwrap_or_else(|_| "Locality live Google Docs e2e".to_string());
    let workspace = bootstrap_api
        .create_file(DriveCreateFileRequest::folder(
            format!("{workspace_prefix} {}", timestamp_string()),
            None,
        ))
        .expect("create scratch Google Docs workspace folder");
    let mut cleanup = DriveTrashGuard::new(bootstrap_api, workspace.id.clone());

    let connector = resolve_google_docs_from_store(
        &fixture.state_root,
        connection_id.clone(),
        RemoteId::new(workspace.id.clone()),
    );
    let api = HttpGoogleApiClient::new(connector.config().access_token.clone());
    cleanup.use_api(api.clone());

    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open state store");
    let plain_mount_id = MountId::new("google-docs-main");
    let plain_mount_root = fixture.mount_root.join("google-docs-main");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: plain_mount_id.clone(),
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            root: plain_mount_root.clone(),
            remote_root_id: Some(RemoteId::new(workspace.id.clone())),
            connection_id: Some(connection_id.clone()),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live Google Docs plain workspace");
    let pull = run_pull_with_state_root(
        &mut store,
        &connector,
        &plain_mount_root,
        Some(&fixture.state_root),
    )
    .expect("pull live Google Docs plain workspace");
    assert!(pull.ok, "{pull:#?}");

    let page_dir = plain_mount_root.join("draft-plan");
    fs::create_dir_all(&page_dir).expect("create draft page directory");
    let page_path = page_dir.join("page.md");
    fs::write(
        &page_path,
        "---\ntitle: Draft Plan\n---\n# Draft Plan\n\nCreated from live Google Docs e2e.\n",
    )
    .expect("write draft page");
    let create_diff = run_diff_with_state_root(&store, &page_path, Some(&fixture.state_root))
        .expect("diff Google Docs create");
    assert!(create_diff.ok, "{create_diff:#?}");
    assert_eq!(create_diff.action, "confirm_plan", "{create_diff:#?}");
    let create_plan = create_diff.plan.as_ref().expect("Google Docs create plan");
    assert_eq!(create_plan.summary.entities_created, 1, "{create_plan:#?}");

    let create_push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push Google Docs create");
    assert!(create_push.ok, "{create_push:#?}");
    assert_eq!(create_push.action, "reconciled", "{create_push:#?}");
    let created_id = create_push
        .changed_remote_ids
        .iter()
        .find(|id| *id != &workspace.id)
        .cloned()
        .expect("created Google Docs remote id");
    let created_doc = api
        .get_document(&created_id)
        .expect("fetch created Google Doc");
    assert_eq!(created_doc.document_id, created_id);
    let created_file = api
        .get_file(&created_id)
        .expect("fetch created Google Drive file");
    assert_eq!(created_file.name, "Draft Plan");
    assert_eq!(created_file.parents, vec![workspace.id.clone()]);

    let created_markdown = fs::read_to_string(&page_path).expect("read created page");
    fs::write(
        &page_path,
        created_markdown.replace(
            "Created from live Google Docs e2e.",
            "Edited from live Google Docs e2e.",
        ),
    )
    .expect("write edited page");
    let edit_push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push Google Docs edit");
    assert!(edit_push.ok, "{edit_push:#?}");
    assert_eq!(edit_push.action, "reconciled", "{edit_push:#?}");
    assert!(
        edit_push.changed_remote_ids.contains(&created_id),
        "{edit_push:#?}"
    );
    let edited_doc = api
        .get_document(&created_id)
        .expect("fetch edited Google Doc");
    let edited_text = google_document_text(&edited_doc);
    assert!(
        edited_text.contains("Edited from live Google Docs e2e."),
        "{edited_text}"
    );
    let plain_status = run_status(
        &store,
        StatusOptions {
            path: Some(plain_mount_root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("plain status after create/edit");
    assert!(plain_status.clean, "{plain_status:#?}");

    let second_doc = api
        .create_file(DriveCreateFileRequest::google_doc(
            "Move Me",
            workspace.id.clone(),
        ))
        .expect("create second Google Doc for virtual move");
    let archive_folder = api
        .create_file(DriveCreateFileRequest::folder(
            "Archive",
            Some(workspace.id.as_str()),
        ))
        .expect("create Archive Drive folder");
    let virtual_mount_id = MountId::new("google-docs-virtual");
    let virtual_mount_root = fixture.mount_root.join("google-docs-virtual");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: virtual_mount_id.clone(),
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            root: virtual_mount_root.clone(),
            remote_root_id: Some(RemoteId::new(workspace.id.clone())),
            connection_id: Some(connection_id.clone()),
            read_only: false,
            projection: ProjectionMode::LinuxFuse,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live Google Docs virtual workspace");
    let virtual_pull = run_pull_with_state_root(
        &mut store,
        &connector,
        &virtual_mount_root,
        Some(&fixture.state_root),
    )
    .expect("pull live Google Docs virtual workspace");
    assert!(virtual_pull.ok, "{virtual_pull:#?}");

    let virtual_mount = MountConfig::new(
        virtual_mount_id.clone(),
        GOOGLE_DOCS_CONNECTOR_ID,
        virtual_mount_root.clone(),
    )
    .with_remote_root_id(RemoteId::new(workspace.id.clone()))
    .with_connection_id(connection_id.clone())
    .projection(ProjectionMode::LinuxFuse);
    let content_root = virtual_fs_content_root(&fixture.state_root, &virtual_mount_id);
    let mount_point_root = mount_point_identifier(&virtual_mount);
    let second_doc_item = refresh_virtual_children_until_remote_item(
        &mut store,
        &connector,
        &content_root,
        &virtual_mount_id,
        &mount_point_root,
        &second_doc.id,
        VirtualFsItemKind::Folder,
    );
    assert_eq!(
        second_doc_item.identifier,
        format!("children:{}", second_doc.id)
    );
    let archive_item = refresh_virtual_children_until_remote_item(
        &mut store,
        &connector,
        &content_root,
        &virtual_mount_id,
        &mount_point_root,
        &archive_folder.id,
        VirtualFsItemKind::Folder,
    );
    materialize_virtual_fs_item_with_content_root(
        &mut store,
        &connector,
        &content_root,
        &virtual_mount_id,
        &second_doc.id,
    )
    .expect("materialize second Google Doc");

    let renamed = rename_virtual_fs_item(
        &mut store,
        &content_root,
        &virtual_mount_id,
        &second_doc_item.identifier,
        &archive_item.identifier,
        "Renamed Plan",
    )
    .expect("move and rename second Google Doc into Archive");
    assert_eq!(renamed.identifier, format!("children:{}", second_doc.id));
    assert_eq!(renamed.item.kind, VirtualFsItemKind::Folder);
    assert_eq!(renamed.item.path, "Archive/Renamed Plan");
    let renamed_page_path = virtual_mount_root.join(&renamed.item.path).join("page.md");
    let move_diff = run_diff_with_state_root(&store, &renamed_page_path, Some(&fixture.state_root))
        .expect("diff Google Docs virtual move");
    assert!(move_diff.ok, "{move_diff:#?}");
    assert_eq!(move_diff.action, "confirm_plan", "{move_diff:#?}");
    let move_plan = move_diff.plan.as_ref().expect("Google Docs move plan");
    assert_eq!(move_plan.summary.entities_moved, 1, "{move_plan:#?}");

    let move_push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &renamed_page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push Google Docs virtual move");
    assert!(move_push.ok, "{move_push:#?}");
    assert_eq!(move_push.action, "reconciled", "{move_push:#?}");
    assert_eq!(move_push.changed_remote_ids, vec![second_doc.id.clone()]);
    let moved_file = api
        .get_file(&second_doc.id)
        .expect("fetch moved Google Drive file");
    assert_eq!(moved_file.name, "Renamed Plan");
    assert_eq!(moved_file.parents, vec![archive_folder.id.clone()]);

    let trashed = trash_virtual_fs_item(
        &mut store,
        &content_root,
        &virtual_mount_id,
        &renamed.identifier,
    )
    .expect("trash moved Google Doc through virtual filesystem");
    assert_eq!(trashed.identifier, format!("children:{}", second_doc.id));
    let trash_diff =
        run_diff_with_state_root(&store, &virtual_mount_root, Some(&fixture.state_root))
            .expect("diff Google Docs virtual trash");
    assert!(trash_diff.ok, "{trash_diff:#?}");
    assert_eq!(trash_diff.action, "confirm_plan", "{trash_diff:#?}");
    let trash_plan = trash_diff.plan.as_ref().expect("Google Docs trash plan");
    assert_eq!(trash_plan.summary.entities_archived, 1, "{trash_plan:#?}");

    let trash_push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &virtual_mount_root,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: true,
        },
        Some(&fixture.state_root),
    )
    .expect("push Google Docs virtual trash");
    assert!(trash_push.ok, "{trash_push:#?}");
    assert_eq!(trash_push.action, "reconciled", "{trash_push:#?}");
    assert_eq!(trash_push.changed_remote_ids, vec![second_doc.id.clone()]);
    let trashed_file = api
        .get_file(&second_doc.id)
        .expect("fetch trashed Google Drive file");
    assert!(trashed_file.trashed, "{trashed_file:#?}");

    let virtual_status = run_status(
        &store,
        StatusOptions {
            path: Some(virtual_mount_root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("virtual status after move/trash");
    assert!(virtual_status.clean, "{virtual_status:#?}");
}

#[test]
#[ignore = "requires LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON with broker refresh handle and LOCALITY_GMAIL_LIVE_TEST_RECIPIENT; sends a scratch Gmail message"]
fn live_gmail_pull_send_read_only_guardrail_and_threads_round_trip() {
    if !gmail_live_env_available() {
        eprintln!(
            "skipping live Gmail e2e: set {LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON} and {LOCALITY_GMAIL_LIVE_TEST_RECIPIENT}"
        );
        return;
    }

    let fixture = LiveFixture::new("loc-live-gmail");
    let connection_id = ConnectionId::new("gmail-live");
    seed_live_connection(
        &fixture.state_root,
        GoogleLiveConnector::Gmail,
        &connection_id,
    );

    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open state store");
    let messages_mount_id = MountId::new("gmail-main");
    let messages_mount_root = fixture.mount_root.join("gmail-main");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: messages_mount_id.clone(),
            connector: GMAIL_CONNECTOR_ID.to_string(),
            root: messages_mount_root.clone(),
            remote_root_id: None,
            connection_id: Some(connection_id.clone()),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: gmail_live_settings_json(),
        },
    )
    .expect("mount live Gmail messages view");
    let messages_connector =
        resolve_gmail_mount_from_store(&fixture.state_root, messages_mount_id.clone());
    let messages_pull = run_pull_with_state_root(
        &mut store,
        &messages_connector,
        &messages_mount_root,
        Some(&fixture.state_root),
    )
    .expect("pull live Gmail messages view");
    assert!(messages_pull.ok, "{messages_pull:#?}");
    assert!(messages_mount_root.join("inbox").is_dir());
    assert!(messages_mount_root.join("sent").is_dir());
    assert!(messages_mount_root.join("draft").is_dir());

    let marker = format!(
        "loc-live-gmail-{}-{}",
        std::process::id(),
        timestamp_millis_string()
    );
    let subject = format!("Locality live Gmail e2e {marker}");
    let draft_path = messages_mount_root
        .join("draft")
        .join(format!("{marker}.md"));
    fs::write(
        &draft_path,
        format!(
            "---\nto:\n  - \"{}\"\nsubject: \"{}\"\n---\n{}\n",
            live_gmail_recipient(),
            subject,
            GMAIL_LIVE_BODY
        ),
    )
    .expect("write live Gmail draft");

    let create_diff = run_diff_with_state_root(&store, &draft_path, Some(&fixture.state_root))
        .expect("diff Gmail draft create");
    assert!(create_diff.ok, "{create_diff:#?}");
    assert_eq!(create_diff.action, "confirm_plan", "{create_diff:#?}");
    let create_plan = create_diff.plan.as_ref().expect("Gmail create plan");
    assert_eq!(create_plan.summary.entities_created, 1, "{create_plan:#?}");

    let create_push = run_push_with_daemon_at_state_root(
        &mut store,
        &messages_connector,
        &draft_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push Gmail draft send");
    assert!(create_push.ok, "{create_push:#?}");
    assert_eq!(create_push.action, "reconciled", "{create_push:#?}");
    assert_eq!(create_push.apply_effect_count, 1, "{create_push:#?}");
    assert!(
        !draft_path.exists(),
        "sent Gmail draft should be reconciled away from draft/"
    );

    let sent_verification_mount_id = MountId::new("gmail-messages-verify");
    let sent_verification_mount_root = fixture.mount_root.join("gmail-messages-verify");
    mount_live_gmail_plain_files(
        &mut store,
        sent_verification_mount_id.clone(),
        sent_verification_mount_root.clone(),
        connection_id.clone(),
        gmail_live_settings_json(),
        "mount live Gmail messages verification view",
    );
    let sent_verification_connector =
        resolve_gmail_mount_from_store(&fixture.state_root, sent_verification_mount_id.clone());
    let sent_file = pull_until_hydrated_file_containing(
        &mut store,
        &sent_verification_connector,
        &sent_verification_mount_root,
        Some(&fixture.state_root),
        &sent_verification_mount_root.join("sent"),
        &marker,
    )
    .expect("sent Gmail message containing marker");
    let sent_contents = fs::read_to_string(&sent_file).expect("read sent Gmail message");
    assert!(sent_contents.contains(&marker), "{sent_contents}");
    assert!(
        sent_contents.contains("connector: gmail"),
        "{sent_contents}"
    );
    assert!(sent_contents.contains("gmail:"), "{sent_contents}");
    assert!(
        sent_contents.contains("mailbox: \"sent\""),
        "{sent_contents}"
    );
    assert!(sent_contents.contains("message_id:"), "{sent_contents}");
    assert!(sent_contents.contains("thread_id:"), "{sent_contents}");
    assert!(sent_contents.contains(GMAIL_LIVE_BODY), "{sent_contents}");
    assert!(
        !sent_contents.contains(CanonicalDocument::STUB_MARKER),
        "{sent_contents}"
    );

    fs::write(
        &sent_file,
        format!("{sent_contents}\nLocal edit that must stay read-only.\n"),
    )
    .expect("edit sent Gmail message");
    let read_only_diff = run_diff_with_state_root(&store, &sent_file, Some(&fixture.state_root))
        .expect("diff read-only Gmail sent edit");
    assert!(!read_only_diff.ok, "{read_only_diff:#?}");
    assert!(
        read_only_diff
            .validation
            .iter()
            .any(|issue| issue.code == "gmail_read_only_mailbox"),
        "{read_only_diff:#?}"
    );

    let threads_mount_id = MountId::new("gmail-threads");
    let threads_mount_root = fixture.mount_root.join("gmail-threads");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: threads_mount_id.clone(),
            connector: GMAIL_CONNECTOR_ID.to_string(),
            root: threads_mount_root.clone(),
            remote_root_id: None,
            connection_id: Some(connection_id.clone()),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: gmail_thread_settings_json(),
        },
    )
    .expect("mount live Gmail threads view");
    let threads_connector =
        resolve_gmail_mount_from_store(&fixture.state_root, threads_mount_id.clone());
    let thread_file = pull_until_hydrated_file_containing(
        &mut store,
        &threads_connector,
        &threads_mount_root,
        Some(&fixture.state_root),
        &threads_mount_root.join("sent"),
        &marker,
    )
    .expect("sent Gmail thread view containing marker");
    let thread_contents = fs::read_to_string(&thread_file).expect("read Gmail thread view file");
    assert!(thread_contents.contains(&marker), "{thread_contents}");
    assert!(
        thread_contents.contains("connector: gmail"),
        "{thread_contents}"
    );
    assert!(thread_contents.contains("gmail:"), "{thread_contents}");
    assert!(thread_contents.contains("thread_id:"), "{thread_contents}");
    assert!(
        thread_contents.contains(GMAIL_LIVE_BODY),
        "{thread_contents}"
    );
    assert!(
        !thread_contents.contains(CanonicalDocument::STUB_MARKER),
        "{thread_contents}"
    );
}

#[test]
fn live_gmail_recipient_validation_requires_single_email_like_value() {
    assert!(valid_live_gmail_recipient("locality.live+test@example.com"));

    for invalid in [
        "",
        "locality.live+test",
        "locality.live+test@example",
        "locality.live+test@example..com",
        "locality.live+test@",
        "@example.com",
        "locality live@example.com",
        " locality.live+test@example.com",
        "locality.live+test@example.com ",
        "locality.live+test@example.com\n",
        "locality.live+test@example.com\t",
        "locality.live+test@example.com,other@example.com",
        "locality.live+test@example.com;other@example.com",
        "locality.live+test@example.com other@example.com",
        "locality.live+test@@example.com",
        "Locality <locality.live+test@example.com>",
        "locality.live+test@example.com (Locality)",
    ] {
        assert!(
            !valid_live_gmail_recipient(invalid),
            "recipient should be rejected: {invalid:?}"
        );
    }
}

#[test]
fn gmail_default_date_window_uses_yesterday_and_tomorrow_utc() {
    let noon_epoch = UNIX_EPOCH
        .checked_add(Duration::from_secs(12 * 60 * 60))
        .expect("noon epoch");

    assert_eq!(
        gmail_default_date_window(noon_epoch),
        ("1969-12-31".to_string(), "1970-01-02".to_string())
    );
}

fn stored_google_docs_secret() -> String {
    let secret = required_env(LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON);
    let stored =
        serde_json::from_str::<StoredGoogleDocsCredential>(&secret).expect("Google Docs secret");
    assert_eq!(stored.connector, GOOGLE_DOCS_CONNECTOR_ID);
    assert_eq!(stored.kind, "oauth");
    assert_broker_url(stored.oauth_broker_url.as_deref(), "Google Docs");
    assert_refresh_handle(stored.refresh_token_handle.as_deref());
    secret
}

fn stored_gmail_secret() -> String {
    let secret = required_env(LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON);
    let stored = serde_json::from_str::<StoredGmailCredential>(&secret).expect("Gmail secret");
    assert_eq!(stored.connector, GMAIL_CONNECTOR_ID);
    assert_eq!(stored.kind, "oauth");
    assert_broker_url(stored.oauth_broker_url.as_deref(), "Gmail");
    assert_refresh_handle(stored.refresh_token_handle.as_deref());
    secret
}

fn force_expired_secret(connector: GoogleLiveConnector, secret: &str) -> String {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            let mut stored =
                serde_json::from_str::<StoredGoogleDocsCredential>(secret).expect("Google Docs");
            assert_eq!(stored.connector, GOOGLE_DOCS_CONNECTOR_ID);
            assert_eq!(stored.kind, "oauth");
            assert_broker_url(stored.oauth_broker_url.as_deref(), "Google Docs");
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
            stored.access_token = EXPIRED_SENTINEL_ACCESS_TOKEN.to_string();
            stored.acquired_at = 1;
            stored.expires_at = Some(1);
            serde_json::to_string(&stored).expect("serialize expired Google Docs credential")
        }
        GoogleLiveConnector::Gmail => {
            let mut stored = serde_json::from_str::<StoredGmailCredential>(secret).expect("Gmail");
            assert_eq!(stored.connector, GMAIL_CONNECTOR_ID);
            assert_eq!(stored.kind, "oauth");
            assert_broker_url(stored.oauth_broker_url.as_deref(), "Gmail");
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
            stored.access_token = EXPIRED_SENTINEL_ACCESS_TOKEN.to_string();
            stored.acquired_at = 1;
            stored.expires_at = Some(1);
            serde_json::to_string(&stored).expect("serialize expired Gmail credential")
        }
    }
}

fn live_account_label(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .account_label
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .account_label
        }
    }
}

fn live_workspace_id(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .workspace_id
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .workspace_id
        }
    }
}

fn live_workspace_name(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .workspace_name
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .workspace_name
        }
    }
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} to run live Google connector tests"))
}

fn timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .to_string()
}

fn timestamp_millis_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        .to_string()
}

fn resolve_google_docs_from_store(
    state_root: &Path,
    connection_id: ConnectionId,
    workspace_folder_id: RemoteId,
) -> GoogleDocsConnector {
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state store");
    let credentials = FileCredentialStore::new(state_root);
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let mount = MountConfig::new(
        MountId::new("google-docs-main"),
        GOOGLE_DOCS_CONNECTOR_ID,
        mount_root_for_state_root(state_root).join("google-docs-main"),
    )
    .with_remote_root_id(workspace_folder_id.clone())
    .with_connection_id(connection_id.clone())
    .projection(ProjectionMode::PlainFiles);
    store
        .save_mount(mount.clone())
        .expect("save Google Docs mount");

    let source =
        resolve_source_for_mount(&store, &credentials, &mount).expect("resolve Google Docs");
    let ResolvedSource::GoogleDocs(connector) = source else {
        panic!("expected Google Docs connector");
    };
    assert_eq!(
        connector.config().workspace_folder_id.as_ref(),
        Some(&workspace_folder_id)
    );
    assert_ne!(
        connector.config().access_token,
        EXPIRED_SENTINEL_ACCESS_TOKEN,
        "Google Docs token was not refreshed"
    );
    assert_forced_refresh_persisted(&credentials, GoogleLiveConnector::GoogleDocs, &secret_ref);
    connector
}

fn resolve_gmail_from_store(state_root: &Path, connection_id: ConnectionId) -> GmailConnector {
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state store");
    let mount = MountConfig::new(
        MountId::new("gmail-main"),
        GMAIL_CONNECTOR_ID,
        mount_root_for_state_root(state_root).join("gmail-main"),
    )
    .with_connection_id(connection_id.clone())
    .projection(ProjectionMode::PlainFiles);
    store.save_mount(mount.clone()).expect("save Gmail mount");

    resolve_gmail_mount_from_store(state_root, mount.mount_id)
}

fn resolve_gmail_mount_from_store(state_root: &Path, mount_id: MountId) -> GmailConnector {
    let store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state store");
    let mount = store
        .get_mount(&mount_id)
        .expect("load Gmail mount")
        .unwrap_or_else(|| panic!("missing Gmail mount `{}`", mount_id.as_str()));
    let credentials = FileCredentialStore::new(state_root);
    let connection_id = mount.connection_id.as_ref().unwrap_or_else(|| {
        panic!(
            "Gmail mount `{}` must have connection_id",
            mount_id.as_str()
        )
    });
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let source = resolve_source_for_mount(&store, &credentials, &mount).expect("resolve Gmail");
    let ResolvedSource::Gmail(connector) = source else {
        panic!("expected Gmail connector");
    };
    assert_ne!(
        connector.config().access_token,
        EXPIRED_SENTINEL_ACCESS_TOKEN,
        "Gmail token was not refreshed"
    );
    assert_forced_refresh_persisted(&credentials, GoogleLiveConnector::Gmail, &secret_ref);
    connector
}

fn gmail_live_settings_json() -> String {
    gmail_settings_json(GmailProjectionView::Messages)
}

fn gmail_thread_settings_json() -> String {
    gmail_settings_json(GmailProjectionView::Threads)
}

fn gmail_settings_json(view: GmailProjectionView) -> String {
    let (after, before) = gmail_live_date_window();
    GmailMountSettings::with_date_window(&after, &before)
        .expect("valid Gmail live date window")
        .with_view(view)
        .to_json()
        .expect("serialize Gmail live settings")
}

fn gmail_live_date_window() -> (String, String) {
    static WINDOW: OnceLock<(String, String)> = OnceLock::new();
    WINDOW
        .get_or_init(|| {
            let (default_after, default_before) = gmail_default_date_window(SystemTime::now());
            let after = std::env::var(LOCALITY_GMAIL_LIVE_AFTER).unwrap_or(default_after);
            let before = std::env::var(LOCALITY_GMAIL_LIVE_BEFORE).unwrap_or(default_before);
            (after, before)
        })
        .clone()
}

fn gmail_default_date_window(now: SystemTime) -> (String, String) {
    let days = now
        .duration_since(UNIX_EPOCH)
        .expect("Gmail live date window requires time after Unix epoch")
        .as_secs() as i64
        / 86_400;
    (
        utc_date_string_from_unix_days(days - 1),
        utc_date_string_from_unix_days(days + 1),
    )
}

fn utc_date_string_from_unix_days(days: i64) -> String {
    let (year, month, day) = civil_date_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_date_from_unix_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn gmail_live_env_available() -> bool {
    std::env::var_os(LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON).is_some()
        && std::env::var_os(LOCALITY_GMAIL_LIVE_TEST_RECIPIENT).is_some()
}

fn live_gmail_recipient() -> String {
    let recipient = required_env(LOCALITY_GMAIL_LIVE_TEST_RECIPIENT);
    assert!(
        valid_live_gmail_recipient(&recipient),
        "{LOCALITY_GMAIL_LIVE_TEST_RECIPIENT} must be a single email-like recipient"
    );
    recipient
}

fn valid_live_gmail_recipient(value: &str) -> bool {
    if value.is_empty()
        || value.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || is_gmail_recipient_delimiter(character)
        })
        || value.matches('@').count() != 1
    {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !local.starts_with('.')
        && !local.ends_with('.')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain.split('.').all(|part| !part.is_empty())
}

fn is_gmail_recipient_delimiter(character: char) -> bool {
    matches!(
        character,
        ',' | ';' | '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'
    )
}

fn find_file_containing(root: &Path, needle: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            if fs::read_to_string(&path)
                .ok()
                .is_some_and(|contents| contents.contains(needle))
            {
                return Some(path);
            }
        }
    }
    None
}

fn pull_until_hydrated_file_containing(
    store: &mut SqliteStateStore,
    connector: &GmailConnector,
    mount_root: &Path,
    state_root: Option<&Path>,
    search_root: &Path,
    needle: &str,
) -> Option<PathBuf> {
    for _ in 0..8 {
        let pull = run_pull_with_state_root(store, connector, mount_root, state_root)
            .expect("pull Gmail while waiting for sent message");
        assert!(pull.ok, "{pull:#?}");
        if let Some(path) = find_file_containing(search_root, needle) {
            let hydrate = run_pull_with_state_root(store, connector, &path, state_root)
                .expect("hydrate Gmail file containing marker");
            assert!(hydrate.ok, "{hydrate:#?}");
            let contents = fs::read_to_string(&path).expect("read hydrated Gmail file");
            if contents.contains(needle) && !contents.contains(CanonicalDocument::STUB_MARKER) {
                return Some(path);
            }
        }
        std::thread::sleep(Duration::from_millis(750));
    }
    None
}

fn mount_live_gmail_plain_files(
    store: &mut SqliteStateStore,
    mount_id: MountId,
    root: PathBuf,
    connection_id: ConnectionId,
    settings_json: String,
    context: &str,
) {
    run_mount(
        store,
        MountOptions {
            mount_id,
            connector: GMAIL_CONNECTOR_ID.to_string(),
            root,
            remote_root_id: None,
            connection_id: Some(connection_id),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json,
        },
    )
    .unwrap_or_else(|error| panic!("{context}: {error:?}"));
}

fn assert_broker_url(oauth_broker_url: Option<&str>, connector_name: &str) {
    let oauth_broker_url = oauth_broker_url.unwrap_or_else(|| {
        panic!("{connector_name} credential must include oauth_broker_url");
    });
    assert!(
        !oauth_broker_url.trim().is_empty(),
        "{connector_name} credential oauth_broker_url cannot be empty"
    );
}

fn assert_refresh_handle(refresh_token_handle: Option<&str>) {
    let refresh_token_handle = refresh_token_handle.expect("refresh token handle");
    assert!(
        refresh_token_handle.starts_with("locrh_v1."),
        "refresh token handle must be a locrh_v1 handle"
    );
}

fn assert_forced_refresh_persisted(
    credentials: &dyn CredentialStore,
    connector: GoogleLiveConnector,
    secret_ref: &str,
) {
    if std::env::var(LOCALITY_GOOGLE_LIVE_FORCE_REFRESH)
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }

    let secret = credentials
        .get(secret_ref)
        .expect("read refreshed live credential");
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            let stored = serde_json::from_str::<StoredGoogleDocsCredential>(&secret)
                .expect("refreshed Google Docs credential");
            assert_ne!(stored.access_token, EXPIRED_SENTINEL_ACCESS_TOKEN);
            assert_ne!(stored.acquired_at, 1);
            assert_ne!(stored.expires_at, Some(1));
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
        }
        GoogleLiveConnector::Gmail => {
            let stored = serde_json::from_str::<StoredGmailCredential>(&secret)
                .expect("refreshed Gmail credential");
            assert_ne!(stored.access_token, EXPIRED_SENTINEL_ACCESS_TOKEN);
            assert_ne!(stored.acquired_at, 1);
            assert_ne!(stored.expires_at, Some(1));
            assert_refresh_handle(stored.refresh_token_handle.as_deref());
        }
    }
}

fn connector_id(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => GOOGLE_DOCS_CONNECTOR_ID,
        GoogleLiveConnector::Gmail => GMAIL_CONNECTOR_ID,
    }
}

fn connector_display_name(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => "Google Docs",
        GoogleLiveConnector::Gmail => "Gmail",
    }
}

fn connector_version(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => "google-docs.v1",
        GoogleLiveConnector::Gmail => "gmail.v1",
    }
}

fn connector_enabled_actions_json(connector: GoogleLiveConnector) -> &'static str {
    match connector {
        GoogleLiveConnector::GoogleDocs => "[\"read\",\"write\"]",
        GoogleLiveConnector::Gmail => "[\"read\",\"send\"]",
    }
}

fn connector_scopes(connector: GoogleLiveConnector) -> Vec<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => GOOGLE_DOCS_OAUTH_SCOPES,
        GoogleLiveConnector::Gmail => GMAIL_OAUTH_SCOPES,
    }
    .iter()
    .map(|scope| scope.to_string())
    .collect()
}

fn connector_capabilities_json(connector: GoogleLiveConnector) -> String {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            google_docs_capabilities_json().expect("Google Docs capabilities")
        }
        GoogleLiveConnector::Gmail => gmail_capabilities_json().expect("Gmail capabilities"),
    }
}

fn credential_expires_at(connector: GoogleLiveConnector, secret: &str) -> Option<SystemTime> {
    let expires_at = match connector {
        GoogleLiveConnector::GoogleDocs => {
            serde_json::from_str::<StoredGoogleDocsCredential>(secret)
                .expect("Google Docs credential")
                .expires_at
        }
        GoogleLiveConnector::Gmail => {
            serde_json::from_str::<StoredGmailCredential>(secret)
                .expect("Gmail credential")
                .expires_at
        }
    }?;
    UNIX_EPOCH.checked_add(std::time::Duration::from_secs(expires_at))
}

fn mount_root_for_state_root(state_root: &Path) -> PathBuf {
    state_root
        .parent()
        .map(|root| root.join("Locality"))
        .unwrap_or_else(|| state_root.join("Locality"))
}

fn google_document_text(document: &GoogleDocument) -> String {
    let mut text = String::new();
    collect_google_structural_text(&document.body.content, &mut text);
    text
}

fn collect_google_structural_text(elements: &[StructuralElement], text: &mut String) {
    for element in elements {
        if let Some(paragraph) = &element.paragraph {
            for paragraph_element in &paragraph.elements {
                if let Some(text_run) = &paragraph_element.text_run {
                    text.push_str(&text_run.content);
                }
            }
        }
        if let Some(table) = &element.table {
            for row in &table.table_rows {
                for cell in &row.table_cells {
                    collect_google_structural_text(&cell.content, text);
                }
            }
        }
    }
}

fn refresh_virtual_children_until_remote_item(
    store: &mut SqliteStateStore,
    connector: &GoogleDocsConnector,
    content_root: &Path,
    mount_id: &MountId,
    container_identifier: &str,
    remote_id: &str,
    kind: VirtualFsItemKind,
) -> VirtualFsItem {
    let mut last_children = None;
    for _ in 0..8 {
        refresh_virtual_fs_children(store, connector, mount_id, container_identifier)
            .expect("refresh Google Docs virtual children");
        let report = virtual_fs_children_with_content_root(
            store,
            content_root,
            mount_id,
            container_identifier,
        )
        .expect("list Google Docs virtual children after refresh");
        if let Some(item) = report
            .children
            .iter()
            .find(|item| item.remote_id.as_deref() == Some(remote_id) && item.kind == kind)
            .cloned()
        {
            return item;
        }
        last_children = Some(report.children);
        std::thread::sleep(Duration::from_millis(500));
    }

    panic!(
        "missing virtual {kind:?} for {remote_id} after refreshed parent `{container_identifier}`: {:#?}",
        last_children.unwrap_or_default()
    );
}
