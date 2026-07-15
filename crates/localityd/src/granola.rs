use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{TimeDelta, TimeZone, Utc};
use locality_connector::{Connector, FetchRequest};
use locality_core::hydration::HydrationRequest;
use locality_core::model::RemoteId;
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::validation::ValidationReport;
use locality_core::{LocalityError, LocalityResult};
use locality_granola::{
    GRANOLA_CONNECTOR_ID, GranolaConfig, GranolaConnector, GranolaNativeBundle, remote_version,
    render_granola_note,
};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, ConnectorStateRecord,
    ConnectorStateRepository, CredentialError, CredentialStore, MountConfig,
};
use serde::{Deserialize, Serialize};

use crate::hydration::{HydratedEntity, HydrationSource};
use crate::notion::ConnectorResolveError;
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};

const GRANOLA_CONNECT_COMMAND: &str = "loc connect granola --api-key-stdin";
const GRANOLA_DISCOVERY_STATE_VERSION: i64 = 1;
const GRANOLA_DISCOVERY_SCOPE_KIND: &str = "mount";
const DEFAULT_GRANOLA_DISCOVERY_OVERLAP_DAYS: i64 = 2;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct GranolaDiscoveryState {
    last_success_unix_ms: Option<u64>,
}

pub fn resolve_granola_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<GranolaConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository + ConnectorStateRepository + ?Sized,
{
    if mount.connector != GRANOLA_CONNECTOR_ID {
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
                suggested_command: GRANOLA_CONNECT_COMMAND.to_string(),
            })?
    } else {
        let active = active_connections(store)?;
        if active.len() != 1 {
            let message = if active.is_empty() {
                "missing Granola connection; run `loc connect granola --api-key-stdin`".to_string()
            } else {
                "mount has no connection_id and multiple Granola connections exist".to_string()
            };
            return Err(ConnectorResolveError::MissingConnection {
                message,
                suggested_command: GRANOLA_CONNECT_COMMAND.to_string(),
            });
        }
        active.into_iter().next().expect("one active connection")
    };

    validate_profile(store, &connection)?;
    let updated_after = granola_updated_after(store, mount)?;
    connector_from_connection(credentials, &connection, updated_after)
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
            connection.connector == GRANOLA_CONNECTOR_ID
                && connection.status == "active"
                && connection.auth_kind == "api_key"
        })
        .collect())
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
    updated_after: Option<String>,
) -> Result<GranolaConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: GRANOLA_CONNECT_COMMAND.to_string(),
        });
    }
    if connection.connector != GRANOLA_CONNECTOR_ID || connection.auth_kind != "api_key" {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            message: Some("Granola connections require an API key".to_string()),
            suggested_command: GRANOLA_CONNECT_COMMAND.to_string(),
        });
    }
    let key = credentials
        .get(&connection.secret_ref)
        .map_err(|error| credential_error(connection, error))?;
    let config = updated_after
        .map(|updated_after| GranolaConfig::new(key.clone()).with_updated_after(updated_after))
        .unwrap_or_else(|| GranolaConfig::new(key));
    Ok(GranolaConnector::new(config))
}

fn granola_updated_after<S>(
    store: &S,
    mount: &MountConfig,
) -> Result<Option<String>, ConnectorResolveError>
where
    S: ConnectorStateRepository + ?Sized,
{
    let Some(record) = store
        .get_connector_state(
            GRANOLA_CONNECTOR_ID,
            GRANOLA_DISCOVERY_SCOPE_KIND,
            mount.mount_id.as_str(),
        )
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
    else {
        return Ok(None);
    };
    if record.state_version > GRANOLA_DISCOVERY_STATE_VERSION
        || record.min_reader_version > GRANOLA_DISCOVERY_STATE_VERSION
    {
        return Err(ConnectorResolveError::CredentialStoreUnavailable(format!(
            "Granola discovery state for mount `{}` requires a newer Locality version",
            mount.mount_id.0
        )));
    }
    let state =
        serde_json::from_str::<GranolaDiscoveryState>(&record.state_json).map_err(|error| {
            ConnectorResolveError::CredentialStoreUnavailable(format!(
                "Granola discovery state for mount `{}` is invalid: {error}",
                mount.mount_id.0
            ))
        })?;
    let Some(last_success_unix_ms) = state.last_success_unix_ms else {
        return Ok(None);
    };
    let last_success = Utc
        .timestamp_millis_opt(i64::try_from(last_success_unix_ms).unwrap_or(i64::MAX))
        .single()
        .ok_or_else(|| {
            ConnectorResolveError::CredentialStoreUnavailable(format!(
                "Granola discovery state for mount `{}` has an invalid timestamp",
                mount.mount_id.0
            ))
        })?;
    let updated_after = last_success - TimeDelta::days(DEFAULT_GRANOLA_DISCOVERY_OVERLAP_DAYS);
    Ok(Some(updated_after.format("%Y-%m-%d").to_string()))
}

pub fn record_granola_discovery_success<S>(store: &mut S, mount: &MountConfig) -> LocalityResult<()>
where
    S: ConnectorStateRepository + ?Sized,
{
    let last_success_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let state_json = serde_json::to_string(&GranolaDiscoveryState {
        last_success_unix_ms: Some(last_success_unix_ms),
    })
    .map_err(|error| {
        LocalityError::Io(format!("Granola discovery state encode failed: {error}"))
    })?;
    store
        .save_connector_state(ConnectorStateRecord {
            connector: GRANOLA_CONNECTOR_ID.to_string(),
            scope_kind: GRANOLA_DISCOVERY_SCOPE_KIND.to_string(),
            scope_id: mount.mount_id.0.clone(),
            state_version: GRANOLA_DISCOVERY_STATE_VERSION,
            min_reader_version: 1,
            state_json,
            updated_at: format!("unix_ms:{last_success_unix_ms}"),
        })
        .map_err(LocalityError::from)
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
            suggested_command: GRANOLA_CONNECT_COMMAND.to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != GRANOLA_CONNECTOR_ID
        || profile.auth_kind != "api_key"
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: GRANOLA_CONNECT_COMMAND.to_string(),
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
            suggested_command: GRANOLA_CONNECT_COMMAND.to_string(),
        },
        CredentialError::Unavailable(message) | CredentialError::Io(message) => {
            ConnectorResolveError::CredentialStoreUnavailable(message)
        }
    }
}

impl SourcePushValidator for GranolaConnector {}
impl SourceAdapter for GranolaConnector {}

impl HydrationSource for GranolaConnector {
    fn fetch_render(&self, request: &HydrationRequest) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let bundle = serde_json::from_slice::<GranolaNativeBundle>(&native.raw)
            .map_err(|error| LocalityError::Io(format!("Granola native decode failed: {error}")))?;
        let document = render_granola_note(&bundle)?;
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
            remote_edited_at: Some(remote_version(&bundle.note)),
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

pub(crate) fn validate_granola_frontmatter(
    _context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    Ok(ValidationReport::clean())
}

#[cfg(test)]
mod tests {
    use locality_core::model::MountId;
    use locality_store::{
        ConnectorStateRecord, ConnectorStateRepository, InMemoryStateStore, MountConfig,
    };

    use super::{
        GRANOLA_CONNECTOR_ID, GRANOLA_DISCOVERY_SCOPE_KIND, GranolaDiscoveryState,
        granola_updated_after, record_granola_discovery_success,
    };

    #[test]
    fn discovery_state_uses_an_overlapping_date_window() {
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(
            MountId::new("granola-main"),
            GRANOLA_CONNECTOR_ID,
            "/tmp/granola",
        );
        let timestamp = 1_783_987_200_000_u64; // 2026-07-14T00:00:00Z
        store
            .save_connector_state(ConnectorStateRecord {
                connector: GRANOLA_CONNECTOR_ID.to_string(),
                scope_kind: GRANOLA_DISCOVERY_SCOPE_KIND.to_string(),
                scope_id: mount.mount_id.0.clone(),
                state_version: 1,
                min_reader_version: 1,
                state_json: serde_json::to_string(&GranolaDiscoveryState {
                    last_success_unix_ms: Some(timestamp),
                })
                .expect("state json"),
                updated_at: format!("unix_ms:{timestamp}"),
            })
            .expect("save state");

        assert_eq!(
            granola_updated_after(&store, &mount).expect("updated after"),
            Some("2026-07-12".to_string())
        );
    }

    #[test]
    fn successful_discovery_persists_versioned_mount_state() {
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(
            MountId::new("granola-main"),
            GRANOLA_CONNECTOR_ID,
            "/tmp/granola",
        );

        record_granola_discovery_success(&mut store, &mount).expect("record discovery");

        let state = store
            .get_connector_state(
                GRANOLA_CONNECTOR_ID,
                GRANOLA_DISCOVERY_SCOPE_KIND,
                mount.mount_id.as_str(),
            )
            .expect("load state")
            .expect("state exists");
        assert_eq!(state.state_version, 1);
        assert_eq!(state.min_reader_version, 1);
        assert!(state.state_json.contains("last_success_unix_ms"));
    }
}
