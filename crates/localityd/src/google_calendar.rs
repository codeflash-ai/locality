use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::oauth_broker::OAuthBrokerRefresh;
use locality_connector::{Connector, FetchRequest};
use locality_core::canonical::parse_canonical_markdown;
use locality_core::diff::property_value_from_frontmatter;
use locality_core::hydration::HydrationRequest;
use locality_core::model::RemoteId;
use locality_core::planner::PropertyValue;
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_google_calendar::oauth::GoogleCalendarOAuthScopeError;
use locality_google_calendar::{
    GOOGLE_CALENDAR_CONNECTOR_ID, GoogleCalendarConfig, GoogleCalendarConnector,
    GoogleCalendarMountSettings, HttpGoogleCalendarOAuthBrokerClient,
    StoredGoogleCalendarCredential,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

pub const GOOGLE_CALENDAR_CONNECT_COMMAND: &str = "loc connect google-calendar";

pub fn resolve_google_calendar_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<GoogleCalendarConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != GOOGLE_CALENDAR_CONNECTOR_ID {
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
                suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
            })?;
        validate_connection_profile(store, &connection)?;
        return connector_from_connection(credentials, &connection, mount);
    }

    let active = active_google_calendar_connections(store)?;
    if active.len() == 1 {
        validate_connection_profile(store, &active[0])?;
        return connector_from_connection(credentials, &active[0], mount);
    }

    let message = if active.is_empty() {
        "missing Google Calendar connection; run `loc connect google-calendar`".to_string()
    } else {
        "mount has no connection_id and multiple Google Calendar connections exist".to_string()
    };
    Err(ConnectorResolveError::MissingConnection {
        message,
        suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
    })
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
    mount: &MountConfig,
) -> Result<GoogleCalendarConnector, ConnectorResolveError> {
    if connection.connector != GOOGLE_CALENDAR_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            connection.connector.clone(),
        ));
    }

    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
        });
    }

    if connection.auth_kind != "oauth" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some(format!(
                "Google Calendar connection `{}` must use OAuth credentials",
                connection.connection_id.0
            )),
            suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
        });
    }

    let token = connection_access_token(credentials, connection)?;
    Ok(GoogleCalendarConnector::new(
        google_calendar_config_from_mount(token, mount)?,
    ))
}

fn google_calendar_config_from_mount(
    token: String,
    mount: &MountConfig,
) -> Result<GoogleCalendarConfig, ConnectorResolveError> {
    let settings =
        GoogleCalendarMountSettings::from_json(&mount.settings_json).map_err(|error| {
            ConnectorResolveError::CredentialStoreUnavailable(format!(
                "Google Calendar mount `{}` settings are invalid: {}",
                mount.mount_id.0,
                google_calendar_settings_error_message(error)
            ))
        })?;
    Ok(GoogleCalendarConfig::new(token).with_settings(settings))
}

fn google_calendar_settings_error_message(error: LocalityError) -> String {
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
    let mut stored = serde_json::from_str::<StoredGoogleCalendarCredential>(&secret)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    if stored.expires_soon(timestamp_secs()) {
        let refreshed = refresh_oauth_credential(connection, &stored)?;
        stored = stored
            .refreshed(refreshed, timestamp_secs())
            .map_err(|error| google_calendar_refresh_scope_error(connection, error))?;
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
    stored: &StoredGoogleCalendarCredential,
) -> Result<locality_connector::oauth_broker::OAuthBrokerToken, ConnectorResolveError> {
    let Some(refresh_token_handle) = stored.refresh_token_handle.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
        });
    };
    let Some(broker_url) = stored.oauth_broker_url.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
        });
    };

    HttpGoogleCalendarOAuthBrokerClient::new(broker_url.clone())
        .refresh_token(&OAuthBrokerRefresh {
            connector: GOOGLE_CALENDAR_CONNECTOR_ID.to_string(),
            refresh_token_handle: Some(refresh_token_handle),
        })
        .map_err(|error| google_calendar_refresh_error(connection, &broker_url, error))
}

fn google_calendar_refresh_scope_error(
    connection: &ConnectionRecord,
    error: GoogleCalendarOAuthScopeError,
) -> ConnectorResolveError {
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Google Calendar credential for connection `{}` could not be refreshed through OAuth broker: {error}",
            connection.connection_id.0
        )),
        suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
    }
}

fn google_calendar_refresh_error(
    connection: &ConnectionRecord,
    broker_url: &str,
    error: LocalityError,
) -> ConnectorResolveError {
    let hint = if is_loopback_broker_url(broker_url) {
        "reconnect with the default hosted broker or keep the local broker running"
    } else {
        "reconnect to issue a fresh Google Calendar refresh handle"
    };
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Google Calendar credential for connection `{}` could not be refreshed through OAuth broker at `{broker_url}`: {error}; {hint}",
            connection.connection_id.0
        )),
        suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
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

fn active_google_calendar_connections<S>(
    store: &S,
) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository + ?Sized,
{
    let connections = store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    Ok(connections
        .into_iter()
        .filter(|connection| {
            connection.connector == GOOGLE_CALENDAR_CONNECTOR_ID
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
            suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
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
            suggested_command: GOOGLE_CALENDAR_CONNECT_COMMAND.to_string(),
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

impl SourcePushValidator for GoogleCalendarConnector {
    fn validate_changed_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_google_calendar_changed_frontmatter(context)
    }

    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_google_calendar_create_frontmatter(context)
    }
}

impl SourceAdapter for GoogleCalendarConnector {}

pub(crate) fn validate_google_calendar_changed_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    if first_path_component(context.relative_path) == Some("events") {
        report.push(ValidationIssue::new(
            "google_calendar_events_read_only",
            context.relative_path,
            Some(1),
            "Google Calendar event files are read-only",
            Some("create a new Markdown file directly under draft/ to create an event".to_string()),
        ));
    }
    Ok(report)
}

pub(crate) fn validate_google_calendar_create_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();

    if !is_direct_draft_child(context.relative_path) {
        report.push(ValidationIssue::new(
            "google_calendar_create_outside_draft",
            context.relative_path,
            Some(1),
            "Google Calendar creates are only supported directly inside draft/",
            Some("move the new event Markdown file directly under draft/".to_string()),
        ));
    }

    if !frontmatter_property_non_empty(&context.parsed.frontmatter.properties, "start") {
        report.push(ValidationIssue::new(
            "google_calendar_draft_missing_start",
            context.relative_path,
            Some(1),
            "Google Calendar draft requires `start` frontmatter",
            Some("add start frontmatter with date or dateTime".to_string()),
        ));
    }

    if !frontmatter_property_non_empty(&context.parsed.frontmatter.properties, "end") {
        report.push(ValidationIssue::new(
            "google_calendar_draft_missing_end",
            context.relative_path,
            Some(1),
            "Google Calendar draft requires `end` frontmatter",
            Some("add end frontmatter with date or dateTime".to_string()),
        ));
    }

    let has_summary = frontmatter_string(&context.parsed.frontmatter.properties, "summary")
        .as_deref()
        .is_some_and(|summary| !summary.trim().is_empty())
        || context
            .parsed
            .frontmatter
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty());
    if !has_summary {
        report.push(ValidationIssue::new(
            "google_calendar_draft_missing_summary",
            context.relative_path,
            Some(1),
            "Google Calendar draft requires `summary` or `title` frontmatter",
            Some("add `summary: \"Event title\"` or a non-empty `title`".to_string()),
        ));
    }

    Ok(report)
}

fn frontmatter_property_non_empty(
    properties: &locality_core::canonical::FrontmatterProperties,
    key: &str,
) -> bool {
    properties
        .get(key)
        .map(property_value_from_frontmatter)
        .is_some_and(property_value_non_empty)
}

fn property_value_non_empty(value: PropertyValue) -> bool {
    match value {
        PropertyValue::Null => false,
        PropertyValue::String(value) | PropertyValue::Number(value) => !value.trim().is_empty(),
        PropertyValue::List(values) => values.iter().any(|value| !value.trim().is_empty()),
        PropertyValue::Array(values) => values.into_iter().any(property_value_non_empty),
        PropertyValue::Object(values) => !values.is_empty(),
        PropertyValue::Bool(_) => true,
    }
}

fn frontmatter_string(
    properties: &locality_core::canonical::FrontmatterProperties,
    key: &str,
) -> Option<String> {
    properties
        .get(key)
        .map(property_value_from_frontmatter)
        .and_then(|value| match value {
            PropertyValue::String(value) => Some(value),
            _ => None,
        })
}

fn first_path_component(path: &Path) -> Option<&str> {
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

impl HydrationSource for GoogleCalendarConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let document = self.render(&native)?;
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
        let frontmatter = if document.frontmatter.ends_with('\n') {
            document.frontmatter.clone()
        } else {
            format!("{}\n", document.frontmatter)
        };
        let remote_edited_at =
            parse_canonical_markdown(&format!("---\n{frontmatter}---\n{}", document.body))
                .ok()
                .and_then(|parsed| parsed.frontmatter.loc.and_then(|loc| loc.remote_edited_at));
        Ok(HydratedEntity {
            document,
            shadow,
            remote_edited_at,
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
