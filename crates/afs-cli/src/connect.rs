use std::time::{SystemTime, UNIX_EPOCH};

use afs_notion::NotionConfig;
use afs_notion::client::{DEFAULT_NOTION_TOKEN_ENV, HttpNotionApi, NotionApi};
use afs_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, CredentialError, CredentialStore,
    StoreError,
};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectOptions {
    pub connection_id: Option<ConnectionId>,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectReport {
    pub ok: bool,
    pub command: &'static str,
    pub connection_id: String,
    pub connector: String,
    pub display_name: String,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub auth_kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectionsReport {
    pub ok: bool,
    pub command: &'static str,
    pub connections: Vec<ConnectionSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectionShowReport {
    pub ok: bool,
    pub command: &'static str,
    pub connection: ConnectionSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DisconnectReport {
    pub ok: bool,
    pub command: &'static str,
    pub connection_id: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectionSummary {
    pub connection_id: String,
    pub connector: String,
    pub display_name: String,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub auth_kind: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionConnectionProbeResult {
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
}

pub trait NotionConnectionProbe {
    fn probe(&self, token: &str) -> Result<NotionConnectionProbeResult, ConnectError>;
}

#[derive(Clone, Debug, Default)]
pub struct HttpNotionConnectionProbe;

impl NotionConnectionProbe for HttpNotionConnectionProbe {
    fn probe(&self, token: &str) -> Result<NotionConnectionProbeResult, ConnectError> {
        let api = HttpNotionApi::new(NotionConfig {
            workspace_id: None,
            root_page_id: None,
            token: Some(token.to_string()),
            token_key: DEFAULT_NOTION_TOKEN_ENV.to_string(),
        });
        let user = api
            .retrieve_current_user()
            .map_err(|error| ConnectError::ConnectionProbeFailed(error.to_string()))?;
        Ok(NotionConnectionProbeResult {
            account_label: user_label(&user),
            workspace_id: string_field(&user, "/bot/workspace_id"),
            workspace_name: string_field(&user, "/bot/workspace_name"),
        })
    }
}

pub fn run_connect_notion<S, P>(
    store: &mut S,
    credentials: &dyn CredentialStore,
    options: ConnectOptions,
    probe: &P,
) -> Result<ConnectReport, ConnectError>
where
    S: ConnectionRepository,
    P: NotionConnectionProbe,
{
    let connection_id = match options.connection_id {
        Some(connection_id) => connection_id,
        None => default_connection_id(store)?,
    };
    let probe_result = probe.probe(&options.token)?;
    let secret_ref = format!("connection:{}", connection_id.0);
    credentials
        .put(&secret_ref, &options.token)
        .map_err(ConnectError::Credential)?;

    let now = timestamp();
    let display_name = connection_id.0.clone();
    let connection = ConnectionRecord {
        connection_id: connection_id.clone(),
        connector: "notion".to_string(),
        display_name: display_name.clone(),
        account_label: probe_result.account_label.clone(),
        workspace_id: probe_result.workspace_id.clone(),
        workspace_name: probe_result.workspace_name.clone(),
        auth_kind: "token".to_string(),
        secret_ref,
        scopes: vec![],
        capabilities_json: "{}".to_string(),
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
        expires_at: None,
    };
    store
        .save_connection(connection)
        .map_err(ConnectError::Store)?;

    Ok(ConnectReport {
        ok: true,
        command: "connect",
        connection_id: connection_id.0,
        connector: "notion".to_string(),
        display_name,
        account_label: probe_result.account_label,
        workspace_id: probe_result.workspace_id,
        workspace_name: probe_result.workspace_name,
        auth_kind: "token".to_string(),
    })
}

pub fn run_connections<S>(store: &S) -> Result<ConnectionsReport, ConnectError>
where
    S: ConnectionRepository,
{
    let mut connections = store
        .list_connections()
        .map_err(ConnectError::Store)?
        .into_iter()
        .map(ConnectionSummary::from)
        .collect::<Vec<_>>();
    connections.sort_by(|left, right| left.connection_id.cmp(&right.connection_id));
    Ok(ConnectionsReport {
        ok: true,
        command: "connections",
        connections,
    })
}

pub fn run_connection_show<S>(
    store: &S,
    connection_id: ConnectionId,
) -> Result<ConnectionShowReport, ConnectError>
where
    S: ConnectionRepository,
{
    let connection = store
        .get_connection(&connection_id)
        .map_err(ConnectError::Store)?
        .ok_or_else(|| ConnectError::ConnectionMissing(connection_id.0.clone()))?;
    Ok(ConnectionShowReport {
        ok: true,
        command: "connection show",
        connection: ConnectionSummary::from(connection),
    })
}

pub fn run_disconnect<S>(
    store: &mut S,
    credentials: &dyn CredentialStore,
    connection_id: ConnectionId,
) -> Result<DisconnectReport, ConnectError>
where
    S: ConnectionRepository,
{
    let mut connection = store
        .get_connection(&connection_id)
        .map_err(ConnectError::Store)?
        .ok_or_else(|| ConnectError::ConnectionMissing(connection_id.0.clone()))?;
    credentials
        .delete(&connection.secret_ref)
        .map_err(ConnectError::Credential)?;
    connection.status = "revoked".to_string();
    connection.updated_at = timestamp();
    store
        .save_connection(connection)
        .map_err(ConnectError::Store)?;
    Ok(DisconnectReport {
        ok: true,
        command: "disconnect",
        connection_id: connection_id.0,
        status: "revoked".to_string(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    ConnectionMissing(String),
    ConnectionNameRequired,
    ConnectionProbeFailed(String),
    Credential(CredentialError),
    Store(StoreError),
}

impl ConnectError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConnectionMissing(_) => "missing_connection",
            Self::ConnectionNameRequired => "usage",
            Self::ConnectionProbeFailed(_) => "connection_probe_failed",
            Self::Credential(error) => error.code(),
            Self::Store(_) => "store_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::ConnectionMissing(connection_id) => {
                format!("connection `{connection_id}` was not found")
            }
            Self::ConnectionNameRequired => {
                "multiple Notion connections exist; pass --name <id>".to_string()
            }
            Self::ConnectionProbeFailed(message) => {
                format!("Notion connection probe failed: {message}")
            }
            Self::Credential(error) => error.to_string(),
            Self::Store(error) => error.to_string(),
        }
    }

    pub fn suggested_command(&self) -> Option<&'static str> {
        match self {
            Self::ConnectionMissing(_) | Self::ConnectionProbeFailed(_) => {
                Some("afs connect notion")
            }
            Self::Credential(_) => Some("afs connect notion"),
            Self::ConnectionNameRequired | Self::Store(_) => None,
        }
    }
}

impl From<ConnectionRecord> for ConnectionSummary {
    fn from(value: ConnectionRecord) -> Self {
        Self {
            connection_id: value.connection_id.0,
            connector: value.connector,
            display_name: value.display_name,
            account_label: value.account_label,
            workspace_id: value.workspace_id,
            workspace_name: value.workspace_name,
            auth_kind: value.auth_kind,
            status: value.status,
            created_at: value.created_at,
            updated_at: value.updated_at,
            expires_at: value.expires_at,
        }
    }
}

fn default_connection_id<S>(store: &S) -> Result<ConnectionId, ConnectError>
where
    S: ConnectionRepository,
{
    let notion_connections = store
        .list_connections()
        .map_err(ConnectError::Store)?
        .into_iter()
        .filter(|connection| connection.connector == "notion")
        .count();
    if notion_connections == 0 {
        Ok(ConnectionId::new("notion-default"))
    } else {
        Err(ConnectError::ConnectionNameRequired)
    }
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn user_label(value: &serde_json::Value) -> Option<String> {
    string_field(value, "/name")
        .or_else(|| string_field(value, "/person/email"))
        .or_else(|| string_field(value, "/bot/workspace_name"))
}

fn string_field(value: &serde_json::Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}
