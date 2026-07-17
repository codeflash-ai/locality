# Gmail And Google Docs Live E2E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add real CI-backed live e2e coverage for the Gmail and Google Docs connectors, including auth refresh, mounted filesystem workflows, and the major read/write guardrails.

**Architecture:** Add a focused live Google connector test file instead of expanding the already-large Notion workflow test. CI seeds isolated Locality state with broker-backed stored credential JSON, forces refresh through the hosted OAuth broker, then runs live connector and mounted workflow tests against disposable Google Docs scratch folders and a dedicated Gmail test mailbox.

**Tech Stack:** Rust integration tests, Locality `SqliteStateStore` and `FileCredentialStore`, Gmail and Google Docs HTTP connector clients, Linux FUSE smoke scripts, GitHub Actions environments, existing OAuth broker refresh handles.

---

## Scope Check

Gmail and Google Docs are different connectors, but this is one plan because the CI authentication, stored credential seeding, workflow shape, and docs coverage are shared. The connector-specific tests stay independent inside the same live Google suite so either connector can be run alone with an exact test filter.

## File Structure

- Create `.github/workflows/google-live-e2e.yml`: CI workflow for live Google Docs, live Gmail, and Linux FUSE mounted workflow jobs.
- Create `crates/loc-cli/tests/live_google_connectors.rs`: ignored live e2e tests plus helper code for seeding broker-backed Google credentials, creating isolated stores, forcing refresh, and cleaning scratch resources.
- Modify `crates/localityd/tests/source_descriptor.rs`: add the missing positive Gmail expired-credential refresh regression test.
- Create `tests/live_google_docs_vfs_push_pull.sh`: product-path CLI, daemon, and Linux FUSE test for Google Docs workspace browse, create, edit, move, and archive.
- Create `tests/live_gmail_vfs_read_send.sh`: product-path CLI, daemon, and Linux FUSE test for Gmail mailbox browse, read-only guardrails, draft send, sent reconciliation, and thread projection.
- Modify `docs/e2e-behavior-coverage.md`: document how to run the live Google suite and add coverage rows for Gmail and Google Docs.
- Modify `docs/google-docs-connector.md`: add the live e2e fixture, secrets, cleanup, and CI behavior.
- Modify `docs/gmail-connector.md`: add the live e2e fixture, dedicated mailbox requirement, unavoidable sent-message retention, and CI behavior.

## CI Auth Decision

Use broker-backed stored credential JSON, not raw provider refresh tokens and not short-lived access-token-only secrets.

Required GitHub environment: `google-live-e2e`.

Required secrets:

- `LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON`: one-line JSON from the Locality credential file for `connection:google-docs-live`. It must contain `connector: "google-docs"`, `kind: "oauth"`, `oauth_broker_url`, `refresh_token_handle`, and all granted scopes.
- `LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON`: one-line JSON from the Locality credential file for `connection:gmail-live`. It must contain `connector: "gmail"`, `kind: "oauth"`, `oauth_broker_url`, `refresh_token_handle`, and the scoped Gmail readonly and compose grants.
- `LOCALITY_GMAIL_LIVE_TEST_RECIPIENT`: a mailbox controlled by the test account. Use the same Gmail account when possible so sent and received copies stay in a dedicated mailbox.

Optional secrets:

- `LOCALITY_AUTH_BROKER_URL`: only needed when testing a non-default broker.
- `LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_PREFIX`: defaults to `Locality live Google Docs e2e`.

Every CI job sets `LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1`. The seeding helper rewrites the stored access token to an expired sentinel while preserving `refresh_token_handle` and `oauth_broker_url`. The first resolver call must refresh through the broker and persist the new stored credential before any connector API call succeeds.

---

### Task 1: Add Positive Gmail Refresh Regression

**Files:**
- Modify: `crates/localityd/tests/source_descriptor.rs`

- [ ] **Step 1: Write the failing test**

Add this test near the existing expired Gmail credential tests, before `resolving_expired_gmail_credential_rejects_refresh_missing_required_scope`:

```rust
#[test]
fn resolving_expired_gmail_credential_refreshes_with_broker_handle() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let (connection_id, secret_ref) =
        save_gmail_connection(&mut store, "gmail-default", GMAIL_CONNECTOR_ID, "oauth");
    let refresh_response = serde_json::json!({
        "access_token": "new-gmail-access-token",
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token_handle": "handle-2",
        "account_id": "acct-1",
        "account_label": "user@example.com",
        "workspace_id": "gmail",
        "workspace_name": "Gmail",
        "scopes": GMAIL_OAUTH_SCOPES,
    })
    .to_string();
    let (broker_url, broker) = spawn_refresh_broker("HTTP/1.1 200 OK", refresh_response);
    let stored = expired_gmail_credential("expired-access-token", broker_url);
    credentials
        .put(
            &secret_ref,
            &serde_json::to_string(&stored).expect("credential json"),
        )
        .expect("save credential");
    let mount = gmail_mount().with_connection_id(connection_id);

    let source = resolve_source_for_mount(&store, &credentials, &mount).expect("resolve gmail");
    broker.join().expect("broker thread");

    let ResolvedSource::Gmail(connector) = source else {
        panic!("expected gmail source");
    };
    assert_eq!(connector.config().access_token, "new-gmail-access-token");
    let saved = credentials.get(&secret_ref).expect("saved credential");
    let saved = serde_json::from_str::<StoredGmailCredential>(&saved).expect("stored credential");
    assert_eq!(saved.access_token, "new-gmail-access-token");
    assert_eq!(saved.refresh_token_handle.as_deref(), Some("handle-2"));
}
```

- [ ] **Step 2: Run the focused test and verify it passes**

Run:

```bash
cargo test -p localityd --test source_descriptor resolving_expired_gmail_credential_refreshes_with_broker_handle -- --exact
```

Expected: the output ends with `test result: ok. 1 passed; 0 failed`.

- [ ] **Step 3: Commit**

```bash
git add crates/localityd/tests/source_descriptor.rs
git commit -m "test: cover gmail oauth refresh success"
```

---

### Task 2: Add Live Google Test Harness

**Files:**
- Create: `crates/loc-cli/tests/live_google_connectors.rs`

- [ ] **Step 1: Create the test file with auth and fixture helpers**

Create `crates/loc-cli/tests/live_google_connectors.rs` with these top-level imports, constants, and helper types. Keep the helper functions in this file so the live suite is self-contained.

```rust
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::diff::run_diff_with_state_root;
use loc_cli::mount::{MountOptions, run_mount};
use loc_cli::pull::run_pull_with_state_root;
use loc_cli::push::{PushOptions, run_push_with_daemon_at_state_root};
use loc_cli::status::{StatusOptions, run_status};
use locality_core::model::{MountId, RemoteId};
use locality_google_docs::client::{GoogleDocsApi, GoogleDriveApi, HttpGoogleApiClient};
use locality_google_docs::drive_dto::{DriveCreateFileRequest, DriveUpdateFileRequest};
use locality_google_docs::{
    GOOGLE_DOCS_CONNECTOR_ID, GOOGLE_DOCS_OAUTH_SCOPES, GoogleDocsConfig, GoogleDocsConnector,
    StoredGoogleDocsCredential, google_docs_capabilities_json,
};
use locality_gmail::{
    GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, GmailConfig, GmailConnector, GmailMountSettings,
    GmailProjectionView, StoredGmailCredential, gmail_capabilities_json,
};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorProfileRepository, CredentialStore, FileCredentialStore,
    MountConfig, MountRepository, ProjectionMode, SqliteStateStore,
};
use localityd::source::{ResolvedSource, resolve_source_for_mount};

const GOOGLE_DOCS_CREDENTIAL_ENV: &str = "LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON";
const GMAIL_CREDENTIAL_ENV: &str = "LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON";
const GMAIL_RECIPIENT_ENV: &str = "LOCALITY_GMAIL_LIVE_TEST_RECIPIENT";
const FORCE_REFRESH_ENV: &str = "LOCALITY_GOOGLE_LIVE_FORCE_REFRESH";

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
        if std::env::var("LOCALITY_GOOGLE_LIVE_KEEP_TMP").ok().as_deref() != Some("1") {
            let _ = fs::remove_dir_all(
                self.state_root
                    .parent()
                    .expect("fixture root parent"),
            );
        }
    }
}
```

- [ ] **Step 2: Add credential parsing and seeding helpers**

Append these helpers. They seed both the secret file and SQLite connection/profile records through the same repository traits used by production code.

```rust
fn seed_live_connection(
    state_root: &Path,
    connector: GoogleLiveConnector,
    connection_id: &ConnectionId,
) -> String {
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let raw_secret = match connector {
        GoogleLiveConnector::GoogleDocs => stored_google_docs_secret(),
        GoogleLiveConnector::Gmail => stored_gmail_secret(),
    };
    let secret = if std::env::var(FORCE_REFRESH_ENV).ok().as_deref() == Some("1") {
        force_expired_secret(connector, &raw_secret)
    } else {
        raw_secret
    };

    let credentials = FileCredentialStore::new(state_root);
    credentials
        .put(&secret_ref, &secret)
        .expect("seed live Google credential");

    let now = timestamp_string();
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open live state");
    let (profile_id, connector_id, display_name, scopes, capabilities, version) = match connector {
        GoogleLiveConnector::GoogleDocs => (
            ConnectorProfileId::new("google-docs-oauth-default"),
            GOOGLE_DOCS_CONNECTOR_ID,
            "Google Docs OAuth",
            GOOGLE_DOCS_OAUTH_SCOPES.iter().map(|scope| scope.to_string()).collect(),
            google_docs_capabilities_json().expect("google docs capabilities"),
            "google-docs.v1",
        ),
        GoogleLiveConnector::Gmail => (
            ConnectorProfileId::new("gmail-oauth-default"),
            GMAIL_CONNECTOR_ID,
            "Gmail OAuth",
            GMAIL_OAUTH_SCOPES.iter().map(|scope| scope.to_string()).collect(),
            gmail_capabilities_json().expect("gmail capabilities"),
            "gmail.v1",
        ),
    };

    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: connector_id.to_string(),
            display_name: display_name.to_string(),
            auth_kind: "oauth".to_string(),
            scopes: scopes.clone(),
            capabilities_json: capabilities.clone(),
            enabled_actions_json: "[\"read\",\"write\"]".to_string(),
            connector_version: version.to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed connector profile");

    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: connector_id.to_string(),
            display_name: connection_id.as_str().to_string(),
            account_label: live_account_label(connector, &secret),
            workspace_id: live_workspace_id(connector, &secret),
            workspace_name: live_workspace_name(connector, &secret),
            auth_kind: "oauth".to_string(),
            secret_ref: secret_ref.clone(),
            scopes,
            capabilities_json: capabilities,
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed connection");
    secret_ref
}

fn stored_google_docs_secret() -> String {
    let value = required_env(GOOGLE_DOCS_CREDENTIAL_ENV);
    let stored =
        serde_json::from_str::<StoredGoogleDocsCredential>(&value).expect("google docs credential json");
    assert_eq!(stored.connector, GOOGLE_DOCS_CONNECTOR_ID);
    assert_eq!(stored.kind, "oauth");
    assert!(
        stored.refresh_token_handle.as_deref().is_some_and(|handle| handle.starts_with("locrh_v1.")),
        "google docs live credential must contain an opaque broker refresh handle"
    );
    value
}

fn stored_gmail_secret() -> String {
    let value = required_env(GMAIL_CREDENTIAL_ENV);
    let stored = serde_json::from_str::<StoredGmailCredential>(&value).expect("gmail credential json");
    assert_eq!(stored.connector, GMAIL_CONNECTOR_ID);
    assert_eq!(stored.kind, "oauth");
    assert!(
        stored.refresh_token_handle.as_deref().is_some_and(|handle| handle.starts_with("locrh_v1.")),
        "gmail live credential must contain an opaque broker refresh handle"
    );
    value
}

fn force_expired_secret(connector: GoogleLiveConnector, secret: &str) -> String {
    match connector {
        GoogleLiveConnector::GoogleDocs => {
            let mut stored =
                serde_json::from_str::<StoredGoogleDocsCredential>(secret).expect("google docs credential");
            stored.access_token = "expired-google-docs-live-access-token".to_string();
            stored.acquired_at = 1;
            stored.expires_at = Some(1);
            serde_json::to_string(&stored).expect("serialize expired google docs credential")
        }
        GoogleLiveConnector::Gmail => {
            let mut stored = serde_json::from_str::<StoredGmailCredential>(secret).expect("gmail credential");
            stored.access_token = "expired-gmail-live-access-token".to_string();
            stored.acquired_at = 1;
            stored.expires_at = Some(1);
            serde_json::to_string(&stored).expect("serialize expired gmail credential")
        }
    }
}

fn live_account_label(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => serde_json::from_str::<StoredGoogleDocsCredential>(secret)
            .expect("google docs credential")
            .account_label,
        GoogleLiveConnector::Gmail => serde_json::from_str::<StoredGmailCredential>(secret)
            .expect("gmail credential")
            .account_label,
    }
}

fn live_workspace_id(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => serde_json::from_str::<StoredGoogleDocsCredential>(secret)
            .expect("google docs credential")
            .workspace_id,
        GoogleLiveConnector::Gmail => serde_json::from_str::<StoredGmailCredential>(secret)
            .expect("gmail credential")
            .workspace_id,
    }
}

fn live_workspace_name(connector: GoogleLiveConnector, secret: &str) -> Option<String> {
    match connector {
        GoogleLiveConnector::GoogleDocs => serde_json::from_str::<StoredGoogleDocsCredential>(secret)
            .expect("google docs credential")
            .workspace_name,
        GoogleLiveConnector::Gmail => serde_json::from_str::<StoredGmailCredential>(secret)
            .expect("gmail credential")
            .workspace_name,
    }
}
```

- [ ] **Step 3: Add resolver helpers that prove refresh occurred**

Append:

```rust
fn resolve_google_docs_from_store(
    state_root: &Path,
    connection_id: ConnectionId,
    workspace_folder_id: RemoteId,
) -> GoogleDocsConnector {
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state");
    let mount = MountConfig::new(
        MountId::new("google-docs-live"),
        GOOGLE_DOCS_CONNECTOR_ID,
        state_root.join("google-docs-live"),
    )
    .with_connection_id(connection_id)
    .with_remote_root_id(workspace_folder_id);
    let credentials = FileCredentialStore::new(state_root);
    let source = resolve_source_for_mount(&store, &credentials, &mount).expect("resolve google docs");
    let ResolvedSource::GoogleDocs(connector) = source else {
        panic!("expected google docs connector");
    };
    if std::env::var(FORCE_REFRESH_ENV).ok().as_deref() == Some("1") {
        assert_ne!(connector.config().access_token, "expired-google-docs-live-access-token");
    }
    store.save_mount(mount).expect("save google docs mount for status paths");
    connector
}

fn resolve_gmail_from_store(state_root: &Path, connection_id: ConnectionId) -> GmailConnector {
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open state");
    let mount = MountConfig::new(
        MountId::new("gmail-live"),
        GMAIL_CONNECTOR_ID,
        state_root.join("gmail-live"),
    )
    .with_connection_id(connection_id);
    let credentials = FileCredentialStore::new(state_root);
    let source = resolve_source_for_mount(&store, &credentials, &mount).expect("resolve gmail");
    let ResolvedSource::Gmail(connector) = source else {
        panic!("expected gmail connector");
    };
    if std::env::var(FORCE_REFRESH_ENV).ok().as_deref() == Some("1") {
        assert_ne!(connector.config().access_token, "expired-gmail-live-access-token");
    }
    store.save_mount(mount).expect("save gmail mount for status paths");
    connector
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("set {name} to run live Google connector tests"))
        .trim()
        .to_string()
}

fn timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .to_string()
}
```

- [ ] **Step 4: Run the ignored test file without live tests**

Run:

```bash
cargo test -p loc-cli --test live_google_connectors -- --ignored --exact __no_such_live_google_test__
```

Expected: the file compiles and the output ends with `0 filtered out` or `0 passed`; compilation failures must be fixed before adding live cases.

- [ ] **Step 5: Commit**

```bash
git add crates/loc-cli/tests/live_google_connectors.rs
git commit -m "test: add live google connector harness"
```

---

### Task 3: Add Live Google Docs Connector E2E

**Files:**
- Modify: `crates/loc-cli/tests/live_google_connectors.rs`

- [ ] **Step 1: Add the live Google Docs test**

Append this ignored test. It creates one disposable Drive workspace folder, mounts that folder, creates a document through Locality, edits it through mounted Markdown, moves and renames it into a Drive subfolder, archives it, verifies remote state through Drive and Docs APIs, and trashes the scratch workspace in cleanup.

```rust
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

    let bootstrap_secret = stored_google_docs_secret();
    let bootstrap = if std::env::var(FORCE_REFRESH_ENV).ok().as_deref() == Some("1") {
        serde_json::from_str::<StoredGoogleDocsCredential>(&force_expired_secret(
            GoogleLiveConnector::GoogleDocs,
            &bootstrap_secret,
        ))
        .expect("forced google docs credential")
    } else {
        serde_json::from_str::<StoredGoogleDocsCredential>(&bootstrap_secret)
            .expect("google docs credential")
    };
    let bootstrap_api = HttpGoogleApiClient::new(bootstrap.access_token);
    let workspace_name = format!(
        "{} {}",
        std::env::var("LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_PREFIX")
            .unwrap_or_else(|_| "Locality live Google Docs e2e".to_string()),
        timestamp_string()
    );
    let workspace = bootstrap_api
        .create_file(DriveCreateFileRequest::folder(&workspace_name, None))
        .expect("create scratch workspace folder");

    let connector = resolve_google_docs_from_store(
        &fixture.state_root,
        connection_id.clone(),
        RemoteId::new(workspace.id.clone()),
    );
    let api = HttpGoogleApiClient::new(connector.config().access_token.clone());

    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open state");
    let mount_root = fixture.mount_root.join("google-docs-main");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("google-docs-main"),
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            root: mount_root.clone(),
            remote_root_id: Some(RemoteId::new(workspace.id.clone())),
            connection_id: Some(connection_id),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live google docs");

    let pull = run_pull_with_state_root(&mut store, &connector, &mount_root, Some(&fixture.state_root))
        .expect("pull empty live google docs workspace");
    assert!(pull.ok, "{pull:#?}");

    let page_dir = mount_root.join("draft-plan");
    fs::create_dir_all(&page_dir).expect("create page directory");
    let page_path = page_dir.join("page.md");
    fs::write(
        &page_path,
        "---\ntitle: Draft Plan\n---\n# Draft Plan\n\nCreated from live Google Docs e2e.\n",
    )
    .expect("write google docs page");

    let diff = run_diff_with_state_root(&store, &page_path, Some(&fixture.state_root))
        .expect("diff google docs create");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    assert_eq!(diff.plan.as_ref().expect("plan").summary.entities_created, 1);

    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push google docs create");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    let created_id = push
        .changed_remote_ids
        .first()
        .expect("created doc id")
        .to_string();

    let created_doc = api.get_document(&created_id).expect("fetch created google doc");
    assert_eq!(created_doc.document_id, created_id);
    let created_file = api.get_file(&created_id).expect("fetch created drive file");
    assert_eq!(created_file.name, "Draft Plan");
    assert_eq!(created_file.parents, vec![workspace.id.clone()]);

    let original = fs::read_to_string(&page_path).expect("read reconciled page");
    fs::write(
        &page_path,
        original.replace(
            "Created from live Google Docs e2e.",
            "Edited from live Google Docs e2e.",
        ),
    )
    .expect("edit google docs page");
    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push google docs edit");
    assert!(push.ok, "{push:#?}");

    let archive = api
        .create_file(DriveCreateFileRequest::folder("Archive", Some(&workspace.id)))
        .expect("create archive folder");
    let archive_dir = mount_root.join("archive");
    run_pull_with_state_root(&mut store, &connector, &mount_root, Some(&fixture.state_root))
        .expect("pull archive folder");
    fs::rename(&page_dir, archive_dir.join("renamed-plan")).expect("move and rename page dir");
    let moved_path = archive_dir.join("renamed-plan/page.md");
    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &moved_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push google docs move");
    assert!(push.ok, "{push:#?}");
    let moved_file = api.get_file(&created_id).expect("fetch moved drive file");
    assert_eq!(moved_file.name, "Renamed Plan");
    assert_eq!(moved_file.parents, vec![archive.id]);

    fs::remove_dir_all(archive_dir.join("renamed-plan")).expect("remove page dir");
    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &archive_dir,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: true,
        },
        Some(&fixture.state_root),
    )
    .expect("push google docs archive");
    assert!(push.ok, "{push:#?}");
    let archived = api.get_file(&created_id).expect("fetch archived drive file");
    assert!(archived.trashed, "{archived:#?}");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(mount_root),
            ..StatusOptions::default()
        },
    )
    .expect("google docs status");
    assert!(status.clean, "{status:#?}");

    api.update_file(&workspace.id, DriveUpdateFileRequest::trash())
        .expect("trash scratch workspace");
}
```

- [ ] **Step 2: Run the focused live test locally with secrets**

Run:

```bash
LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON="$LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" \
cargo test -p loc-cli --test live_google_connectors live_google_docs_workspace_create_edit_move_archive_round_trip -- --ignored --exact --test-threads=1 --nocapture
```

Expected: the test passes and the scratch Drive workspace is trashed.

- [ ] **Step 3: Commit**

```bash
git add crates/loc-cli/tests/live_google_connectors.rs
git commit -m "test: add live google docs e2e"
```

---

### Task 4: Add Live Gmail Connector E2E

**Files:**
- Modify: `crates/loc-cli/tests/live_google_connectors.rs`

- [ ] **Step 1: Add Gmail helper functions**

Append helpers for date-window settings and recipient validation.

```rust
fn gmail_live_settings_json() -> String {
    let after = std::env::var("LOCALITY_GMAIL_LIVE_AFTER")
        .unwrap_or_else(|_| current_utc_date_for_gmail_window());
    let before = std::env::var("LOCALITY_GMAIL_LIVE_BEFORE")
        .unwrap_or_else(|_| next_utc_date_for_gmail_window());
    let settings = GmailMountSettings::with_date_window(&after, &before)
        .expect("gmail live date window")
        .with_view(GmailProjectionView::Messages);
    serde_json::to_string(&settings).expect("gmail settings json")
}

fn gmail_thread_settings_json() -> String {
    let after = std::env::var("LOCALITY_GMAIL_LIVE_AFTER")
        .unwrap_or_else(|_| current_utc_date_for_gmail_window());
    let before = std::env::var("LOCALITY_GMAIL_LIVE_BEFORE")
        .unwrap_or_else(|_| next_utc_date_for_gmail_window());
    let settings = GmailMountSettings::with_date_window(&after, &before)
        .expect("gmail live date window")
        .with_view(GmailProjectionView::Threads);
    serde_json::to_string(&settings).expect("gmail thread settings json")
}

fn current_utc_date_for_gmail_window() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .expect("date command");
    assert!(output.status.success(), "date command failed");
    String::from_utf8(output.stdout).expect("date utf8").trim().to_string()
}

fn next_utc_date_for_gmail_window() -> String {
    let output = Command::new("date")
        .args(["-u", "-d", "tomorrow", "+%Y-%m-%d"])
        .output()
        .expect("date command");
    assert!(output.status.success(), "date tomorrow command failed");
    String::from_utf8(output.stdout).expect("date utf8").trim().to_string()
}

fn live_gmail_recipient() -> String {
    let recipient = required_env(GMAIL_RECIPIENT_ENV);
    assert!(recipient.contains('@'), "gmail test recipient must be an email address");
    recipient
}
```

- [ ] **Step 2: Add the live Gmail test**

Append this ignored test. It sends one real email from a dedicated CI Gmail account, then verifies Locality can read it back from `sent/`, rejects read-only mailbox edits, and projects the same run in thread view.

```rust
#[test]
#[ignore = "requires LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON and LOCALITY_GMAIL_LIVE_TEST_RECIPIENT; sends one real email from the dedicated test mailbox"]
fn live_gmail_pull_send_read_only_guardrail_and_threads_round_trip() {
    let fixture = LiveFixture::new("loc-live-gmail");
    let connection_id = ConnectionId::new("gmail-live");
    seed_live_connection(&fixture.state_root, GoogleLiveConnector::Gmail, &connection_id);
    let connector = resolve_gmail_from_store(&fixture.state_root, connection_id.clone());

    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open gmail state");
    let mount_root = fixture.mount_root.join("gmail-main");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("gmail-main"),
            connector: GMAIL_CONNECTOR_ID.to_string(),
            root: mount_root.clone(),
            remote_root_id: None,
            connection_id: Some(connection_id.clone()),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: gmail_live_settings_json(),
        },
    )
    .expect("mount live gmail");

    let pull = run_pull_with_state_root(&mut store, &connector, &mount_root, Some(&fixture.state_root))
        .expect("initial gmail pull");
    assert!(pull.ok, "{pull:#?}");
    assert!(mount_root.join("inbox").is_dir());
    assert!(mount_root.join("sent").is_dir());
    assert!(mount_root.join("draft").is_dir());

    let marker = format!("Locality live Gmail e2e {}", timestamp_string());
    let draft_path = mount_root.join("draft/live-gmail-e2e.md");
    fs::write(
        &draft_path,
        format!(
            "---\nto: [\"{}\"]\nsubject: {}\n---\nThis message was sent by the Locality live Gmail e2e suite.\n",
            live_gmail_recipient(),
            marker
        ),
    )
    .expect("write gmail draft");

    let diff = run_diff_with_state_root(&store, &draft_path, Some(&fixture.state_root))
        .expect("diff gmail draft");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    assert_eq!(diff.plan.as_ref().expect("plan").summary.entities_created, 1);

    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &draft_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push gmail draft send");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert!(!draft_path.exists(), "sent draft should reconcile out of draft/");

    run_pull_with_state_root(&mut store, &connector, &mount_root, Some(&fixture.state_root))
        .expect("pull gmail after send");
    let sent_file = find_file_containing(&mount_root.join("sent"), &marker)
        .unwrap_or_else(|| panic!("sent message containing marker {marker} was not projected"));
    let sent = fs::read_to_string(&sent_file).expect("read sent message");
    assert!(sent.contains(&marker), "{sent}");
    assert!(sent.contains("connector: gmail"), "{sent}");

    fs::write(&sent_file, sent.replace(&marker, "Edited read-only sent subject"))
        .expect("edit sent file");
    let diff = run_diff_with_state_root(&store, &sent_file, Some(&fixture.state_root))
        .expect("diff read-only sent edit");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert_eq!(diff.validation[0].code, "gmail_read_only_mailbox", "{diff:#?}");

    let thread_mount_root = fixture.mount_root.join("gmail-threads");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("gmail-threads"),
            connector: GMAIL_CONNECTOR_ID.to_string(),
            root: thread_mount_root.clone(),
            remote_root_id: None,
            connection_id: Some(connection_id),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: gmail_thread_settings_json(),
        },
    )
    .expect("mount live gmail threads");
    let thread_connector = GmailConnector::new(
        GmailConfig::new(connector.config().access_token.clone())
            .with_settings(GmailMountSettings::from_json(&gmail_thread_settings_json()).expect("thread settings")),
    );
    run_pull_with_state_root(
        &mut store,
        &thread_connector,
        &thread_mount_root,
        Some(&fixture.state_root),
    )
    .expect("pull gmail threads");
    let thread_page = find_file_containing(&thread_mount_root.join("sent"), &marker)
        .unwrap_or_else(|| panic!("thread projection containing marker {marker} was not projected"));
    assert!(
        thread_page.ends_with("page.md") || thread_page.extension().and_then(|ext| ext.to_str()) == Some("md"),
        "thread projection should expose markdown for the sent conversation: {}",
        thread_page.display()
    );
}

fn find_file_containing(root: &Path, needle: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = fs::read_dir(&path).ok()?;
        for entry in entries {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                let text = fs::read_to_string(&path).ok()?;
                if text.contains(needle) {
                    return Some(path);
                }
            }
        }
    }
    None
}
```

- [ ] **Step 3: Run the focused live Gmail test locally with a dedicated mailbox**

Run:

```bash
LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON="$LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" \
LOCALITY_GMAIL_LIVE_TEST_RECIPIENT="$LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" \
cargo test -p loc-cli --test live_google_connectors live_gmail_pull_send_read_only_guardrail_and_threads_round_trip -- --ignored --exact --test-threads=1 --nocapture
```

Expected: the test passes, sends exactly one email, and leaves that email in the dedicated test Gmail account.

- [ ] **Step 4: Commit**

```bash
git add crates/loc-cli/tests/live_google_connectors.rs
git commit -m "test: add live gmail e2e"
```

---

### Task 5: Add Linux FUSE Product-Path Scripts

**Files:**
- Modify: `crates/loc-cli/tests/live_google_connectors.rs`
- Create: `tests/live_google_docs_vfs_push_pull.sh`
- Create: `tests/live_gmail_vfs_read_send.sh`

- [ ] **Step 1: Add VFS seed and cleanup helpers to the Rust live harness**

Append these ignored tests to `crates/loc-cli/tests/live_google_connectors.rs`. The shell scripts call them to seed isolated SQLite and credential state without duplicating store schema details in Bash.

```rust
#[test]
#[ignore = "helper invoked by tests/live_google_docs_vfs_push_pull.sh"]
fn live_google_docs_seed_state_for_vfs() {
    let state_root = PathBuf::from(required_env("LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT"));
    let workspace_id_file =
        PathBuf::from(required_env("LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_ID_FILE"));
    fs::create_dir_all(&state_root).expect("create vfs state root");
    let connection_id = ConnectionId::new("google-docs-live");
    seed_live_connection(&state_root, GoogleLiveConnector::GoogleDocs, &connection_id);
    let connector = resolve_google_docs_from_store(
        &state_root,
        connection_id,
        RemoteId::new("bootstrap-workspace"),
    );
    let api = HttpGoogleApiClient::new(connector.config().access_token.clone());
    let workspace_name = format!("Locality live Google Docs FUSE {}", timestamp_string());
    let workspace = api
        .create_file(DriveCreateFileRequest::folder(&workspace_name, None))
        .expect("create vfs scratch workspace");
    fs::write(workspace_id_file, workspace.id).expect("write workspace id file");
}

#[test]
#[ignore = "helper invoked by tests/live_google_docs_vfs_push_pull.sh cleanup"]
fn live_google_docs_trash_vfs_workspace() {
    let state_root = PathBuf::from(required_env("LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT"));
    let workspace_id = required_env("LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_ID");
    let connection_id = ConnectionId::new("google-docs-live");
    let connector = resolve_google_docs_from_store(
        &state_root,
        connection_id,
        RemoteId::new(workspace_id.clone()),
    );
    let api = HttpGoogleApiClient::new(connector.config().access_token.clone());
    api.update_file(&workspace_id, DriveUpdateFileRequest::trash())
        .expect("trash vfs scratch workspace");
}

#[test]
#[ignore = "helper invoked by tests/live_gmail_vfs_read_send.sh"]
fn live_gmail_seed_state_for_vfs() {
    let state_root = PathBuf::from(required_env("LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT"));
    fs::create_dir_all(&state_root).expect("create vfs state root");
    let connection_id = ConnectionId::new("gmail-live");
    seed_live_connection(&state_root, GoogleLiveConnector::Gmail, &connection_id);
    let _ = resolve_gmail_from_store(&state_root, connection_id);
}
```

- [ ] **Step 2: Create `tests/live_google_docs_vfs_push_pull.sh`**

Create an executable Bash script modeled after `tests/live_notion_vfs_push_pull.sh`. The script must:

```bash
#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GOOGLE_DOCS_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 to run the live Google Docs VFS test"
  exit 0
fi

for command in fusermount3 mountpoint python3 sqlite3; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "missing required live Google Docs VFS dependency: $command" >&2
    exit 1
  fi
done
if [[ ! -e /dev/fuse ]]; then
  echo "/dev/fuse is not available on this runner" >&2
  exit 1
fi
if [[ -z "${LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON:-}" ]]; then
  echo "missing LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" >&2
  exit 1
fi

loc_bin="${LOCALITY_BIN:-./target/debug/loc}"
localityd_bin="${LOCALITYD_BIN:-./target/debug/localityd}"
fuse_bin="${LOCALITY_FUSE_BIN:-./target/debug/locality-fuse}"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-live-google-docs-vfs.XXXXXX")"
state_root="$tmp_root/state"
locality_root="$tmp_root/Locality"
mount_root="$locality_root/google-docs-main"
daemon_log="$tmp_root/localityd.log"
fuse_log="$tmp_root/locality-fuse.log"
localityd_pid=""
fuse_pid=""
step="initializing"

cleanup() {
  set +e
  if mountpoint -q "$locality_root"; then
    fusermount3 -uz "$locality_root" >/dev/null 2>&1
  fi
  if [[ -n "$fuse_pid" ]] && kill -0 "$fuse_pid" >/dev/null 2>&1; then
    kill "$fuse_pid" >/dev/null 2>&1
    wait "$fuse_pid" >/dev/null 2>&1
  fi
  if [[ -n "$localityd_pid" ]] && kill -0 "$localityd_pid" >/dev/null 2>&1; then
    kill "$localityd_pid" >/dev/null 2>&1
    wait "$localityd_pid" >/dev/null 2>&1
  fi
  if [[ "${LOCALITY_GOOGLE_LIVE_KEEP_TMP:-}" == "1" ]]; then
    echo "kept private live Google Docs temp state"
  else
    rm -rf "$tmp_root"
  fi
}
trap cleanup EXIT
```

Continue the script with these concrete product-path steps:

```bash
step="building Locality live-test binaries"
cargo build -p loc-cli -p localityd -p locality-fuse >/dev/null

workspace_id_file="$tmp_root/google-docs-workspace-id"
step="seeding live Google Docs state"
LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT="$state_root" \
LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_ID_FILE="$workspace_id_file" \
  cargo test -p loc-cli --test live_google_connectors live_google_docs_seed_state_for_vfs -- --ignored --exact --test-threads=1 --nocapture
workspace_id="$(cat "$workspace_id_file")"

step="registering Google Docs Linux FUSE mount"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_DISABLE=1 \
  "$loc_bin" mount google-docs "$mount_root" \
    --workspace-folder "$workspace_id" \
    --connection google-docs-live \
    --mount-id google-docs-main \
    --projection linux-fuse \
    --json >/dev/null

step="starting localityd"
LOCALITY_STATE_DIR="$state_root" LOCALITY_DAEMON_TCP_ADDR=off \
  "$localityd_bin" >"$daemon_log" 2>&1 &
localityd_pid="$!"

step="starting locality-fuse"
LOCALITY_STATE_DIR="$state_root" "$fuse_bin" \
  --state-dir "$state_root" \
  --mountpoint "$locality_root" >"$fuse_log" 2>&1 &
fuse_pid="$!"

step="creating and pushing a Google Doc through FUSE"
mkdir -p "$mount_root/fuse-draft"
cat >"$mount_root/fuse-draft/page.md" <<'MD'
---
title: FUSE Draft
---
# FUSE Draft

Created from live Google Docs FUSE e2e.
MD
LOCALITY_STATE_DIR="$state_root" "$loc_bin" diff "$mount_root/fuse-draft/page.md" --json >/dev/null
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push "$mount_root/fuse-draft/page.md" -y --json >/dev/null
LOCALITY_STATE_DIR="$state_root" "$loc_bin" status "$mount_root" --json >/dev/null

step="archiving the Google Doc through FUSE"
rm -rf "$mount_root/fuse-draft"
LOCALITY_STATE_DIR="$state_root" "$loc_bin" push "$mount_root" --confirm --json >/dev/null

step="trashing scratch Google Docs workspace"
LOCALITY_GOOGLE_LIVE_VFS_STATE_ROOT="$state_root" \
LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_ID="$workspace_id" \
  cargo test -p loc-cli --test live_google_connectors live_google_docs_trash_vfs_workspace -- --ignored --exact --test-threads=1 --nocapture
```

- [ ] **Step 3: Create `tests/live_gmail_vfs_read_send.sh`**

Create an executable Bash script with the same process shape and these required actions:

```bash
#!/usr/bin/env bash
set -euo pipefail

if [[ "${LOCALITY_LIVE_GMAIL_VFS:-}" != "1" ]]; then
  echo "skip: set LOCALITY_LIVE_GMAIL_VFS=1 to run the live Gmail VFS test"
  exit 0
fi

for command in fusermount3 mountpoint python3 sqlite3; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "missing required live Gmail VFS dependency: $command" >&2
    exit 1
  fi
done
if [[ ! -e /dev/fuse ]]; then
  echo "/dev/fuse is not available on this runner" >&2
  exit 1
fi
if [[ -z "${LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON:-}" ]]; then
  echo "missing LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" >&2
  exit 1
fi
if [[ -z "${LOCALITY_GMAIL_LIVE_TEST_RECIPIENT:-}" ]]; then
  echo "missing LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" >&2
  exit 1
fi
```

The script must seed the same isolated state, mount Gmail with `--projection linux-fuse --after "$after" --before "$before"`, start daemon and FUSE, verify `inbox/`, `sent/`, and `draft/` are visible, write a draft Markdown file under `draft/`, run `loc diff`, push with `-y`, run `loc pull`, verify the sent marker appears under `sent/`, edit that sent file, verify `loc diff` reports `gmail_read_only_mailbox`, remount thread view under `gmail-threads`, pull, and verify the same marker appears in thread projection.

- [ ] **Step 4: Make scripts executable and run shell syntax checks**

Run:

```bash
chmod +x tests/live_google_docs_vfs_push_pull.sh tests/live_gmail_vfs_read_send.sh
bash -n tests/live_google_docs_vfs_push_pull.sh
bash -n tests/live_gmail_vfs_read_send.sh
```

Expected: both `bash -n` commands exit 0.

- [ ] **Step 5: Run the scripts with live env on Linux**

Run:

```bash
LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 \
LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON="$LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" \
tests/live_google_docs_vfs_push_pull.sh
```

Expected: the script exits 0 and reports no leaked credentials.

Run:

```bash
LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_LIVE_GMAIL_VFS=1 \
LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON="$LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" \
LOCALITY_GMAIL_LIVE_TEST_RECIPIENT="$LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" \
tests/live_gmail_vfs_read_send.sh
```

Expected: the script exits 0 and sends exactly one Gmail message.

- [ ] **Step 6: Commit**

```bash
git add crates/loc-cli/tests/live_google_connectors.rs tests/live_google_docs_vfs_push_pull.sh tests/live_gmail_vfs_read_send.sh
git commit -m "test: add live google linux fuse workflows"
```

---

### Task 6: Add Google Live GitHub Action

**Files:**
- Create: `.github/workflows/google-live-e2e.yml`

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/google-live-e2e.yml`:

```yaml
name: google-live-e2e

on:
  push:
    branches:
      - main
    paths:
      - ".github/workflows/google-live-e2e.yml"
      - "Cargo.toml"
      - "Cargo.lock"
      - "crates/locality-connector/**"
      - "crates/locality-core/**"
      - "crates/locality-gmail/**"
      - "crates/locality-google-docs/**"
      - "crates/locality-store/**"
      - "crates/localityd/**"
      - "crates/loc-cli/**"
      - "platform/linux/locality-fuse/**"
      - "tests/live_google_docs_vfs_push_pull.sh"
      - "tests/live_gmail_vfs_read_send.sh"
  schedule:
    - cron: "17 16 * * 2"
  workflow_dispatch:

concurrency:
  group: google-live-e2e
  cancel-in-progress: false

permissions:
  contents: read

jobs:
  google-docs-live:
    name: Google Docs connector live e2e
    runs-on: ubuntu-latest
    timeout-minutes: 20
    environment: google-live-e2e
    env:
      LOCALITY_GOOGLE_LIVE_FORCE_REFRESH: "1"
      LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON: ${{ secrets.LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON }}
      LOCALITY_AUTH_BROKER_URL: ${{ secrets.LOCALITY_AUTH_BROKER_URL }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Check live Google Docs secrets
        shell: bash
        run: |
          missing=()
          [ -n "$LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" ] || missing+=("LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON")
          if [ "${#missing[@]}" -gt 0 ]; then
            echo "::error::Live Google Docs e2e is missing required secrets: ${missing[*]}"
            exit 1
          fi
      - name: Run live Google Docs connector workflow
        run: cargo test -p loc-cli --test live_google_connectors live_google_docs_workspace_create_edit_move_archive_round_trip -- --ignored --exact --test-threads=1 --nocapture

  gmail-live:
    name: Gmail connector live e2e
    runs-on: ubuntu-latest
    timeout-minutes: 20
    environment: google-live-e2e
    env:
      LOCALITY_GOOGLE_LIVE_FORCE_REFRESH: "1"
      LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON: ${{ secrets.LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON }}
      LOCALITY_GMAIL_LIVE_TEST_RECIPIENT: ${{ secrets.LOCALITY_GMAIL_LIVE_TEST_RECIPIENT }}
      LOCALITY_AUTH_BROKER_URL: ${{ secrets.LOCALITY_AUTH_BROKER_URL }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Check live Gmail secrets
        shell: bash
        run: |
          missing=()
          [ -n "$LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" ] || missing+=("LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON")
          [ -n "$LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" ] || missing+=("LOCALITY_GMAIL_LIVE_TEST_RECIPIENT")
          if [ "${#missing[@]}" -gt 0 ]; then
            echo "::error::Live Gmail e2e is missing required secrets: ${missing[*]}"
            exit 1
          fi
      - name: Run live Gmail connector workflow
        run: cargo test -p loc-cli --test live_google_connectors live_gmail_pull_send_read_only_guardrail_and_threads_round_trip -- --ignored --exact --test-threads=1 --nocapture

  linux-fuse-live:
    name: Google connectors Linux FUSE live e2e
    runs-on: ubuntu-latest
    timeout-minutes: 35
    environment: google-live-e2e
    env:
      LOCALITY_GOOGLE_LIVE_FORCE_REFRESH: "1"
      LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON: ${{ secrets.LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON }}
      LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON: ${{ secrets.LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON }}
      LOCALITY_GMAIL_LIVE_TEST_RECIPIENT: ${{ secrets.LOCALITY_GMAIL_LIVE_TEST_RECIPIENT }}
      LOCALITY_AUTH_BROKER_URL: ${{ secrets.LOCALITY_AUTH_BROKER_URL }}
      LOCALITY_LIVE_GOOGLE_DOCS_VFS: "1"
      LOCALITY_LIVE_GMAIL_VFS: "1"
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Install FUSE dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            fuse3 \
            libfuse3-dev \
            libglib2.0-dev \
            libgtk-3-dev \
            libwebkit2gtk-4.1-dev \
            libayatana-appindicator3-dev \
            librsvg2-dev \
            patchelf \
            pkg-config \
            sqlite3
      - name: Check live Google FUSE secrets and device
        shell: bash
        run: |
          missing=()
          [ -n "$LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" ] || missing+=("LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON")
          [ -n "$LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" ] || missing+=("LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON")
          [ -n "$LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" ] || missing+=("LOCALITY_GMAIL_LIVE_TEST_RECIPIENT")
          if [ "${#missing[@]}" -gt 0 ]; then
            echo "::error::Live Google FUSE e2e is missing required secrets: ${missing[*]}"
            exit 1
          fi
          if [ ! -e /dev/fuse ]; then
            echo "::error::/dev/fuse is unavailable; the real Linux FUSE Google e2e cannot run"
            exit 1
          fi
      - name: Run live Google Docs mounted workflow
        run: tests/live_google_docs_vfs_push_pull.sh
      - name: Run live Gmail mounted workflow
        run: tests/live_gmail_vfs_read_send.sh
```

- [ ] **Step 2: Validate workflow YAML shape**

Run:

```bash
python3 - <<'PY'
from pathlib import Path
text = Path(".github/workflows/google-live-e2e.yml").read_text()
required = [
    "name: google-live-e2e",
    "environment: google-live-e2e",
    "LOCALITY_GOOGLE_LIVE_FORCE_REFRESH",
    "tests/live_google_docs_vfs_push_pull.sh",
    "tests/live_gmail_vfs_read_send.sh",
]
missing = [item for item in required if item not in text]
if missing:
    raise SystemExit(f"missing expected workflow text: {missing}")
PY
```

Expected: the command exits 0.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/google-live-e2e.yml
git commit -m "ci: add live google connector e2e"
```

---

### Task 7: Update E2E Coverage Docs

**Files:**
- Modify: `docs/e2e-behavior-coverage.md`
- Modify: `docs/google-docs-connector.md`
- Modify: `docs/gmail-connector.md`

- [ ] **Step 1: Update `docs/e2e-behavior-coverage.md`**

Add this section after "How To Run Live Notion E2E":

````markdown
## How To Run Live Google E2E

Required environment:

```sh
export LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON='{"kind":"oauth","connector":"google-docs","access_token":"expired","token_type":"Bearer","oauth_client_id":"google-client-id","oauth_broker_url":"https://afs-oauth-broker.saurabh-b07.workers.dev","account_id":"acct","account_label":"locality-ci@example.com","workspace_id":"google-drive","workspace_name":"Google Drive","scopes":["openid","email","profile","https://www.googleapis.com/auth/documents","https://www.googleapis.com/auth/drive.file","https://www.googleapis.com/auth/drive.metadata"],"refresh_token_handle":"locrh_v1.example","acquired_at":1,"expires_at":1}'
export LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON='{"kind":"oauth","connector":"gmail","access_token":"expired","token_type":"Bearer","oauth_client_id":"google-client-id","oauth_broker_url":"https://afs-oauth-broker.saurabh-b07.workers.dev","account_id":"acct","account_label":"locality-ci@example.com","workspace_id":"gmail","workspace_name":"Gmail","scopes":["openid","email","profile","https://www.googleapis.com/auth/gmail.readonly","https://www.googleapis.com/auth/gmail.compose"],"refresh_token_handle":"locrh_v1.example","acquired_at":1,"expires_at":1}'
export LOCALITY_GMAIL_LIVE_TEST_RECIPIENT=locality-ci@example.com
```

The Google credential JSON values are copied from the Locality credential store
after running `loc connect google-docs --name google-docs-live` and
`loc connect gmail --name gmail-live` against a dedicated test Google account.
They must include `refresh_token_handle` values produced by the Locality OAuth
broker. CI sets `LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1`, so live tests rewrite
the access token to an expired sentinel and must refresh through the broker
before calling Google APIs.

Run:

```sh
LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
  cargo test -p loc-cli --test live_google_connectors live_google_docs_workspace_create_edit_move_archive_round_trip -- --ignored --exact --test-threads=1

LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
  cargo test -p loc-cli --test live_google_connectors live_gmail_pull_send_read_only_guardrail_and_threads_round_trip -- --ignored --exact --test-threads=1

LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 \
  tests/live_google_docs_vfs_push_pull.sh

LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 LOCALITY_LIVE_GMAIL_VFS=1 \
  tests/live_gmail_vfs_read_send.sh
```

GitHub Actions runs `.github/workflows/google-live-e2e.yml` for relevant changes
on `main`, weekly, and on manual dispatch. The workflow uses the
`google-live-e2e` environment, creates disposable Google Docs scratch folders,
trashes its Google Docs scratch content, sends one Gmail message from the
dedicated test mailbox, and uploads no artifacts.
````

Then add two rows to the expected behavior table:

```markdown
| E2E-040 | Google Docs connects through broker-backed OAuth refresh handles, mounts a Drive workspace folder, projects folders and Docs, hydrates `page.md`, creates a new Doc, edits body content, moves/renames a Doc, archives a Doc, reconciles local Markdown, and returns status clean. | Covered live | `crates/loc-cli/tests/live_google_connectors.rs::live_google_docs_workspace_create_edit_move_archive_round_trip`; `tests/live_google_docs_vfs_push_pull.sh`; local fake Google Docs e2e in `crates/loc-cli/tests/e2e_push_workflow.rs`. | Real macOS File Provider remains manual because hosted macOS runners cannot exercise a signed, approved File Provider domain reliably. |
| E2E-041 | Gmail connects through broker-backed OAuth refresh handles, mounts inbox/sent/draft, pulls bounded date-window messages, sends a draft through the filesystem push path, reconciles the sent message, rejects inbox/sent edits before connector apply, and projects thread view. | Covered live | `crates/loc-cli/tests/live_google_connectors.rs::live_gmail_pull_send_read_only_guardrail_and_threads_round_trip`; `tests/live_gmail_vfs_read_send.sh`; connector and daemon Gmail unit tests. | Gmail live send leaves one message in the dedicated CI mailbox because the connector intentionally does not request delete or modify scopes. |
```

- [ ] **Step 2: Update connector docs**

Add this section to `docs/google-docs-connector.md` before "Useful Commands":

```markdown
## Live E2E

The live Google Docs suite uses a dedicated Google account connected through the
Locality OAuth broker. CI stores the broker-backed `StoredGoogleDocsCredential`
JSON in `LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON`; the JSON must include an
opaque `refresh_token_handle`, not a raw refresh token. The tests force token
refresh with `LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1`, create a scratch Drive
workspace folder, run the mount/pull/diff/push workflow, and trash scratch
content before exit.
```

Add this section to `docs/gmail-connector.md` before "Useful Commands":

```markdown
## Live E2E

The live Gmail suite uses a dedicated Gmail account connected through the
Locality OAuth broker. CI stores the broker-backed `StoredGmailCredential` JSON
in `LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON`; the JSON must include an opaque
`refresh_token_handle`, not a raw refresh token. The tests force token refresh
with `LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1`, send one real message to
`LOCALITY_GMAIL_LIVE_TEST_RECIPIENT`, verify sent-message reconciliation, and
verify read-only mailbox guardrails. The test account must tolerate retained
sent messages because the connector intentionally avoids broad Gmail modify or
delete scopes.
```

- [ ] **Step 3: Run docs grep checks**

Run:

```bash
rg -n "Live Google E2E|E2E-040|E2E-041|LOCALITY_GOOGLE_LIVE_FORCE_REFRESH|refresh_token_handle" docs/e2e-behavior-coverage.md docs/google-docs-connector.md docs/gmail-connector.md
```

Expected: every searched file has at least one matching line.

- [ ] **Step 4: Commit**

```bash
git add docs/e2e-behavior-coverage.md docs/google-docs-connector.md docs/gmail-connector.md
git commit -m "docs: document live google e2e"
```

---

### Task 8: Final Verification

**Files:**
- Verify: whole workspace

- [ ] **Step 1: Run focused non-live tests**

Run:

```bash
cargo test -p localityd --test source_descriptor resolving_expired_gmail_credential_refreshes_with_broker_handle -- --exact
cargo test -p loc-cli --test live_google_connectors -- --ignored --exact __no_such_live_google_test__
bash -n tests/live_google_docs_vfs_push_pull.sh
bash -n tests/live_gmail_vfs_read_send.sh
```

Expected: the Gmail refresh test passes, the live test file compiles, and both shell syntax checks pass.

- [ ] **Step 2: Run live connector tests in a prepared environment**

Run:

```bash
LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON="$LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" \
cargo test -p loc-cli --test live_google_connectors live_google_docs_workspace_create_edit_move_archive_round_trip -- --ignored --exact --test-threads=1 --nocapture

LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON="$LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" \
LOCALITY_GMAIL_LIVE_TEST_RECIPIENT="$LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" \
cargo test -p loc-cli --test live_google_connectors live_gmail_pull_send_read_only_guardrail_and_threads_round_trip -- --ignored --exact --test-threads=1 --nocapture
```

Expected: both tests pass. The Google Docs test trashes its scratch workspace. The Gmail test sends exactly one message.

- [ ] **Step 3: Run live Linux FUSE tests in a prepared Linux environment**

Run:

```bash
LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 \
LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON="$LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON" \
tests/live_google_docs_vfs_push_pull.sh

LOCALITY_GOOGLE_LIVE_FORCE_REFRESH=1 \
LOCALITY_LIVE_GMAIL_VFS=1 \
LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON="$LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON" \
LOCALITY_GMAIL_LIVE_TEST_RECIPIENT="$LOCALITY_GMAIL_LIVE_TEST_RECIPIENT" \
tests/live_gmail_vfs_read_send.sh
```

Expected: both scripts pass with `/dev/fuse` available.

- [ ] **Step 4: Run the normal workspace baseline**

Run:

```bash
cargo test --workspace --all-targets
```

Expected: all non-live tests pass.

- [ ] **Step 5: Commit final adjustments**

```bash
git status --short
git add .
git commit -m "test: add live google connector e2e coverage"
```

Expected: the final commit includes only the live Google e2e implementation, scripts, workflow, and docs.

---

## Self-Review

- Spec coverage: The plan covers real CI e2e for Gmail and Google Docs, major connector use cases, GitHub Actions integration, token refresh through broker handles, local live run commands, and docs updates.
- Placeholder scan: The plan contains no incomplete sections, deferred implementation markers, or unspecified secret names.
- Type consistency: Connector IDs, env vars, test names, profile IDs, and workflow job names are consistent across tasks.
