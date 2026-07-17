use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::oauth_broker::OAuthBrokerRefresh;
use locality_connector::{Connector, FetchRequest};
use locality_core::hydration::HydrationRequest;
use locality_core::model::RemoteId;
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_slack::{
    HttpSlackOAuthBrokerClient, SLACK_CONNECTOR_ID, SlackConfig, SlackConnector,
    SlackMountSettings, SlackNativeBundle, SlackOAuthScopeError, SlackRenderedKind,
    StoredSlackCredential, render_slack_entity,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

const SLACK_CONNECT_COMMAND: &str = "loc connect slack";

pub fn resolve_slack_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<SlackConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != SLACK_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            mount.connector.clone(),
        ));
    }

    if let Some(connection_id) = &mount.connection_id {
        let connection = store
            .get_connection(connection_id)
            .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
            .ok_or_else(|| ConnectorResolveError::MissingConnection {
                message: format!("connection `{}` was not found", connection_id.0),
                suggested_command: SLACK_CONNECT_COMMAND.to_string(),
            })?;
        validate_connection_profile(store, &connection)?;
        return connector_from_connection(credentials, &connection, mount);
    }

    let active = active_slack_connections(store)?;
    if active.len() == 1 {
        validate_connection_profile(store, &active[0])?;
        return connector_from_connection(credentials, &active[0], mount);
    }

    let message = if active.is_empty() {
        "missing Slack connection; run `loc connect slack`".to_string()
    } else {
        "mount has no connection_id and multiple Slack connections exist".to_string()
    };
    Err(ConnectorResolveError::MissingConnection {
        message,
        suggested_command: SLACK_CONNECT_COMMAND.to_string(),
    })
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
    mount: &MountConfig,
) -> Result<SlackConnector, ConnectorResolveError> {
    if connection.connector != SLACK_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            connection.connector.clone(),
        ));
    }

    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: SLACK_CONNECT_COMMAND.to_string(),
        });
    }

    if connection.auth_kind != "oauth" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some(format!(
                "Slack connection `{}` must use OAuth credentials",
                connection.connection_id.0
            )),
            suggested_command: SLACK_CONNECT_COMMAND.to_string(),
        });
    }

    let token = connection_access_token(credentials, connection)?;
    Ok(SlackConnector::new(slack_config_from_mount(token, mount)?))
}

fn slack_config_from_mount(
    token: String,
    mount: &MountConfig,
) -> Result<SlackConfig, ConnectorResolveError> {
    let settings = SlackMountSettings::from_json(&mount.settings_json).map_err(|error| {
        ConnectorResolveError::CredentialStoreUnavailable(format!(
            "Slack mount `{}` settings are invalid: {}",
            mount.mount_id.0,
            slack_settings_error_message(error)
        ))
    })?;
    Ok(SlackConfig::new(token).with_settings(settings))
}

fn slack_settings_error_message(error: LocalityError) -> String {
    match error {
        LocalityError::Validation(issues) => issues
            .into_iter()
            .map(|issue| issue.message)
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

fn connection_access_token(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<String, ConnectorResolveError> {
    let secret = credentials
        .get(&connection.secret_ref)
        .map_err(|error| credential_error(connection, error))?;
    let mut stored = serde_json::from_str::<StoredSlackCredential>(&secret)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    if stored.expires_soon(timestamp_secs()) {
        let refreshed = refresh_oauth_credential(connection, &stored)?;
        stored = stored
            .refreshed(refreshed, timestamp_secs())
            .map_err(|error| slack_refresh_scope_error(connection, error))?;
        let secret = serde_json::to_string(&stored).map_err(|error| {
            ConnectorResolveError::CredentialStoreUnavailable(error.to_string())
        })?;
        credentials
            .put(&connection.secret_ref, &secret)
            .map_err(|error| credential_error(connection, error))?;
    }
    Ok(stored.access_token)
}

fn refresh_oauth_credential(
    connection: &ConnectionRecord,
    stored: &StoredSlackCredential,
) -> Result<locality_connector::oauth_broker::OAuthBrokerToken, ConnectorResolveError> {
    let Some(refresh_token_handle) = stored.refresh_token_handle.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: SLACK_CONNECT_COMMAND.to_string(),
        });
    };
    let Some(broker_url) = stored.oauth_broker_url.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: SLACK_CONNECT_COMMAND.to_string(),
        });
    };

    HttpSlackOAuthBrokerClient::new(broker_url.clone())
        .refresh_token(&OAuthBrokerRefresh {
            connector: SLACK_CONNECTOR_ID.to_string(),
            refresh_token_handle: Some(refresh_token_handle),
        })
        .map_err(|error| slack_refresh_error(connection, &broker_url, error))
}

fn slack_refresh_scope_error(
    connection: &ConnectionRecord,
    error: SlackOAuthScopeError,
) -> ConnectorResolveError {
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Slack credential for connection `{}` could not be refreshed through OAuth broker: {error}",
            connection.connection_id.0
        )),
        suggested_command: SLACK_CONNECT_COMMAND.to_string(),
    }
}

fn slack_refresh_error(
    connection: &ConnectionRecord,
    broker_url: &str,
    error: LocalityError,
) -> ConnectorResolveError {
    let hint = if is_loopback_broker_url(broker_url) {
        "reconnect with the default hosted broker or keep the local broker running"
    } else {
        "reconnect to issue a fresh Slack refresh handle"
    };
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Slack credential for connection `{}` could not be refreshed through OAuth broker at `{broker_url}`: {error}; {hint}",
            connection.connection_id.0
        )),
        suggested_command: SLACK_CONNECT_COMMAND.to_string(),
    }
}

fn is_loopback_broker_url(url: &str) -> bool {
    let Some(authority) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let host_port = authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase();
    let host = if host_port.starts_with('[') {
        host_port
            .split(']')
            .next()
            .map(|value| format!("{value}]"))
            .unwrap_or(host_port)
    } else {
        host_port
            .split(':')
            .next()
            .unwrap_or(host_port.as_str())
            .to_string()
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

fn active_slack_connections<S>(store: &S) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository + ?Sized,
{
    let connections = store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    Ok(connections
        .into_iter()
        .filter(|connection| {
            connection.connector == SLACK_CONNECTOR_ID
                && connection.status == "active"
                && connection.auth_kind == "oauth"
        })
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
            suggested_command: SLACK_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: SLACK_CONNECT_COMMAND.to_string(),
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
            suggested_command: SLACK_CONNECT_COMMAND.to_string(),
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

impl SourcePushValidator for SlackConnector {
    fn validate_changed_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_slack_frontmatter(context)
    }

    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_slack_frontmatter(context)
    }
}

impl SourceAdapter for SlackConnector {}

impl HydrationSource for SlackConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<SlackNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("Slack native decode failed: {error}")))?;
        let document = render_slack_entity(&bundle)?;
        let block_ids: Vec<RemoteId> = segment_markdown_body(&document.body, 1)
            .into_iter()
            .filter(|block| !block.is_directive())
            .enumerate()
            .map(|(index, _)| RemoteId::new(format!("{}:body:{index}", request.remote_id.0)))
            .collect();
        let shadow = ShadowDocument::from_synced_body(
            request.remote_id.clone(),
            document.body.clone(),
            1,
            block_ids,
        )
        .map_err(|error| LocalityError::InvalidState(error.to_string()))?
        .with_frontmatter(document.frontmatter.clone());
        Ok(HydratedEntity {
            remote_edited_at: Some(remote_version(&bundle)),
            document,
            shadow,
            assets: Vec::new(),
        })
    }

    fn fetch_database_schema_yaml(
        &self,
        _database_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

fn remote_version(bundle: &SlackNativeBundle) -> String {
    match bundle.kind {
        SlackRenderedKind::Users => "users".to_string(),
        SlackRenderedKind::Recent => bundle
            .messages
            .iter()
            .map(|message| message.ts.as_str())
            .max()
            .unwrap_or("empty")
            .to_string(),
    }
}

pub(crate) fn validate_slack_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    report.push(ValidationIssue::new(
        "slack_read_only",
        context.relative_path,
        Some(1),
        "Slack conversations are read-only",
        Some("do not edit files under Slack mounts".to_string()),
    ));
    Ok(report)
}
