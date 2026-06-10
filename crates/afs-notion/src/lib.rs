//! Notion connector.
//!
//! The connector keeps Notion API transport, DTOs, and block rendering separate
//! from the connector-neutral sync contracts in `afs-core`.

pub mod client;
pub mod dto;
pub mod fetch;
pub mod mapping;
pub mod projection;
pub mod render;

use std::sync::Arc;

use afs_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity,
};
use afs_core::model::{CanonicalDocument, TreeEntry};
use afs_core::{AfsError, AfsResult};

use crate::client::{DEFAULT_NOTION_TOKEN_ENV, HttpNotionApi, NotionApi};
use crate::fetch::fetch_page_bundle;
use crate::projection::{enumerate_root_page_tree, enumerate_shared_pages};
use crate::render::{NotionRenderedEntity, render_native_entity};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionConfig {
    pub workspace_id: Option<String>,
    pub root_page_id: Option<afs_core::model::RemoteId>,
    /// Environment variable or future keychain key used to find the bearer token.
    pub token_key: String,
}

impl Default for NotionConfig {
    fn default() -> Self {
        Self {
            workspace_id: None,
            root_page_id: None,
            token_key: DEFAULT_NOTION_TOKEN_ENV.to_string(),
        }
    }
}

impl NotionConfig {
    pub fn with_root_page_id(mut self, root_page_id: afs_core::model::RemoteId) -> Self {
        self.root_page_id = Some(root_page_id);
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

    pub fn with_root_page_id(&self, root_page_id: afs_core::model::RemoteId) -> Self {
        let mut config = self.config.clone();
        config.root_page_id = Some(root_page_id);
        Self {
            config,
            api: Arc::clone(&self.api),
        }
    }

    pub fn render_native_entity(&self, entity: &NativeEntity) -> AfsResult<NotionRenderedEntity> {
        render_native_entity(entity)
    }
}

impl Connector for NotionConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("notion")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: true,
            supports_oauth: true,
        }
    }

    fn enumerate(&self, request: EnumerateRequest) -> AfsResult<Vec<TreeEntry>> {
        if let Some(root_page_id) = &self.config.root_page_id {
            enumerate_root_page_tree(self.api.as_ref(), request.mount_id, root_page_id)
        } else {
            enumerate_shared_pages(self.api.as_ref(), request.mount_id)
        }
    }

    fn fetch(&self, request: FetchRequest) -> AfsResult<NativeEntity> {
        let bundle = fetch_page_bundle(self.api.as_ref(), request.remote_id.as_str())?;
        let remote_id = afs_core::model::RemoteId::new(bundle.page.id.clone());
        let raw = serde_json::to_vec(&bundle)
            .map_err(|error| AfsError::Io(format!("notion native encode failed: {error}")))?;

        Ok(NativeEntity {
            remote_id,
            kind: "notion_page".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> AfsResult<CanonicalDocument> {
        self.render_native_entity(entity)
            .map(|rendered| rendered.document)
    }

    fn parse(&self, _document: &CanonicalDocument) -> AfsResult<ParsedEntity> {
        Err(AfsError::NotImplemented("Notion parse"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> AfsResult<()> {
        Err(AfsError::NotImplemented("Notion concurrency check"))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> AfsResult<ApplyPlanResult> {
        Err(AfsError::NotImplemented("Notion apply"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> AfsResult<ApplyUndoResult> {
        Err(AfsError::NotImplemented("Notion undo apply"))
    }
}
