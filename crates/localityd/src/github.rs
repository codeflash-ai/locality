use locality_connector::{Connector, FetchRequest};
use locality_core::hydration::HydrationRequest;
use locality_core::model::RemoteId;
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::validation::ValidationReport;
use locality_core::{LocalityError, LocalityResult};
use locality_github::{
    GITHUB_CONNECTOR_ID, GitHubConfig, GitHubConnector, GitHubNativeBundle, render_github_entity,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

pub(crate) const GITHUB_CONNECT_COMMAND: &str = "loc connect github --api-key-stdin";

pub fn resolve_github_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<GitHubConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != GITHUB_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            mount.connector.clone(),
        ));
    }

    let connection = if let Some(connection_id) = &mount.connection_id {
        store
            .get_connection(connection_id)
            .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
            .ok_or_else(|| ConnectorResolveError::MissingConnection {
                message: format!("connection `{}` was not found", connection_id.0),
                suggested_command: GITHUB_CONNECT_COMMAND.to_string(),
            })?
    } else {
        let active = active_connections(store)?;
        if active.len() != 1 {
            let message = if active.is_empty() {
                "missing GitHub connection; run `loc connect github --api-key-stdin`".to_string()
            } else {
                "mount has no connection_id and multiple GitHub connections exist".to_string()
            };
            return Err(ConnectorResolveError::MissingConnection {
                message,
                suggested_command: GITHUB_CONNECT_COMMAND.to_string(),
            });
        }
        active.into_iter().next().expect("one active connection")
    };

    validate_profile(store, &connection)?;
    connector_from_connection(credentials, &connection)
}

fn active_connections<S>(store: &S) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository + ?Sized,
{
    Ok(store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
        .into_iter()
        .filter(|connection| {
            connection.connector == GITHUB_CONNECTOR_ID
                && connection.status == "active"
                && connection.auth_kind == "api_key"
        })
        .collect())
}

fn validate_profile<S>(
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
            suggested_command: GITHUB_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != GITHUB_CONNECTOR_ID
        || profile.auth_kind != "api_key"
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: GITHUB_CONNECT_COMMAND.to_string(),
        });
    }
    Ok(())
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<GitHubConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: GITHUB_CONNECT_COMMAND.to_string(),
        });
    }
    if connection.connector != GITHUB_CONNECTOR_ID || connection.auth_kind != "api_key" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some("GitHub connections require a personal access token".to_string()),
            suggested_command: GITHUB_CONNECT_COMMAND.to_string(),
        });
    }
    let token = credentials
        .get(&connection.secret_ref)
        .map_err(|error| credential_error(connection, error))?;
    Ok(GitHubConnector::new(GitHubConfig::new(token)))
}

fn credential_error(
    connection: &ConnectionRecord,
    error: CredentialError,
) -> ConnectorResolveError {
    match error {
        CredentialError::NotFound(_) => ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: GITHUB_CONNECT_COMMAND.to_string(),
        },
        CredentialError::Unavailable(message) | CredentialError::Io(message) => {
            ConnectorResolveError::CredentialStoreUnavailable(message)
        }
    }
}

impl SourcePushValidator for GitHubConnector {}
impl SourceAdapter for GitHubConnector {}

impl HydrationSource for GitHubConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<GitHubNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("GitHub native decode failed: {error}")))?;
        let document = render_github_entity(&bundle)?;
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
            document,
            shadow,
            remote_edited_at: None,
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

pub fn validate_github_frontmatter(
    _context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    Ok(ValidationReport::clean())
}
