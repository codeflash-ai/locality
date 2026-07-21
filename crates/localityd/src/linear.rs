use std::collections::BTreeSet;

use locality_connector::{Connector, FetchRequest};
use locality_core::canonical::{
    Frontmatter, LocalityMetadata, ParsedCanonicalDocument, parse_canonical_markdown,
    render_canonical_markdown,
};
use locality_core::hydration::HydrationRequest;
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::{ShadowDocument, rendered_bodies_equivalent, segment_markdown_body};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_linear::{
    LINEAR_CONNECTOR_ID, LinearConfig, LinearConnector, LinearNativeBundle, remote_version,
    render_linear_issue,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig,
};

use crate::hydration::{HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

pub(crate) const LINEAR_CONNECT_COMMAND: &str = "loc connect linear --api-key-stdin";

pub fn resolve_linear_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<LinearConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
{
    if mount.connector != LINEAR_CONNECTOR_ID {
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
                suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
            })?
    } else {
        let active = active_connections(store)?;
        if active.len() != 1 {
            let message = if active.is_empty() {
                "missing Linear connection; run `loc connect linear --api-key-stdin`".to_string()
            } else {
                "mount has no connection_id and multiple Linear connections exist".to_string()
            };
            return Err(ConnectorResolveError::MissingConnection {
                message,
                suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
            });
        }
        active.into_iter().next().expect("one active connection")
    };

    validate_connection_record(&connection)?;
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
            connection.connector == LINEAR_CONNECTOR_ID
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
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != LINEAR_CONNECTOR_ID
        || profile.auth_kind != "api_key"
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    Ok(())
}

fn validate_connection_record(connection: &ConnectionRecord) -> Result<(), ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    if connection.connector != LINEAR_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            connection.connector.clone(),
        ));
    }
    if connection.auth_kind != "api_key" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some("Linear connections require an API key".to_string()),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    Ok(())
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
) -> Result<LinearConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    if connection.connector != LINEAR_CONNECTOR_ID {
        return Err(ConnectorResolveError::UnsupportedConnector(
            connection.connector.clone(),
        ));
    }
    if connection.auth_kind != "api_key" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some("Linear connections require an API key".to_string()),
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        });
    }
    let token = credentials
        .get(&connection.secret_ref)
        .map_err(|error| credential_error(connection, error))?;
    Ok(LinearConnector::new(LinearConfig::new(token)))
}

fn credential_error(
    connection: &ConnectionRecord,
    error: CredentialError,
) -> ConnectorResolveError {
    match error {
        CredentialError::NotFound(_) => ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: None,
            suggested_command: LINEAR_CONNECT_COMMAND.to_string(),
        },
        CredentialError::Unavailable(message) | CredentialError::Io(message) => {
            ConnectorResolveError::CredentialStoreUnavailable(message)
        }
    }
}

impl SourcePushValidator for LinearConnector {}
impl SourceAdapter for LinearConnector {}

impl HydrationSource for LinearConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<LinearNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("Linear native decode failed: {error}")))?;
        let document = render_linear_issue(&bundle.issue)?;
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
            remote_edited_at: Some(remote_version(&bundle.issue)),
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

const LINEAR_LIFECYCLE_FRONTMATTER_KEYS: &[&str] = &[
    "created_at",
    "updated_at",
    "archived_at",
    "started_at",
    "completed_at",
    "canceled_at",
    "auto_archived_at",
    "auto_closed_at",
    "started_triage_at",
    "triaged_at",
    "snoozed_until_at",
    "added_to_cycle_at",
    "added_to_project_at",
    "added_to_team_at",
    "due_date",
];

pub(crate) fn linear_shadow_matches_with_legacy_lifecycle_frontmatter(
    synced_tree_shadow: &ShadowDocument,
    remote_tree_shadow: &ShadowDocument,
) -> bool {
    if !rendered_bodies_equivalent(
        &synced_tree_shadow.rendered_body,
        &remote_tree_shadow.rendered_body,
    ) {
        return false;
    }
    let Some(synced) = parse_shadow_frontmatter(synced_tree_shadow) else {
        return false;
    };
    let Some(remote) = parse_shadow_frontmatter(remote_tree_shadow) else {
        return false;
    };
    if !loc_metadata_matches_ignoring_sync_metadata(&synced.frontmatter, &remote.frontmatter)
        || synced.frontmatter.title != remote.frontmatter.title
    {
        return false;
    }

    let lifecycle_keys = LINEAR_LIFECYCLE_FRONTMATTER_KEYS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut repaired_missing_lifecycle_key = false;
    for key in synced
        .frontmatter
        .properties
        .keys()
        .chain(remote.frontmatter.properties.keys())
        .collect::<BTreeSet<_>>()
    {
        let synced_value = synced.frontmatter.properties.get(key);
        let remote_value = remote.frontmatter.properties.get(key);
        if lifecycle_keys.contains(key.as_str()) {
            if synced_value.is_none() && remote_value.is_some() {
                repaired_missing_lifecycle_key = true;
                continue;
            }
            if synced_value == remote_value {
                continue;
            }
            return false;
        }
        if synced_value != remote_value {
            return false;
        }
    }

    repaired_missing_lifecycle_key
}

fn parse_shadow_frontmatter(shadow: &ShadowDocument) -> Option<ParsedCanonicalDocument> {
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    )))
    .ok()
}

fn loc_metadata_matches_ignoring_sync_metadata(left: &Frontmatter, right: &Frontmatter) -> bool {
    match (&left.loc, &right.loc) {
        (None, None) => true,
        (Some(left), Some(right)) => locality_metadata_matches_ignoring_sync_metadata(left, right),
        _ => false,
    }
}

fn locality_metadata_matches_ignoring_sync_metadata(
    left: &LocalityMetadata,
    right: &LocalityMetadata,
) -> bool {
    left.id == right.id
        && left.entity_type == right.entity_type
        && left.raw_entity_type == right.raw_entity_type
        && left.parent == right.parent
}

pub(crate) fn validate_linear_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let Some(shadow) = context.shadow else {
        return Ok(ValidationReport::clean());
    };
    let mut shadow_markdown = String::from("---\n");
    shadow_markdown.push_str(&shadow.frontmatter);
    if !shadow_markdown.ends_with('\n') {
        shadow_markdown.push('\n');
    }
    shadow_markdown.push_str("---\n");
    let shadow_parsed = parse_canonical_markdown(&shadow_markdown).map_err(|error| {
        LocalityError::InvalidState(format!(
            "synced Linear shadow frontmatter is no longer parseable: {error}"
        ))
    })?;
    let mut report = ValidationReport::clean();
    for key in shadow_parsed
        .frontmatter
        .properties
        .keys()
        .chain(context.parsed.frontmatter.properties.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if matches!(key.as_str(), "Status" | "Project" | "Assignee") {
            continue;
        }
        let synced = shadow_parsed.frontmatter.properties.get(key);
        let edited = context.parsed.frontmatter.properties.get(key);
        if synced != edited {
            report.push(ValidationIssue::new(
                "linear_read_only_frontmatter",
                context.relative_path,
                Some(1),
                format!("Linear frontmatter `{key}` is read-only"),
                Some(format!("restore generated Linear `{key}` frontmatter")),
            ));
        }
    }
    Ok(report)
}

pub(crate) fn validate_linear_create_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    let mut report = ValidationReport::clean();
    report.push(ValidationIssue::new(
        "linear_create_unsupported",
        context.relative_path,
        Some(1),
        "Linear issue creates are not supported yet",
        Some("create the Linear issue remotely, then refresh the mount".to_string()),
    ));
    Ok(report)
}
