# Gmail Connector Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a first-party Gmail connector that mounts `inbox/`, `sent/`, and `draft/`, fetches the latest 100 messages for read-only folders, and sends locally created draft Markdown files on `loc push`.

**Architecture:** Gmail is a first-party Rust connector crate registered through the existing daemon source registry. Gmail folders are modeled as source-backed `Directory` entities; `inbox/` and `sent/` message files are read-only, while `draft/` accepts local Markdown creates that become Gmail draft-send operations during push.

**Tech Stack:** Rust workspace crates (`locality-connector`, `locality-core`, `localityd`, `loc-cli`, `locality-gmail`), reqwest blocking HTTP client, serde/serde_json/yaml_serde, Cloudflare Worker OAuth broker in TypeScript, existing virtual filesystem adapters.

---

## Product Decisions For V1

- Folder shape is exactly `inbox/`, `sent/`, and `draft/` at the Gmail mount root.
- `inbox/` and `sent/` list the most recent 100 messages each.
- `draft/` is a local compose surface. V1 does not pull existing remote Gmail drafts.
- Pushing a file created under `draft/` sends the email. There is no separate save-draft remote operation in V1.
- Attachments are unsupported in V1. Emails with attachments render metadata that says attachments are omitted, but draft sends reject attachment directives or fields.
- OAuth uses one Gmail broker profile with `openid`, `email`, `profile`, `https://www.googleapis.com/auth/gmail.readonly`, and `https://www.googleapis.com/auth/gmail.compose`.

## File Structure

- Create `crates/locality-gmail/`: source-specific Gmail API client, DTOs, OAuth constants/storage, render/parse helpers, and connector implementation.
- Modify `crates/locality-connector/src/lib.rs`: add a connector-neutral `DirectoryChildren(RemoteId)` child container for source folder children.
- Modify `crates/localityd/src/source.rs`: register Gmail, expose descriptor metadata, add source write policy helpers, route resolved-source calls.
- Create `crates/localityd/src/gmail.rs`: resolve Gmail connections, refresh OAuth credentials, validate Gmail frontmatter, implement `HydrationSource`.
- Modify `crates/localityd/src/virtual_fs.rs`: add item-level read-only metadata and block writes/deletes/renames outside Gmail `draft/`.
- Modify `platform/linux/locality-fuse/src/linux.rs`: honor `VirtualFsItem.read_only` in write checks and file modes.
- Modify `crates/loc-cli/src/connect.rs` and `crates/loc-cli/src/commands.rs`: add `loc connect gmail` and `loc mount gmail`.
- Modify `apps/oauth-service/src/oauth/`, `apps/oauth-service/src/app.ts`, `apps/oauth-service/src/types.ts`, and tests: add Gmail broker endpoints.
- Modify docs under `docs/` and `docs-site/`: document Gmail connector semantics and limitations.

## Path And Metadata Contract

Rendered read-only message example:

```markdown
---
loc:
  id: "18f9a3"
  type: page
  connector: gmail
  synced_at: "gmail:18f9a3:1720900000000"
  remote_edited_at: "gmail:18f9a3:1720900000000"
title: "Quarterly update"
gmail:
  mailbox: "inbox"
  message_id: "18f9a3"
  thread_id: "thread-1"
  labels: ["INBOX"]
from: "Ann Example <ann@example.com>"
to: ["me@example.com"]
cc: []
bcc: []
subject: "Quarterly update"
date: "Tue, 14 Jul 2026 09:30:00 +0000"
---
Hello from Gmail.
```

Local draft create example:

```markdown
---
loc:
  type: page
  connector: gmail
title: "Quarterly reply"
gmail:
  mailbox: "draft"
to: ["ann@example.com"]
cc: []
bcc: []
subject: "Re: Quarterly update"
---
Thanks, this looks good.
```

## Task 1: Add Connector-Neutral Directory Children

**Files:**
- Modify: `crates/locality-connector/src/lib.rs`
- Modify: `crates/locality-google-docs/src/connector.rs`
- Modify: `crates/localityd/src/virtual_fs.rs`
- Test: `crates/localityd/src/virtual_fs.rs`

- [ ] **Step 1: Write the failing virtual FS child-container test**

Add this test inside `crates/localityd/src/virtual_fs.rs` `mod tests`:

```rust
#[test]
fn directory_entities_refresh_with_directory_child_container() {
    let mount_id = MountId::new("gmail-main");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
        .expect("save mount");
    store
        .save_entity(EntityRecord {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new("gmail-folder:inbox"),
            kind: EntityKind::Directory,
            title: "inbox".to_string(),
            path: "inbox".into(),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some("folder:inbox".to_string()),
        })
        .expect("save inbox");

    let entities = store.list_entities(&mount_id).expect("entities");
    let container = super::child_container_for_identifier(
        &store.get_mount(&mount_id).expect("mount load").expect("mount"),
        &entities,
        "gmail-folder:inbox",
    )
    .expect("child container");

    assert_eq!(
        container,
        Some(locality_connector::ChildContainer::DirectoryChildren(
            RemoteId::new("gmail-folder:inbox")
        ))
    );
}
```

If `virtual_mount_with_connector` does not exist in the test module, add this helper near the existing `virtual_mount` helper:

```rust
fn virtual_mount_with_connector(mount_id: &MountId, connector: &str) -> MountConfig {
    MountConfig::new(
        mount_id.clone(),
        connector,
        format!("/tmp/Locality/{}", mount_id.0),
    )
    .projection(ProjectionMode::LinuxFuse)
}
```

- [ ] **Step 2: Run the failing test**

Run:

```bash
cargo test -p localityd directory_entities_refresh_with_directory_child_container
```

Expected: FAIL because `ChildContainer::DirectoryChildren` does not exist.

- [ ] **Step 3: Add the new child-container variant**

In `crates/locality-connector/src/lib.rs`, change `ChildContainer` to:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildContainer {
    /// The mount root. For workspace mounts, this is the visible workspace root;
    /// for scoped mounts, this is the configured remote root.
    Root,
    /// Child pages/databases under a page.
    PageChildren(RemoteId),
    /// Row pages under a database-like collection.
    DatabaseRows(RemoteId),
    /// Child entities under a source folder/directory.
    DirectoryChildren(RemoteId),
}
```

In `crates/locality-google-docs/src/connector.rs`, update the parent-id match in `list_children`:

```rust
let parent_id = match request.container {
    locality_connector::ChildContainer::Root => self.workspace_folder_id()?.0.clone(),
    locality_connector::ChildContainer::PageChildren(remote_id)
    | locality_connector::ChildContainer::DatabaseRows(remote_id)
    | locality_connector::ChildContainer::DirectoryChildren(remote_id) => remote_id.0,
};
```

In `crates/localityd/src/virtual_fs.rs`, update `child_container_for_identifier`:

```rust
Ok(match entity.kind {
    EntityKind::Page => Some(ChildContainer::PageChildren(remote_id)),
    EntityKind::Database => Some(ChildContainer::DatabaseRows(remote_id)),
    EntityKind::Directory => Some(ChildContainer::DirectoryChildren(remote_id)),
    EntityKind::Asset | EntityKind::Unknown(_) => None,
})
```

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test -p localityd directory_entities_refresh_with_directory_child_container
cargo test -p locality-google-docs list_children
```

Expected: PASS. The second command may report zero tests matching `list_children`; that is acceptable only if the crate has no focused list-children test.

- [ ] **Step 5: Commit**

```bash
git add crates/locality-connector/src/lib.rs crates/locality-google-docs/src/connector.rs crates/localityd/src/virtual_fs.rs
git commit -m "feat: support directory child containers"
```

## Task 2: Add Source Write Policy For Gmail Read-Only Folders

**Files:**
- Modify: `crates/localityd/src/source.rs`
- Modify: `crates/localityd/src/virtual_fs.rs`
- Modify: `platform/linux/locality-fuse/src/linux.rs`
- Test: `crates/localityd/src/virtual_fs.rs`
- Test: `platform/linux/locality-fuse/src/linux.rs`

- [ ] **Step 1: Write failing daemon policy tests**

Add these tests to `crates/localityd/src/virtual_fs.rs` `mod tests`:

```rust
#[test]
fn gmail_inbox_message_rejects_virtual_write_without_dirtying_entity() {
    let mount_id = MountId::new("gmail-main");
    let content_root = temp_root("loc-gmail-readonly").join("content/gmail-main/files");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
        .expect("save mount");
    store
        .save_entity(EntityRecord {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new("msg-inbox-1"),
            kind: EntityKind::Page,
            title: "Inbox One".to_string(),
            path: "inbox/2026-07-14-inbox-one-msg-inbox-1.md".into(),
            hydration: HydrationState::Hydrated,
            content_hash: None,
            remote_edited_at: Some("gmail:msg-inbox-1:1".to_string()),
        })
        .expect("save entity");

    let error = commit_virtual_fs_write(
        &mut store,
        &content_root,
        &mount_id,
        "msg-inbox-1",
        b"edited",
    )
    .expect_err("inbox writes are rejected");

    assert!(matches!(error, LocalityError::Unsupported(message) if message.contains("Gmail inbox and sent items are read-only")));
    let entity = store
        .get_entity(&mount_id, &RemoteId::new("msg-inbox-1"))
        .expect("load entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
}

#[test]
fn gmail_draft_folder_accepts_virtual_create() {
    let mount_id = MountId::new("gmail-main");
    let content_root = temp_root("loc-gmail-draft-create").join("content/gmail-main/files");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(virtual_mount_with_connector(&mount_id, "gmail"))
        .expect("save mount");
    store
        .save_entity(EntityRecord {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new("gmail-folder:draft"),
            kind: EntityKind::Directory,
            title: "draft".to_string(),
            path: "draft".into(),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some("folder:draft".to_string()),
        })
        .expect("save draft folder");

    let report = create_virtual_fs_file(
        &mut store,
        &content_root,
        &mount_id,
        "gmail-folder:draft",
        "reply.md",
    )
    .expect("draft create");

    assert_eq!(report.item.path, "draft/reply.md");
    assert_eq!(report.item.entity_kind, Some(EntityKind::Page));
}
```

- [ ] **Step 2: Run failing tests**

Run:

```bash
cargo test -p localityd gmail_inbox_message_rejects_virtual_write_without_dirtying_entity gmail_draft_folder_accepts_virtual_create
```

Expected: FAIL because Gmail-specific source write policy does not exist and directory create parents are rejected.

- [ ] **Step 3: Add source write policy helpers**

In `crates/localityd/src/source.rs`, add this code near `source_display_name`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceWriteDecision {
    Writable,
    ReadOnly { reason: &'static str },
}

impl SourceWriteDecision {
    pub fn is_writable(self) -> bool {
        matches!(self, Self::Writable)
    }

    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Writable => None,
            Self::ReadOnly { reason } => Some(reason),
        }
    }
}

pub fn source_write_decision_for_path(
    mount: &MountConfig,
    relative_path: &Path,
) -> SourceWriteDecision {
    if mount.read_only {
        return SourceWriteDecision::ReadOnly {
            reason: "mount is read-only",
        };
    }
    if mount.connector == "gmail" {
        return gmail_write_decision_for_path(relative_path);
    }
    SourceWriteDecision::Writable
}

pub fn source_create_decision_for_parent_path(
    mount: &MountConfig,
    parent_path: &Path,
) -> SourceWriteDecision {
    if mount.read_only {
        return SourceWriteDecision::ReadOnly {
            reason: "mount is read-only",
        };
    }
    if mount.connector == "gmail" {
        return if parent_path == Path::new("draft") {
            SourceWriteDecision::Writable
        } else {
            SourceWriteDecision::ReadOnly {
                reason: "Gmail creates are only supported directly inside draft/",
            }
        };
    }
    SourceWriteDecision::Writable
}

fn gmail_write_decision_for_path(relative_path: &Path) -> SourceWriteDecision {
    match relative_path.components().next().and_then(|component| match component {
        std::path::Component::Normal(value) => value.to_str(),
        _ => None,
    }) {
        Some("draft") => SourceWriteDecision::Writable,
        Some("inbox") | Some("sent") => SourceWriteDecision::ReadOnly {
            reason: "Gmail inbox and sent items are read-only",
        },
        _ => SourceWriteDecision::ReadOnly {
            reason: "Gmail writes are only supported under draft/",
        },
    }
}
```

- [ ] **Step 4: Use the policy in virtual filesystem mutations**

In `crates/localityd/src/virtual_fs.rs`, import the helpers:

```rust
use crate::source::{source_create_decision_for_parent_path, source_write_decision_for_path};
```

Add this helper near `missing_identifier` or other validation helpers:

```rust
fn ensure_source_path_writable(mount: &MountConfig, relative_path: &Path) -> LocalityResult<()> {
    match source_write_decision_for_path(mount, relative_path) {
        crate::source::SourceWriteDecision::Writable => Ok(()),
        crate::source::SourceWriteDecision::ReadOnly { reason } => Err(LocalityError::Unsupported(
            reason.to_string(),
        )),
    }
}

fn ensure_source_parent_accepts_create(
    mount: &MountConfig,
    parent_path: &Path,
) -> LocalityResult<()> {
    match source_create_decision_for_parent_path(mount, parent_path) {
        crate::source::SourceWriteDecision::Writable => Ok(()),
        crate::source::SourceWriteDecision::ReadOnly { reason } => Err(LocalityError::Unsupported(
            reason.to_string(),
        )),
    }
}
```

Call `ensure_source_path_writable(&mount, &entity.path)?;` in `commit_virtual_fs_write` after confirming `entity.kind == EntityKind::Page`.

Call `ensure_source_parent_accepts_create(&mount, &parent_path)?;` in `create_virtual_fs_file` and `create_virtual_fs_directory` immediately after `parent_path` is calculated.

Call `ensure_source_path_writable(&mount, &mutation.projected_path)?;` before writing pending-created local files in `commit_virtual_fs_write`.

Call `ensure_source_path_writable(&mount, &entity.path)?;` in `trash_virtual_fs_item` before `record_virtual_fs_page_delete`.

Call `ensure_source_path_writable(&mount, &old_path_relative)?;` in `rename_virtual_fs_item` for existing remote entities. Use the resolved entity path for existing entities and `mutation.projected_path` for local mutations.

- [ ] **Step 5: Allow descriptor-controlled directory create parents**

In `crates/localityd/src/source.rs`, add a field to `SourceDescriptor`:

```rust
create_entity_parent_kinds: Vec<EntityKind>,
```

Add this accessor:

```rust
pub fn create_entity_parent_kinds(&self) -> &[EntityKind] {
    &self.create_entity_parent_kinds
}
```

Populate descriptors:

```rust
// Notion
create_entity_parent_kinds: vec![EntityKind::Page, EntityKind::Database],

// Google Docs
create_entity_parent_kinds: vec![EntityKind::Directory],

// Generic
create_entity_parent_kinds: vec![EntityKind::Page, EntityKind::Database],
```

In `crates/localityd/src/virtual_fs.rs`, change the final match in `create_parent_remote_id` to:

```rust
if source_descriptor(&mount.connector)
    .create_entity_parent_kinds()
    .contains(&entity.kind)
{
    Ok(remote_id)
} else {
    Err(LocalityError::Unsupported(
        "new virtual filesystem files cannot be created under this source item",
    ))
}
```

In `crates/localityd/src/push.rs`, update `pending_create_parent_entity` and `required_parent_entity` only if tests expose a parent-kind restriction. `create_entity_pipeline` already carries the parent kind to the connector, so Gmail validation will enforce `draft/`.

- [ ] **Step 6: Add item-level read-only metadata**

In `crates/localityd/src/virtual_fs.rs`, add a field to `VirtualFsItem`:

```rust
#[serde(default)]
pub read_only: bool,
```

Set it in all `VirtualFsItem` constructors. For entity-backed items use:

```rust
read_only: !source_write_decision_for_path(mount, &entity.path).is_writable(),
```

For source root and mount root folders use `read_only: false`. For guidance and schema files use `read_only: true`. For Gmail `draft/`, the directory item should have `read_only: false` so file creation works; actual deletion and rename remain blocked by the daemon policy.

- [ ] **Step 7: Honor `read_only` in Linux FUSE**

In `platform/linux/locality-fuse/src/linux.rs`, update `ensure_writable_item`:

```rust
fn ensure_writable_item(item: &VirtualFsItem) -> Result<(), FuseError> {
    if item.read_only {
        return Err(FuseError::ReadOnly);
    }
    if item.identifier == DIRECTORY_METADATA_IDENTIFIER {
        return Err(FuseError::ReadOnly);
    }
    if item.identifier.starts_with("schema:") {
        return Err(FuseError::ReadOnly);
    }
    if item
        .entity_kind
        .as_ref()
        .is_some_and(|kind| *kind != EntityKind::Page)
    {
        return Err(FuseError::ReadOnly);
    }
    Ok(())
}
```

Update `attr_for_item` permissions:

```rust
perm: match (item.kind, item.read_only) {
    (VirtualFsItemKind::Folder, true) => 0o555,
    (VirtualFsItemKind::Folder, false) => 0o755,
    (VirtualFsItemKind::File, true) => 0o444,
    (VirtualFsItemKind::File, false) => 0o644,
},
```

- [ ] **Step 8: Run focused tests**

Run:

```bash
cargo test -p localityd gmail_inbox_message_rejects_virtual_write_without_dirtying_entity gmail_draft_folder_accepts_virtual_create
cargo test -p locality-fuse read_only
```

Expected: PASS. If `cargo test -p locality-fuse read_only` has no matching tests, add a unit test for `attr_for_item` with `read_only: true`.

- [ ] **Step 9: Commit**

```bash
git add crates/localityd/src/source.rs crates/localityd/src/virtual_fs.rs platform/linux/locality-fuse/src/linux.rs
git commit -m "feat: enforce Gmail folder write policy"
```

## Task 3: Create The `locality-gmail` Crate With OAuth And DTOs

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/locality-gmail/Cargo.toml`
- Create: `crates/locality-gmail/src/lib.rs`
- Create: `crates/locality-gmail/src/oauth.rs`
- Create: `crates/locality-gmail/src/dto.rs`
- Create: `crates/locality-gmail/src/client.rs`

- [ ] **Step 1: Write OAuth unit tests**

Create `crates/locality-gmail/src/oauth.rs` with this test module first:

```rust
#[cfg(test)]
mod tests {
    use locality_connector::ConnectorCapabilities;
    use locality_connector::oauth_broker::OAuthBrokerToken;

    use super::{GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, StoredGmailCredential, gmail_capabilities_json};

    #[test]
    fn oauth_scopes_cover_read_and_compose_without_full_mailbox_scope() {
        assert!(GMAIL_OAUTH_SCOPES.contains(&"openid"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"email"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"profile"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/gmail.readonly"));
        assert!(GMAIL_OAUTH_SCOPES.contains(&"https://www.googleapis.com/auth/gmail.compose"));
        assert!(!GMAIL_OAUTH_SCOPES.contains(&"https://mail.google.com/"));
    }

    #[test]
    fn stored_capabilities_match_gmail_v1() {
        let capabilities: ConnectorCapabilities =
            serde_json::from_str(&gmail_capabilities_json().expect("capabilities json"))
                .expect("decode capabilities");

        assert!(capabilities.supports_oauth);
        assert!(capabilities.supports_remote_observation);
        assert!(capabilities.supports_lazy_child_enumeration);
        assert!(!capabilities.supports_databases);
        assert!(!capabilities.supports_undo);
    }

    #[test]
    fn broker_credential_stores_refresh_handle_without_secret() {
        let stored = StoredGmailCredential::from_broker_token(
            OAuthBrokerToken {
                access_token: "access-token".to_string(),
                token_type: Some("Bearer".to_string()),
                expires_in: Some(3600),
                refresh_token_handle: Some("handle-1".to_string()),
                account_id: Some("acct-1".to_string()),
                account_label: Some("me@example.com".to_string()),
                workspace_id: Some("gmail".to_string()),
                workspace_name: Some("Gmail".to_string()),
                scopes: vec!["openid".to_string()],
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        assert_eq!(stored.kind, "oauth");
        assert_eq!(stored.connector, GMAIL_CONNECTOR_ID);
        assert_eq!(stored.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(stored.expires_at, Some(3700));
        let json = serde_json::to_string(&stored).expect("serialize");
        assert!(!json.contains("\"refresh_token\":"));
        assert!(!json.contains("client_secret"));
    }
}
```

- [ ] **Step 2: Add the crate to the workspace**

Modify root `Cargo.toml`:

```toml
[workspace]
members = [
  "apps/desktop/src-tauri",
  "crates/loc-cli",
  "crates/localityd",
  "crates/locality-core",
  "crates/locality-connector",
  "crates/locality-platform",
  "crates/locality-store",
  "crates/locality-notion",
  "crates/locality-google-docs",
  "crates/locality-gmail",
  "platform/linux/locality-fuse",
  "platform/windows/locality-cloud-files",
]

[workspace.dependencies]
locality-core = { path = "crates/locality-core" }
locality-connector = { path = "crates/locality-connector" }
locality-platform = { path = "crates/locality-platform" }
locality-store = { path = "crates/locality-store" }
locality-notion = { path = "crates/locality-notion" }
locality-google-docs = { path = "crates/locality-google-docs" }
locality-gmail = { path = "crates/locality-gmail" }
```

Create `crates/locality-gmail/Cargo.toml`:

```toml
[package]
name = "locality-gmail"
version = "0.3.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true

[lib]
name = "locality_gmail"
path = "src/lib.rs"

[dependencies]
locality-core.workspace = true
locality-connector.workspace = true
base64 = "0.22"
reqwest = { version = "0.13", default-features = false, features = ["blocking", "json", "query", "rustls-no-provider"] }
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
yaml_serde = "0.10"
```

Create `crates/locality-gmail/src/lib.rs`:

```rust
pub mod client;
pub mod connector;
pub mod dto;
pub mod oauth;
pub mod render;

pub use connector::{GmailConfig, GmailConnector};
pub use oauth::{
    DEFAULT_GMAIL_OAUTH_BROKER_URL, DEFAULT_GMAIL_OAUTH_REDIRECT_URI, GMAIL_CONNECTOR_ID,
    GMAIL_OAUTH_SCOPES, HttpGmailOAuthBrokerClient, StoredGmailCredential,
    gmail_capabilities_json,
};
```

- [ ] **Step 3: Implement OAuth constants and credential storage**

Replace `crates/locality-gmail/src/oauth.rs` with:

```rust
use std::sync::OnceLock;

use locality_connector::ConnectorCapabilities;
use locality_connector::oauth_broker::{
    OAuthBrokerCodeExchange, OAuthBrokerRefresh, OAuthBrokerStart, OAuthBrokerStartResponse,
    OAuthBrokerToken,
};
use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const GMAIL_CONNECTOR_ID: &str = "gmail";
pub const DEFAULT_GMAIL_OAUTH_BROKER_URL: &str =
    "https://afs-oauth-broker.saurabh-b07.workers.dev";
pub const DEFAULT_GMAIL_OAUTH_REDIRECT_URI: &str =
    "http://localhost:8757/oauth/gmail/callback";
pub const GMAIL_OAUTH_SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/gmail.compose",
];

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGmailCredential {
    pub kind: String,
    pub connector: String,
    pub access_token: String,
    pub token_type: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_broker_url: Option<String>,
    pub account_id: Option<String>,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub scopes: Vec<String>,
    pub refresh_token_handle: Option<String>,
    pub acquired_at: u64,
    pub expires_at: Option<u64>,
}

impl StoredGmailCredential {
    pub fn from_broker_token(
        token: OAuthBrokerToken,
        client_id: String,
        broker_url: String,
        acquired_at: u64,
    ) -> Self {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Self {
            kind: "oauth".to_string(),
            connector: GMAIL_CONNECTOR_ID.to_string(),
            access_token: token.access_token,
            token_type: token.token_type,
            oauth_client_id: Some(client_id),
            oauth_broker_url: Some(broker_url),
            account_id: token.account_id,
            account_label: token.account_label,
            workspace_id: token.workspace_id,
            workspace_name: token.workspace_name,
            scopes: token.scopes,
            refresh_token_handle: token.refresh_token_handle,
            acquired_at,
            expires_at,
        }
    }

    pub fn refreshed(&self, token: OAuthBrokerToken, acquired_at: u64) -> Self {
        let expires_at = token
            .expires_in
            .and_then(|expires_in| acquired_at.checked_add(expires_in));
        Self {
            kind: "oauth".to_string(),
            connector: GMAIL_CONNECTOR_ID.to_string(),
            access_token: token.access_token,
            token_type: token.token_type.or_else(|| self.token_type.clone()),
            oauth_client_id: self.oauth_client_id.clone(),
            oauth_broker_url: self.oauth_broker_url.clone(),
            account_id: token.account_id.or_else(|| self.account_id.clone()),
            account_label: token.account_label.or_else(|| self.account_label.clone()),
            workspace_id: token.workspace_id.or_else(|| self.workspace_id.clone()),
            workspace_name: token.workspace_name.or_else(|| self.workspace_name.clone()),
            scopes: if token.scopes.is_empty() { self.scopes.clone() } else { token.scopes },
            refresh_token_handle: token
                .refresh_token_handle
                .or_else(|| self.refresh_token_handle.clone()),
            acquired_at,
            expires_at,
        }
    }

    pub fn expires_soon(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= now.saturating_add(60))
    }
}

#[derive(Clone, Debug)]
pub struct HttpGmailOAuthBrokerClient {
    base_url: String,
    client: Client,
}

impl HttpGmailOAuthBrokerClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: gmail_http_client(),
        }
    }

    pub fn start(&self, request: &OAuthBrokerStart) -> LocalityResult<OAuthBrokerStartResponse> {
        self.post_json("/v1/oauth/gmail/start", request)
    }

    pub fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/gmail/exchange", request)
    }

    pub fn refresh_token(&self, request: &OAuthBrokerRefresh) -> LocalityResult<OAuthBrokerToken> {
        self.post_json("/v1/oauth/gmail/refresh", request)
    }

    fn post_json<T, B>(&self, path: &str, body: &B) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .map_err(|error| LocalityError::Io(format!("gmail oauth broker request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
            return Err(LocalityError::Io(format!(
                "gmail oauth broker returned HTTP {status}: {body}"
            )));
        }
        response.json().map_err(|error| {
            LocalityError::Io(format!("gmail oauth broker response decode failed: {error}"))
        })
    }
}

pub fn gmail_capabilities_json() -> Result<String, serde_json::Error> {
    let capabilities = ConnectorCapabilities {
        supports_block_updates: false,
        supports_databases: false,
        supports_oauth: true,
        supports_remote_observation: true,
        supports_lazy_child_enumeration: true,
        supports_media_download: false,
        supports_undo: false,
        supports_batch_observation: false,
    };
    serde_json::to_string(&capabilities)
}

fn gmail_http_client() -> Client {
    ensure_reqwest_crypto_provider();
    Client::new()
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
```

- [ ] **Step 4: Add Gmail DTOs**

Create `crates/locality-gmail/src/dto.rs`:

```rust
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessageList {
    #[serde(default)]
    pub messages: Vec<GmailMessageRef>,
    pub next_page_token: Option<String>,
    pub result_size_estimate: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessageRef {
    pub id: String,
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessage {
    pub id: String,
    pub thread_id: Option<String>,
    #[serde(default)]
    pub label_ids: Vec<String>,
    pub snippet: Option<String>,
    pub internal_date: Option<String>,
    pub payload: Option<GmailMessagePart>,
    pub raw: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessagePart {
    pub part_id: Option<String>,
    pub mime_type: Option<String>,
    pub filename: Option<String>,
    #[serde(default)]
    pub headers: Vec<GmailHeader>,
    pub body: Option<GmailMessagePartBody>,
    #[serde(default)]
    pub parts: Vec<GmailMessagePart>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessagePartBody {
    pub size: Option<u64>,
    pub data: Option<String>,
    pub attachment_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailDraftCreateRequest {
    pub message: GmailRawMessage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailDraftSendRequest {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailRawMessage {
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailDraft {
    pub id: String,
    pub message: GmailMessage,
}

pub fn header_map(part: &GmailMessagePart) -> BTreeMap<String, String> {
    part.headers
        .iter()
        .map(|header| (header.name.to_ascii_lowercase(), header.value.clone()))
        .collect()
}
```

- [ ] **Step 5: Add Gmail HTTP client**

Create `crates/locality-gmail/src/client.rs`:

```rust
use std::sync::OnceLock;
use std::time::Duration;

use locality_core::{LocalityError, LocalityResult};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::dto::{
    GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailMessageList,
};

pub const DEFAULT_GMAIL_API_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1";
const GMAIL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

static REQWEST_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();

pub trait GmailApi: std::fmt::Debug + Send + Sync {
    fn list_messages(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> LocalityResult<GmailMessageList>;
    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage>;
    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft>;
    fn send_draft(&self, request: GmailDraftSendRequest) -> LocalityResult<GmailMessage>;
}

#[derive(Clone, Debug)]
pub struct HttpGmailApiClient {
    access_token: String,
    base_url: String,
    client: Client,
}

impl HttpGmailApiClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self::with_base_url(access_token, DEFAULT_GMAIL_API_BASE_URL)
    }

    pub fn with_base_url(access_token: impl Into<String>, base_url: impl Into<String>) -> Self {
        ensure_reqwest_crypto_provider();
        let client = Client::builder()
            .timeout(GMAIL_HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            access_token: access_token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    fn get_json<T>(&self, path: &str, query: Vec<(String, String)>) -> LocalityResult<T>
    where
        T: DeserializeOwned,
    {
        let mut request = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.access_token);
        for (key, value) in query {
            request = request.query(&[(key.as_str(), value.as_str())]);
        }
        decode_response(request.send(), "gmail api GET")
    }

    fn post_json<T, B>(&self, path: &str, body: &B) -> LocalityResult<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        decode_response(
            self.client
                .post(format!("{}{}", self.base_url, path))
                .bearer_auth(&self.access_token)
                .json(body)
                .send(),
            "gmail api POST",
        )
    }
}

impl GmailApi for HttpGmailApiClient {
    fn list_messages(
        &self,
        label_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> LocalityResult<GmailMessageList> {
        let mut query = vec![
            ("labelIds".to_string(), label_id.to_string()),
            ("maxResults".to_string(), max_results.to_string()),
        ];
        if let Some(page_token) = page_token {
            query.push(("pageToken".to_string(), page_token.to_string()));
        }
        self.get_json("/users/me/messages", query)
    }

    fn get_message_metadata(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        self.get_json(
            &format!("/users/me/messages/{message_id}"),
            vec![
                ("format".to_string(), "metadata".to_string()),
                (
                    "metadataHeaders".to_string(),
                    "From,To,Cc,Bcc,Subject,Date,Message-ID".to_string(),
                ),
            ],
        )
    }

    fn get_message_full(&self, message_id: &str) -> LocalityResult<GmailMessage> {
        self.get_json(
            &format!("/users/me/messages/{message_id}"),
            vec![("format".to_string(), "full".to_string())],
        )
    }

    fn create_draft(&self, request: GmailDraftCreateRequest) -> LocalityResult<GmailDraft> {
        self.post_json("/users/me/drafts", &request)
    }

    fn send_draft(&self, request: GmailDraftSendRequest) -> LocalityResult<GmailMessage> {
        self.post_json("/users/me/drafts/send", &request)
    }
}

fn decode_response<T>(
    response: Result<reqwest::blocking::Response, reqwest::Error>,
    context: &str,
) -> LocalityResult<T>
where
    T: DeserializeOwned,
{
    let response = response.map_err(|error| LocalityError::Io(format!("{context} failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .unwrap_or_else(|error| format!("<failed to read error body: {error}>"));
        return Err(LocalityError::Io(format!("{context} returned HTTP {status}: {body}")));
    }
    response
        .json()
        .map_err(|error| LocalityError::Io(format!("{context} response decode failed: {error}")))
}

fn ensure_reqwest_crypto_provider() {
    REQWEST_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
```

- [ ] **Step 6: Run crate tests**

Run:

```bash
cargo test -p locality-gmail
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/locality-gmail
git commit -m "feat: add Gmail connector crate skeleton"
```

## Task 4: Implement Gmail Rendering And Draft MIME Parsing

**Files:**
- Create: `crates/locality-gmail/src/render.rs`
- Test: `crates/locality-gmail/src/render.rs`

- [ ] **Step 1: Write render and draft tests**

Create `crates/locality-gmail/src/render.rs` with these tests first:

```rust
#[cfg(test)]
mod tests {
    use super::{GmailDraftDocument, GmailNativeBundle, build_draft_mime, render_gmail_message};
    use crate::dto::GmailMessage;

    #[test]
    fn renders_plain_text_message_with_gmail_frontmatter() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg-1",
            "threadId": "thread-1",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "From", "value": "Ann <ann@example.com>" },
                    { "name": "To", "value": "me@example.com" },
                    { "name": "Subject", "value": "Hello" },
                    { "name": "Date", "value": "Tue, 14 Jul 2026 09:30:00 +0000" }
                ],
                "body": { "data": "SGVsbG8gZnJvbSBHbWFpbC4K" }
            }
        }))
        .expect("message");
        let rendered = render_gmail_message(&GmailNativeBundle {
            mailbox: "inbox".to_string(),
            message,
        })
        .expect("render");

        assert!(rendered.document.frontmatter.contains("connector: gmail"));
        assert!(rendered.document.frontmatter.contains("mailbox: \"inbox\""));
        assert!(rendered.document.frontmatter.contains("subject: \"Hello\""));
        assert_eq!(rendered.document.body, "Hello from Gmail.\n");
        assert_eq!(rendered.shadow.entity_id.as_str(), "msg-1");
    }

    #[test]
    fn builds_rfc822_mime_from_local_draft() {
        let draft = GmailDraftDocument {
            to: vec!["ann@example.com".to_string()],
            cc: vec!["copy@example.com".to_string()],
            bcc: Vec::new(),
            subject: "Hello".to_string(),
            body: "Thanks.\n".to_string(),
        };

        let mime = build_draft_mime(&draft).expect("mime");

        assert!(mime.contains("To: ann@example.com\r\n"));
        assert!(mime.contains("Cc: copy@example.com\r\n"));
        assert!(mime.contains("Subject: Hello\r\n"));
        assert!(mime.contains("Content-Type: text/plain; charset=\"UTF-8\"\r\n"));
        assert!(mime.ends_with("\r\n\r\nThanks.\n"));
        assert!(!mime.contains("Bcc:"));
    }

    #[test]
    fn draft_requires_recipient_and_subject() {
        let draft = GmailDraftDocument {
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            body: "Body".to_string(),
        };

        let error = build_draft_mime(&draft).expect_err("invalid draft");
        assert!(error.to_string().contains("Gmail draft requires at least one `to` recipient"));
    }
}
```

- [ ] **Step 2: Run failing tests**

Run:

```bash
cargo test -p locality-gmail render
```

Expected: FAIL because render types and functions are not implemented.

- [ ] **Step 3: Implement render helpers**

Replace `crates/locality-gmail/src/render.rs` with:

```rust
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::dto::{GmailMessage, GmailMessagePart, header_map};
use crate::oauth::GMAIL_CONNECTOR_ID;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailNativeBundle {
    pub mailbox: String,
    pub message: GmailMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailDraftDocument {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

pub fn render_gmail_message(bundle: &GmailNativeBundle) -> LocalityResult<GmailRenderedEntity> {
    let body = message_body(&bundle.message)
        .filter(|body| !body.is_empty())
        .unwrap_or_else(|| {
            if has_attachments(&bundle.message) {
                "Attachments are not rendered by Locality Gmail v1.\n".to_string()
            } else {
                String::new()
            }
        });
    let frontmatter = message_frontmatter(bundle);
    let document = CanonicalDocument::new(frontmatter.clone(), body.clone());
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new(bundle.message.id.clone()),
        body,
        1,
        vec![RemoteId::new(format!("{}:body", bundle.message.id))],
    )
    .map_err(|error| LocalityError::InvalidState(error.to_string()))?
    .with_frontmatter(frontmatter);
    Ok(GmailRenderedEntity { document, shadow })
}

pub fn message_frontmatter(bundle: &GmailNativeBundle) -> String {
    let message = &bundle.message;
    let version = remote_version(message);
    let headers = message
        .payload
        .as_ref()
        .map(header_map)
        .unwrap_or_default();
    let subject = headers
        .get("subject")
        .cloned()
        .unwrap_or_else(|| "(no subject)".to_string());
    format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngmail:\n  mailbox: {}\n  message_id: {}\n  thread_id: {}\n  labels: [{}]\nfrom: {}\nto: [{}]\ncc: [{}]\nbcc: []\nsubject: {}\ndate: {}\n",
        yaml_scalar(&message.id),
        GMAIL_CONNECTOR_ID,
        yaml_scalar(&version),
        yaml_scalar(&version),
        yaml_scalar(&subject),
        yaml_scalar(&bundle.mailbox),
        yaml_scalar(&message.id),
        yaml_scalar(message.thread_id.as_deref().unwrap_or("")),
        message.label_ids.iter().map(|label| yaml_scalar(label)).collect::<Vec<_>>().join(", "),
        yaml_scalar(headers.get("from").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("to").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("cc").map(String::as_str).unwrap_or("")),
        yaml_scalar(&subject),
        yaml_scalar(headers.get("date").map(String::as_str).unwrap_or("")),
    )
}

pub fn remote_version(message: &GmailMessage) -> String {
    format!(
        "gmail:{}:{}",
        message.id,
        message.internal_date.as_deref().unwrap_or("unknown")
    )
}

pub fn build_draft_mime(draft: &GmailDraftDocument) -> LocalityResult<String> {
    if draft.to.iter().all(|value| value.trim().is_empty()) {
        return Err(LocalityError::Validation(vec![
            locality_core::validation::ValidationIssue::new(
                "gmail_draft_missing_to",
                std::path::PathBuf::new(),
                Some(1),
                "Gmail draft requires at least one `to` recipient",
                Some("add `to: [\"name@example.com\"]` to the frontmatter".to_string()),
            ),
        ]));
    }
    if draft.subject.trim().is_empty() {
        return Err(LocalityError::Validation(vec![
            locality_core::validation::ValidationIssue::new(
                "gmail_draft_missing_subject",
                std::path::PathBuf::new(),
                Some(1),
                "Gmail draft requires a non-empty subject",
                Some("add `subject: \"Subject text\"` to the frontmatter".to_string()),
            ),
        ]));
    }

    let mut mime = String::new();
    mime.push_str(&format!("To: {}\r\n", draft.to.join(", ")));
    if !draft.cc.is_empty() {
        mime.push_str(&format!("Cc: {}\r\n", draft.cc.join(", ")));
    }
    if !draft.bcc.is_empty() {
        mime.push_str(&format!("Bcc: {}\r\n", draft.bcc.join(", ")));
    }
    mime.push_str(&format!("Subject: {}\r\n", sanitize_header(&draft.subject)));
    mime.push_str("MIME-Version: 1.0\r\n");
    mime.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    mime.push_str("Content-Transfer-Encoding: 8bit\r\n");
    mime.push_str("\r\n");
    mime.push_str(&draft.body);
    Ok(mime)
}

pub fn raw_message_base64url(mime: &str) -> String {
    URL_SAFE_NO_PAD.encode(mime.as_bytes())
}

fn message_body(message: &GmailMessage) -> Option<String> {
    let payload = message.payload.as_ref()?;
    text_part(payload, "text/plain")
        .or_else(|| text_part(payload, "text/html").map(strip_html_tags))
        .map(ensure_trailing_newline)
}

fn text_part(part: &GmailMessagePart, mime_type: &str) -> Option<String> {
    if part.mime_type.as_deref() == Some(mime_type)
        && let Some(data) = part.body.as_ref().and_then(|body| body.data.as_ref())
    {
        return URL_SAFE_NO_PAD
            .decode(data.as_bytes())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
    }
    part.parts.iter().find_map(|part| text_part(part, mime_type))
}

fn has_attachments(message: &GmailMessage) -> bool {
    fn part_has_attachment(part: &GmailMessagePart) -> bool {
        part.body
            .as_ref()
            .and_then(|body| body.attachment_id.as_ref())
            .is_some()
            || part.parts.iter().any(part_has_attachment)
    }
    message.payload.as_ref().is_some_and(part_has_attachment)
}

fn strip_html_tags(input: String) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn yaml_list_items(header: &str) -> String {
    header
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(yaml_scalar)
        .collect::<Vec<_>>()
        .join(", ")
}

fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

fn yaml_scalar(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '/'))
        && !value.is_empty()
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
```

- [ ] **Step 4: Run render tests**

Run:

```bash
cargo test -p locality-gmail render
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/locality-gmail/src/render.rs
git commit -m "feat: render Gmail messages"
```

## Task 5: Implement Gmail Connector Enumeration, Fetch, And Send Apply

**Files:**
- Create: `crates/locality-gmail/src/connector.rs`
- Test: `crates/locality-gmail/src/connector.rs`

- [ ] **Step 1: Write connector behavior tests**

Add tests in `crates/locality-gmail/src/connector.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use locality_connector::{ChildContainer, Connector, EnumerateRequest, ListChildrenRequest};
    use locality_core::journal::{PushId, PushOperationId};
    use locality_core::model::{EntityKind, MountId, RemoteId};
    use locality_core::planner::{PushOperation, PushPlan};
    use locality_core::push::RemotePrecondition;

    use super::{GmailConfig, GmailConnector};
    use crate::client::GmailApi;
    use crate::dto::{
        GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailMessageList,
        GmailMessageRef,
    };

    #[test]
    fn enumerate_projects_three_folders_and_recent_inbox_sent_messages() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect("enumerate");

        assert!(entries.iter().any(|entry| entry.path == std::path::PathBuf::from("inbox")));
        assert!(entries.iter().any(|entry| entry.path == std::path::PathBuf::from("sent")));
        assert!(entries.iter().any(|entry| entry.path == std::path::PathBuf::from("draft")));
        assert!(entries.iter().any(|entry| entry.path.starts_with("inbox/")));
        assert!(entries.iter().any(|entry| entry.path.starts_with("sent/")));
        assert!(!entries.iter().any(|entry| entry.path.starts_with("draft/")));
        assert_eq!(api.calls.lock().expect("calls").list_max_results, vec![100, 100]);
    }

    #[test]
    fn list_children_for_draft_folder_returns_empty_remote_entries() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("gmail-folder:draft")),
                parent_path: "draft".into(),
            })
            .expect("list draft");

        assert!(result.entries.is_empty());
    }

    #[test]
    fn apply_create_entity_creates_and_sends_gmail_draft() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::new(),
                body: "Body\n".to_string(),
                source_path: "draft/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(result.changed_remote_ids, vec![RemoteId::new("sent-msg-1")]);
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert_eq!(calls.sent_drafts, vec!["draft-1"]);
    }

    #[derive(Default, Debug)]
    struct FakeGmailApi {
        calls: Mutex<FakeCalls>,
    }

    #[derive(Default, Debug)]
    struct FakeCalls {
        list_max_results: Vec<u32>,
        created_drafts: usize,
        sent_drafts: Vec<String>,
    }

    impl GmailApi for FakeGmailApi {
        fn list_messages(
            &self,
            label_id: &str,
            max_results: u32,
            _page_token: Option<&str>,
        ) -> locality_core::LocalityResult<GmailMessageList> {
            self.calls.lock().expect("calls").list_max_results.push(max_results);
            let id = match label_id {
                "INBOX" => "inbox-msg-1",
                "SENT" => "sent-msg-1",
                other => panic!("unexpected label {other}"),
            };
            Ok(GmailMessageList {
                messages: vec![GmailMessageRef {
                    id: id.to_string(),
                    thread_id: Some(format!("{id}-thread")),
                }],
                next_page_token: None,
                result_size_estimate: Some(1),
            })
        }

        fn get_message_metadata(&self, message_id: &str) -> locality_core::LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn get_message_full(&self, message_id: &str) -> locality_core::LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn create_draft(
            &self,
            _request: GmailDraftCreateRequest,
        ) -> locality_core::LocalityResult<GmailDraft> {
            self.calls.lock().expect("calls").created_drafts += 1;
            Ok(GmailDraft {
                id: "draft-1".to_string(),
                message: message_fixture("draft-message-1"),
            })
        }

        fn send_draft(
            &self,
            request: GmailDraftSendRequest,
        ) -> locality_core::LocalityResult<GmailMessage> {
            self.calls.lock().expect("calls").sent_drafts.push(request.id);
            Ok(message_fixture("sent-msg-1"))
        }
    }

    fn message_fixture(id: &str) -> GmailMessage {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "threadId": format!("{id}-thread"),
            "labelIds": if id.starts_with("sent") { ["SENT"] } else { ["INBOX"] },
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "From", "value": "Ann <ann@example.com>" },
                    { "name": "To", "value": "me@example.com" },
                    { "name": "Subject", "value": "Hello" },
                    { "name": "Date", "value": "Tue, 14 Jul 2026 09:30:00 +0000" }
                ],
                "body": { "data": "Qm9keQo" }
            }
        }))
        .expect("message")
    }
}
```

- [ ] **Step 2: Run failing connector tests**

Run:

```bash
cargo test -p locality-gmail connector
```

Expected: FAIL because `GmailConnector` is not implemented.

- [ ] **Step 3: Implement connector skeleton and folder projection**

Create `crates/locality-gmail/src/connector.rs` with:

```rust
use std::collections::{BTreeSet, BTreeMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::journal::JournalApplyEffect;
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, RemoteId, TreeEntry};
use locality_core::planner::{PushOperation, PushOperationKind};
use locality_core::{LocalityError, LocalityResult};

use crate::client::{GmailApi, HttpGmailApiClient};
use crate::dto::{GmailDraftCreateRequest, GmailDraftSendRequest, GmailRawMessage};
use crate::oauth::GMAIL_CONNECTOR_ID;
use crate::render::{
    GmailDraftDocument, GmailNativeBundle, build_draft_mime, raw_message_base64url,
    remote_version, render_gmail_message,
};

const RECENT_LIMIT: u32 = 100;
const INBOX_FOLDER_ID: &str = "gmail-folder:inbox";
const SENT_FOLDER_ID: &str = "gmail-folder:sent";
const DRAFT_FOLDER_ID: &str = "gmail-folder:draft";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailConfig {
    pub access_token: String,
}

impl GmailConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
        }
    }
}

#[derive(Clone)]
pub struct GmailConnector {
    config: GmailConfig,
    api: Arc<dyn GmailApi>,
}

impl std::fmt::Debug for GmailConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailConnector")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

impl GmailConnector {
    pub fn new(config: GmailConfig) -> Self {
        let api = Arc::new(HttpGmailApiClient::new(config.access_token.clone()));
        Self::with_api(config, api)
    }

    pub fn with_api(config: GmailConfig, api: Arc<dyn GmailApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &GmailConfig {
        &self.config
    }
}

impl Connector for GmailConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GMAIL_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: false,
            supports_databases: false,
            supports_oauth: true,
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            supports_media_download: false,
            supports_undo: false,
            supports_batch_observation: false,
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        [PushOperationKind::CreateEntity].into_iter().collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        let mut entries = gmail_folder_entries(&request.mount_id);
        entries.extend(list_label_entries(
            self.api.as_ref(),
            &request.mount_id,
            "INBOX",
            "inbox",
        )?);
        entries.extend(list_label_entries(
            self.api.as_ref(),
            &request.mount_id,
            "SENT",
            "sent",
        )?);
        Ok(entries)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => gmail_folder_entries(&request.mount_id),
            ChildContainer::DirectoryChildren(remote_id) if remote_id.as_str() == INBOX_FOLDER_ID => {
                list_label_entries(self.api.as_ref(), &request.mount_id, "INBOX", "inbox")?
            }
            ChildContainer::DirectoryChildren(remote_id) if remote_id.as_str() == SENT_FOLDER_ID => {
                list_label_entries(self.api.as_ref(), &request.mount_id, "SENT", "sent")?
            }
            ChildContainer::DirectoryChildren(remote_id) if remote_id.as_str() == DRAFT_FOLDER_ID => {
                Vec::new()
            }
            _ => Vec::new(),
        };
        Ok(ListChildrenResult { entries })
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        if let Some((title, path)) = folder_title_path(request.remote_id.as_str()) {
            return Ok(RemoteObservation::new(
                request.mount_id,
                request.remote_id,
                EntityKind::Directory,
                title.to_string(),
                PathBuf::from(path),
            )
            .with_remote_version(RemoteVersion::new(format!("folder:{title}"))));
        }
        let message = self.api.get_message_metadata(request.remote_id.as_str())?;
        let mailbox = mailbox_from_labels(&message.label_ids);
        Ok(message_observation(request.mount_id, mailbox, message))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let message = self.api.get_message_full(request.remote_id.as_str())?;
        let mailbox = mailbox_from_labels(&message.label_ids).to_string();
        let raw = serde_json::to_vec(&GmailNativeBundle { mailbox, message }).map_err(|error| {
            LocalityError::Io(format!("gmail native encode failed: {error}"))
        })?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "gmail_message".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle = serde_json::from_slice::<GmailNativeBundle>(&entity.raw).map_err(|error| {
            LocalityError::Io(format!("gmail native decode failed: {error}"))
        })?;
        render_gmail_message(&bundle).map(|rendered| rendered.document)
    }

    fn parse(&self, document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        let draft = parse_draft_document(document)?;
        let raw = serde_json::to_vec(&draft)
            .map_err(|error| LocalityError::Io(format!("gmail draft encode failed: {error}")))?;
        Ok(ParsedEntity {
            remote_id: RemoteId::new("gmail-local-draft"),
            native: NativeEntity {
                remote_id: RemoteId::new("gmail-local-draft"),
                kind: "gmail_draft".to_string(),
                raw,
            },
        })
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        apply_gmail_plan(self.api.as_ref(), request)
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("gmail undo"))
    }
}
```

- [ ] **Step 4: Add helper functions in the same file**

Append the helpers below the connector impl:

```rust
fn gmail_folder_entries(mount_id: &locality_core::model::MountId) -> Vec<TreeEntry> {
    [
        (INBOX_FOLDER_ID, "inbox"),
        (SENT_FOLDER_ID, "sent"),
        (DRAFT_FOLDER_ID, "draft"),
    ]
    .into_iter()
    .map(|(remote_id, title)| TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Directory,
        title: title.to_string(),
        path: PathBuf::from(title),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(format!("folder:{title}")),
        stub_frontmatter: None,
    })
    .collect()
}

fn folder_title_path(remote_id: &str) -> Option<(&'static str, &'static str)> {
    match remote_id {
        INBOX_FOLDER_ID => Some(("inbox", "inbox")),
        SENT_FOLDER_ID => Some(("sent", "sent")),
        DRAFT_FOLDER_ID => Some(("draft", "draft")),
        _ => None,
    }
}

fn list_label_entries(
    api: &dyn GmailApi,
    mount_id: &locality_core::model::MountId,
    label_id: &str,
    mailbox: &str,
) -> LocalityResult<Vec<TreeEntry>> {
    let listed = api.list_messages(label_id, RECENT_LIMIT, None)?;
    let mut entries = Vec::new();
    for message_ref in listed.messages {
        let message = api.get_message_metadata(&message_ref.id)?;
        entries.push(message_tree_entry(mount_id, mailbox, message));
    }
    entries.sort_by(|left, right| right.remote_edited_at.cmp(&left.remote_edited_at).then_with(|| left.path.cmp(&right.path)));
    Ok(entries)
}

fn message_tree_entry(
    mount_id: &locality_core::model::MountId,
    mailbox: &str,
    message: crate::dto::GmailMessage,
) -> TreeEntry {
    let title = message_subject(&message);
    let path = Path::new(mailbox).join(message_filename(&message, &title));
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(message.id.clone()),
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version(&message)),
        stub_frontmatter: Some(crate::render::message_frontmatter(&GmailNativeBundle {
            mailbox: mailbox.to_string(),
            message,
        })),
    }
}

fn message_observation(
    mount_id: locality_core::model::MountId,
    mailbox: &str,
    message: crate::dto::GmailMessage,
) -> RemoteObservation {
    let title = message_subject(&message);
    let path = Path::new(mailbox).join(message_filename(&message, &title));
    RemoteObservation::new(
        mount_id,
        RemoteId::new(message.id.clone()),
        EntityKind::Page,
        title,
        path,
    )
    .with_parent(RemoteId::new(format!("gmail-folder:{mailbox}")))
    .with_remote_version(RemoteVersion::new(remote_version(&message)))
}

fn mailbox_from_labels(labels: &[String]) -> &'static str {
    if labels.iter().any(|label| label == "SENT") {
        "sent"
    } else if labels.iter().any(|label| label == "DRAFT") {
        "draft"
    } else {
        "inbox"
    }
}

fn message_subject(message: &crate::dto::GmailMessage) -> String {
    message
        .payload
        .as_ref()
        .map(crate::dto::header_map)
        .and_then(|headers| headers.get("subject").cloned())
        .filter(|subject| !subject.trim().is_empty())
        .unwrap_or_else(|| "(no subject)".to_string())
}

fn message_filename(message: &crate::dto::GmailMessage, subject: &str) -> String {
    let date = message.internal_date.as_deref().unwrap_or("unknown");
    format!("{}-{}-{}.md", date, slugify(subject), message.id)
}

fn slugify(input: &str) -> String {
    let mut slug = String::new();
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() { "message".to_string() } else { slug.to_string() }
}

fn parse_draft_document(document: &CanonicalDocument) -> LocalityResult<GmailDraftDocument> {
    #[derive(serde::Deserialize)]
    struct RawFrontmatter {
        to: Option<Vec<String>>,
        cc: Option<Vec<String>>,
        bcc: Option<Vec<String>>,
        subject: Option<String>,
    }
    let raw = yaml_serde::from_str::<RawFrontmatter>(&document.frontmatter).map_err(|error| {
        LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
            "gmail_draft_frontmatter_invalid",
            std::path::PathBuf::new(),
            error.location().map(|location| location.line() + 1),
            format!("invalid Gmail draft frontmatter: {error}"),
            Some("fix the YAML frontmatter before pushing".to_string()),
        )])
    })?;
    Ok(GmailDraftDocument {
        to: raw.to.unwrap_or_default(),
        cc: raw.cc.unwrap_or_default(),
        bcc: raw.bcc.unwrap_or_default(),
        subject: raw.subject.unwrap_or_default(),
        body: document.body.clone(),
    })
}

fn apply_gmail_plan(
    api: &dyn GmailApi,
    request: ApplyPlanRequest<'_>,
) -> LocalityResult<ApplyPlanResult> {
    let mut changed_remote_ids = Vec::new();
    let mut effects = Vec::new();
    for (index, operation) in request.plan.operations.iter().enumerate() {
        let operation_id = request
            .operation_ids
            .get(index)
            .cloned()
            .ok_or_else(|| LocalityError::InvalidState("missing operation id".to_string()))?;
        match operation {
            PushOperation::CreateEntity {
                parent_id,
                parent_kind,
                body,
                title,
                properties,
                ..
            } => {
                if parent_id.as_str() != DRAFT_FOLDER_ID || parent_kind != &Some(EntityKind::Directory) {
                    return Err(LocalityError::Unsupported(
                        "Gmail push only sends files created directly under draft/".to_string(),
                    ));
                }
                let draft_doc = draft_doc_from_properties(title, properties, body);
                let mime = build_draft_mime(&draft_doc)?;
                let draft = api.create_draft(GmailDraftCreateRequest {
                    message: GmailRawMessage {
                        raw: raw_message_base64url(&mime),
                    },
                })?;
                let sent = api.send_draft(GmailDraftSendRequest { id: draft.id })?;
                let sent_id = RemoteId::new(sent.id);
                changed_remote_ids.push(sent_id.clone());
                effects.push(JournalApplyEffect::CreatedEntity {
                    operation_id,
                    operation_index: index,
                    parent_id: RemoteId::new(SENT_FOLDER_ID),
                    entity_id: sent_id,
                });
            }
            _ => {
                return Err(LocalityError::Unsupported(
                    "Gmail v1 only supports sending created draft files".to_string(),
                ));
            }
        }
    }
    Ok(ApplyPlanResult {
        changed_remote_ids,
        effects,
    })
}

fn draft_doc_from_properties(
    title: &str,
    properties: &BTreeMap<String, locality_core::planner::PropertyValue>,
    body: &str,
) -> GmailDraftDocument {
    GmailDraftDocument {
        to: string_list_property(properties, "to"),
        cc: string_list_property(properties, "cc"),
        bcc: string_list_property(properties, "bcc"),
        subject: string_property(properties, "subject").unwrap_or_else(|| title.to_string()),
        body: body.to_string(),
    }
}

fn string_property(
    properties: &BTreeMap<String, locality_core::planner::PropertyValue>,
    key: &str,
) -> Option<String> {
    match properties.get(key) {
        Some(locality_core::planner::PropertyValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn string_list_property(
    properties: &BTreeMap<String, locality_core::planner::PropertyValue>,
    key: &str,
) -> Vec<String> {
    match properties.get(key) {
        Some(locality_core::planner::PropertyValue::List(values)) => values.clone(),
        Some(locality_core::planner::PropertyValue::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}
```

- [ ] **Step 5: Run connector tests**

Run:

```bash
cargo test -p locality-gmail connector
```

Expected: PASS after fixing compile errors from imports or formatting.

- [ ] **Step 6: Commit**

```bash
git add crates/locality-gmail/src/connector.rs
git commit -m "feat: implement Gmail connector send path"
```

## Task 6: Register Gmail In The Daemon Source Registry

**Files:**
- Modify: `crates/localityd/Cargo.toml`
- Modify: `crates/localityd/src/lib.rs`
- Create: `crates/localityd/src/gmail.rs`
- Modify: `crates/localityd/src/source.rs`
- Test: `crates/localityd/tests/source_descriptor.rs`

- [ ] **Step 1: Write failing registry tests**

In `crates/localityd/tests/source_descriptor.rs`, add:

```rust
#[test]
fn gmail_descriptor_comes_from_registry() {
    let descriptor = source_descriptor("gmail");

    assert_eq!(descriptor.id(), "gmail");
    assert_eq!(descriptor.display_name(), "Gmail");
    assert_eq!(descriptor.default_mount_id(), "gmail-main");
    assert_eq!(descriptor.connect_command(), Some("loc connect gmail"));
    assert!(descriptor.supports_oauth());
    assert!(descriptor.mount_guidance().contains("Gmail facts"));
    assert_eq!(descriptor.create_entity_parent_kinds(), &[EntityKind::Directory]);
}

#[test]
fn supported_connectors_include_gmail() {
    assert_eq!(
        supported_source_connectors(),
        vec!["notion", "google-docs", "gmail"]
    );
}
```

Make sure the test imports `EntityKind`:

```rust
use locality_core::model::EntityKind;
```

- [ ] **Step 2: Run failing registry tests**

Run:

```bash
cargo test -p localityd gmail_descriptor_comes_from_registry supported_connectors_include_gmail
```

Expected: FAIL because Gmail is not registered.

- [ ] **Step 3: Add Gmail dependency and module**

Modify `crates/localityd/Cargo.toml`:

```toml
locality-gmail.workspace = true
```

Modify `crates/localityd/src/lib.rs`:

```rust
pub mod gmail;
```

- [ ] **Step 4: Implement Gmail resolver**

Create `crates/localityd/src/gmail.rs`:

```rust
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::oauth_broker::OAuthBrokerRefresh;
use locality_connector::{Connector, FetchRequest};
use locality_core::hydration::HydrationRequest;
use locality_core::model::{RemoteId, TreeEntry};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_gmail::{
    GMAIL_CONNECTOR_ID, GmailConfig, GmailConnector, HttpGmailOAuthBrokerClient,
    StoredGmailCredential,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

pub fn resolve_gmail_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<GmailConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != GMAIL_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(mount.connector.clone()));
    }
    let connection = if let Some(connection_id) = &mount.connection_id {
        store
            .get_connection(connection_id)
            .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
            .ok_or_else(|| ConnectorResolveError::MissingConnection {
                message: format!("connection `{}` was not found", connection_id.0),
                suggested_command: "loc connect gmail".to_string(),
            })?
    } else {
        let active = active_gmail_connections(store)?;
        if active.len() != 1 {
            let message = if active.is_empty() {
                "missing Gmail connection; run `loc connect gmail`".to_string()
            } else {
                "mount has no connection_id and multiple Gmail connections exist".to_string()
            };
            return Err(ConnectorResolveError::MissingConnection {
                message,
                suggested_command: "loc connect gmail".to_string(),
            });
        }
        active[0].clone()
    };
    validate_connection_profile(store, &connection)?;
    connector_from_connection(credentials, &connection)
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<GmailConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: "loc connect gmail".to_string(),
        });
    }
    let token = connection_access_token(credentials, connection)?;
    Ok(GmailConnector::new(GmailConfig::new(token)))
}

fn connection_access_token(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<String, ConnectorResolveError> {
    let secret = credentials
        .get(&connection.secret_ref)
        .map_err(|error| credential_error(connection, error))?;
    let mut stored = serde_json::from_str::<StoredGmailCredential>(&secret)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    if stored.expires_soon(timestamp_secs()) {
        let refreshed = refresh_oauth_credential(connection, &stored)?;
        stored = stored.refreshed(refreshed, timestamp_secs());
        let secret = serde_json::to_string(&stored)
            .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
        credentials
            .put(&connection.secret_ref, &secret)
            .map_err(|error| credential_error(connection, error))?;
    }
    Ok(stored.access_token)
}

fn refresh_oauth_credential(
    connection: &ConnectionRecord,
    stored: &StoredGmailCredential,
) -> Result<locality_connector::oauth_broker::OAuthBrokerToken, ConnectorResolveError> {
    let Some(refresh_token_handle) = stored.refresh_token_handle.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: "loc connect gmail".to_string(),
        });
    };
    let Some(broker_url) = stored.oauth_broker_url.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: "loc connect gmail".to_string(),
        });
    };
    HttpGmailOAuthBrokerClient::new(broker_url).refresh_token(&OAuthBrokerRefresh {
        connector: GMAIL_CONNECTOR_ID.to_string(),
        refresh_token_handle: Some(refresh_token_handle),
    })
    .map_err(|error| ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!("Gmail credential could not be refreshed: {error}")),
        suggested_command: "loc connect gmail".to_string(),
    })
}

fn active_gmail_connections<S>(store: &S) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository + ?Sized,
{
    let connections = store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    Ok(connections
        .into_iter()
        .filter(|connection| connection.connector == GMAIL_CONNECTOR_ID && connection.status == "active")
        .collect())
}

fn validate_connection_profile<S>(
    store: &S,
    connection: &ConnectionRecord,
) -> Result<(), ConnectorResolveError>
where
    S: ConnectorProfileRepository + ?Sized,
{
    let Some(profile_id) = &connection.profile_id else {
        return Ok(());
    };
    let profile = store
        .get_connector_profile(profile_id)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
        .ok_or_else(|| ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile_id.0.clone(),
            suggested_command: "loc connect gmail".to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: "loc connect gmail".to_string(),
        });
    }
    Ok(())
}

fn credential_error(
    connection: &ConnectionRecord,
    error: CredentialError,
) -> ConnectorResolveError {
    match error {
        CredentialError::NotFound(_) => ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: "loc connect gmail".to_string(),
        },
        CredentialError::Unavailable(message) | CredentialError::Io(message) => {
            ConnectorResolveError::CredentialStoreUnavailable(message)
        }
    }
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

impl SourcePushValidator for GmailConnector {
    fn validate_changed_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_gmail_changed_frontmatter(context)
    }

    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_gmail_create_frontmatter(context)
    }
}

pub(crate) fn validate_gmail_changed_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    if !context.relative_path.starts_with("draft") {
        report.push(ValidationIssue::new(
            "gmail_read_only_message",
            context.relative_path,
            None,
            "Gmail inbox and sent messages are read-only in Locality",
            Some("create a new Markdown file under draft/ to send email".to_string()),
        ));
    }
    Ok(report)
}

pub(crate) fn validate_gmail_create_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    if !context.relative_path.starts_with("draft/") {
        report.push(ValidationIssue::new(
            "gmail_create_outside_draft",
            context.relative_path,
            None,
            "Gmail can only send new files created directly under draft/",
            Some("move the Markdown file into draft/ and push again".to_string()),
        ));
    }
    if context.parsed.frontmatter.title.as_deref().unwrap_or("").trim().is_empty()
        && !context.parsed.frontmatter.properties.contains_key("subject")
    {
        report.push(ValidationIssue::new(
            "gmail_draft_missing_subject",
            context.relative_path,
            Some(1),
            "Gmail draft requires `subject` or `title` frontmatter",
            Some("add `subject: \"Subject text\"` to the draft frontmatter".to_string()),
        ));
    }
    if !context.parsed.frontmatter.properties.contains_key("to") {
        report.push(ValidationIssue::new(
            "gmail_draft_missing_to",
            context.relative_path,
            Some(1),
            "Gmail draft requires `to` frontmatter",
            Some("add `to: [\"recipient@example.com\"]` to the draft frontmatter".to_string()),
        ));
    }
    Ok(report)
}

impl SourceAdapter for GmailConnector {}

impl HydrationSource for GmailConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<locality_gmail::render::GmailNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("gmail native decode failed: {error}")))?;
        let rendered = locality_gmail::render::render_gmail_message(&bundle)?;
        Ok(HydratedEntity {
            document: rendered.document,
            shadow: rendered.shadow,
            remote_edited_at: Some(locality_gmail::render::remote_version(&bundle.message)),
            assets: Vec::new(),
        })
    }
}

impl crate::reconcile::ScheduledPullSource for GmailConnector {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        self.enumerate(locality_connector::EnumerateRequest {
            mount_id: mount.mount_id.clone(),
            cursor: None,
        })
    }

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        _remote_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}
```

- [ ] **Step 5: Register Gmail in `source.rs`**

Modify imports in `crates/localityd/src/source.rs`:

```rust
use locality_gmail::{GMAIL_CONNECTOR_ID, GmailConnector};
use crate::gmail::resolve_gmail_connector_for_mount;
```

Add variant:

```rust
pub enum ResolvedSource {
    Notion(NotionConnector),
    GoogleDocs(GoogleDocsConnector),
    Gmail(GmailConnector),
}
```

Add registry entry:

```rust
SourceRegistration {
    id: GMAIL_CONNECTOR_ID,
    descriptor: gmail_source_descriptor,
    resolve: resolve_gmail_source,
    validate_changed_frontmatter: crate::gmail::validate_gmail_changed_frontmatter,
    validate_create_frontmatter: crate::gmail::validate_gmail_create_frontmatter,
},
```

Add descriptor:

```rust
fn gmail_source_descriptor() -> SourceDescriptor {
    SourceDescriptor {
        id: Cow::Borrowed(GMAIL_CONNECTOR_ID),
        display_name: Cow::Borrowed("Gmail"),
        default_mount_id: Cow::Borrowed("gmail-main"),
        connect_command: Some(Cow::Borrowed("loc connect gmail")),
        auth_env_var: None,
        supports_oauth: true,
        mount_guidance: Cow::Owned(gmail_mount_guidance()),
        source_root_create_parent_kind: None,
        create_entity_parent_kinds: vec![EntityKind::Directory],
    }
}

fn gmail_mount_guidance() -> String {
    format!(
        "{}\n\
Gmail facts:\n\
- This mount has `inbox/`, `sent/`, and `draft/` folders.\n\
- `inbox/` and `sent/` are read-only and show the latest 100 messages in V1.\n\
- Create Markdown files directly under `draft/` to compose email.\n\
- `loc push` on a draft sends the email; there is no separate remote draft save in V1.\n\
- Attachments are not supported in V1.\n",
        generic_mount_guidance("Gmail")
    )
}
```

Add resolver:

```rust
fn resolve_gmail_source(
    store: &dyn SourceResolverStore,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<ResolvedSource, ConnectorResolveError> {
    resolve_gmail_connector_for_mount(store, credentials, mount).map(ResolvedSource::Gmail)
}
```

Update every `match self` in `impl Connector for ResolvedSource`, `impl HydrationSource for ResolvedSource`, `impl SourcePushValidator for ResolvedSource`, and `impl SourceAdapter for ResolvedSource` with these Gmail arms:

```rust
// Connector methods:
Self::Gmail(source) => source.kind(),
Self::Gmail(source) => source.capabilities(),
Self::Gmail(source) => source.supported_push_operations(),
Self::Gmail(source) => source.enumerate(request),
Self::Gmail(source) => source.observe(request),
Self::Gmail(source) => source.list_children(request),
Self::Gmail(source) => source.fetch(request),
Self::Gmail(source) => source.render(entity),
Self::Gmail(source) => source.parse(document),
Self::Gmail(source) => source.check_concurrency(request),
Self::Gmail(source) => source.apply(request),
Self::Gmail(source) => source.apply_undo(request),

// HydrationSource methods:
Self::Gmail(source) => source.fetch_render(request),
Self::Gmail(source) => source.fetch_database_schema_yaml(database_id),

// SourcePushValidator methods:
Self::Gmail(source) => source.validate_changed_frontmatter(context),
Self::Gmail(source) => source.validate_create_frontmatter(context),

// SourceAdapter scoped_to_mount:
Self::Gmail(source) => Self::Gmail(source.scoped_to_mount(mount)),

// SourceAdapter database_schema_yaml:
Self::Gmail(source) => SourceAdapter::database_schema_yaml(source, database_id),
```

- [ ] **Step 6: Run registry tests**

Run:

```bash
cargo test -p localityd gmail_descriptor_comes_from_registry supported_connectors_include_gmail
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/localityd/Cargo.toml crates/localityd/src/lib.rs crates/localityd/src/gmail.rs crates/localityd/src/source.rs crates/localityd/tests/source_descriptor.rs
git commit -m "feat: register Gmail source"
```

## Task 7: Preserve Draft Recipients Through Create Planning

**Files:**
- Modify: `crates/locality-gmail/src/connector.rs`
- Modify: `crates/localityd/src/gmail.rs`
- Test: `crates/localityd/tests/push_preparation.rs`
- Test: `crates/locality-gmail/src/connector.rs`

- [ ] **Step 1: Write failing push preparation test for recipients**

Add to `crates/localityd/tests/push_preparation.rs`:

```rust
#[test]
fn prepare_gmail_draft_create_keeps_subject_and_recipients_as_properties() {
    let fixture = PushPreparationFixture::new("gmail-draft-create");
    let mut store = fixture.store("gmail");
    store
        .save_entity(EntityRecord {
            mount_id: fixture.mount_id.clone(),
            remote_id: RemoteId::new("gmail-folder:draft"),
            kind: EntityKind::Directory,
            title: "draft".to_string(),
            path: "draft".into(),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some("folder:draft".to_string()),
        })
        .expect("save draft folder");
    let draft_path = fixture.root.join("draft/reply.md");
    std::fs::create_dir_all(draft_path.parent().expect("parent")).expect("draft dir");
    std::fs::write(
        &draft_path,
        "---\nloc:\n  type: page\n  connector: gmail\ntitle: Reply\nsubject: Reply subject\nto: [\"ann@example.com\"]\ncc: [\"copy@example.com\"]\nbcc: []\n---\nBody\n",
    )
    .expect("write draft");

    let prepared = localityd::push::prepare_push_with_state_root(
        &store,
        &localityd::source::LocalSourceValidator,
        &locality_core::push::PushApproval {
            assume_yes: true,
            confirm_dangerous: false,
        },
        &draft_path,
        Some(&fixture.state_root),
    )
    .expect("prepare");

    let plan = prepared.pipeline.plan.expect("plan");
    let operation = plan.operations.first().expect("operation");
    match operation {
        locality_core::planner::PushOperation::CreateEntity { properties, title, body, .. } => {
            assert_eq!(title, "Reply");
            assert_eq!(body, "Body\n");
            assert!(properties.contains_key("subject"));
            assert!(properties.contains_key("to"));
            assert!(properties.contains_key("cc"));
        }
        other => panic!("unexpected operation {other:#?}"),
    }
}
```

If `prepare_push_with_state_root` is private, add this test at the nearest public preparation layer already used in this file and assert the same `CreateEntity` operation.

- [ ] **Step 2: Run failing test**

Run:

```bash
cargo test -p localityd prepare_gmail_draft_create_keeps_subject_and_recipients_as_properties
```

Expected: FAIL until the test uses the correct public helper or the Gmail validator accepts the YAML shape.

- [ ] **Step 3: Parse recipients from `PushOperation::CreateEntity.properties`**

In `crates/locality-gmail/src/connector.rs`, replace `draft_doc_from_source_path` with:

```rust
fn draft_doc_from_properties(
    title: &str,
    properties: &BTreeMap<String, locality_core::planner::PropertyValue>,
    body: &str,
) -> GmailDraftDocument {
    GmailDraftDocument {
        to: string_list_property(properties, "to"),
        cc: string_list_property(properties, "cc"),
        bcc: string_list_property(properties, "bcc"),
        subject: string_property(properties, "subject").unwrap_or_else(|| title.to_string()),
        body: body.to_string(),
    }
}

fn string_property(
    properties: &BTreeMap<String, locality_core::planner::PropertyValue>,
    key: &str,
) -> Option<String> {
    match properties.get(key) {
        Some(locality_core::planner::PropertyValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn string_list_property(
    properties: &BTreeMap<String, locality_core::planner::PropertyValue>,
    key: &str,
) -> Vec<String> {
    match properties.get(key) {
        Some(locality_core::planner::PropertyValue::List(values)) => values.clone(),
        Some(locality_core::planner::PropertyValue::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}
```

Update the `PushOperation::CreateEntity` match:

```rust
PushOperation::CreateEntity {
    parent_id,
    parent_kind,
    body,
    title,
    properties,
    ..
} => {
    if parent_id.as_str() != DRAFT_FOLDER_ID || parent_kind != &Some(EntityKind::Directory) {
        return Err(LocalityError::Unsupported(
            "Gmail push only sends files created directly under draft/".to_string(),
        ));
    }
    let draft_doc = draft_doc_from_properties(title, properties, body);
    let mime = build_draft_mime(&draft_doc)?;
    let draft = api.create_draft(GmailDraftCreateRequest {
        message: GmailRawMessage {
            raw: raw_message_base64url(&mime),
        },
    })?;
    let sent = api.send_draft(GmailDraftSendRequest { id: draft.id })?;
    let sent_id = RemoteId::new(sent.id);
    changed_remote_ids.push(sent_id.clone());
    effects.push(JournalApplyEffect::CreatedEntity {
        operation_id,
        operation_index: index,
        parent_id: RemoteId::new(SENT_FOLDER_ID),
        entity_id: sent_id,
    });
}
```

- [ ] **Step 4: Tighten Gmail create validation**

In `crates/localityd/src/gmail.rs`, add these helpers:

```rust
fn frontmatter_string_list(
    value: Option<&yaml_serde::Value>,
) -> Vec<String> {
    match value {
        Some(yaml_serde::Value::Sequence(values)) => values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect(),
        Some(yaml_serde::Value::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}
```

Update `validate_gmail_create_frontmatter` to check recipient shape:

```rust
let to = frontmatter_string_list(context.parsed.frontmatter.properties.get("to"));
if to.iter().all(|value| value.trim().is_empty()) {
    report.push(ValidationIssue::new(
        "gmail_draft_missing_to",
        context.relative_path,
        Some(1),
        "Gmail draft requires at least one `to` recipient",
        Some("add `to: [\"recipient@example.com\"]` to the draft frontmatter".to_string()),
    ));
}
```

- [ ] **Step 5: Add connector test for properties-to-MIME**

In `crates/locality-gmail/src/connector.rs`, update `apply_create_entity_creates_and_sends_gmail_draft` so the `CreateEntity` operation includes properties:

```rust
let mut properties = std::collections::BTreeMap::new();
properties.insert(
    "subject".to_string(),
    locality_core::planner::PropertyValue::String("Hello".to_string()),
);
properties.insert(
    "to".to_string(),
    locality_core::planner::PropertyValue::List(vec!["ann@example.com".to_string()]),
);
```

Use `properties` in the operation. Extend `FakeCalls`:

```rust
created_raw: Vec<String>,
```

Store the raw payload in `create_draft`:

```rust
self.calls.lock().expect("calls").created_raw.push(request.message.raw);
```

Assert:

```rust
assert_eq!(calls.created_drafts, 1);
assert_eq!(calls.sent_drafts, vec!["draft-1"]);
assert!(!calls.created_raw[0].is_empty());
```

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test -p localityd prepare_gmail_draft_create_keeps_subject_and_recipients_as_properties
cargo test -p locality-gmail apply_create_entity_creates_and_sends_gmail_draft
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/locality-gmail/src/connector.rs crates/localityd/src/gmail.rs crates/localityd/tests/push_preparation.rs
git commit -m "feat: send Gmail draft recipients from frontmatter"
```

## Task 8: Add CLI Connect And Mount Support

**Files:**
- Modify: `crates/loc-cli/Cargo.toml`
- Modify: `crates/loc-cli/src/connect.rs`
- Modify: `crates/loc-cli/src/commands.rs`
- Test: `crates/loc-cli/tests/connect.rs`
- Test: `crates/loc-cli/src/commands.rs`

- [ ] **Step 1: Write failing connect test**

In `crates/loc-cli/tests/connect.rs`, add:

```rust
#[test]
fn connect_gmail_broker_oauth_stores_refresh_handle_without_secrets() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = FakeGmailBrokerOAuthExchange;

    let report = run_connect_gmail_broker_oauth(
        &mut store,
        &credentials,
        GmailBrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("gmail-default")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "google-client-id".to_string(),
            session: "session".to_string(),
            state: "state".to_string(),
            code: "gmail-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/gmail/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect");

    assert_eq!(report.connection_id, "gmail-default");
    assert_eq!(report.profile_id, DEFAULT_GMAIL_OAUTH_PROFILE_ID);
    assert_eq!(report.connector, "gmail");

    let secret = credentials
        .get("connection:gmail-default")
        .expect("secret")
        .expect("stored secret");
    let stored = serde_json::from_str::<locality_gmail::StoredGmailCredential>(&secret)
        .expect("stored gmail credential");
    assert_eq!(stored.refresh_token_handle.as_deref(), Some("gmail-refresh-handle"));
}

struct FakeGmailBrokerOAuthExchange;

impl GmailOAuthBrokerExchange for FakeGmailBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.connector, "gmail");
        assert_eq!(request.code, "gmail-code");
        Ok(OAuthBrokerToken {
            access_token: "gmail-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("gmail-refresh-handle".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("me@example.com".to_string()),
            workspace_id: Some("gmail".to_string()),
            workspace_name: Some("Gmail".to_string()),
            scopes: locality_gmail::GMAIL_OAUTH_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
        })
    }
}
```

Update imports in the test file for the new symbols.

- [ ] **Step 2: Run failing connect test**

Run:

```bash
cargo test -p loc-cli connect_gmail_broker_oauth_stores_refresh_handle_without_secrets
```

Expected: FAIL because Gmail connect functions are missing.

- [ ] **Step 3: Add CLI Gmail connect types**

Modify `crates/loc-cli/Cargo.toml`:

```toml
locality-gmail.workspace = true
```

In `crates/loc-cli/src/connect.rs`, import:

```rust
use locality_gmail::{
    GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, HttpGmailOAuthBrokerClient, StoredGmailCredential,
    gmail_capabilities_json,
};
```

Add constant and options:

```rust
pub const DEFAULT_GMAIL_OAUTH_PROFILE_ID: &str = "gmail-oauth-default";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GmailBrokerOAuthConnectOptions {
    pub connection_id: Option<ConnectionId>,
    pub broker_url: String,
    pub client_id: String,
    pub session: String,
    pub state: String,
    pub code: String,
    pub redirect_uri: String,
}

pub trait GmailOAuthBrokerExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, ConnectError>;
}

impl GmailOAuthBrokerExchange for HttpGmailOAuthBrokerClient {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, ConnectError> {
        HttpGmailOAuthBrokerClient::exchange_code(self, request)
            .map_err(|error| ConnectError::OAuthExchangeFailed(error.to_string()))
    }
}
```

Add `run_connect_gmail_broker_oauth` mirroring Google Docs, with these exact Gmail substitutions:

```rust
pub fn run_connect_gmail_broker_oauth<S, E>(
    store: &mut S,
    credentials: &dyn CredentialStore,
    options: GmailBrokerOAuthConnectOptions,
    exchange: &E,
) -> Result<ConnectReport, ConnectError>
where
    S: ConnectionRepository + ConnectorProfileRepository,
    E: GmailOAuthBrokerExchange,
{
    let connection_id = match options.connection_id {
        Some(connection_id) => connection_id,
        None => default_connection_id_for_connector(
            store,
            GMAIL_CONNECTOR_ID,
            "gmail-default",
            "Gmail",
        )?,
    };
    let exchange_request = OAuthBrokerCodeExchange {
        connector: GMAIL_CONNECTOR_ID.to_string(),
        session: options.session,
        state: options.state,
        code: options.code,
        redirect_uri: options.redirect_uri,
    };
    let token = exchange.exchange_code(&exchange_request)?;
    let acquired_at = timestamp_secs();
    let secret_ref = format!("connection:{}", connection_id.0);
    let stored = StoredGmailCredential::from_broker_token(
        token.clone(),
        options.client_id,
        options.broker_url,
        acquired_at,
    );
    let secret = serde_json::to_string(&stored)
        .map_err(|error| ConnectError::CredentialEncode(error.to_string()))?;
    credentials
        .put(&secret_ref, &secret)
        .map_err(ConnectError::Credential)?;

    let now = timestamp();
    let profile_id = ConnectorProfileId::new(DEFAULT_GMAIL_OAUTH_PROFILE_ID);
    store
        .save_connector_profile(default_gmail_oauth_profile(now.clone()))
        .map_err(ConnectError::Store)?;

    let display_name = connection_id.0.clone();
    let account_label = token
        .account_label
        .clone()
        .or_else(|| token.account_id.clone())
        .or_else(|| token.workspace_name.clone());
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id.clone()),
            connector: GMAIL_CONNECTOR_ID.to_string(),
            display_name: display_name.clone(),
            account_label: account_label.clone(),
            workspace_id: token.workspace_id.clone(),
            workspace_name: token.workspace_name.clone(),
            auth_kind: "oauth".to_string(),
            secret_ref,
            scopes: token.scopes.clone(),
            capabilities_json: gmail_capabilities_json()
                .map_err(|error| ConnectError::CredentialEncode(error.to_string()))?,
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: stored.expires_at.map(|expires_at| expires_at.to_string()),
        })
        .map_err(ConnectError::Store)?;

    Ok(ConnectReport {
        ok: true,
        command: "connect",
        connection_id: connection_id.0,
        profile_id: profile_id.0,
        connector: GMAIL_CONNECTOR_ID.to_string(),
        display_name,
        account_label,
        workspace_id: token.workspace_id,
        workspace_name: token.workspace_name,
        auth_kind: "oauth".to_string(),
    })
}
```

Add:

```rust
fn default_gmail_oauth_profile(now: String) -> ConnectorProfileRecord {
    ConnectorProfileRecord {
        profile_id: ConnectorProfileId::new(DEFAULT_GMAIL_OAUTH_PROFILE_ID),
        connector: GMAIL_CONNECTOR_ID.to_string(),
        display_name: "Gmail OAuth".to_string(),
        auth_kind: "oauth".to_string(),
        scopes: GMAIL_OAUTH_SCOPES.iter().map(|scope| scope.to_string()).collect(),
        capabilities_json: gmail_capabilities_json().unwrap_or_else(|_| "{}".to_string()),
        enabled_actions_json: "[\"read\",\"send\"]".to_string(),
        connector_version: "gmail.v1".to_string(),
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
    }
}
```

- [ ] **Step 4: Add command parsing and local OAuth config**

In `crates/loc-cli/src/commands.rs`, add imports:

```rust
use locality_gmail::{
    DEFAULT_GMAIL_OAUTH_BROKER_URL, DEFAULT_GMAIL_OAUTH_REDIRECT_URI, GMAIL_CONNECTOR_ID,
    HttpGmailOAuthBrokerClient,
};
```

Add `Gmail(ConnectGmailArgs)` and `Gmail(MountGmailArgs)` variants:

```rust
#[command(about = "Connect Gmail")]
Gmail(ConnectGmailArgs),

#[command(about = "Mount Gmail")]
Gmail(MountGmailArgs),
```

Add arg structs:

```rust
#[derive(Debug, Args)]
struct ConnectGmailArgs {
    #[arg(long, value_name = "ID", help = "Connection id to save. Defaults to gmail-default.")]
    name: Option<String>,
    #[arg(long, help = "Print the OAuth URL instead of opening a browser.")]
    no_browser: bool,
    #[arg(long, value_name = "URL", help = "OAuth broker base URL.")]
    broker_url: Option<String>,
    #[arg(long, value_name = "URI", help = "OAuth redirect URI for the local callback listener.")]
    redirect_uri: Option<String>,
}

#[derive(Debug, Args)]
struct MountGmailArgs {
    #[arg(value_name = "path", help = "Local directory where the Gmail mount should be registered.")]
    path: String,
    #[arg(long, value_name = "id", help = "Connection id to use for this mount.")]
    connection: Option<String>,
    #[arg(long, value_name = "id", help = "Mount id to save. Defaults to gmail-main.")]
    mount_id: Option<String>,
    #[arg(long, value_name = "mode", help = "Projection mode. Supported values depend on the host platform.")]
    projection: Option<String>,
    #[arg(long, help = "Register the mount as read-only and block push operations.")]
    read_only: bool,
}
```

Add `GmailOAuthBrokerCliConfig`, `gmail_oauth_broker_config`, and a Gmail local error helper mirroring Google Docs with Gmail constants.

Update `legacy_args_for_command` for `connect gmail` and `mount gmail`.

Update `connect(args, json)` dispatch to call:

```rust
let config = gmail_oauth_broker_config(args)?;
let authorization = run_gmail_broker_local_oauth(&config, has_flag(args, "--no-browser"), json)?;
run_connect_gmail_broker_oauth(
    &mut store,
    credentials.as_ref(),
    GmailBrokerOAuthConnectOptions {
        connection_id: flag_value(args, "--name").map(ConnectionId::new),
        broker_url: config.broker_url,
        client_id: authorization.client_id,
        session: authorization.session,
        state: authorization.state,
        code: authorization.code,
        redirect_uri: authorization.redirect_uri,
    },
    &HttpGmailOAuthBrokerClient::new(config.broker_url),
)
```

Use the existing Google Docs broker OAuth flow as the exact local model; keep the redirect path `/oauth/gmail/callback`.

Update `mount_usage()` to include:

```text
loc mount gmail <path> [--connection <id>] [--mount-id <id>] [--projection <mode>] [--read-only] [--json]
```

In `mount_remote_root_id`, add:

```rust
GMAIL_CONNECTOR_ID => {
    if has_flag(args, "--workspace") || flag_value(args, "--root-page").is_some() || flag_value(args, "--workspace-folder").is_some() {
        return Err(CommandError::new(
            "mount",
            "usage",
            "loc mount gmail does not accept Notion or Google Docs root flags",
        ));
    }
    Ok(None)
}
```

- [ ] **Step 5: Add help tests**

In `crates/loc-cli/src/commands.rs`, update `clap_help_is_available_for_commands_and_nested_subcommands` expected cases:

```rust
(
    vec!["connect", "--help"],
    vec!["Usage: loc connect", "Commands:", "notion", "google-docs", "gmail", "--json"],
),
(
    vec!["connect", "gmail", "--help"],
    vec!["Usage: loc connect gmail", "Connect Gmail", "--broker-url", "--redirect-uri"],
),
(
    vec!["mount", "--help"],
    vec!["Usage: loc mount", "Commands:", "notion", "google-docs", "gmail", "--json"],
),
(
    vec!["mount", "gmail", "--help"],
    vec!["Usage: loc mount gmail", "Mount Gmail", "--connection", "--projection"],
),
```

Add a legacy args case:

```rust
let cli = parse_cli([
    "connect",
    "gmail",
    "--name",
    "gmail-work",
    "--no-browser",
    "--broker-url",
    "https://auth.example.test",
]);
assert_eq!(
    legacy_args_for_command(cli.command.as_ref().expect("command")),
    vec![
        "connect",
        "gmail",
        "--name",
        "gmail-work",
        "--no-browser",
        "--broker-url",
        "https://auth.example.test",
    ]
);
```

- [ ] **Step 6: Run CLI tests**

Run:

```bash
cargo test -p loc-cli connect_gmail_broker_oauth_stores_refresh_handle_without_secrets
cargo test -p loc-cli clap_help_is_available_for_commands_and_nested_subcommands clap_parsed_commands_convert_to_legacy_args_for_execution
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/loc-cli/Cargo.toml crates/loc-cli/src/connect.rs crates/loc-cli/src/commands.rs crates/loc-cli/tests/connect.rs
git commit -m "feat: add Gmail CLI connect and mount"
```

## Task 9: Add OAuth Broker Gmail Endpoints

**Files:**
- Create: `apps/oauth-service/src/oauth/gmail.ts`
- Modify: `apps/oauth-service/src/app.ts`
- Modify: `apps/oauth-service/src/types.ts`
- Modify: `apps/oauth-service/test/app.test.ts`
- Modify: `apps/oauth-service/README.md`

- [ ] **Step 1: Write failing broker tests**

In `apps/oauth-service/test/app.test.ts`, add Gmail env values:

```ts
LOCALITY_GOOGLE_CLIENT_ID: "google-client-id",
LOCALITY_GOOGLE_CLIENT_SECRET: "google-client-secret",
LOCALITY_GMAIL_API_BASE_URL: "https://oauth2.example.test",
LOCALITY_GMAIL_AUTH_BASE_URL: "https://accounts.example.test",
LOCALITY_GMAIL_REDIRECT_URIS: "http://localhost:8757/oauth/gmail/callback"
```

Add tests:

```ts
it("starts Gmail OAuth sessions", async () => {
  const response = await app.request("/v1/oauth/gmail/start", { method: "POST" }, env);
  expect(response.status).toBe(200);
  const body = await response.json() as any;
  expect(body.connector).toBe("gmail");
  expect(body.client_id).toBe("google-client-id");
  expect(body.authorization_url).toContain("client_id=google-client-id");
  expect(body.authorization_url).toContain("gmail.readonly");
  expect(body.authorization_url).toContain("gmail.compose");
  expect(body.redirect_uri).toBe("http://localhost:8757/oauth/gmail/callback");
});

it("exchanges and refreshes Gmail OAuth tokens", async () => {
  const start = await app.request("/v1/oauth/gmail/start", { method: "POST" }, env);
  const started = await start.json() as any;
  const fetchMock = vi.spyOn(globalThis, "fetch");
  fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({
    access_token: "gmail-access-token",
    refresh_token: "gmail-refresh-token",
    expires_in: 3600,
    token_type: "Bearer",
    scope: "openid email profile https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.compose"
  }), { status: 200, headers: { "Content-Type": "application/json" } }));

  const exchange = await app.request("/v1/oauth/gmail/exchange", {
    method: "POST",
    body: JSON.stringify({
      session: started.session,
      state: started.state,
      code: "gmail-code",
      redirect_uri: started.redirect_uri
    }),
    headers: { "Content-Type": "application/json" }
  }, env);
  expect(exchange.status).toBe(200);
  const body = await exchange.json() as any;
  expect(body.access_token).toBe("gmail-access-token");
  expect(body.refresh_token_handle).toBeTruthy();

  fetchMock.mockRestore();
});
```

- [ ] **Step 2: Run failing broker tests**

Run:

```bash
cd apps/oauth-service && npm test -- --runInBand
```

Expected: FAIL because Gmail routes do not exist. If Vitest rejects `--runInBand`, run `npm test`.

- [ ] **Step 3: Add Gmail OAuth implementation**

Create `apps/oauth-service/src/oauth/gmail.ts`:

```ts
import { configError, upstreamError } from "../http/errors";
import type { BrokerEnv } from "../types";

export const GMAIL_OAUTH_SCOPES = [
  "openid",
  "email",
  "profile",
  "https://www.googleapis.com/auth/gmail.readonly",
  "https://www.googleapis.com/auth/gmail.compose"
];

export interface GmailTokenResponse {
  access_token: string;
  token_type?: string;
  refresh_token?: string;
  expires_in?: number;
  scope?: string;
  id_token?: string;
}

export function gmailAuthorizeUrl(env: BrokerEnv, redirectUri: string, state: string): string {
  const url = new URL(`${gmailAuthBaseUrl(env)}/o/oauth2/v2/auth`);
  url.searchParams.set("client_id", requireEnv(env.LOCALITY_GOOGLE_CLIENT_ID, "LOCALITY_GOOGLE_CLIENT_ID"));
  url.searchParams.set("response_type", "code");
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("scope", GMAIL_OAUTH_SCOPES.join(" "));
  url.searchParams.set("state", state);
  url.searchParams.set("access_type", "offline");
  url.searchParams.set("prompt", "consent");
  url.searchParams.set("include_granted_scopes", "true");
  return url.toString();
}

export async function exchangeGmailCode(
  env: BrokerEnv,
  code: string,
  redirectUri: string,
  fetcher: typeof fetch = fetch
): Promise<GmailTokenResponse> {
  return gmailTokenRequest(env, { grant_type: "authorization_code", code, redirect_uri: redirectUri }, fetcher);
}

export async function refreshGmailToken(
  env: BrokerEnv,
  refreshToken: string,
  fetcher: typeof fetch = fetch
): Promise<GmailTokenResponse> {
  return gmailTokenRequest(env, { grant_type: "refresh_token", refresh_token: refreshToken }, fetcher);
}

async function gmailTokenRequest(
  env: BrokerEnv,
  body: Record<string, string>,
  fetcher: typeof fetch
): Promise<GmailTokenResponse> {
  const clientId = requireEnv(env.LOCALITY_GOOGLE_CLIENT_ID, "LOCALITY_GOOGLE_CLIENT_ID");
  const clientSecret = requireEnv(env.LOCALITY_GOOGLE_CLIENT_SECRET, "LOCALITY_GOOGLE_CLIENT_SECRET");
  const params = new URLSearchParams({ client_id: clientId, client_secret: clientSecret });
  for (const [key, value] of Object.entries(body)) {
    params.set(key, value);
  }
  const response = await fetcher(`${gmailApiBaseUrl(env)}/token`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: params.toString()
  });
  if (!response.ok) {
    throw upstreamError(`Gmail OAuth returned HTTP ${response.status}`);
  }
  return response.json() as Promise<GmailTokenResponse>;
}

function gmailAuthBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_GMAIL_AUTH_BASE_URL ?? "https://accounts.google.com").replace(/\/+$/, "");
}

function gmailApiBaseUrl(env: BrokerEnv): string {
  return (env.LOCALITY_GMAIL_API_BASE_URL ?? "https://oauth2.googleapis.com").replace(/\/+$/, "");
}

function requireEnv(value: string | undefined, name: string): string {
  if (!value) {
    throw configError(`${name} is required`);
  }
  return value;
}
```

- [ ] **Step 4: Wire routes in app and types**

In `apps/oauth-service/src/types.ts`, add:

```ts
LOCALITY_GOOGLE_CLIENT_ID?: string;
LOCALITY_GOOGLE_CLIENT_SECRET?: string;
LOCALITY_GMAIL_API_BASE_URL?: string;
LOCALITY_GMAIL_AUTH_BASE_URL?: string;
LOCALITY_GMAIL_REDIRECT_URIS?: string;
```

In `apps/oauth-service/src/app.ts`, import Gmail functions:

```ts
import {
  exchangeGmailCode,
  gmailAuthorizeUrl,
  refreshGmailToken,
  type GmailTokenResponse
} from "./oauth/gmail";
```

Add Gmail route blocks mirroring Google Docs:

```ts
app.post("/v1/oauth/gmail/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const redirectUri = validateGmailRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/gmail/callback"
  );
  const now = nowSeconds();
  const state = randomBase64Url();
  const session = await signSession(
    { v: 1, connector: "gmail", state, redirect_uri: redirectUri, iat: now, exp: now + SESSION_TTL_SECONDS, nonce: randomBase64Url() },
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  return c.json({
    connector: "gmail",
    client_id: c.env.LOCALITY_GOOGLE_CLIENT_ID,
    authorization_url: gmailAuthorizeUrl(c.env, redirectUri, state),
    redirect_uri: redirectUri,
    session,
    state,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/gmail/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const redirectUri = validateGmailRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== "gmail" || payload.state !== state || payload.redirect_uri !== redirectUri) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeGmailCode(c.env, code, redirectUri);
  return c.json(await shapeGmailTokenResponse(c.env, token));
});

app.post("/v1/oauth/gmail/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "gmail", body);
  const token = await refreshGmailToken(c.env, refreshToken);
  return c.json(await shapeGmailTokenResponse(c.env, token));
});
```

Add:

```ts
async function shapeGmailTokenResponse(env: BrokerEnv, token: GmailTokenResponse) {
  const refresh = await shapeRefreshToken(env, "gmail", token.refresh_token);
  return {
    connector: "gmail",
    access_token: token.access_token,
    token_type: token.token_type,
    expires_in: token.expires_in,
    refresh_token_handle: refresh.refresh_token_handle,
    account_id: undefined,
    account_label: undefined,
    workspace_id: "gmail",
    workspace_name: "Gmail",
    scopes: token.scope?.split(/\s+/).filter(Boolean) ?? []
  };
}
```

In `apps/oauth-service/src/security/redirects.ts`, add `validateGmailRedirectUri` mirroring Google Docs and reading `LOCALITY_GMAIL_REDIRECT_URIS`.

- [ ] **Step 5: Update broker README**

In `apps/oauth-service/README.md`, add Gmail sections for:

```text
POST /v1/oauth/gmail/start
POST /v1/oauth/gmail/exchange
POST /v1/oauth/gmail/refresh
LOCALITY_GOOGLE_CLIENT_ID
LOCALITY_GOOGLE_CLIENT_SECRET
LOCALITY_GMAIL_REDIRECT_URIS
```

- [ ] **Step 6: Run broker checks**

Run:

```bash
cd apps/oauth-service && npm run check
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add apps/oauth-service/src/oauth/gmail.ts apps/oauth-service/src/app.ts apps/oauth-service/src/types.ts apps/oauth-service/src/security/redirects.ts apps/oauth-service/test/app.test.ts apps/oauth-service/README.md
git commit -m "feat: add Gmail OAuth broker endpoints"
```

## Task 10: Add Gmail Docs And Public Connector Page

**Files:**
- Create: `docs/gmail-connector.md`
- Modify: `docs/connector-sdk.md`
- Modify: `docs/enumeration-and-hydration.md`
- Create: `docs-site/connectors/gmail.mdx`
- Modify: `docs-site/docs.json`

- [ ] **Step 1: Write connector docs**

Create `docs/gmail-connector.md`:

```markdown
# Gmail Connector Summary

Gmail is registered as a first-party Locality source connector named `gmail`.
It projects a mailbox as three local folders:

```text
gmail-main/
  inbox/
  sent/
  draft/
```

`inbox/` and `sent/` are read-only. Pull and lazy folder listing fetch the most
recent 100 messages for each folder in V1. Local edits, renames, and deletes in
those folders are blocked by virtual filesystem policy and by push validation.

`draft/` is the only writable folder. V1 treats it as a local compose surface:
create a Markdown file directly under `draft/`, add recipients and subject in
frontmatter, write the body as Markdown/plain text, and run `loc push` to send
the email. Locality creates a Gmail draft and immediately sends it as part of
push. Existing remote Gmail drafts are not pulled in V1.

## Draft Format

```markdown
---
loc:
  type: page
  connector: gmail
title: "Reply"
gmail:
  mailbox: "draft"
to: ["ann@example.com"]
cc: []
bcc: []
subject: "Reply"
---
Thanks, this looks good.
```

Attachments, remote draft editing, mailbox-wide sync, labels beyond `INBOX` and
`SENT`, and thread views are outside the V1 behavior.
```

- [ ] **Step 2: Update connector SDK docs**

In `docs/connector-sdk.md`, add Gmail to the first-party connector section:

```markdown
## Gmail connector

`locality-gmail` is a first-party connector for email-shaped data. It uses the
same connector boundary as document connectors but maps Gmail labels to source
directories. `inbox/` and `sent/` are read-only message projections; `draft/`
accepts local Markdown creates whose approved `CreateEntity` push operation is
lowered to Gmail draft creation plus draft send.

Gmail also exercises `ChildContainer::DirectoryChildren`, which exists for
source folders that are neither pages nor databases.
```

- [ ] **Step 3: Update enumeration docs**

In `docs/enumeration-and-hydration.md`, add a note under child enumeration:

```markdown
Gmail uses `ChildContainer::DirectoryChildren` for the `inbox/`, `sent/`, and
`draft/` folders. The Gmail connector returns metadata for the latest 100
messages in `inbox/` and `sent/`, and returns no remote children for `draft/`
because V1 draft files are local compose items until push sends them.
```

- [ ] **Step 4: Add docs-site page**

Create `docs-site/connectors/gmail.mdx`:

```mdx
---
title: Gmail
description: Mount Gmail as read-only inbox and sent mail plus a writable local draft folder.
---

Gmail mounts expose three folders: `inbox`, `sent`, and `draft`.

`inbox` and `sent` are read-only and show the most recent 100 messages in the
current connector version. Open a message to hydrate its Markdown body.

Create new Markdown files directly under `draft` to compose mail. `loc push`
sends those draft files through Gmail. Attachments and editing existing Gmail
drafts are not part of the current Gmail connector.

```bash
loc connect gmail
loc mount gmail ~/Locality/gmail-main --projection linux-fuse
loc pull ~/Locality/gmail-main
```
```

Update `docs-site/docs.json` to include `connectors/gmail` beside Notion and Google Docs.

- [ ] **Step 5: Run docs-adjacent checks**

Run:

```bash
rg -n "Gmail|gmail" docs docs-site
```

Expected: output includes the new Gmail docs and no misspelling of `draft/`, `inbox/`, or `sent/`.

- [ ] **Step 6: Commit**

```bash
git add docs/gmail-connector.md docs/connector-sdk.md docs/enumeration-and-hydration.md docs-site/connectors/gmail.mdx docs-site/docs.json
git commit -m "docs: document Gmail connector"
```

## Task 11: End-To-End Local Verification

**Files:**
- No new source files unless tests reveal integration misses.

- [ ] **Step 1: Run Rust workspace tests for touched crates**

Run:

```bash
cargo test -p locality-connector
cargo test -p locality-gmail
cargo test -p localityd
cargo test -p loc-cli
cargo test -p locality-fuse
```

Expected: all PASS.

- [ ] **Step 2: Run OAuth broker checks**

Run:

```bash
cd apps/oauth-service && npm run check
```

Expected: PASS.

- [ ] **Step 3: Run formatting**

Run:

```bash
cargo fmt --all --check
```

Expected: PASS. If it fails, run `cargo fmt --all`, review the diff, then rerun `cargo fmt --all --check`.

- [ ] **Step 4: Run workspace check**

Run:

```bash
cargo check --workspace
```

Expected: PASS.

- [ ] **Step 5: Manual local smoke without live Gmail**

Run:

```bash
cargo test -p locality-gmail enumerate_projects_three_folders_and_recent_inbox_sent_messages apply_create_entity_creates_and_sends_gmail_draft
cargo test -p localityd gmail_inbox_message_rejects_virtual_write_without_dirtying_entity gmail_draft_folder_accepts_virtual_create
```

Expected: PASS.

- [ ] **Step 6: Commit final integration fixes**

If verification required fixes, commit them:

```bash
git add .
git commit -m "test: verify Gmail connector integration"
```

If there are no fixes, do not create an empty commit.

## Task 12: Optional Live Gmail Verification

**Files:**
- No source changes expected.

- [ ] **Step 1: Connect a scratch Gmail account**

Run with a Gmail-capable OAuth broker:

```bash
cargo run -p loc-cli -- connect gmail --name gmail-scratch --no-browser
```

Expected: command prints an authorization URL. Complete OAuth in the browser and confirm JSON or text output reports connector `gmail`.

- [ ] **Step 2: Mount Gmail**

Run:

```bash
mkdir -p /tmp/locality-gmail-main
cargo run -p loc-cli -- mount gmail /tmp/locality-gmail-main --connection gmail-scratch --mount-id gmail-main --projection plain-files
```

Expected: mount report has `connector: gmail`, `mount_id: gmail-main`, and no remote root id requirement.

- [ ] **Step 3: Pull bounded metadata**

Run:

```bash
cargo run -p loc-cli -- pull /tmp/locality-gmail-main --json
```

Expected: local tree contains `inbox/`, `sent/`, and `draft/`, with at most 100 message files under each read-only folder.

- [ ] **Step 4: Send a scratch draft**

Create `/tmp/locality-gmail-main/draft/locality-smoke.md`:

```markdown
---
loc:
  type: page
  connector: gmail
title: "Locality Gmail smoke"
subject: "Locality Gmail smoke"
to: ["YOUR_SCRATCH_RECIPIENT@example.com"]
cc: []
bcc: []
---
This is a Locality Gmail connector smoke test.
```

Run:

```bash
cargo run -p loc-cli -- diff /tmp/locality-gmail-main/draft/locality-smoke.md
cargo run -p loc-cli -- push /tmp/locality-gmail-main/draft/locality-smoke.md -y
```

Expected: `diff` shows one entity create/send plan, and `push` succeeds. Verify the recipient received the message or the sender account shows it in Sent.

- [ ] **Step 5: Clean up scratch artifacts**

Delete the local smoke mount directory and disconnect the scratch connection if it is no longer needed:

```bash
cargo run -p loc-cli -- disconnect gmail-scratch
rm -rf /tmp/locality-gmail-main
```

Expected: disconnect report marks the Gmail connection revoked or removed according to existing CLI behavior.

## Self-Review

- Spec coverage: folder shape, recent 100 pull limit, read-only inbox/sent, writable local draft folder, push-as-send, no attachments, and single OAuth broker profile are covered by Tasks 2 through 10.
- Placeholder scan: no flagged placeholder strings remain. Each implementation step names exact files and concrete code.
- Type consistency: connector id is consistently `gmail`; folder ids are `gmail-folder:inbox`, `gmail-folder:sent`, and `gmail-folder:draft`; folder paths are `inbox`, `sent`, and `draft`; the only supported Gmail push operation is `CreateEntity`.
