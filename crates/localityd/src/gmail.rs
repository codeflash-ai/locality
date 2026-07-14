use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::oauth_broker::OAuthBrokerRefresh;
use locality_connector::{Connector, EnumerateRequest, FetchRequest};
use locality_core::hydration::HydrationRequest;
use locality_core::model::{RemoteId, TreeEntry};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_gmail::render::{GmailNativeBundle, remote_version, render_gmail_message};
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

const GMAIL_CONNECT_COMMAND: &str = "loc connect gmail";

pub fn resolve_gmail_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<GmailConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != GMAIL_CONNECTOR_ID {
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
                suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
            })?;
        validate_connection_profile(store, &connection)?;
        return connector_from_connection(credentials, &connection);
    }

    let active = active_gmail_connections(store)?;
    if active.len() == 1 {
        validate_connection_profile(store, &active[0])?;
        return connector_from_connection(credentials, &active[0]);
    }

    let message = if active.is_empty() {
        "missing Gmail connection; run `loc connect gmail`".to_string()
    } else {
        "mount has no connection_id and multiple Gmail connections exist".to_string()
    };
    Err(ConnectorResolveError::MissingConnection {
        message,
        suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
    })
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<GmailConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
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
    if connection.auth_kind != "oauth" {
        return Ok(secret);
    }

    let mut stored = serde_json::from_str::<StoredGmailCredential>(&secret)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    if stored.expires_soon(timestamp_secs()) {
        let refreshed = refresh_oauth_credential(connection, &stored)?;
        stored = stored.refreshed(refreshed, timestamp_secs());
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
    stored: &StoredGmailCredential,
) -> Result<locality_connector::oauth_broker::OAuthBrokerToken, ConnectorResolveError> {
    let Some(refresh_token_handle) = stored.refresh_token_handle.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        });
    };
    let Some(broker_url) = stored.oauth_broker_url.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        });
    };

    HttpGmailOAuthBrokerClient::new(broker_url.clone())
        .refresh_token(&OAuthBrokerRefresh {
            connector: GMAIL_CONNECTOR_ID.to_string(),
            refresh_token_handle: Some(refresh_token_handle),
        })
        .map_err(|error| gmail_refresh_error(connection, &broker_url, error))
}

fn gmail_refresh_error(
    connection: &ConnectionRecord,
    broker_url: &str,
    error: LocalityError,
) -> ConnectorResolveError {
    let hint = if is_loopback_broker_url(broker_url) {
        "reconnect with the default hosted broker or keep the local broker running"
    } else {
        "reconnect to issue a fresh Gmail refresh handle"
    };
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Gmail credential for connection `{}` could not be refreshed through OAuth broker at `{broker_url}`: {error}; {hint}",
            connection.connection_id.0
        )),
        suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
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

fn active_gmail_connections<S>(store: &S) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository + ?Sized,
{
    let connections = store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    Ok(connections
        .into_iter()
        .filter(|connection| {
            connection.connector == GMAIL_CONNECTOR_ID && connection.status == "active"
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
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
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
            suggested_command: GMAIL_CONNECT_COMMAND.to_string(),
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

impl SourceAdapter for GmailConnector {}

pub(crate) fn validate_gmail_changed_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    if gmail_mailbox_from_path(context.relative_path)
        .is_some_and(|mailbox| matches!(mailbox, "inbox" | "sent"))
    {
        report.push(ValidationIssue::new(
            "gmail_read_only_mailbox",
            context.relative_path,
            Some(1),
            "Gmail inbox and sent items are read-only",
            Some("create a new Markdown file directly under draft/ to send mail".to_string()),
        ));
    }
    Ok(report)
}

pub(crate) fn validate_gmail_create_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();

    if !is_direct_draft_child(context.relative_path) {
        report.push(ValidationIssue::new(
            "gmail_create_outside_draft",
            context.relative_path,
            Some(1),
            "Gmail creates are only supported directly inside draft/",
            Some("move the new email Markdown file directly under draft/".to_string()),
        ));
    }

    let has_subject = context
        .parsed
        .frontmatter
        .properties
        .contains_key("subject")
        || context
            .parsed
            .frontmatter
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty());
    if !has_subject {
        report.push(ValidationIssue::new(
            "gmail_draft_missing_subject",
            context.relative_path,
            Some(1),
            "Gmail draft requires `subject` or `title` frontmatter",
            Some("add `subject: \"Subject text\"` or a non-empty `title`".to_string()),
        ));
    }

    if !context.parsed.frontmatter.properties.contains_key("to") {
        report.push(ValidationIssue::new(
            "gmail_draft_missing_to",
            context.relative_path,
            Some(1),
            "Gmail draft requires `to` frontmatter",
            Some("add `to: [\"name@example.com\"]` to the frontmatter".to_string()),
        ));
    }

    Ok(report)
}

fn gmail_mailbox_from_path(path: &Path) -> Option<&str> {
    path.components()
        .next()
        .and_then(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
}

fn is_direct_draft_child(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::Normal(component)) if component == "draft"
    ) && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

impl HydrationSource for GmailConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<GmailNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("gmail native decode failed: {error}")))?;
        let rendered = render_gmail_message(&bundle)?;
        Ok(HydratedEntity {
            document: rendered.document,
            shadow: rendered.shadow,
            remote_edited_at: Some(remote_version(&bundle.message)),
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

impl crate::reconcile::ScheduledPullSource for GmailConnector {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        self.enumerate(EnumerateRequest {
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
