use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_connector::{Connector, FetchRequest};
use afs_core::model::MountId;
use afs_core::{AfsError, AfsResult};
use afs_notion::client::DEFAULT_NOTION_TOKEN_ENV;
use afs_notion::dto::NotionPageBundle;
use afs_notion::media::fetch_media_assets;
use afs_notion::oauth::{
    HttpNotionOAuthBrokerClient, HttpNotionOAuthClient, NotionOAuthBrokerRefresh,
    NotionOAuthRefresh, StoredNotionCredential,
};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, MountConfig, MountRepository,
};

use crate::hydration::{HydratedAsset, HydratedAssetMedia, HydratedEntity, HydrationSource};

static ENV_FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectorResolveError {
    MountMissing(String),
    UnsupportedConnector(String),
    MissingConnection {
        message: String,
        suggested_command: String,
    },
    AuthRequired {
        connection_id: String,
        suggested_command: String,
    },
    ConnectionRevoked {
        connection_id: String,
        suggested_command: String,
    },
    AuthProfileUnavailable {
        profile_id: String,
        suggested_command: String,
    },
    CredentialStoreUnavailable(String),
}

impl ConnectorResolveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MountMissing(_) => "mount_not_found",
            Self::UnsupportedConnector(_) => "unsupported_connector",
            Self::MissingConnection { .. } => "missing_connection",
            Self::AuthRequired { .. } => "auth_required",
            Self::ConnectionRevoked { .. } => "connection_revoked",
            Self::AuthProfileUnavailable { .. } => "auth_profile_unavailable",
            Self::CredentialStoreUnavailable(_) => "credential_store_unavailable",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::MountMissing(path) => format!("no mount contains `{path}`"),
            Self::UnsupportedConnector(connector) => {
                format!("connector `{connector}` is not supported by this build")
            }
            Self::MissingConnection { message, .. } => message.clone(),
            Self::AuthRequired { connection_id, .. } => {
                format!("credential for connection `{connection_id}` was not found")
            }
            Self::ConnectionRevoked { connection_id, .. } => {
                format!("connection `{connection_id}` is revoked")
            }
            Self::AuthProfileUnavailable { profile_id, .. } => {
                format!("connector profile `{profile_id}` is unavailable")
            }
            Self::CredentialStoreUnavailable(message) => message.clone(),
        }
    }

    pub fn suggested_command(&self) -> Option<&str> {
        match self {
            Self::MissingConnection {
                suggested_command, ..
            }
            | Self::AuthRequired {
                suggested_command, ..
            }
            | Self::ConnectionRevoked {
                suggested_command, ..
            }
            | Self::AuthProfileUnavailable {
                suggested_command, ..
            } => Some(suggested_command),
            _ => None,
        }
    }
}

impl From<ConnectorResolveError> for AfsError {
    fn from(value: ConnectorResolveError) -> Self {
        AfsError::InvalidState(value.message())
    }
}

pub fn resolve_notion_connector_for_path<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    path: impl AsRef<Path>,
) -> Result<NotionConnector, ConnectorResolveError>
where
    S: MountRepository + ConnectionRepository + ConnectorProfileRepository,
{
    let target = absolute_path(path.as_ref()).map_err(ConnectorResolveError::MountMissing)?;
    let mounts = store
        .load_mounts()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    let mount = find_mount_for_path(&mounts, &target)
        .ok_or_else(|| ConnectorResolveError::MountMissing(target.display().to_string()))?;
    resolve_notion_connector_for_mount(store, credentials, mount)
}

pub fn resolve_notion_connector_for_mount_id<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount_id: &MountId,
) -> Result<NotionConnector, ConnectorResolveError>
where
    S: MountRepository + ConnectionRepository + ConnectorProfileRepository,
{
    let mount = store
        .get_mount(mount_id)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
        .ok_or_else(|| ConnectorResolveError::MountMissing(mount_id.0.clone()))?;
    resolve_notion_connector_for_mount(store, credentials, &mount)
}

pub fn resolve_notion_connector_for_mount<S>(
    store: &S,
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
) -> Result<NotionConnector, ConnectorResolveError>
where
    S: ConnectionRepository + ConnectorProfileRepository,
{
    if mount.connector != "notion" {
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
                suggested_command: "afs connect notion".to_string(),
            })?;
        validate_connection_profile(store, &connection)?;
        return connector_from_connection(credentials, mount, &connection);
    }

    let active = active_notion_connections(store)?;
    if active.len() == 1 {
        validate_connection_profile(store, &active[0])?;
        return connector_from_connection(credentials, mount, &active[0]);
    }

    if std::env::var(DEFAULT_NOTION_TOKEN_ENV).is_ok() {
        warn_env_fallback_once();
        let config = NotionConfig {
            root_page_id: mount.remote_root_id.clone(),
            ..Default::default()
        };
        return Ok(NotionConnector::new(config));
    }

    let message = if active.is_empty() {
        "missing Notion connection; run `afs connect notion`".to_string()
    } else {
        "mount has no connection_id and multiple Notion connections exist".to_string()
    };
    Err(ConnectorResolveError::MissingConnection {
        message,
        suggested_command: "afs connect notion".to_string(),
    })
}

pub struct ResolvedNotionSource {
    connectors: BTreeMap<MountId, NotionConnector>,
}

impl ResolvedNotionSource {
    pub fn new<S>(
        store: &S,
        credentials: &dyn CredentialStore,
        mounts: &[MountConfig],
    ) -> Result<Self, ConnectorResolveError>
    where
        S: ConnectionRepository + ConnectorProfileRepository,
    {
        let mut connectors = BTreeMap::new();
        for mount in mounts {
            connectors.insert(
                mount.mount_id.clone(),
                resolve_notion_connector_for_mount(store, credentials, mount)?,
            );
        }
        Ok(Self { connectors })
    }
}

impl crate::reconcile::ScheduledPullSource for ResolvedNotionSource {
    fn enumerate_mount(&self, mount: &MountConfig) -> AfsResult<Vec<afs_core::model::TreeEntry>> {
        let connector = self.connectors.get(&mount.mount_id).ok_or_else(|| {
            AfsError::InvalidState(format!("mount `{}` was not resolved", mount.mount_id.0))
        })?;
        crate::reconcile::ScheduledPullSource::enumerate_mount(connector, mount)
    }

    fn database_schema_yaml(
        &self,
        mount: &MountConfig,
        remote_id: &afs_core::model::RemoteId,
    ) -> AfsResult<Option<String>> {
        let connector = self.connectors.get(&mount.mount_id).ok_or_else(|| {
            AfsError::InvalidState(format!("mount `{}` was not resolved", mount.mount_id.0))
        })?;
        crate::reconcile::ScheduledPullSource::database_schema_yaml(connector, mount, remote_id)
    }
}

fn connector_from_connection(
    credentials: &dyn CredentialStore,
    mount: &MountConfig,
    connection: &ConnectionRecord,
) -> Result<NotionConnector, ConnectorResolveError> {
    if connection.status != "active" {
        return Err(ConnectorResolveError::ConnectionRevoked {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: "afs connect notion".to_string(),
        });
    }

    let token = connection_access_token(credentials, connection)?;
    let config = NotionConfig {
        workspace_id: connection.workspace_id.clone(),
        root_page_id: mount.remote_root_id.clone(),
        token: Some(token),
        token_key: DEFAULT_NOTION_TOKEN_ENV.to_string(),
    };
    Ok(NotionConnector::new(config))
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

    let mut stored = serde_json::from_str::<StoredNotionCredential>(&secret)
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
    stored: &StoredNotionCredential,
) -> Result<afs_notion::oauth::NotionOAuthToken, ConnectorResolveError> {
    if let Some(broker_url) = stored.oauth_broker_url.clone() {
        if stored.refresh_token.is_none() && stored.refresh_token_handle.is_none() {
            return Err(ConnectorResolveError::AuthRequired {
                connection_id: connection.connection_id.0.clone(),
                suggested_command: "afs connect notion".to_string(),
            });
        }
        return HttpNotionOAuthBrokerClient::new(broker_url)
            .refresh_token(&NotionOAuthBrokerRefresh {
                refresh_token: stored.refresh_token.clone(),
                refresh_token_handle: stored.refresh_token_handle.clone(),
            })
            .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()));
    }

    let (Some(client_id), Some(client_secret), Some(refresh_token)) = (
        stored.oauth_client_id.clone(),
        stored.oauth_client_secret.clone(),
        stored.refresh_token.clone(),
    ) else {
        return Err(ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: "afs connect notion".to_string(),
        });
    };
    HttpNotionOAuthClient::new()
        .refresh_token(&NotionOAuthRefresh {
            client_id,
            client_secret,
            refresh_token,
        })
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))
}

fn credential_error(
    connection: &ConnectionRecord,
    error: CredentialError,
) -> ConnectorResolveError {
    match error {
        CredentialError::NotFound(_) => ConnectorResolveError::AuthRequired {
            connection_id: connection.connection_id.0.clone(),
            suggested_command: "afs connect notion".to_string(),
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

fn active_notion_connections<S>(store: &S) -> Result<Vec<ConnectionRecord>, ConnectorResolveError>
where
    S: ConnectionRepository,
{
    let connections = store
        .list_connections()
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?;
    Ok(connections
        .into_iter()
        .filter(|connection| connection.connector == "notion" && connection.status == "active")
        .collect())
}

fn validate_connection_profile<S>(
    store: &S,
    connection: &ConnectionRecord,
) -> Result<(), ConnectorResolveError>
where
    S: ConnectorProfileRepository,
{
    let Some(profile_id) = &connection.profile_id else {
        return Ok(());
    };
    let profile = store
        .get_connector_profile(profile_id)
        .map_err(|error| ConnectorResolveError::CredentialStoreUnavailable(error.to_string()))?
        .ok_or_else(|| ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile_id.0.clone(),
            suggested_command: "afs connect notion".to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: "afs connect notion".to_string(),
        });
    }
    Ok(())
}

fn warn_env_fallback_once() {
    if !ENV_FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "afs using NOTION_TOKEN env fallback; run `afs connect notion` to store a provider connection"
        );
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| error.to_string())
}

fn find_mount_for_path<'a>(mounts: &'a [MountConfig], path: &Path) -> Option<&'a MountConfig> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.root))
        .max_by_key(|mount| mount.root.components().count())
}

impl HydrationSource for NotionConnector {
    fn fetch_render(
        &self,
        request: &afs_core::hydration::HydrationRequest,
    ) -> AfsResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let rendered = self.render_native_entity_for_path(&native, &request.path)?;
        let bundle = serde_json::from_slice::<NotionPageBundle>(&native.raw).map_err(|error| {
            afs_core::AfsError::Io(format!("notion native decode failed: {error}"))
        })?;
        let assets = fetch_media_assets(&rendered.media_assets)?
            .into_iter()
            .map(|asset| HydratedAsset {
                path: asset.local_path,
                bytes: asset.bytes,
                media: Some(HydratedAssetMedia {
                    block_id: asset.block_id,
                    kind: asset.kind,
                    source_url: asset.source_url,
                }),
            })
            .collect();

        Ok(HydratedEntity {
            document: rendered.document,
            shadow: rendered.shadow,
            remote_edited_at: bundle.page.last_edited_time,
            assets,
        })
    }
}
