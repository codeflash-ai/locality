use std::collections::BTreeSet;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    ParsedEntity, PortableBatchAuthority, PortableBootstrapRequest, PortableChangeBatch,
    PortableCheckpoint, PortableCompleteness, PortableEnumerateRequest, PortableFetchReason,
    PortableFetchRequest, PortableSourceScope, PortableSyncHint, PortableSyncMode,
    PortableSyncRequest,
};
use locality_core::LocalityResult;
use locality_core::model::{CanonicalDocument, EntityKind, TreeEntry};
use locality_core::portable::{LogicalPath, SourceConnectionId};
use serde_json::json;

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

#[test]
fn portable_batch_authority_defaults_to_incremental_and_rejects_unknown_values() {
    let complete = PortableChangeBatch {
        changes: Vec::new(),
        next_checkpoint: PortableCheckpoint {
            format_version: 7,
            opaque: r#"{"provider_cursor":"keep-opaque"}"#.to_string(),
        },
        completeness: PortableCompleteness::complete(),
        authority: PortableBatchAuthority::CompleteScopeSnapshot,
    };
    let serialized = serde_json::to_value(&complete).expect("portable batch JSON");
    assert_eq!(
        serialized.get("authority"),
        Some(&json!("complete_scope_snapshot"))
    );

    let mut legacy = serialized.clone();
    legacy
        .as_object_mut()
        .expect("portable batch object")
        .remove("authority");
    let legacy: PortableChangeBatch =
        serde_json::from_value(legacy).expect("legacy portable batch");
    assert_eq!(legacy.authority, PortableBatchAuthority::Incremental);
    assert_eq!(
        legacy.next_checkpoint.opaque,
        r#"{"provider_cursor":"keep-opaque"}"#
    );

    let mut unknown = serialized;
    unknown["authority"] = json!("future_authority");
    assert!(serde_json::from_value::<PortableChangeBatch>(unknown).is_err());
}

#[test]
fn portable_sync_defaults_to_hints_only_and_prior_metadata_is_optional() {
    let legacy = json!({
        "source_connection_id": "source-1",
        "scope": { "root_remote_ids": ["root-1"] },
        "checkpoint": {
            "format_version": 3,
            "opaque": "provider-owned-not-host-json"
        },
        "hints": [{ "remote_id": "page-1" }],
        "max_changes": 50
    });
    let request: PortableSyncRequest =
        serde_json::from_value(legacy).expect("legacy portable sync request");
    assert_eq!(request.mode, PortableSyncMode::HintsOnly);
    assert_eq!(request.checkpoint.opaque, "provider-owned-not-host-json");
    assert_eq!(request.hints.len(), 1);
    assert_eq!(request.hints[0].provider_version, None);
    assert_eq!(request.hints[0].logical_path, None);
    assert_eq!(request.hints[0].source_kind, None);
    assert_eq!(request.hints[0].owning_root_remote_id, None);
    assert_eq!(
        serde_json::to_value(&request.hints[0]).expect("portable sync hint JSON"),
        json!({ "remote_id": "page-1" })
    );

    let rich_hint = PortableSyncHint {
        remote_id: locality_core::model::RemoteId::new("page-2"),
        provider_version: Some("provider-version-2".to_string()),
        logical_path: Some(LogicalPath::new("Roadmap/page.md").expect("logical path")),
        source_kind: Some(EntityKind::Page),
        owning_root_remote_id: Some(locality_core::model::RemoteId::new("root-1")),
    };
    assert_eq!(
        serde_json::to_value(rich_hint).expect("rich portable sync hint JSON"),
        json!({
            "remote_id": "page-2",
            "provider_version": "provider-version-2",
            "logical_path": "Roadmap/page.md",
            "source_kind": "page",
            "owning_root_remote_id": "root-1"
        })
    );

    let mut unknown = serde_json::to_value(request).expect("portable sync request JSON");
    unknown["mode"] = json!("future_mode");
    assert!(serde_json::from_value::<PortableSyncRequest>(unknown).is_err());
}
