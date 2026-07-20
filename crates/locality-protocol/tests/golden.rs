use std::collections::BTreeSet;

use locality_core::journal::PushOperationId;
use locality_core::model::RemoteId;
use locality_core::planner::PlanSummary;
use locality_core::portable::{
    AccessSetId, ChangesetId, ContentVersionId, LogicalPath, PrincipalId, ProjectionEntry,
    ProjectionFileKind, ProjectionId, ProjectionInput, ProjectionVersionId, ReplicaRevisionId,
    SessionId, SourceAction, SourceConnectionId, SourceOperation, SourceOperationPlan,
    SourceVersionId, TenantId,
};
use locality_core::readable_diff::readable_diff_for_file;
use locality_protocol::{
    ACCESS_SET_GOLDEN_JSON, AUTHORIZED_SESSION_QUERY_GOLDEN_JSON, AccessSetContract, AccessSubject,
    AuditReference, AuthorizedChangesetUpload, AuthorizedSessionQuery, BootstrapExchangeRequest,
    CHANGESET_ENVELOPE_GOLDEN_JSON, COMPONENT_VERSIONS, COMPONENT_VERSIONS_GOLDEN_JSON,
    CONTENT_VERSION_GOLDEN_JSON, ChangesetContent, ChangesetEnvelope, ChangesetSourceObject,
    ClientValidationResult, ComponentVersions, ContentVersionContract, DELIVERED_COUNT_GOLDEN_JSON,
    DeliveredChangesetBase, DeliveredCount, EditedCanonicalBody, ORDERED_EXPORT_ROWS_GOLDEN_JSON,
    OrderedExportRow, PROJECTION_VERSION_GOLDEN_JSON, ProjectionVersionContract,
    READY_REPLICA_REVISION_GOLDEN_JSON, ReadyReplicaRevision, SOURCE_VERSION_GOLDEN_JSON,
    SessionCapability, SessionReplicaRevision, SourceVersionContract,
    WRITABLE_EXPORT_METADATA_GOLDEN_JSON, WritableExportMetadata, WritableMetadataEntry,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn assert_exact_round_trip<T>(golden: &[u8], expected: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let decoded = serde_json::from_slice::<T>(golden).expect("golden value must deserialize");
    assert_eq!(&decoded, expected);
    assert_eq!(exact_pretty_json(&decoded), golden);
}

#[test]
fn component_versions_are_exact_golden_bytes() {
    assert_exact_round_trip(COMPONENT_VERSIONS_GOLDEN_JSON, &COMPONENT_VERSIONS);
}

#[test]
fn authorized_session_query_is_exact_golden_bytes() {
    assert_exact_round_trip(AUTHORIZED_SESSION_QUERY_GOLDEN_JSON, &authorized_query());
}

#[test]
fn source_content_projection_and_access_contracts_are_exact_golden_bytes() {
    assert_exact_round_trip(SOURCE_VERSION_GOLDEN_JSON, &source_version());
    assert_exact_round_trip(CONTENT_VERSION_GOLDEN_JSON, &content_version());
    assert_exact_round_trip(PROJECTION_VERSION_GOLDEN_JSON, &projection_version());
    assert_exact_round_trip(ACCESS_SET_GOLDEN_JSON, &access_set());
}

#[test]
fn publication_delivery_and_writable_metadata_are_exact_golden_bytes() {
    assert_exact_round_trip(
        READY_REPLICA_REVISION_GOLDEN_JSON,
        &ready_replica_revision(),
    );
    let delivered = delivered_count();
    assert!(delivered.is_exact());
    assert_exact_round_trip(DELIVERED_COUNT_GOLDEN_JSON, &delivered);
    let writable_metadata = writable_export_metadata();
    assert!(writable_metadata.writable_entries.iter().all(|entry| {
        entry.effective_actions.iter().any(|action| {
            !matches!(
                action,
                SourceAction::Read | SourceAction::Search | SourceAction::DownloadAttachment
            )
        })
    }));
    assert_exact_round_trip(WRITABLE_EXPORT_METADATA_GOLDEN_JSON, &writable_metadata);
}

#[test]
fn ordered_export_rows_are_exact_golden_bytes() {
    assert_exact_round_trip(ORDERED_EXPORT_ROWS_GOLDEN_JSON, &ordered_rows());
}

#[test]
fn portable_changeset_is_exact_golden_bytes() {
    let changeset = changeset();
    assert!(matches!(changeset.content, ChangesetContent::Inline { .. }));
    assert_eq!(
        changeset.operation_ids.len(),
        changeset.advisory_operations.operations.len()
    );
    assert_exact_round_trip(CHANGESET_ENVELOPE_GOLDEN_JSON, &changeset);
}

#[test]
fn capability_debug_output_is_redacted() {
    let bootstrap = BootstrapExchangeRequest {
        versions: COMPONENT_VERSIONS,
        bootstrap_token: "bootstrap-secret".to_string(),
    };
    let session = SessionCapability {
        session_id: SessionId::new("session-7"),
        opaque_capability: "session-secret".to_string(),
        expires_at: "2026-07-19T13:00:00Z".to_string(),
    };
    let upload = AuthorizedChangesetUpload {
        upload_id: "upload-1".to_string(),
        opaque_capability: "upload-secret".to_string(),
        content_sha256: "sha256:upload".to_string(),
        byte_length: 99,
    };

    for (debug, secret) in [
        (format!("{bootstrap:?}"), "bootstrap-secret"),
        (format!("{session:?}"), "session-secret"),
        (format!("{upload:?}"), "upload-secret"),
    ] {
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains(secret), "{debug}");
    }
}

#[test]
fn public_goldens_never_expose_physical_serving_identifiers() {
    for golden in [
        SOURCE_VERSION_GOLDEN_JSON,
        CONTENT_VERSION_GOLDEN_JSON,
        PROJECTION_VERSION_GOLDEN_JSON,
        ACCESS_SET_GOLDEN_JSON,
        READY_REPLICA_REVISION_GOLDEN_JSON,
        AUTHORIZED_SESSION_QUERY_GOLDEN_JSON,
        ORDERED_EXPORT_ROWS_GOLDEN_JSON,
        DELIVERED_COUNT_GOLDEN_JSON,
        WRITABLE_EXPORT_METADATA_GOLDEN_JSON,
        CHANGESET_ENVELOPE_GOLDEN_JSON,
    ] {
        let value: serde_json::Value = serde_json::from_slice(golden).expect("valid golden JSON");
        assert_forbidden_physical_keys_absent(&value);
    }
}

#[test]
fn newer_required_component_returns_needs_update() {
    let required = ComponentVersions {
        changeset: COMPONENT_VERSIONS.changeset + 1,
        ..COMPONENT_VERSIONS
    };

    let error = required
        .validate_required()
        .expect_err("newer protocol must not be opened");
    assert_eq!(
        error,
        locality_protocol::VersionCompatibilityError::NeedsUpdate {
            component: locality_protocol::ProtocolComponent::Changeset,
            required: 2,
            supported: 1,
        }
    );
}

#[test]
fn current_component_versions_are_supported() {
    COMPONENT_VERSIONS
        .validate_required()
        .expect("current versions must open");
}

fn authorized_query() -> AuthorizedSessionQuery {
    AuthorizedSessionQuery {
        versions: COMPONENT_VERSIONS,
        tenant_id: TenantId::new("tenant-acme"),
        session_id: SessionId::new("session-7"),
        acting_principal_id: PrincipalId::new("principal-agent"),
        workload_id: "workload-sandbox".to_string(),
        authorization_revision: 42,
        policy_revision: 17,
        profile_revision: 9,
        effective_actions: BTreeSet::from([SourceAction::Read, SourceAction::Update]),
        replica_revisions: vec![SessionReplicaRevision {
            source_connection_id: SourceConnectionId::new("source-notion"),
            replica_revision_id: ReplicaRevisionId::new("replica-108"),
        }],
        validated_filter_digest: "sha256:filter".to_string(),
        max_entries: 10_000,
        max_bytes: 104_857_600,
    }
}

fn source_version() -> SourceVersionContract {
    SourceVersionContract {
        tenant_id: TenantId::new("tenant-acme"),
        source_connection_id: SourceConnectionId::new("source-notion"),
        source_version_id: SourceVersionId::new("source-version-roadmap-v4"),
        remote_id: RemoteId::new("page-roadmap"),
        provider_version: "opaque-v4".to_string(),
        native_sha256: "sha256:native-roadmap".to_string(),
        canonical_sha256: "sha256:canonical-roadmap".to_string(),
        observed_at: "2026-07-19T11:58:00Z".to_string(),
    }
}

fn content_version() -> ContentVersionContract {
    ContentVersionContract {
        tenant_id: TenantId::new("tenant-acme"),
        source_connection_id: SourceConnectionId::new("source-notion"),
        content_version_id: ContentVersionId::new("content-roadmap-v4"),
        sha256: "sha256:roadmap".to_string(),
        byte_length: 10,
        media_type: "text/markdown; charset=utf-8".to_string(),
    }
}

fn projection_version() -> ProjectionVersionContract {
    ProjectionVersionContract {
        tenant_id: TenantId::new("tenant-acme"),
        source_connection_id: SourceConnectionId::new("source-notion"),
        projection_version_id: ProjectionVersionId::new("projection-version-roadmap-v4"),
        projection: ProjectionEntry {
            projection_id: ProjectionId::new("projection-roadmap"),
            logical_path: LogicalPath::new("Projects/Roadmap/page.md").expect("path"),
            content_version_id: Some(ContentVersionId::new("content-roadmap-v4")),
            inputs: vec![ProjectionInput {
                source_remote_id: RemoteId::new("page-roadmap"),
                source_version_id: SourceVersionId::new("source-version-roadmap-v4"),
            }],
            file_kind: ProjectionFileKind::Markdown,
            format_version: 1,
            supported_actions: full_action_vocabulary(),
        },
    }
}

fn access_set() -> AccessSetContract {
    AccessSetContract {
        tenant_id: TenantId::new("tenant-acme"),
        access_set_id: AccessSetId::new("access-set-engineering"),
        revision: 42,
        source_connection_id: SourceConnectionId::new("source-notion"),
        subjects: BTreeSet::from([
            AccessSubject::Principal(PrincipalId::new("principal-agent")),
            AccessSubject::Group("group-engineering".to_string()),
            AccessSubject::Workload("workload-sandbox".to_string()),
        ]),
        source_remote_ids: BTreeSet::from([
            RemoteId::new("page-public"),
            RemoteId::new("page-roadmap"),
        ]),
        actions: full_action_vocabulary(),
    }
}

fn ready_replica_revision() -> ReadyReplicaRevision {
    ReadyReplicaRevision {
        tenant_id: TenantId::new("tenant-acme"),
        source_connection_id: SourceConnectionId::new("source-notion"),
        replica_revision_id: ReplicaRevisionId::new("replica-108"),
        source_watermark: "notion-cursor:108".to_string(),
        projection_revision: 73,
        coverage_complete: true,
        published_at: "2026-07-19T11:59:00Z".to_string(),
    }
}

fn delivered_count() -> DeliveredCount {
    DeliveredCount {
        selected_entries: 2,
        delivered_entries: 2,
        delivered_bytes: 17,
        inventory_sha256: "sha256:delivered-inventory".to_string(),
    }
}

fn writable_export_metadata() -> WritableExportMetadata {
    WritableExportMetadata {
        versions: COMPONENT_VERSIONS,
        session_id: SessionId::new("session-7"),
        replica_revisions: vec![SessionReplicaRevision {
            source_connection_id: SourceConnectionId::new("source-notion"),
            replica_revision_id: ReplicaRevisionId::new("replica-108"),
        }],
        writable_entries: vec![WritableMetadataEntry {
            projection_id: ProjectionId::new("projection-roadmap"),
            logical_path: LogicalPath::new("Projects/Roadmap/page.md").expect("path"),
            source_remote_ids: vec![RemoteId::new("page-roadmap")],
            delivered_content_sha256: "sha256:roadmap".to_string(),
            provider_precondition: "opaque-v4".to_string(),
            effective_actions: BTreeSet::from([
                SourceAction::Read,
                SourceAction::Update,
                SourceAction::UpdateProperties,
            ]),
            baseline_required: true,
        }],
    }
}

fn full_action_vocabulary() -> BTreeSet<SourceAction> {
    SourceAction::all().into_iter().collect()
}

fn assert_forbidden_physical_keys_absent(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                assert!(
                    !matches!(
                        key.as_str(),
                        "scope_root_id" | "export_order" | "content_storage_id"
                    ),
                    "public golden exposed backend-only field `{key}`"
                );
                assert_forbidden_physical_keys_absent(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                assert_forbidden_physical_keys_absent(value);
            }
        }
        _ => {}
    }
}

fn ordered_rows() -> Vec<OrderedExportRow> {
    vec![
        OrderedExportRow {
            tenant_id: TenantId::new("tenant-acme"),
            source_connection_id: SourceConnectionId::new("source-notion"),
            projection_id: ProjectionId::new("projection-roadmap"),
            logical_path: LogicalPath::new("Projects/Roadmap/page.md").expect("path"),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: BTreeSet::from([SourceAction::Read, SourceAction::Update]),
            provider_version: "opaque-v4".to_string(),
            content_sha256: "sha256:roadmap".to_string(),
            byte_length: 10,
            body: b"# Roadmap\n".to_vec(),
        },
        OrderedExportRow {
            tenant_id: TenantId::new("tenant-acme"),
            source_connection_id: SourceConnectionId::new("source-notion"),
            projection_id: ProjectionId::new("projection-public"),
            logical_path: LogicalPath::new("Projects/Public/page.md").expect("path"),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: BTreeSet::from([SourceAction::Read]),
            provider_version: "opaque-v2".to_string(),
            content_sha256: "sha256:public".to_string(),
            byte_length: 7,
            body: b"Public\n".to_vec(),
        },
    ]
}

fn changeset() -> ChangesetEnvelope {
    let summary = PlanSummary {
        blocks_updated: 1,
        ..PlanSummary::default()
    };
    let source_object = ChangesetSourceObject {
        source_connection_id: SourceConnectionId::new("source-notion"),
        remote_id: RemoteId::new("page-roadmap"),
    };
    let readable_diff = readable_diff_for_file(
        "Projects/Roadmap/page.md",
        Some("Old paragraph.\n"),
        Some("Changed paragraph.\n"),
    )
    .expect("diff");
    ChangesetEnvelope {
        versions: COMPONENT_VERSIONS,
        changeset_id: ChangesetId::new("changeset-3"),
        tenant_id: TenantId::new("tenant-acme"),
        session_id: SessionId::new("session-7"),
        acting_principal_id: PrincipalId::new("principal-agent"),
        workload_id: "workload-sandbox".to_string(),
        authorization_revision: 42,
        policy_revision: 17,
        profile_revision: 9,
        parent_changeset_id: None,
        replica_revisions: vec![SessionReplicaRevision {
            source_connection_id: SourceConnectionId::new("source-notion"),
            replica_revision_id: ReplicaRevisionId::new("replica-108"),
        }],
        affected_projection_ids: BTreeSet::from([ProjectionId::new("projection-roadmap")]),
        affected_source_objects: BTreeSet::from([source_object.clone()]),
        delivered_bases: vec![DeliveredChangesetBase {
            projection_id: ProjectionId::new("projection-roadmap"),
            source_object,
            provider_precondition: "opaque-v4".to_string(),
            delivered_content_sha256: "sha256:roadmap".to_string(),
            delivered_shadow_sha256: "sha256:roadmap-shadow".to_string(),
        }],
        content: ChangesetContent::Inline {
            edited_canonical_bodies: vec![EditedCanonicalBody {
                projection_id: ProjectionId::new("projection-roadmap"),
                logical_path: LogicalPath::new("Projects/Roadmap/page.md").expect("path"),
                canonical_sha256: "sha256:edited-roadmap".to_string(),
                canonical_markdown: "---\nloc:\n  id: page-roadmap\n  type: page\ntitle: Roadmap\n---\nChanged paragraph.\n".to_string(),
            }],
        },
        advisory_operations: SourceOperationPlan {
            affected_entities: vec![RemoteId::new("page-roadmap")],
            operations: vec![SourceOperation::UpdateBlock {
                block_id: RemoteId::new("block-1"),
                content: "Changed paragraph.".to_string(),
            }],
            summary,
            degradations: Vec::new(),
        },
        readable_diff,
        readable_diff_sha256: "sha256:diff".to_string(),
        operation_ids: vec![PushOperationId("changeset-3:0:update_block:block-1".to_string())],
        idempotency_key: "changeset-3".to_string(),
        client_validation_results: vec![ClientValidationResult {
            code: "validated".to_string(),
            projection_id: Some(ProjectionId::new("projection-roadmap")),
            logical_path: LogicalPath::new("Projects/Roadmap/page.md").expect("path"),
            line: None,
            message: "client validation passed".to_string(),
            suggested_fix: None,
        }],
        audit_reference: Some(AuditReference {
            kind: "git_commit".to_string(),
            reference: "0123456789abcdef".to_string(),
        }),
        content_digest: "sha256:changeset".to_string(),
        submitted_at: "2026-07-19T12:00:00Z".to_string(),
    }
}
