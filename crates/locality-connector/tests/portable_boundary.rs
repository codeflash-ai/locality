use std::collections::BTreeSet;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity, PortableBootstrapRequest, PortableEnumerateRequest, PortableFetchReason,
    PortableFetchRequest, PortableSourceScope,
};
use locality_core::LocalityResult;
use locality_core::model::{CanonicalDocument, TreeEntry};
use locality_core::portable::SourceConnectionId;

#[derive(Clone)]
struct LegacyOnlyConnector;

impl Connector for LegacyOnlyConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("legacy-only")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::default()
    }

    fn supported_push_operations(&self) -> BTreeSet<locality_core::planner::PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, _request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Ok(Vec::new())
    }

    fn fetch(&self, _request: FetchRequest) -> LocalityResult<NativeEntity> {
        unreachable!("not used by boundary test")
    }

    fn render(&self, _entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        unreachable!("not used by boundary test")
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        unreachable!("not used by boundary test")
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        unreachable!("not used by boundary test")
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        unreachable!("not used by boundary test")
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        unreachable!("not used by boundary test")
    }
}

#[test]
fn legacy_connectors_compile_and_do_not_invent_portable_identity() {
    let connector = LegacyOnlyConnector;
    let error = connector
        .enumerate_portable(PortableEnumerateRequest {
            source_connection_id: SourceConnectionId::new("source-1"),
            cursor: None,
        })
        .expect_err("legacy connector must require an explicit portable implementation");

    assert_eq!(
        error,
        locality_core::LocalityError::Unsupported(
            "connector does not support portable enumeration"
        )
    );

    let bootstrap_request = PortableBootstrapRequest {
        source_connection_id: SourceConnectionId::new("source-1"),
        scope: PortableSourceScope::explicit_roots([locality_core::model::RemoteId::new("root-1")]),
        checkpoint: None,
        max_changes: 100,
    };
    let serialized = serde_json::to_value(&bootstrap_request).expect("portable request JSON");
    let serialized = serialized.as_object().expect("portable request object");
    assert_eq!(
        serialized.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["checkpoint", "max_changes", "scope", "source_connection_id"]
    );

    let error = connector
        .bootstrap_portable(bootstrap_request)
        .expect_err("legacy connector must not invent a portable bootstrap");
    assert_eq!(
        error,
        locality_core::LocalityError::Unsupported("connector does not support portable bootstrap")
    );

    let error = connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-1"),
            remote_id: locality_core::model::RemoteId::new("page-1"),
            reason: PortableFetchReason::Bootstrap,
        })
        .expect_err("legacy connector must not invent a portable fetch");
    assert_eq!(
        error,
        locality_core::LocalityError::Unsupported("connector does not support portable fetch")
    );
}
