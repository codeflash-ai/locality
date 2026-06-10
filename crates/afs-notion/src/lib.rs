pub mod mapping;

use afs_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity,
};
use afs_core::model::{CanonicalDocument, TreeEntry};
use afs_core::{AfsError, AfsResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionConfig {
    pub workspace_id: Option<String>,
    pub token_key: String,
}

#[derive(Clone, Debug)]
pub struct NotionConnector {
    config: NotionConfig,
}

impl NotionConnector {
    pub fn new(config: NotionConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &NotionConfig {
        &self.config
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

    fn enumerate(&self, _request: EnumerateRequest) -> AfsResult<Vec<TreeEntry>> {
        Err(AfsError::NotImplemented("Notion enumerate"))
    }

    fn fetch(&self, _request: FetchRequest) -> AfsResult<NativeEntity> {
        Err(AfsError::NotImplemented("Notion fetch"))
    }

    fn render(&self, _entity: &NativeEntity) -> AfsResult<CanonicalDocument> {
        Err(AfsError::NotImplemented("Notion render"))
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
