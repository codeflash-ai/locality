use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::oauth_broker::OAuthBrokerRefresh;
use locality_connector::{Connector, FetchRequest};
use locality_core::hydration::HydrationRequest;
use locality_core::model::{RemoteId, TreeEntry};
use locality_core::validation::ValidationReport;
use locality_core::{LocalityError, LocalityResult};
use locality_google_docs::{
    GOOGLE_DOCS_CONNECTOR_ID, GoogleDocsConfig, GoogleDocsConnector,
    HttpGoogleDocsOAuthBrokerClient, StoredGoogleDocsCredential,
    render::{
        GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND, GOOGLE_DOCS_TABLE_NATIVE_KIND,
        render_comments_sidecar,
    },
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedAsset, HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

pub fn resolve_google_docs_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<GoogleDocsConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != GOOGLE_DOCS_CONNECTOR_ID {
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
                suggested_command: "loc connect google-docs".to_string(),
            })?;
        validate_connection_profile(store, &connection)?;
        return connector_from_connection(credentials, mount, &connection);
    }

    let active = active_google_docs_connections(store)?;
    if active.len() == 1 {
        validate_connection_profile(store, &active[0])?;
        return connector_from_connection(credentials, mount, &active[0]);
    }

    let message = if active.is_empty() {
        "missing Google Docs connection; run `loc connect google-docs`".to_string()
    } else {
        "mount has no connection_id and multiple Google Docs connections exist".to_string()
    };
    Err(ConnectorResolveError::MissingConnection {
        message,
        suggested_command: "loc connect google-docs".to_string(),
    })
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
    connection: &ConnectionRecord,
) -> Result<GoogleDocsConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: "loc connect google-docs".to_string(),
        });
    }

    let token = connection_access_token(credentials, connection)?;
    let mut config = GoogleDocsConfig::new(token);
    if let Some(remote_root_id) = &mount.remote_root_id {
        config = config.with_workspace_folder_id(remote_root_id.clone());
    }
    Ok(GoogleDocsConnector::new(config))
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

    let mut stored = serde_json::from_str::<StoredGoogleDocsCredential>(&secret)
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
    stored: &StoredGoogleDocsCredential,
) -> Result<locality_connector::oauth_broker::OAuthBrokerToken, ConnectorResolveError> {
    let Some(refresh_token_handle) = stored.refresh_token_handle.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: "loc connect google-docs".to_string(),
        });
    };
    let Some(broker_url) = stored.oauth_broker_url.clone() else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: "loc connect google-docs".to_string(),
        });
    };

    HttpGoogleDocsOAuthBrokerClient::new(broker_url.clone())
        .refresh_token(&OAuthBrokerRefresh {
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            refresh_token_handle: Some(refresh_token_handle),
        })
        .map_err(|error| google_docs_refresh_error(connection, &broker_url, error))
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

fn google_docs_refresh_error(
    connection: &ConnectionRecord,
    broker_url: &str,
    error: LocalityError,
) -> ConnectorResolveError {
    let hint = if is_loopback_broker_url(broker_url) {
        "reconnect with the default hosted broker or keep the local broker running"
    } else {
        "reconnect to issue a fresh Google Docs refresh handle"
    };
    ConnectorResolveError::AuthRequired {
        connection_id: connection.connection_id.0.clone(),
        message: Some(format!(
            "Google Docs credential for connection `{}` could not be refreshed through OAuth broker at `{broker_url}`: {error}; {hint}",
            connection.connection_id.0
        )),
        suggested_command: "loc connect google-docs".to_string(),
    }
}

fn active_google_docs_connections<S>(
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
            connection.connector == GOOGLE_DOCS_CONNECTOR_ID && connection.status == "active"
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
            suggested_command: "loc connect google-docs".to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: "loc connect google-docs".to_string(),
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
            suggested_command: "loc connect google-docs".to_string(),
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

impl SourcePushValidator for GoogleDocsConnector {
    fn validate_changed_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_google_docs_frontmatter(context)
    }

    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_google_docs_frontmatter(context)
    }
}

impl SourceAdapter for GoogleDocsConnector {
    fn scoped_to_mount(&self, mount: &MountConfig) -> Self
    where
        Self: Sized + Clone,
    {
        mount
            .remote_root_id
            .as_ref()
            .map(|root| self.with_workspace_folder_id(root.clone()))
            .unwrap_or_else(|| self.clone())
    }
}

pub(crate) fn validate_google_docs_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    if let Some(loc) = context.parsed.frontmatter.loc.as_ref()
        && loc
            .raw_entity_type
            .as_deref()
            .is_some_and(|entity_type| entity_type != "page")
    {
        report.push(locality_core::validation::ValidationIssue::new(
            "google_docs_invalid_entity_type",
            context.relative_path,
            Some(1),
            "Google Docs files must keep `loc.type: page`".to_string(),
            Some("restore the Locality frontmatter identity fields".to_string()),
        ));
    }
    let mut reported_unsupported = false;
    for directive in &context.parsed.directives {
        if directive.directive_type.as_deref() == Some("google_docs_unsupported") {
            reported_unsupported = true;
            report.push(locality_core::validation::ValidationIssue::new(
                "google_docs_unsupported_document_structure",
                context.relative_path,
                Some(directive.line),
                "this Google Doc contains an unsupported structure that blocks push".to_string(),
                Some(
                    "leave the directive unchanged and edit supported content around it"
                        .to_string(),
                ),
            ));
        }
    }
    if !reported_unsupported
        && let Some(shadow_block) = context.shadow.and_then(|shadow| {
            shadow.blocks.iter().find(|block| {
                matches!(
                    &block.kind,
                    locality_core::shadow::MarkdownBlockKind::Directive {
                        directive_type: Some(directive_type),
                        ..
                    } if directive_type == "google_docs_unsupported"
                )
            })
        })
    {
        report.push(locality_core::validation::ValidationIssue::new(
            "google_docs_unsupported_document_structure",
            context.relative_path,
            Some(shadow_block.source_span.start_line),
            "this Google Doc contains an unsupported structure that blocks push".to_string(),
            Some(
                "restore the generated unsupported-structure directive before pushing".to_string(),
            ),
        ));
    }
    if let Some(shadow) = context.shadow {
        for block in shadow.blocks.iter().filter(|block| {
            block.native_kind.as_deref() == Some(GOOGLE_DOCS_TABLE_NATIVE_KIND)
                && !context.parsed.document.body.contains(&block.text)
        }) {
            report.push(locality_core::validation::ValidationIssue::new(
                "google_docs_table_edit_unsupported",
                context.relative_path,
                Some(block.source_span.start_line),
                "editing Google Docs tables is not supported yet".to_string(),
                Some(
                    "restore the rendered Markdown table or edit the table in Google Docs"
                        .to_string(),
                ),
            ));
        }
        for block in shadow.blocks.iter().filter(|block| {
            block.native_kind.as_deref() == Some(GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND)
                && !context.parsed.document.body.contains(&block.text)
        }) {
            report.push(locality_core::validation::ValidationIssue::new(
                "google_docs_inline_object_edit_unsupported",
                context.relative_path,
                Some(block.source_span.start_line),
                "editing rendered Google Docs inline images is not supported yet".to_string(),
                Some(
                    "restore the rendered image Markdown or edit the image in Google Docs"
                        .to_string(),
                ),
            ));
        }
    }
    Ok(report)
}

impl HydrationSource for GoogleDocsConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle =
            serde_json::from_slice::<locality_google_docs::render::GoogleDocsNativeBundle>(
                &native.raw,
            )
            .map_err(|error| {
                LocalityError::Io(format!("google docs native decode failed: {error}"))
            })?;
        let rendered = locality_google_docs::render::render_google_document(&bundle)?;
        let assets = render_comments_sidecar(&bundle)
            .map(|comments| {
                let path = request
                    .path
                    .parent()
                    .map(|parent| parent.join(".comments.md"))
                    .unwrap_or_else(|| std::path::PathBuf::from(".comments.md"));
                vec![HydratedAsset {
                    path,
                    bytes: comments.into_bytes(),
                    media: None,
                }]
            })
            .unwrap_or_default();
        Ok(HydratedEntity {
            document: rendered.document,
            shadow: rendered.shadow,
            remote_edited_at: Some(locality_google_docs::render::combined_remote_version(
                &bundle.drive_file,
                bundle.document.revision_id.as_deref(),
            )),
            assets,
        })
    }

    fn fetch_database_schema_yaml(
        &self,
        _database_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(None)
    }
}

impl crate::reconcile::ScheduledPullSource for GoogleDocsConnector {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        let connector = match &mount.remote_root_id {
            Some(root) => self.with_workspace_folder_id(root.clone()),
            None => self.clone(),
        };
        connector.enumerate(locality_connector::EnumerateRequest {
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
