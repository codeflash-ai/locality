# Google Docs Connector Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first working Google Docs connector slice: shared broker OAuth DTOs, a `locality-google-docs` crate with brokered credential handling, `loc connect google-docs`, and source-registry descriptor support.

**Architecture:** Keep remote read/write behavior unsupported in this first slice, but wire the product-auth path and connector registration exactly as the full connector will use them. Shared broker DTOs live in `locality-connector`; Google-specific credential persistence and broker HTTP paths live in `locality-google-docs`; CLI code persists connection/profile records through the existing store.

**Tech Stack:** Rust 2024, workspace crates, `reqwest` blocking JSON transport, `serde`, `serde_json`, existing `local_oauth` localhost callback helper, existing SQLite/keychain-backed connection storage.

---

### Task 1: Add Shared OAuth Broker DTOs

**Files:**
- Modify: `crates/locality-connector/src/lib.rs`
- Create: `crates/locality-connector/src/oauth_broker.rs`
- Test: `crates/locality-connector/src/oauth_broker.rs`

- [ ] **Step 1: Write shared DTO tests**

Add this test module in the new file:

```rust
#[cfg(test)]
mod tests {
    use super::{OAuthBrokerStart, OAuthBrokerToken};

    #[test]
    fn start_request_carries_connector_and_redirect_uri() {
        let request = OAuthBrokerStart {
            connector: "google-docs".to_string(),
            redirect_uri: "http://localhost:8757/oauth/google-docs/callback".to_string(),
        };

        let json = serde_json::to_value(&request).expect("serialize request");

        assert_eq!(json["connector"], "google-docs");
        assert_eq!(
            json["redirect_uri"],
            "http://localhost:8757/oauth/google-docs/callback"
        );
    }

    #[test]
    fn token_payload_can_carry_refresh_handle_and_scopes_without_refresh_token() {
        let payload = serde_json::json!({
            "access_token": "access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token_handle": "handle-1",
            "account_id": "acct-1",
            "account_label": "user@example.com",
            "workspace_id": "google-drive",
            "workspace_name": "Google Drive",
            "scopes": ["openid", "https://www.googleapis.com/auth/documents"]
        });

        let token: OAuthBrokerToken = serde_json::from_value(payload).expect("decode token");

        assert_eq!(token.access_token, "access");
        assert_eq!(token.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(token.account_label.as_deref(), Some("user@example.com"));
        assert_eq!(token.scopes.len(), 2);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p locality-connector oauth_broker --lib`

Expected: fail because `oauth_broker.rs` and the DTOs do not exist.

- [ ] **Step 3: Implement shared DTOs**

Create `crates/locality-connector/src/oauth_broker.rs` with these public types:

```rust
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerStart {
    pub connector: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerStartResponse {
    pub connector: String,
    pub client_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub session: String,
    pub state: String,
    pub expires_in: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerCodeExchange {
    pub connector: String,
    pub session: String,
    pub state: String,
    pub code: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerRefresh {
    pub connector: String,
    pub refresh_token_handle: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerToken {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<u64>,
    pub refresh_token_handle: Option<String>,
    pub account_id: Option<String>,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}
```

Expose the module from `crates/locality-connector/src/lib.rs`:

```rust
pub mod oauth_broker;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p locality-connector oauth_broker --lib`

Expected: pass.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/locality-connector/src/lib.rs crates/locality-connector/src/oauth_broker.rs
git commit -m "Add shared OAuth broker DTOs"
```

### Task 2: Add `locality-google-docs` Crate Skeleton

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/loc-cli/Cargo.toml`
- Modify: `crates/localityd/Cargo.toml`
- Create: `crates/locality-google-docs/Cargo.toml`
- Create: `crates/locality-google-docs/src/lib.rs`
- Create: `crates/locality-google-docs/src/oauth.rs`
- Test: `crates/locality-google-docs/src/oauth.rs`

- [ ] **Step 1: Write Google credential tests**

Create `crates/locality-google-docs/src/oauth.rs` with tests for broker credential persistence:

```rust
#[cfg(test)]
mod tests {
    use locality_connector::oauth_broker::OAuthBrokerToken;

    use super::{StoredGoogleDocsCredential, GOOGLE_DOCS_CONNECTOR_ID};

    #[test]
    fn broker_credential_stores_refresh_handle_without_refresh_token_or_secret() {
        let stored = StoredGoogleDocsCredential::from_broker_token(
            OAuthBrokerToken {
                access_token: "access-token".to_string(),
                token_type: Some("Bearer".to_string()),
                expires_in: Some(3600),
                refresh_token_handle: Some("handle-1".to_string()),
                account_id: Some("acct-1".to_string()),
                account_label: Some("user@example.com".to_string()),
                workspace_id: Some("google-drive".to_string()),
                workspace_name: Some("Google Drive".to_string()),
                scopes: vec!["openid".to_string()],
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        assert_eq!(stored.kind, "oauth");
        assert_eq!(stored.connector, GOOGLE_DOCS_CONNECTOR_ID);
        assert_eq!(stored.oauth_client_id.as_deref(), Some("client-id"));
        assert_eq!(
            stored.oauth_broker_url.as_deref(),
            Some("https://auth.example.test")
        );
        assert_eq!(stored.refresh_token_handle.as_deref(), Some("handle-1"));
        assert_eq!(stored.expires_at, Some(3700));

        let json = serde_json::to_string(&stored).expect("serialize stored credential");
        assert!(!json.contains("refresh_token"));
        assert!(!json.contains("client_secret"));
    }

    #[test]
    fn refreshed_broker_credential_rotates_access_token_and_handle() {
        let stored = StoredGoogleDocsCredential::from_broker_token(
            OAuthBrokerToken {
                access_token: "access-token".to_string(),
                token_type: Some("Bearer".to_string()),
                expires_in: Some(3600),
                refresh_token_handle: Some("handle-1".to_string()),
                account_id: Some("acct-1".to_string()),
                account_label: Some("user@example.com".to_string()),
                workspace_id: Some("google-drive".to_string()),
                workspace_name: Some("Google Drive".to_string()),
                scopes: vec!["openid".to_string()],
            },
            "client-id".to_string(),
            "https://auth.example.test".to_string(),
            100,
        );

        let refreshed = stored.refreshed(
            OAuthBrokerToken {
                access_token: "new-access-token".to_string(),
                token_type: Some("Bearer".to_string()),
                expires_in: Some(7200),
                refresh_token_handle: Some("handle-2".to_string()),
                account_id: None,
                account_label: None,
                workspace_id: None,
                workspace_name: None,
                scopes: vec![],
            },
            200,
        );

        assert_eq!(refreshed.access_token, "new-access-token");
        assert_eq!(refreshed.refresh_token_handle.as_deref(), Some("handle-2"));
        assert_eq!(refreshed.account_label.as_deref(), Some("user@example.com"));
        assert_eq!(refreshed.scopes, vec!["openid".to_string()]);
        assert_eq!(refreshed.expires_at, Some(7400));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p locality-google-docs oauth --lib`

Expected: fail because the crate is not in the workspace.

- [ ] **Step 3: Implement crate skeleton**

Add the workspace member and dependency:

```toml
"crates/locality-google-docs",
locality-google-docs = { path = "crates/locality-google-docs" }
```

Create `crates/locality-google-docs/Cargo.toml`:

```toml
[package]
name = "locality-google-docs"
version = "0.1.3"
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true

[lib]
name = "locality_google_docs"
path = "src/lib.rs"

[dependencies]
locality-core.workspace = true
locality-connector.workspace = true
reqwest = { version = "0.13", default-features = false, features = ["blocking", "json", "rustls-no-provider"] }
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```

Create `src/lib.rs`:

```rust
pub mod oauth;

pub use oauth::{
    DEFAULT_GOOGLE_DOCS_OAUTH_BROKER_URL, DEFAULT_GOOGLE_DOCS_OAUTH_REDIRECT_URI,
    GOOGLE_DOCS_CONNECTOR_ID, GOOGLE_DOCS_OAUTH_SCOPES, StoredGoogleDocsCredential,
    google_docs_capabilities_json,
};
```

Implement the tested `StoredGoogleDocsCredential`, constants, `google_docs_capabilities_json`,
and `HttpGoogleDocsOAuthBrokerClient` in `oauth.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p locality-google-docs oauth --lib`

Expected: pass.

- [ ] **Step 5: Commit**

Run:

```bash
git add Cargo.toml crates/locality-google-docs crates/loc-cli/Cargo.toml crates/localityd/Cargo.toml
git commit -m "Add Google Docs connector crate"
```

### Task 3: Add `run_connect_google_docs_broker_oauth`

**Files:**
- Modify: `crates/loc-cli/src/connect.rs`
- Modify: `crates/loc-cli/tests/connect.rs`

- [ ] **Step 1: Write connection persistence test**

Add a test to `crates/loc-cli/tests/connect.rs`:

```rust
#[test]
fn connect_google_docs_broker_oauth_stores_refresh_handle_without_secrets() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();
    let exchange = FakeGoogleDocsBrokerOAuthExchange;

    let report = run_connect_google_docs_broker_oauth(
        &mut store,
        &credentials,
        GoogleDocsBrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("docs-work")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/google-docs/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect google docs oauth");

    assert_eq!(report.connection_id, "docs-work");
    assert_eq!(report.profile_id, DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID);
    assert_eq!(report.connector, "google-docs");
    assert_eq!(report.auth_kind, "oauth");
    assert_eq!(report.account_label.as_deref(), Some("user@example.com"));

    let secret = credentials.get("connection:docs-work").expect("credential saved");
    let stored =
        serde_json::from_str::<StoredGoogleDocsCredential>(&secret).expect("stored oauth");
    assert_eq!(stored.refresh_token_handle.as_deref(), Some("opaque-refresh-handle"));
    assert_eq!(stored.oauth_broker_url.as_deref(), Some("https://auth.example.test"));

    let json = serde_json::to_string(&report).expect("json");
    assert!(!json.contains("oauth-access-token"));
    assert!(!json.contains("opaque-refresh-handle"));
    assert!(!json.contains("client-secret"));
    assert!(!json.contains("secret_ref"));
}
```

Add fake exchange:

```rust
#[derive(Clone, Debug)]
struct FakeGoogleDocsBrokerOAuthExchange;

impl GoogleDocsOAuthBrokerExchange for FakeGoogleDocsBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.connector, "google-docs");
        assert_eq!(request.session, "broker-session");
        assert_eq!(request.state, "state-1");
        assert_eq!(request.code, "oauth-code");
        Ok(OAuthBrokerToken {
            access_token: "oauth-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("opaque-refresh-handle".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("google-drive".to_string()),
            workspace_name: Some("Google Drive".to_string()),
            scopes: GOOGLE_DOCS_OAUTH_SCOPES.iter().map(|scope| scope.to_string()).collect(),
        })
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p loc-cli --test connect connect_google_docs_broker_oauth_stores_refresh_handle_without_secrets`

Expected: fail because the public API does not exist.

- [ ] **Step 3: Implement connect function and profile**

In `connect.rs`, add:

```rust
pub const DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID: &str = "google-docs-oauth-default";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleDocsBrokerOAuthConnectOptions {
    pub connection_id: Option<ConnectionId>,
    pub broker_url: String,
    pub client_id: String,
    pub session: String,
    pub state: String,
    pub code: String,
    pub redirect_uri: String,
}

pub trait GoogleDocsOAuthBrokerExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, ConnectError>;
}

pub fn run_connect_google_docs_broker_oauth<S, E>(
    store: &mut S,
    credentials: &dyn CredentialStore,
    options: GoogleDocsBrokerOAuthConnectOptions,
    exchange: &E,
) -> Result<ConnectReport, ConnectError>
where
    S: ConnectionRepository + ConnectorProfileRepository,
    E: GoogleDocsOAuthBrokerExchange,
```

The implementation should mirror `run_connect_notion_broker_oauth`, but use:

- connector `google-docs`
- profile `google-docs-oauth-default`
- scopes from the broker token
- `StoredGoogleDocsCredential::from_broker_token`
- `google_docs_capabilities_json`
- default connection id `google-docs-default` when no Google Docs connection exists

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p loc-cli --test connect connect_google_docs_broker_oauth_stores_refresh_handle_without_secrets`

Expected: pass.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/loc-cli/src/connect.rs crates/loc-cli/tests/connect.rs
git commit -m "Add Google Docs broker connect persistence"
```

### Task 4: Wire `loc connect google-docs`

**Files:**
- Modify: `crates/loc-cli/src/commands.rs`

- [ ] **Step 1: Write CLI parsing tests**

Add tests in the existing `commands.rs` test module:

```rust
#[test]
fn help_lists_google_docs_connect_command() {
    let output = command_help(&["connect"]);

    assert!(output.contains("google-docs"));
}

#[test]
fn google_docs_oauth_broker_config_accepts_explicit_broker_url() {
    let args = vec![
        "google-docs".to_string(),
        "--broker-url".to_string(),
        "https://auth.example.test".to_string(),
        "--redirect-uri".to_string(),
        "http://localhost:8757/oauth/google-docs/callback".to_string(),
    ];

    let config = google_docs_oauth_broker_config(&args).expect("broker config");

    assert_eq!(config.broker_url, "https://auth.example.test");
    assert_eq!(
        config.redirect_uri,
        "http://localhost:8757/oauth/google-docs/callback"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p loc-cli commands::help_lists_google_docs_connect_command commands::google_docs_oauth_broker_config_accepts_explicit_broker_url`

Expected: fail because `google-docs` is not a CLI subcommand.

- [ ] **Step 3: Implement command routing**

Update CLI command enum:

```rust
#[derive(Debug, Subcommand)]
enum ConnectCommand {
    #[command(about = "Connect a Notion workspace")]
    Notion(ConnectNotionArgs),
    #[command(name = "google-docs", about = "Connect Google Docs")]
    GoogleDocs(ConnectGoogleDocsArgs),
}
```

Add args:

```rust
#[derive(Debug, Args)]
struct ConnectGoogleDocsArgs {
    #[arg(long, value_name = "ID", help = "Connection id to save. Defaults to google-docs-default.")]
    name: Option<String>,
    #[arg(long, help = "Print the OAuth URL instead of opening a browser.")]
    no_browser: bool,
    #[arg(long, value_name = "URL", help = "OAuth broker base URL.")]
    broker_url: Option<String>,
    #[arg(long, value_name = "URI", help = "OAuth redirect URI for the local callback listener.")]
    redirect_uri: Option<String>,
}
```

Update legacy `connect(args, json)` routing so first positional `google-docs`
starts the Google Docs broker flow. Use `HttpGoogleDocsOAuthBrokerClient`,
`OAuthBrokerStart`, `run_local_oauth_authorization("Google Docs", &start.authorization_url, &start.redirect_uri, &start.state, has_flag(args, "--no-browser"), json)`, and
`run_connect_google_docs_broker_oauth`.

- [ ] **Step 4: Run focused CLI tests**

Run: `cargo test -p loc-cli commands::help_lists_google_docs_connect_command commands::google_docs_oauth_broker_config_accepts_explicit_broker_url`

Expected: pass.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/loc-cli/src/commands.rs
git commit -m "Wire Google Docs connect command"
```

### Task 5: Register Google Docs Source Descriptor

**Files:**
- Modify: `crates/localityd/src/source.rs`
- Modify: `crates/localityd/tests/source_descriptor.rs`

- [ ] **Step 1: Write descriptor tests**

Add tests:

```rust
#[test]
fn google_docs_descriptor_comes_from_registry() {
    let descriptor = source_descriptor("google-docs");

    assert_eq!(descriptor.id(), "google-docs");
    assert_eq!(descriptor.display_name(), "Google Docs");
    assert_eq!(descriptor.default_mount_id(), "google-docs-main");
    assert_eq!(descriptor.connect_command(), Some("loc connect google-docs"));
    assert!(descriptor.supports_oauth());
}

#[test]
fn supported_sources_include_google_docs() {
    assert!(supported_source_connectors().contains(&"google-docs"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p localityd --test source_descriptor google_docs`

Expected: fail because the descriptor is not registered.

- [ ] **Step 3: Implement descriptor-only registration**

Add Google Docs registration with a resolver that returns
`ConnectorResolveError::UnsupportedConnector("google-docs".to_string())` until
the full connector implementation lands. The descriptor should have:

```rust
id: "google-docs"
display_name: "Google Docs"
default_mount_id: "google-docs-main"
connect_command: "loc connect google-docs"
auth_env_var: None
supports_oauth: true
mount_guidance: generic_mount_guidance("Google Docs")
```

Do not expose remote I/O as supported yet.

- [ ] **Step 4: Run descriptor tests**

Run: `cargo test -p localityd --test source_descriptor google_docs`

Expected: pass.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/localityd/src/source.rs crates/localityd/tests/source_descriptor.rs
git commit -m "Register Google Docs source descriptor"
```

### Task 6: Verify Foundation Slice

**Files:**
- All files changed in Tasks 1-5.

- [ ] **Step 1: Run focused tests**

Run:

```bash
cargo test -p locality-connector oauth_broker --lib
cargo test -p locality-google-docs oauth --lib
cargo test -p loc-cli --test connect google_docs
cargo test -p localityd --test source_descriptor google_docs
```

Expected: all pass.

- [ ] **Step 2: Run workspace compile**

Run: `cargo check --workspace --all-targets`

Expected: pass. Pre-existing warnings may remain, but no new Google Docs warnings should appear.

- [ ] **Step 3: Record follow-up scope**

Append a short "Next implementation slices" section to this plan noting that Drive mount/enumeration, Docs render/fetch, and push/apply are not part of this foundation slice.

- [ ] **Step 4: Commit plan tracking update if changed**

Run:

```bash
git add docs/superpowers/plans/2026-06-25-google-docs-connector-foundation.md
git commit -m "Track Google Docs foundation plan progress"
```

## Next implementation slices

This foundation slice intentionally stops at broker-backed connection setup,
credential persistence, and descriptor registration. It does not implement:

- `loc mount google-docs` command parsing or mount configuration.
- Drive API tree enumeration and folder/document projection.
- Google Docs `documents.get` fetch and canonical Markdown rendering.
- Markdown parse and Google Docs `documents.batchUpdate` write planning.
- Drive file create, rename, move, trash/delete, or post-push reconciliation.

Those should be implemented as separate plan slices so each one has focused
tests and can keep Notion behavior unchanged.
