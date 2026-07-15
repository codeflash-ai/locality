use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::ConnectorCapabilities;
use locality_connector::oauth_broker::{OAuthBrokerCodeExchange, OAuthBrokerToken};
use locality_gmail::{
    GMAIL_CONNECTOR_ID, GMAIL_OAUTH_SCOPES, HttpGmailOAuthBrokerClient, StoredGmailCredential,
    gmail_capabilities_json, validate_gmail_oauth_scopes,
};
use locality_google_docs::{
    GOOGLE_DOCS_CONNECTOR_ID, GOOGLE_DOCS_OAUTH_SCOPES, HttpGoogleDocsOAuthBrokerClient,
    StoredGoogleDocsCredential, google_docs_capabilities_json,
};
use locality_granola::{GRANOLA_CONNECTOR_ID, GranolaApi, HttpGranolaApiClient};
use locality_notion::NotionConfig;
use locality_notion::client::{DEFAULT_NOTION_TOKEN_ENV, HttpNotionApi, NotionApi};
use locality_notion::oauth::{
    HttpNotionOAuthBrokerClient, HttpNotionOAuthClient, NotionOAuthBrokerCodeExchange,
    NotionOAuthCodeExchange, NotionOAuthToken, StoredNotionCredential,
};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileId,
    ConnectorProfileRecord, ConnectorProfileRepository, CredentialError, CredentialStore,
    StoreError,
};
use serde::Serialize;

pub const DEFAULT_NOTION_PROFILE_ID: &str = "notion-token-default";
pub const DEFAULT_NOTION_OAUTH_PROFILE_ID: &str = "notion-oauth-default";
pub const DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID: &str = "google-docs-oauth-default";
pub const DEFAULT_GMAIL_OAUTH_PROFILE_ID: &str = "gmail-oauth-default";
pub const DEFAULT_GRANOLA_API_KEY_PROFILE_ID: &str = "granola-api-key-default";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectOptions {
    pub connection_id: Option<ConnectionId>,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthConnectOptions {
    pub connection_id: Option<ConnectionId>,
    pub client_id: String,
    pub client_secret: String,
    pub code: String,
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerOAuthConnectOptions {
    pub connection_id: Option<ConnectionId>,
    pub broker_url: String,
    pub client_id: String,
    pub session: String,
    pub state: String,
    pub code: String,
    pub redirect_uri: String,
}

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectReport {
    pub ok: bool,
    pub command: &'static str,
    pub connection_id: String,
    pub profile_id: String,
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
pub struct ProfilesReport {
    pub ok: bool,
    pub command: &'static str,
    pub profiles: Vec<ConnectorProfileSummary>,
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
    pub profile_id: Option<String>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectorProfileSummary {
    pub profile_id: String,
    pub connector: String,
    pub display_name: String,
    pub auth_kind: String,
    pub scopes: Vec<String>,
    pub capabilities_json: String,
    pub enabled_actions_json: String,
    pub connector_version: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
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

pub trait GranolaConnectionProbe {
    fn probe(&self, api_key: &str) -> Result<(), ConnectError>;
}

#[derive(Clone, Debug, Default)]
pub struct HttpGranolaConnectionProbe;

impl GranolaConnectionProbe for HttpGranolaConnectionProbe {
    fn probe(&self, api_key: &str) -> Result<(), ConnectError> {
        HttpGranolaApiClient::new(api_key)
            .list_notes(None, 1, None, None, None)
            .map(|_| ())
            .map_err(|error| ConnectError::ConnectionProbeFailed(error.to_string()))
    }
}

pub trait NotionOAuthExchange {
    fn exchange_code(
        &self,
        request: &NotionOAuthCodeExchange,
    ) -> Result<NotionOAuthToken, ConnectError>;
}

pub trait NotionOAuthBrokerExchange {
    fn exchange_code(
        &self,
        request: &NotionOAuthBrokerCodeExchange,
    ) -> Result<NotionOAuthToken, ConnectError>;
}

pub trait GoogleDocsOAuthBrokerExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, ConnectError>;
}

pub trait GmailOAuthBrokerExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, ConnectError>;
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

impl NotionOAuthExchange for HttpNotionOAuthClient {
    fn exchange_code(
        &self,
        request: &NotionOAuthCodeExchange,
    ) -> Result<NotionOAuthToken, ConnectError> {
        HttpNotionOAuthClient::exchange_code(self, request).map_err(|error| {
            ConnectError::OAuthExchangeFailed(OAuthExchangeFailure::notion(error.to_string()))
        })
    }
}

impl NotionOAuthBrokerExchange for HttpNotionOAuthBrokerClient {
    fn exchange_code(
        &self,
        request: &NotionOAuthBrokerCodeExchange,
    ) -> Result<NotionOAuthToken, ConnectError> {
        HttpNotionOAuthBrokerClient::exchange_code(self, request).map_err(|error| {
            ConnectError::OAuthExchangeFailed(OAuthExchangeFailure::notion(error.to_string()))
        })
    }
}

impl GoogleDocsOAuthBrokerExchange for HttpGoogleDocsOAuthBrokerClient {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, ConnectError> {
        HttpGoogleDocsOAuthBrokerClient::exchange_code(self, request).map_err(|error| {
            ConnectError::OAuthExchangeFailed(OAuthExchangeFailure::google_docs(error.to_string()))
        })
    }
}

impl GmailOAuthBrokerExchange for HttpGmailOAuthBrokerClient {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, ConnectError> {
        HttpGmailOAuthBrokerClient::exchange_code(self, request).map_err(|error| {
            ConnectError::OAuthExchangeFailed(OAuthExchangeFailure::gmail(error.to_string()))
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
    S: ConnectionRepository + ConnectorProfileRepository,
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
        .map_err(|error| ConnectError::Credential(CredentialStorageFailure::notion(error)))?;

    let now = timestamp();
    let profile_id = ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID);
    store
        .save_connector_profile(default_notion_token_profile(now.clone()))
        .map_err(ConnectError::Store)?;

    let display_name = connection_id.0.clone();
    let connection = ConnectionRecord {
        connection_id: connection_id.clone(),
        profile_id: Some(profile_id.clone()),
        connector: "notion".to_string(),
        display_name: display_name.clone(),
        account_label: probe_result.account_label.clone(),
        workspace_id: probe_result.workspace_id.clone(),
        workspace_name: probe_result.workspace_name.clone(),
        auth_kind: "token".to_string(),
        secret_ref,
        scopes: vec![],
        capabilities_json: notion_capabilities_json()?,
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
        profile_id: profile_id.0,
        connector: "notion".to_string(),
        display_name,
        account_label: probe_result.account_label,
        workspace_id: probe_result.workspace_id,
        workspace_name: probe_result.workspace_name,
        auth_kind: "token".to_string(),
    })
}

pub fn run_connect_granola<S, P>(
    store: &mut S,
    credentials: &dyn CredentialStore,
    options: ConnectOptions,
    probe: &P,
) -> Result<ConnectReport, ConnectError>
where
    S: ConnectionRepository + ConnectorProfileRepository,
    P: GranolaConnectionProbe,
{
    let connection_id = match options.connection_id {
        Some(connection_id) => connection_id,
        None => default_connection_id_for_connector(
            store,
            GRANOLA_CONNECTOR_ID,
            "granola-default",
            "Granola",
        )?,
    };
    probe.probe(&options.token)?;
    let secret_ref = format!("connection:{}", connection_id.0);
    credentials
        .put(&secret_ref, &options.token)
        .map_err(|error| ConnectError::Credential(CredentialStorageFailure::granola(error)))?;

    let now = timestamp();
    let profile_id = ConnectorProfileId::new(DEFAULT_GRANOLA_API_KEY_PROFILE_ID);
    store
        .save_connector_profile(default_granola_api_key_profile(now.clone()))
        .map_err(ConnectError::Store)?;
    let display_name = connection_id.0.clone();
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id.clone()),
            connector: GRANOLA_CONNECTOR_ID.to_string(),
            display_name: display_name.clone(),
            account_label: None,
            workspace_id: None,
            workspace_name: None,
            auth_kind: "api_key".to_string(),
            secret_ref,
            scopes: vec!["read".to_string()],
            capabilities_json: granola_capabilities_json()?,
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .map_err(ConnectError::Store)?;

    Ok(ConnectReport {
        ok: true,
        command: "connect",
        connection_id: connection_id.0,
        profile_id: profile_id.0,
        connector: GRANOLA_CONNECTOR_ID.to_string(),
        display_name,
        account_label: None,
        workspace_id: None,
        workspace_name: None,
        auth_kind: "api_key".to_string(),
    })
}

pub fn run_connect_notion_oauth<S, E>(
    store: &mut S,
    credentials: &dyn CredentialStore,
    options: OAuthConnectOptions,
    exchange: &E,
) -> Result<ConnectReport, ConnectError>
where
    S: ConnectionRepository + ConnectorProfileRepository,
    E: NotionOAuthExchange,
{
    let connection_id = match options.connection_id {
        Some(connection_id) => connection_id,
        None => default_connection_id(store)?,
    };
    let exchange_request = NotionOAuthCodeExchange {
        client_id: options.client_id,
        client_secret: options.client_secret,
        code: options.code,
        redirect_uri: options.redirect_uri,
    };
    let token = exchange.exchange_code(&exchange_request)?;
    let acquired_at = timestamp_secs();
    let secret_ref = format!("connection:{}", connection_id.0);
    let stored = StoredNotionCredential::from_oauth_token(
        token.clone(),
        exchange_request.client_id,
        exchange_request.client_secret,
        acquired_at,
    );
    let secret = serde_json::to_string(&stored).map_err(|error| {
        ConnectError::CredentialEncode(CredentialEncodeFailure::notion(error.to_string()))
    })?;
    credentials
        .put(&secret_ref, &secret)
        .map_err(|error| ConnectError::Credential(CredentialStorageFailure::notion(error)))?;

    let now = timestamp();
    let profile_id = ConnectorProfileId::new(DEFAULT_NOTION_OAUTH_PROFILE_ID);
    store
        .save_connector_profile(default_notion_oauth_profile(now.clone()))
        .map_err(ConnectError::Store)?;

    let display_name = connection_id.0.clone();
    let account_label = token
        .workspace_name
        .clone()
        .or_else(|| token.workspace_id.clone())
        .or_else(|| token.bot_id.clone());
    let connection = ConnectionRecord {
        connection_id: connection_id.clone(),
        profile_id: Some(profile_id.clone()),
        connector: "notion".to_string(),
        display_name: display_name.clone(),
        account_label: account_label.clone(),
        workspace_id: token.workspace_id.clone(),
        workspace_name: token.workspace_name.clone(),
        auth_kind: "oauth".to_string(),
        secret_ref,
        scopes: vec![],
        capabilities_json: notion_capabilities_json()?,
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
        expires_at: stored.expires_at.map(|expires_at| expires_at.to_string()),
    };
    store
        .save_connection(connection)
        .map_err(ConnectError::Store)?;

    Ok(ConnectReport {
        ok: true,
        command: "connect",
        connection_id: connection_id.0,
        profile_id: profile_id.0,
        connector: "notion".to_string(),
        display_name,
        account_label,
        workspace_id: token.workspace_id,
        workspace_name: token.workspace_name,
        auth_kind: "oauth".to_string(),
    })
}

pub fn run_connect_notion_broker_oauth<S, E>(
    store: &mut S,
    credentials: &dyn CredentialStore,
    options: BrokerOAuthConnectOptions,
    exchange: &E,
) -> Result<ConnectReport, ConnectError>
where
    S: ConnectionRepository + ConnectorProfileRepository,
    E: NotionOAuthBrokerExchange,
{
    let connection_id = match options.connection_id {
        Some(connection_id) => connection_id,
        None => default_connection_id(store)?,
    };
    let exchange_request = NotionOAuthBrokerCodeExchange {
        session: options.session,
        state: options.state,
        code: options.code,
        redirect_uri: options.redirect_uri,
    };
    let token = exchange.exchange_code(&exchange_request)?;
    let acquired_at = timestamp_secs();
    let secret_ref = format!("connection:{}", connection_id.0);
    let stored = StoredNotionCredential::from_broker_oauth_token(
        token.clone(),
        options.client_id,
        options.broker_url,
        acquired_at,
    );
    let secret = serde_json::to_string(&stored).map_err(|error| {
        ConnectError::CredentialEncode(CredentialEncodeFailure::notion(error.to_string()))
    })?;
    credentials
        .put(&secret_ref, &secret)
        .map_err(|error| ConnectError::Credential(CredentialStorageFailure::notion(error)))?;

    let now = timestamp();
    let profile_id = ConnectorProfileId::new(DEFAULT_NOTION_OAUTH_PROFILE_ID);
    store
        .save_connector_profile(default_notion_oauth_profile(now.clone()))
        .map_err(ConnectError::Store)?;

    let display_name = connection_id.0.clone();
    let account_label = token
        .workspace_name
        .clone()
        .or_else(|| token.workspace_id.clone())
        .or_else(|| token.bot_id.clone());
    let connection = ConnectionRecord {
        connection_id: connection_id.clone(),
        profile_id: Some(profile_id.clone()),
        connector: "notion".to_string(),
        display_name: display_name.clone(),
        account_label: account_label.clone(),
        workspace_id: token.workspace_id.clone(),
        workspace_name: token.workspace_name.clone(),
        auth_kind: "oauth".to_string(),
        secret_ref,
        scopes: vec![],
        capabilities_json: notion_capabilities_json()?,
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
        expires_at: stored.expires_at.map(|expires_at| expires_at.to_string()),
    };
    store
        .save_connection(connection)
        .map_err(ConnectError::Store)?;

    Ok(ConnectReport {
        ok: true,
        command: "connect",
        connection_id: connection_id.0,
        profile_id: profile_id.0,
        connector: "notion".to_string(),
        display_name,
        account_label,
        workspace_id: token.workspace_id,
        workspace_name: token.workspace_name,
        auth_kind: "oauth".to_string(),
    })
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
{
    let connection_id = match options.connection_id {
        Some(connection_id) => connection_id,
        None => default_connection_id_for_connector(
            store,
            GOOGLE_DOCS_CONNECTOR_ID,
            "google-docs-default",
            "Google Docs",
        )?,
    };
    let exchange_request = OAuthBrokerCodeExchange {
        connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
        session: options.session,
        state: options.state,
        code: options.code,
        redirect_uri: options.redirect_uri,
    };
    let token = exchange.exchange_code(&exchange_request)?;
    let acquired_at = timestamp_secs();
    let secret_ref = format!("connection:{}", connection_id.0);
    let stored = StoredGoogleDocsCredential::from_broker_token(
        token.clone(),
        options.client_id,
        options.broker_url,
        acquired_at,
    );
    let secret = serde_json::to_string(&stored).map_err(|error| {
        ConnectError::CredentialEncode(CredentialEncodeFailure::google_docs(error.to_string()))
    })?;
    credentials
        .put(&secret_ref, &secret)
        .map_err(|error| ConnectError::Credential(CredentialStorageFailure::google_docs(error)))?;

    let now = timestamp();
    let profile_id = ConnectorProfileId::new(DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID);
    store
        .save_connector_profile(default_google_docs_oauth_profile(now.clone()))
        .map_err(ConnectError::Store)?;

    let display_name = connection_id.0.clone();
    let account_label = token
        .account_label
        .clone()
        .or_else(|| token.account_id.clone())
        .or_else(|| token.workspace_name.clone());
    let connection = ConnectionRecord {
        connection_id: connection_id.clone(),
        profile_id: Some(profile_id.clone()),
        connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
        display_name: display_name.clone(),
        account_label: account_label.clone(),
        workspace_id: token.workspace_id.clone(),
        workspace_name: token.workspace_name.clone(),
        auth_kind: "oauth".to_string(),
        secret_ref,
        scopes: token.scopes.clone(),
        capabilities_json: google_docs_capabilities_json().map_err(|error| {
            ConnectError::CredentialEncode(CredentialEncodeFailure::google_docs(error.to_string()))
        })?,
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
        expires_at: stored.expires_at.map(|expires_at| expires_at.to_string()),
    };
    store
        .save_connection(connection)
        .map_err(ConnectError::Store)?;

    Ok(ConnectReport {
        ok: true,
        command: "connect",
        connection_id: connection_id.0,
        profile_id: profile_id.0,
        connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
        display_name,
        account_label,
        workspace_id: token.workspace_id,
        workspace_name: token.workspace_name,
        auth_kind: "oauth".to_string(),
    })
}

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
    validate_gmail_oauth_scopes(&token.scopes).map_err(|error| {
        ConnectError::OAuthExchangeFailed(OAuthExchangeFailure::gmail(error.to_string()))
    })?;
    let acquired_at = timestamp_secs();
    let secret_ref = format!("connection:{}", connection_id.0);
    let stored = StoredGmailCredential::from_broker_token(
        token.clone(),
        options.client_id,
        options.broker_url,
        acquired_at,
    );
    let secret = serde_json::to_string(&stored).map_err(|error| {
        ConnectError::CredentialEncode(CredentialEncodeFailure::gmail(error.to_string()))
    })?;
    credentials
        .put(&secret_ref, &secret)
        .map_err(|error| ConnectError::Credential(CredentialStorageFailure::gmail(error)))?;

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
    let connection = ConnectionRecord {
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
        capabilities_json: gmail_capabilities_json().map_err(|error| {
            ConnectError::CredentialEncode(CredentialEncodeFailure::gmail(error.to_string()))
        })?,
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
        expires_at: stored.expires_at.map(|expires_at| expires_at.to_string()),
    };
    store
        .save_connection(connection)
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

pub fn run_profiles<S>(store: &S) -> Result<ProfilesReport, ConnectError>
where
    S: ConnectorProfileRepository,
{
    let mut profiles = store
        .list_connector_profiles()
        .map_err(ConnectError::Store)?
        .into_iter()
        .map(ConnectorProfileSummary::from)
        .collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
    Ok(ProfilesReport {
        ok: true,
        command: "profiles",
        profiles,
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
        .map_err(|error| {
            ConnectError::Credential(CredentialStorageFailure::for_connector_id(
                &connection.connector,
                error,
            ))
        })?;
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
pub struct OAuthExchangeFailure {
    connector: OAuthExchangeConnector,
    message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OAuthExchangeConnector {
    Notion,
    GoogleDocs,
    Gmail,
}

impl OAuthExchangeFailure {
    pub fn notion(message: impl Into<String>) -> Self {
        Self {
            connector: OAuthExchangeConnector::Notion,
            message: message.into(),
        }
    }

    pub fn google_docs(message: impl Into<String>) -> Self {
        Self {
            connector: OAuthExchangeConnector::GoogleDocs,
            message: message.into(),
        }
    }

    pub fn gmail(message: impl Into<String>) -> Self {
        Self {
            connector: OAuthExchangeConnector::Gmail,
            message: message.into(),
        }
    }

    fn connector_display_name(&self) -> &'static str {
        match self.connector {
            OAuthExchangeConnector::Notion => "Notion",
            OAuthExchangeConnector::GoogleDocs => "Google Docs",
            OAuthExchangeConnector::Gmail => "Gmail",
        }
    }

    fn suggested_command(&self) -> &'static str {
        match self.connector {
            OAuthExchangeConnector::Notion => "loc connect notion",
            OAuthExchangeConnector::GoogleDocs => "loc connect google-docs",
            OAuthExchangeConnector::Gmail => "loc connect gmail",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialEncodeFailure {
    connector: CredentialFailureConnector,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialStorageFailure {
    connector: CredentialFailureConnector,
    error: CredentialError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialFailureConnector {
    Notion,
    GoogleDocs,
    Gmail,
    Granola,
}

impl CredentialEncodeFailure {
    pub fn notion(message: impl Into<String>) -> Self {
        Self::new(CredentialFailureConnector::Notion, message)
    }

    pub fn google_docs(message: impl Into<String>) -> Self {
        Self::new(CredentialFailureConnector::GoogleDocs, message)
    }

    pub fn gmail(message: impl Into<String>) -> Self {
        Self::new(CredentialFailureConnector::Gmail, message)
    }

    pub fn granola(message: impl Into<String>) -> Self {
        Self::new(CredentialFailureConnector::Granola, message)
    }

    fn new(connector: CredentialFailureConnector, message: impl Into<String>) -> Self {
        Self {
            connector,
            message: message.into(),
        }
    }

    fn message(&self) -> String {
        format!(
            "failed to encode {} credential: {}",
            self.connector.display_name(),
            self.message
        )
    }

    fn suggested_command(&self) -> &'static str {
        self.connector.suggested_command()
    }
}

impl CredentialStorageFailure {
    pub fn notion(error: CredentialError) -> Self {
        Self::new(CredentialFailureConnector::Notion, error)
    }

    pub fn google_docs(error: CredentialError) -> Self {
        Self::new(CredentialFailureConnector::GoogleDocs, error)
    }

    pub fn gmail(error: CredentialError) -> Self {
        Self::new(CredentialFailureConnector::Gmail, error)
    }

    pub fn granola(error: CredentialError) -> Self {
        Self::new(CredentialFailureConnector::Granola, error)
    }

    fn new(connector: CredentialFailureConnector, error: CredentialError) -> Self {
        Self { connector, error }
    }

    fn for_connector_id(connector: &str, error: CredentialError) -> Self {
        Self::new(
            CredentialFailureConnector::from_connector_id(connector),
            error,
        )
    }

    fn code(&self) -> &'static str {
        self.error.code()
    }

    fn message(&self) -> String {
        match self.connector {
            CredentialFailureConnector::Notion => self.error.to_string(),
            _ => format!(
                "failed to store {} credential: {}",
                self.connector.display_name(),
                self.error
            ),
        }
    }

    fn suggested_command(&self) -> &'static str {
        self.connector.suggested_command()
    }
}

impl CredentialFailureConnector {
    fn from_connector_id(connector: &str) -> Self {
        match connector {
            GOOGLE_DOCS_CONNECTOR_ID => Self::GoogleDocs,
            GMAIL_CONNECTOR_ID => Self::Gmail,
            GRANOLA_CONNECTOR_ID => Self::Granola,
            _ => Self::Notion,
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            Self::Notion => "Notion",
            Self::GoogleDocs => "Google Docs",
            Self::Gmail => "Gmail",
            Self::Granola => "Granola",
        }
    }

    fn suggested_command(&self) -> &'static str {
        match self {
            Self::Notion => "loc connect notion",
            Self::GoogleDocs => "loc connect google-docs",
            Self::Gmail => "loc connect gmail",
            Self::Granola => "loc connect granola --api-key-stdin",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    ConnectionMissing(String),
    ConnectionNameRequired(String),
    ConnectionProbeFailed(String),
    OAuthExchangeFailed(OAuthExchangeFailure),
    CredentialEncode(CredentialEncodeFailure),
    Credential(CredentialStorageFailure),
    Store(StoreError),
}

impl ConnectError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConnectionMissing(_) => "missing_connection",
            Self::ConnectionNameRequired(_) => "usage",
            Self::ConnectionProbeFailed(_) => "connection_probe_failed",
            Self::OAuthExchangeFailed(_) => "oauth_exchange_failed",
            Self::CredentialEncode(_) => "credential_store_unavailable",
            Self::Credential(failure) => failure.code(),
            Self::Store(_) => "store_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::ConnectionMissing(connection_id) => {
                format!("connection `{connection_id}` was not found")
            }
            Self::ConnectionNameRequired(connector) => {
                format!("multiple {connector} connections exist; pass --name <id>")
            }
            Self::ConnectionProbeFailed(message) => {
                format!("Notion connection probe failed: {message}")
            }
            Self::OAuthExchangeFailed(failure) => {
                format!(
                    "{} OAuth exchange failed: {}",
                    failure.connector_display_name(),
                    failure.message
                )
            }
            Self::CredentialEncode(failure) => failure.message(),
            Self::Credential(failure) => failure.message(),
            Self::Store(error) => error.to_string(),
        }
    }

    pub fn suggested_command(&self) -> Option<&'static str> {
        match self {
            Self::ConnectionMissing(_) | Self::ConnectionProbeFailed(_) => {
                Some("loc connect notion")
            }
            Self::OAuthExchangeFailed(failure) => Some(failure.suggested_command()),
            Self::Credential(failure) => Some(failure.suggested_command()),
            Self::CredentialEncode(failure) => Some(failure.suggested_command()),
            Self::ConnectionNameRequired(_) | Self::Store(_) => None,
        }
    }
}

impl From<ConnectionRecord> for ConnectionSummary {
    fn from(value: ConnectionRecord) -> Self {
        Self {
            connection_id: value.connection_id.0,
            profile_id: value.profile_id.map(|profile_id| profile_id.0),
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

impl From<ConnectorProfileRecord> for ConnectorProfileSummary {
    fn from(value: ConnectorProfileRecord) -> Self {
        Self {
            profile_id: value.profile_id.0,
            connector: value.connector,
            display_name: value.display_name,
            auth_kind: value.auth_kind,
            scopes: value.scopes,
            capabilities_json: value.capabilities_json,
            enabled_actions_json: value.enabled_actions_json,
            connector_version: value.connector_version,
            status: value.status,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

fn default_notion_token_profile(now: String) -> ConnectorProfileRecord {
    ConnectorProfileRecord {
        profile_id: ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID),
        connector: "notion".to_string(),
        display_name: "Notion token auth".to_string(),
        auth_kind: "token".to_string(),
        scopes: vec![],
        capabilities_json: notion_capabilities_json().unwrap_or_else(|_| "{}".to_string()),
        enabled_actions_json: "[\"read\",\"write\"]".to_string(),
        connector_version: "notion.v1".to_string(),
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
    }
}

fn default_notion_oauth_profile(now: String) -> ConnectorProfileRecord {
    ConnectorProfileRecord {
        profile_id: ConnectorProfileId::new(DEFAULT_NOTION_OAUTH_PROFILE_ID),
        connector: "notion".to_string(),
        display_name: "Notion OAuth".to_string(),
        auth_kind: "oauth".to_string(),
        scopes: vec![],
        capabilities_json: notion_capabilities_json().unwrap_or_else(|_| "{}".to_string()),
        enabled_actions_json: "[\"read\",\"write\"]".to_string(),
        connector_version: "notion.v1".to_string(),
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
    }
}

fn default_google_docs_oauth_profile(now: String) -> ConnectorProfileRecord {
    ConnectorProfileRecord {
        profile_id: ConnectorProfileId::new(DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID),
        connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
        display_name: "Google Docs OAuth".to_string(),
        auth_kind: "oauth".to_string(),
        scopes: GOOGLE_DOCS_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect(),
        capabilities_json: google_docs_capabilities_json().unwrap_or_else(|_| "{}".to_string()),
        enabled_actions_json: "[\"read\",\"write\"]".to_string(),
        connector_version: "google-docs.v1".to_string(),
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
    }
}

fn default_gmail_oauth_profile(now: String) -> ConnectorProfileRecord {
    ConnectorProfileRecord {
        profile_id: ConnectorProfileId::new(DEFAULT_GMAIL_OAUTH_PROFILE_ID),
        connector: GMAIL_CONNECTOR_ID.to_string(),
        display_name: "Gmail OAuth".to_string(),
        auth_kind: "oauth".to_string(),
        scopes: GMAIL_OAUTH_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect(),
        capabilities_json: gmail_capabilities_json().unwrap_or_else(|_| "{}".to_string()),
        enabled_actions_json: "[\"read\",\"send\"]".to_string(),
        connector_version: "gmail.v1".to_string(),
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
    }
}

fn default_granola_api_key_profile(now: String) -> ConnectorProfileRecord {
    ConnectorProfileRecord {
        profile_id: ConnectorProfileId::new(DEFAULT_GRANOLA_API_KEY_PROFILE_ID),
        connector: GRANOLA_CONNECTOR_ID.to_string(),
        display_name: "Granola API key".to_string(),
        auth_kind: "api_key".to_string(),
        scopes: vec!["read".to_string()],
        capabilities_json: granola_capabilities_json().unwrap_or_else(|_| "{}".to_string()),
        enabled_actions_json: "[\"read\"]".to_string(),
        connector_version: "granola.v1".to_string(),
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
    }
}

fn granola_capabilities_json() -> Result<String, ConnectError> {
    serde_json::to_string(&ConnectorCapabilities::read_only()).map_err(|error| {
        ConnectError::CredentialEncode(CredentialEncodeFailure::granola(error.to_string()))
    })
}

fn notion_capabilities_json() -> Result<String, ConnectError> {
    let capabilities = ConnectorCapabilities {
        supports_block_updates: true,
        supports_entity_body_updates: false,
        supports_databases: true,
        supports_oauth: true,
        supports_remote_observation: true,
        supports_lazy_child_enumeration: true,
        supports_media_download: true,
        supports_undo: true,
        supports_batch_observation: false,
    };
    serde_json::to_string(&capabilities).map_err(|error| {
        ConnectError::CredentialEncode(CredentialEncodeFailure::notion(error.to_string()))
    })
}

fn default_connection_id<S>(store: &S) -> Result<ConnectionId, ConnectError>
where
    S: ConnectionRepository,
{
    default_connection_id_for_connector(store, "notion", "notion-default", "Notion")
}

fn default_connection_id_for_connector<S>(
    store: &S,
    connector: &str,
    default_id: &str,
    display_name: &str,
) -> Result<ConnectionId, ConnectError>
where
    S: ConnectionRepository,
{
    let connection_count = store
        .list_connections()
        .map_err(ConnectError::Store)?
        .into_iter()
        .filter(|connection| connection.connector == connector && connection.status == "active")
        .count();
    if connection_count == 0 {
        Ok(ConnectionId::new(default_id))
    } else {
        Err(ConnectError::ConnectionNameRequired(
            display_name.to_string(),
        ))
    }
}

fn timestamp() -> String {
    timestamp_secs().to_string()
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use locality_store::{
        ConnectionId, ConnectionRepository, CredentialStore, InMemoryCredentialStore,
        InMemoryStateStore,
    };

    use super::{ConnectOptions, GranolaConnectionProbe, run_connect_granola};

    #[derive(Debug)]
    struct SuccessfulGranolaProbe;

    impl GranolaConnectionProbe for SuccessfulGranolaProbe {
        fn probe(&self, api_key: &str) -> Result<(), super::ConnectError> {
            assert_eq!(api_key, "grn_test_secret");
            Ok(())
        }
    }

    #[test]
    fn granola_connect_stores_secret_outside_connection_metadata() {
        let mut store = InMemoryStateStore::new();
        let credentials = InMemoryCredentialStore::new();
        let report = run_connect_granola(
            &mut store,
            &credentials,
            ConnectOptions {
                connection_id: Some(ConnectionId::new("granola-work")),
                token: "grn_test_secret".to_string(),
            },
            &SuccessfulGranolaProbe,
        )
        .expect("connect Granola");

        assert_eq!(report.connector, "granola");
        assert_eq!(report.auth_kind, "api_key");
        let connection = store
            .get_connection(&ConnectionId::new("granola-work"))
            .expect("read connection")
            .expect("connection");
        assert_eq!(connection.secret_ref, "connection:granola-work");
        assert!(!format!("{connection:?}").contains("grn_test_secret"));
        assert_eq!(
            credentials
                .get("connection:granola-work")
                .expect("stored credential"),
            "grn_test_secret"
        );
    }
}
