use std::collections::BTreeSet;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, NativeEntity,
    PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES, PORTABLE_SYNC_V2_MAX_HINTS,
    PORTABLE_SYNC_V2_MAX_ID_BYTES, PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES,
    PORTABLE_SYNC_V2_MAX_SOURCE_KIND_BYTES, ParsedEntity, PortableBatchAuthority,
    PortableBootstrapRequest, PortableChangeBatch, PortableChangeBatchV2, PortableCheckpoint,
    PortableCompleteness, PortableEnumerateRequest, PortableFetchReason, PortableFetchRequest,
    PortableSourceScope, PortableSyncHint, PortableSyncHintV2, PortableSyncMode,
    PortableSyncRequest, PortableSyncRequestV2,
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
    let complete = PortableChangeBatchV2 {
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
    let legacy: PortableChangeBatchV2 =
        serde_json::from_value(legacy).expect("v2 batch without authority");
    assert_eq!(legacy.authority, PortableBatchAuthority::Incremental);
    assert_eq!(
        legacy.next_checkpoint.opaque,
        r#"{"provider_cursor":"keep-opaque"}"#
    );

    let mut unknown = serialized;
    unknown["authority"] = json!("future_authority");
    assert!(serde_json::from_value::<PortableChangeBatchV2>(unknown).is_err());
}

#[test]
fn legacy_portable_sync_wire_shape_remains_unchanged() {
    let request = PortableSyncRequest {
        source_connection_id: SourceConnectionId::new("source-1"),
        scope: PortableSourceScope::explicit_roots([locality_core::model::RemoteId::new("root-1")]),
        checkpoint: PortableCheckpoint {
            format_version: 3,
            opaque: "provider-owned-not-host-json".to_string(),
        },
        hints: vec![PortableSyncHint {
            remote_id: locality_core::model::RemoteId::new("page-1"),
        }],
        max_changes: 50,
    };
    assert_eq!(
        serde_json::to_value(&request).expect("legacy portable sync request JSON"),
        json!({
            "source_connection_id": "source-1",
            "scope": { "root_remote_ids": ["root-1"] },
            "checkpoint": {
                "format_version": 3,
                "opaque": "provider-owned-not-host-json"
            },
            "hints": [{ "remote_id": "page-1" }],
            "max_changes": 50
        })
    );
    let batch = PortableChangeBatch {
        changes: Vec::new(),
        next_checkpoint: request.checkpoint,
        completeness: PortableCompleteness::complete(),
    };
    assert!(
        serde_json::to_value(batch)
            .expect("legacy portable batch JSON")
            .get("authority")
            .is_none()
    );
}

#[test]
fn portable_sync_v2_defaults_to_hints_only_and_prior_metadata_is_optional() {
    let v2_without_additive_fields = json!({
        "source_connection_id": "source-1",
        "scope": { "root_remote_ids": ["root-1"] },
        "checkpoint": {
            "format_version": 3,
            "opaque": "provider-owned-not-host-json"
        },
        "hints": [{ "remote_id": "page-1" }],
        "max_changes": 50
    });
    let request: PortableSyncRequestV2 = serde_json::from_value(v2_without_additive_fields)
        .expect("v2 portable sync request with defaulted fields");
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

    let rich_hint = PortableSyncHintV2 {
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
    assert!(serde_json::from_value::<PortableSyncRequestV2>(unknown).is_err());
}

fn valid_v2_request(hints: Vec<PortableSyncHintV2>) -> PortableSyncRequestV2 {
    PortableSyncRequestV2 {
        source_connection_id: SourceConnectionId::new("source-1"),
        scope: PortableSourceScope::explicit_roots([locality_core::model::RemoteId::new("root-1")]),
        checkpoint: PortableCheckpoint {
            format_version: 1,
            opaque: "checkpoint".to_string(),
        },
        mode: PortableSyncMode::HintsOnly,
        hints,
        max_changes: 100,
    }
}

fn valid_v2_hint(remote_id: impl Into<String>) -> PortableSyncHintV2 {
    PortableSyncHintV2 {
        remote_id: locality_core::model::RemoteId::new(remote_id),
        provider_version: None,
        logical_path: None,
        source_kind: None,
        owning_root_remote_id: Some(locality_core::model::RemoteId::new("root-1")),
    }
}

#[test]
fn portable_sync_v2_enforces_exact_metadata_bounds() {
    let hints = (0..PORTABLE_SYNC_V2_MAX_HINTS)
        .map(|index| valid_v2_hint(format!("page-{index}")))
        .collect::<Vec<_>>();
    let mut request = valid_v2_request(hints);
    request.validate().expect("exact hint ceiling");
    request.hints.push(valid_v2_hint("one-too-many"));
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 has {} hints; maximum is {}",
            PORTABLE_SYNC_V2_MAX_HINTS + 1,
            PORTABLE_SYNC_V2_MAX_HINTS
        )))
    );

    let mut request = valid_v2_request(vec![valid_v2_hint(
        "a".repeat(PORTABLE_SYNC_V2_MAX_ID_BYTES),
    )]);
    request.validate().expect("exact ID ceiling");
    request.hints[0].remote_id =
        locality_core::model::RemoteId::new("a".repeat(PORTABLE_SYNC_V2_MAX_ID_BYTES + 1));
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 hint remote ID must contain 1..={} UTF-8 bytes",
            PORTABLE_SYNC_V2_MAX_ID_BYTES
        )))
    );

    request = valid_v2_request(vec![valid_v2_hint("page-1")]);
    request.hints[0].provider_version =
        Some("v".repeat(PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES));
    request.validate().expect("exact provider-version ceiling");
    request.hints[0].provider_version =
        Some("v".repeat(PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES + 1));
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 provider version exceeds {} UTF-8 bytes",
            PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES
        )))
    );

    request = valid_v2_request(vec![valid_v2_hint("page-1")]);
    request.hints[0].source_kind = Some(EntityKind::Unknown(
        "k".repeat(PORTABLE_SYNC_V2_MAX_SOURCE_KIND_BYTES),
    ));
    request.validate().expect("exact source-kind ceiling");
    request.hints[0].source_kind = Some(EntityKind::Unknown(
        "k".repeat(PORTABLE_SYNC_V2_MAX_SOURCE_KIND_BYTES + 1),
    ));
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 source kind exceeds {} UTF-8 bytes",
            PORTABLE_SYNC_V2_MAX_SOURCE_KIND_BYTES
        )))
    );

    request = valid_v2_request(Vec::new());
    request.checkpoint.opaque = "c".repeat(PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES);
    request.validate().expect("exact checkpoint ceiling");
    request.checkpoint.opaque = "c".repeat(PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES + 1);
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(format!(
            "portable sync v2 checkpoint is {} UTF-8 bytes; maximum is {}",
            PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES + 1,
            PORTABLE_SYNC_V2_MAX_CHECKPOINT_BYTES
        )))
    );
}

#[test]
fn portable_sync_v2_rejects_duplicate_ids_and_out_of_scope_owners() {
    let request = valid_v2_request(vec![valid_v2_hint("page-1"), valid_v2_hint("page-1")]);
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(
            "portable sync v2 contains duplicate hint remote IDs".to_string()
        ))
    );

    let mut request = valid_v2_request(Vec::new());
    let duplicate_root = request
        .scope
        .root_remote_ids
        .first()
        .expect("fixture scope root")
        .clone();
    request.scope.root_remote_ids.push(duplicate_root);
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(
            "portable sync v2 contains duplicate scope root remote IDs".to_string()
        ))
    );

    let mut request = valid_v2_request(vec![valid_v2_hint("page-1")]);
    request.hints[0].owning_root_remote_id =
        Some(locality_core::model::RemoteId::new("other-root"));
    assert_eq!(
        request.validate(),
        Err(locality_core::LocalityError::InvalidState(
            "portable sync v2 hint owning root is outside the request scope".to_string()
        ))
    );
}
