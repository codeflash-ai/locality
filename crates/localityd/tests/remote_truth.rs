use std::path::PathBuf;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorExecutionPolicy, ConnectorKind, EnumerateRequest, FetchRequest,
    NativeEntity, ParsedEntity,
};
use locality_core::LocalityResult;
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use localityd::remote_truth::{DirectSourceReplica, RemoteTruthAuthority, RemoteTruthProvider};

#[derive(Clone)]
struct RecordingDirectConnector {
    execution_policy: ConnectorExecutionPolicy,
}

impl Connector for RecordingDirectConnector {
    fn with_execution_policy(&self, execution_policy: ConnectorExecutionPolicy) -> Self {
        Self { execution_policy }
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind("recording-direct")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::default()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Ok(vec![TreeEntry {
            mount_id: request.mount_id,
            remote_id: RemoteId::new("page-1"),
            kind: EntityKind::Page,
            title: "Roadmap".to_string(),
            path: PathBuf::from("Roadmap/page.md"),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some("opaque-v1".to_string()),
            stub_frontmatter: None,
        }])
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "page".to_string(),
            raw: Vec::new(),
        })
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        Ok(CanonicalDocument::new("title: Roadmap\n", "Body\n"))
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Ok(ParsedEntity {
            remote_id: RemoteId::new("page-1"),
            native: NativeEntity {
                remote_id: RemoteId::new("page-1"),
                kind: "page".to_string(),
                raw: Vec::new(),
            },
        })
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Ok(ApplyPlanResult {
            changed_remote_ids: Vec::new(),
            effects: Vec::new(),
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Ok(ApplyUndoResult {
            changed_remote_ids: Vec::new(),
        })
    }
}

#[test]
fn direct_source_replica_declares_authority_and_delegates_without_fallback() {
    let connector = RecordingDirectConnector {
        execution_policy: ConnectorExecutionPolicy::Inline,
    }
    .with_execution_policy(ConnectorExecutionPolicy::DeferProviderCooldown);
    let provider = DirectSourceReplica::new(&connector);

    assert_eq!(provider.authority(), RemoteTruthAuthority::DirectSource);
    assert_eq!(
        provider.source().execution_policy,
        ConnectorExecutionPolicy::DeferProviderCooldown
    );
    assert_eq!(provider.source().kind(), ConnectorKind("recording-direct"));
    let entries = provider
        .source()
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("notion-main"),
            cursor: None,
        })
        .expect("direct enumeration");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].remote_id, RemoteId::new("page-1"));
}
