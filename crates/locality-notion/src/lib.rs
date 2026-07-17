//! Notion connector.
//!
//! The connector keeps Notion API transport, DTOs, and block rendering separate
//! from the connector-neutral sync contracts in `locality-core`.

pub mod apply;
pub mod client;
pub mod database;
pub mod database_create;
pub mod dto;
pub mod fetch;
pub mod mapping;
pub mod markdown_table;
pub mod media;
pub mod oauth;
pub mod projection;
pub mod render;
pub mod schema;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorExecutionPolicy, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::RemoteObservation;
use locality_core::model::{CanonicalDocument, RemoteId, TreeEntry};
use locality_core::planner::PushOperationKind;
use locality_core::{LocalityError, LocalityResult};

use crate::apply::{apply_plan, apply_undo, check_concurrency};
use crate::client::{DEFAULT_NOTION_TOKEN_ENV, HttpNotionApi, NotionApi};
use crate::fetch::fetch_page_bundle;
use crate::media::{MediaDownloadReport, download_media_assets};
use crate::projection::{
    enumerate_root_page_tree, enumerate_shared_pages, list_container_children, observe_entity,
    resolve_notion_object_path_entries, resolve_page_path_entries,
};
use crate::render::{
    NotionRenderedEntity, RenderOptions, render_native_entity, render_native_entity_with_options,
};

#[derive(Clone, PartialEq, Eq)]
pub struct NotionConfig {
    pub workspace_id: Option<String>,
    pub root_page_id: Option<locality_core::model::RemoteId>,
    /// Resolved bearer token from a provider connection. Never log this field.
    pub token: Option<String>,
    /// Environment variable or future keychain key used to find the bearer token.
    pub token_key: String,
    pub execution_policy: ConnectorExecutionPolicy,
}

impl std::fmt::Debug for NotionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConfig")
            .field("workspace_id", &self.workspace_id)
            .field("root_page_id", &self.root_page_id)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("token_key", &self.token_key)
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

impl Default for NotionConfig {
    fn default() -> Self {
        Self {
            workspace_id: None,
            root_page_id: None,
            token: None,
            token_key: DEFAULT_NOTION_TOKEN_ENV.to_string(),
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
    }
}

impl NotionConfig {
    pub fn with_root_page_id(mut self, root_page_id: locality_core::model::RemoteId) -> Self {
        self.root_page_id = Some(root_page_id);
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_execution_policy(mut self, execution_policy: ConnectorExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }
}

#[derive(Clone)]
pub struct NotionConnector {
    config: NotionConfig,
    api: Arc<dyn NotionApi>,
}

impl std::fmt::Debug for NotionConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConnector")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl NotionConnector {
    pub fn new(config: NotionConfig) -> Self {
        Self::with_api(config.clone(), Arc::new(HttpNotionApi::new(config)))
    }

    pub fn with_api(config: NotionConfig, api: Arc<dyn NotionApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &NotionConfig {
        &self.config
    }

    pub fn with_root_page_id(&self, root_page_id: locality_core::model::RemoteId) -> Self {
        let mut config = self.config.clone();
        config.root_page_id = Some(root_page_id);
        Self {
            config,
            api: Arc::clone(&self.api),
        }
    }

    pub fn render_native_entity(
        &self,
        entity: &NativeEntity,
    ) -> LocalityResult<NotionRenderedEntity> {
        render_native_entity(entity)
    }

    pub fn render_native_entity_for_path(
        &self,
        entity: &NativeEntity,
        page_path: impl AsRef<Path>,
    ) -> LocalityResult<NotionRenderedEntity> {
        render_native_entity_with_options(
            entity,
            &RenderOptions::with_page_path(page_path.as_ref()),
        )
    }

    pub fn render_native_entity_for_path_with_local_media_blocks(
        &self,
        entity: &NativeEntity,
        page_path: impl AsRef<Path>,
        block_ids: impl IntoIterator<Item = String>,
    ) -> LocalityResult<NotionRenderedEntity> {
        render_native_entity_with_options(
            entity,
            &RenderOptions::with_page_path(page_path.as_ref())
                .with_local_media_block_ids(block_ids),
        )
    }

    pub fn download_rendered_media(
        &self,
        rendered: &NotionRenderedEntity,
        mount_root: impl AsRef<Path>,
    ) -> LocalityResult<MediaDownloadReport> {
        download_media_assets(mount_root.as_ref(), &rendered.media_assets)
    }

    pub fn database_schema_yaml(&self, database_id: &RemoteId) -> LocalityResult<String> {
        database::database_schema_yaml(self.api.as_ref(), database_id.as_str())
    }

    pub fn resolve_page_path_entries(
        &self,
        mount_id: locality_core::model::MountId,
        page_id: &RemoteId,
    ) -> LocalityResult<Vec<TreeEntry>> {
        resolve_page_path_entries(
            self.api.as_ref(),
            mount_id,
            self.config.root_page_id.as_ref(),
            page_id,
        )
    }

    pub fn resolve_object_path_entries(
        &self,
        mount_id: locality_core::model::MountId,
        object_id: &RemoteId,
    ) -> LocalityResult<Vec<TreeEntry>> {
        resolve_notion_object_path_entries(
            self.api.as_ref(),
            mount_id,
            self.config.root_page_id.as_ref(),
            object_id,
        )
    }
}

impl Connector for NotionConnector {
    fn with_execution_policy(&self, policy: ConnectorExecutionPolicy) -> Self {
        Self::new(self.config.clone().with_execution_policy(policy))
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind("notion")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_entity_body_updates: false,
            supports_databases: true,
            supports_oauth: true,
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            supports_media_download: true,
            supports_undo: true,
            supports_batch_observation: false,
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        [
            PushOperationKind::UpdateBlock,
            PushOperationKind::ReplaceBlock,
            PushOperationKind::AppendBlock,
            PushOperationKind::MoveBlock,
            PushOperationKind::UpdateMedia,
            PushOperationKind::ArchiveBlock,
            PushOperationKind::ArchiveEntity,
            PushOperationKind::UpdateProperties,
            PushOperationKind::MoveEntity,
            PushOperationKind::CreateEntity,
            PushOperationKind::CreateDatabase,
        ]
        .into_iter()
        .collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        if let Some(root_page_id) = &self.config.root_page_id {
            enumerate_root_page_tree(self.api.as_ref(), request.mount_id, root_page_id)
        } else {
            enumerate_shared_pages(self.api.as_ref(), request.mount_id)
        }
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = list_container_children(
            self.api.as_ref(),
            request.mount_id,
            self.config.root_page_id.as_ref(),
            request.container,
            &request.parent_path,
        )?;

        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        observe_entity(self.api.as_ref(), request.mount_id, &request.remote_id)
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let bundle = fetch_page_bundle(self.api.as_ref(), request.remote_id.as_str())?;
        let remote_id = locality_core::model::RemoteId::new(bundle.page.id.clone());
        let raw = serde_json::to_vec(&bundle)
            .map_err(|error| LocalityError::Io(format!("notion native encode failed: {error}")))?;

        Ok(NativeEntity {
            remote_id,
            kind: "notion_page".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        self.render_native_entity(entity)
            .map(|rendered| rendered.document)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::NotImplemented("Notion parse"))
    }

    fn check_concurrency(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        check_concurrency(self.api.as_ref(), request)
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        apply_plan(self.api.as_ref(), request)
    }

    fn apply_undo(&self, request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        apply_undo(self.api.as_ref(), request)
    }
}
