use std::collections::{BTreeMap, BTreeSet};

use locality_core::model::{EntityKind, RemoteId};
use locality_core::planner::PlanSummary;
use locality_core::portable::{
    AccessSetId, ChangesetId, ContentVersionId, LogicalPath, PrincipalId, ProjectionEntry,
    ProjectionFileKind, ProjectionId, ProjectionInput, ProjectionVersionId, ReplicaRevisionId,
    SessionId, SourceAction, SourceConnectionId, SourceOperation, SourceOperationPlan,
    SourceVersionId, TenantId,
};
use locality_protocol::{
    ACCESS_SET_GOLDEN_JSON, AUTHORIZED_SESSION_QUERY_GOLDEN_JSON, AccessSetContract, AccessSubject,
    AuthorizedSessionQuery, CHANGESET_ENVELOPE_GOLDEN_JSON, COMPONENT_VERSIONS,
    COMPONENT_VERSIONS_GOLDEN_JSON, CONTENT_VERSION_GOLDEN_JSON, ChangesetEnvelope,
    ComponentVersions, ContentVersionContract, DELIVERED_COUNT_GOLDEN_JSON, DeliveredCount,
    ORDERED_EXPORT_ROWS_GOLDEN_JSON, OrderedExportRow, PROJECTION_VERSION_GOLDEN_JSON,
    ProjectionVersionContract, READY_REPLICA_REVISION_GOLDEN_JSON, ReadyReplicaRevision,
    SOURCE_VERSION_GOLDEN_JSON, SessionReplicaRevision, SourceVersionContract,
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
    assert_exact_round_trip(CHANGESET_ENVELOPE_GOLDEN_JSON, &changeset());
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
        authorization_revision: 42,
        profile_revision: 9,
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
        entities_created: 1,
        ..PlanSummary::default()
    };
    ChangesetEnvelope {
        versions: COMPONENT_VERSIONS,
        changeset_id: ChangesetId::new("changeset-3"),
        session_id: SessionId::new("session-7"),
        parent_changeset_id: None,
        base_revisions: vec![SessionReplicaRevision {
            source_connection_id: SourceConnectionId::new("source-notion"),
            replica_revision_id: ReplicaRevisionId::new("replica-108"),
        }],
        operations: SourceOperationPlan {
            affected_entities: Vec::new(),
            operations: vec![SourceOperation::CreateEntity {
                parent_id: RemoteId::new("page-parent"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Agent draft".to_string(),
                properties: BTreeMap::new(),
                body: "Draft body.\n".to_string(),
                source_path: LogicalPath::new("Projects/Agent draft/page.md").expect("path"),
            }],
            summary,
            degradations: Vec::new(),
        },
        readable_diff_sha256: "sha256:diff".to_string(),
        content_digest: "sha256:changeset".to_string(),
        submitted_at: "2026-07-19T12:00:00Z".to_string(),
    }
}
