use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_connector::{Connector, EnumerateRequest, FetchRequest};
use locality_core::model::{EntityKind, MountId, RemoteId, TreeEntry};
use locality_core::validation::{ValidationIssue, ValidationReport};
use locality_core::{LocalityError, LocalityResult};
use locality_notion::client::DEFAULT_NOTION_TOKEN_ENV;
use locality_notion::dto::NotionPageBundle;
use locality_notion::media::fetch_media_asset_report;
use locality_notion::oauth::{
    HttpNotionOAuthBrokerClient, HttpNotionOAuthClient, NotionOAuthBrokerRefresh,
    NotionOAuthRefresh, StoredNotionCredential,
};
use locality_notion::{NotionConfig, NotionConnector, PrivateWorkspaceCreateAuthMode};
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, CredentialError,
    CredentialStore, EntityRecord, MountConfig, MountRepository,
};

use crate::hydration::{HydratedAsset, HydratedAssetMedia, HydratedEntity, HydrationSource};
use crate::source::{SourceAdapter, SourcePushValidator, SourceValidationContext};
use crate::virtual_fs::virtual_fs_content_root;

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
        message: Option<String>,
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
            Self::AuthRequired {
                connection_id,
                message,
                ..
            } => message.clone().unwrap_or_else(|| {
                format!("credential for connection `{connection_id}` was not found")
            }),
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

impl From<ConnectorResolveError> for LocalityError {
    fn from(value: ConnectorResolveError) -> Self {
        LocalityError::InvalidState(value.message())
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
    S: ConnectionRepository + ConnectorProfileRepository + ?Sized,
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
                suggested_command: "loc connect notion".to_string(),
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
            private_workspace_create_auth_mode: PrivateWorkspaceCreateAuthMode::ProbeTokenSubject,
            ..Default::default()
        };
        return Ok(NotionConnector::new(config));
    }

    let message = if active.is_empty() {
        "missing Notion connection; run `loc connect notion`".to_string()
    } else {
        "mount has no connection_id and multiple Notion connections exist".to_string()
    };
    Err(ConnectorResolveError::MissingConnection {
        message,
        suggested_command: "loc connect notion".to_string(),
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
    fn enumerate_mount(
        &self,
        mount: &MountConfig,
    ) -> LocalityResult<Vec<locality_core::model::TreeEntry>> {
        let connector = self.connectors.get(&mount.mount_id).ok_or_else(|| {
            LocalityError::InvalidState(format!("mount `{}` was not resolved", mount.mount_id.0))
        })?;
        crate::reconcile::ScheduledPullSource::enumerate_mount(connector, mount)
    }

    fn database_schema_yaml(
        &self,
        mount: &MountConfig,
        remote_id: &locality_core::model::RemoteId,
    ) -> LocalityResult<Option<String>> {
        let connector = self.connectors.get(&mount.mount_id).ok_or_else(|| {
            LocalityError::InvalidState(format!("mount `{}` was not resolved", mount.mount_id.0))
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
            suggested_command: "loc connect notion".to_string(),
        });
    }

    let token = connection_access_token(credentials, connection)?;
    let private_workspace_create_auth_mode = if connection.auth_kind == "oauth" {
        PrivateWorkspaceCreateAuthMode::Allow
    } else {
        PrivateWorkspaceCreateAuthMode::ProbeTokenSubject
    };
    let config = NotionConfig {
        workspace_id: connection.workspace_id.clone(),
        root_page_id: mount.remote_root_id.clone(),
        private_workspace_create_auth_mode,
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
) -> Result<locality_notion::oauth::NotionOAuthToken, ConnectorResolveError> {
    if let Some(broker_url) = stored.oauth_broker_url.clone() {
        if stored.refresh_token.is_none() && stored.refresh_token_handle.is_none() {
            return Err(ConnectorResolveError::AuthRequired {
                connection_id: connection.connection_id.0.clone(),
                message: None,
                suggested_command: "loc connect notion".to_string(),
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
            message: None,
            suggested_command: "loc connect notion".to_string(),
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
            message: None,
            suggested_command: "loc connect notion".to_string(),
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
    S: ConnectionRepository + ?Sized,
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
            suggested_command: "loc connect notion".to_string(),
        })?;
    if profile.status != "active"
        || profile.connector != connection.connector
        || profile.auth_kind != connection.auth_kind
    {
        return Err(ConnectorResolveError::AuthProfileUnavailable {
            profile_id: profile.profile_id.0,
            suggested_command: "loc connect notion".to_string(),
        });
    }
    Ok(())
}

fn warn_env_fallback_once() {
    if !ENV_FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "loc using NOTION_TOKEN env fallback; run `loc connect notion` to store a provider connection"
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

impl SourcePushValidator for NotionConnector {
    fn validate_changed_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_notion_changed_frontmatter(context)
    }

    fn validate_create_frontmatter(
        &self,
        context: SourceValidationContext<'_>,
    ) -> LocalityResult<ValidationReport> {
        validate_notion_create_frontmatter(context)
    }
}

impl SourceAdapter for NotionConnector {
    fn scoped_to_mount(&self, mount: &MountConfig) -> Self
    where
        Self: Sized + Clone,
    {
        mount
            .remote_root_id
            .as_ref()
            .map(|root_page_id| self.with_root_page_id(root_page_id.clone()))
            .unwrap_or_else(|| self.clone())
    }

    fn database_schema_yaml(&self, database_id: &RemoteId) -> LocalityResult<Option<String>> {
        self.database_schema_yaml(database_id).map(Some)
    }
}

pub(crate) fn validate_notion_changed_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    if context.mount.read_only {
        return Ok(ValidationReport::clean());
    }
    let Some(parent) = context
        .parent
        .filter(|entity| entity.kind == EntityKind::Database)
    else {
        return Ok(ValidationReport::clean());
    };
    let Some(shadow) = context.shadow else {
        return Ok(ValidationReport::clean());
    };

    Ok(
        match notion_schema_yaml_or_issue(
            context.state_root,
            context.mount,
            parent,
            context.relative_path,
        ) {
            Ok(schema) => locality_notion::schema::validate_changed_row_frontmatter(
                &schema,
                shadow,
                context.parsed,
                context.relative_path,
            ),
            Err(report) => report,
        },
    )
}

pub(crate) fn validate_notion_create_frontmatter(
    context: SourceValidationContext<'_>,
) -> LocalityResult<ValidationReport> {
    if context.mount.read_only {
        return Ok(ValidationReport::clean());
    }
    let Some(parent) = context
        .parent
        .filter(|entity| entity.kind == EntityKind::Database)
    else {
        return Ok(ValidationReport::clean());
    };

    Ok(
        match notion_schema_yaml_or_issue(
            context.state_root,
            context.mount,
            parent,
            context.relative_path,
        ) {
            Ok(schema) => locality_notion::schema::validate_create_row_frontmatter(
                &schema,
                context.parsed,
                context.relative_path,
            ),
            Err(report) => report,
        },
    )
}

fn notion_schema_yaml_or_issue(
    state_root: Option<&Path>,
    mount: &MountConfig,
    database: &EntityRecord,
    relative_path: &Path,
) -> Result<String, ValidationReport> {
    let schema_path = schema_path(state_root, mount, database);
    match std::fs::read_to_string(&schema_path) {
        Ok(schema) => Ok(schema),
        Err(error) => {
            let code = if error.kind() == std::io::ErrorKind::NotFound {
                "notion_schema_missing"
            } else {
                "notion_schema_unreadable"
            };
            let mut report = ValidationReport::clean();
            report.push(ValidationIssue::new(
                code,
                relative_path,
                Some(1),
                format!(
                    "Notion database row writes require readable schema file `{}`",
                    schema_path.display()
                ),
                Some(
                    "run `loc pull` on the database directory to regenerate `_schema.yaml`"
                        .to_string(),
                ),
            ));
            Err(report)
        }
    }
}

fn schema_path(state_root: Option<&Path>, mount: &MountConfig, database: &EntityRecord) -> PathBuf {
    if mount.projection.uses_virtual_filesystem() {
        return state_root
            .map(|root| virtual_fs_content_root(root, &mount.mount_id))
            .unwrap_or_else(|| mount.root.clone())
            .join(&database.path)
            .join("_schema.yaml");
    }

    mount.root.join(&database.path).join("_schema.yaml")
}

impl HydrationSource for NotionConnector {
    fn fetch_render(
        &self,
        request: &locality_core::hydration::HydrationRequest,
    ) -> LocalityResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let mut rendered = self.render_native_entity_for_path(&native, &request.path)?;
        let bundle = serde_json::from_slice::<NotionPageBundle>(&native.raw).map_err(|error| {
            locality_core::LocalityError::Io(format!("notion native decode failed: {error}"))
        })?;
        let fetched = fetch_media_asset_report(&rendered.media_assets);
        if !fetched.failed.is_empty() {
            let local_media_block_ids = fetched
                .downloaded
                .iter()
                .map(|asset| asset.block_id.clone())
                .collect::<Vec<_>>();
            rendered = self.render_native_entity_for_path_with_local_media_blocks(
                &native,
                &request.path,
                local_media_block_ids,
            )?;
        }
        let assets = fetched
            .downloaded
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

    fn fetch_database_schema_yaml(&self, database_id: &RemoteId) -> LocalityResult<Option<String>> {
        self.database_schema_yaml(database_id).map(Some)
    }
}

impl crate::reconcile::ScheduledPullSource for NotionConnector {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        let connector = match &mount.remote_root_id {
            Some(root_page_id) => self.with_root_page_id(root_page_id.clone()),
            None => self.clone(),
        };

        connector.enumerate(EnumerateRequest {
            mount_id: mount.mount_id.clone(),
            cursor: None,
        })
    }

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        remote_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        self.database_schema_yaml(remote_id).map(Some)
    }
}
