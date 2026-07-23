use std::collections::BTreeSet;

use locality_core::journal::PushOperationId;
use locality_core::model::RemoteId;
use locality_core::planner::PlanSummary;
use locality_core::portable::{
    AccessSetId, ChangesetId, ContentVersionId, ExportAttemptId, LogicalPath, PrincipalId,
    ProjectionEntry, ProjectionFileKind, ProjectionId, ProjectionInput, ProjectionVersionId,
    ReplicaRevisionId, SessionId, SourceAction, SourceConnectionId, SourceGenerationId,
    SourceOperation, SourceOperationPlan, SourceScopeId, SourceVersionId, TenantId,
};
use locality_core::readable_diff::readable_diff_for_file;
use locality_protocol::{
    ACCESS_SET_GOLDEN_JSON, AUTHORIZED_SESSION_QUERY_GOLDEN_JSON, AccessSetContract, AccessSubject,
    AuditReference, AuthorizedChangesetUpload, AuthorizedSessionQuery, AuthorizedSourceScope,
    BOOTSTRAP_EXCHANGE_GOLDEN_JSON, BootstrapExchangeRequest,
    CANONICAL_EXPORT_INVENTORY_GOLDEN_JSON, CANONICAL_EXPORT_RECORDS_GOLDEN_JSON,
    CHANGESET_ENVELOPE_GOLDEN_JSON, COMPONENT_VERSIONS, COMPONENT_VERSIONS_GOLDEN_JSON,
    CONTENT_VERSION_GOLDEN_JSON, CanonicalControlOrderKey, CanonicalDirectoryOrderKey,
    CanonicalExportRecord, CanonicalFileOrderKey, ChangesetContent, ChangesetEnvelope,
    ChangesetSourceObject, ClientValidationResult, CompatibleAuthorizedSessionQuery,
    ComponentVersions, ContentVersionContract, DELIVERED_COUNT_GOLDEN_JSON, DeliveredBodyDigestV2,
    DeliveredChangesetBase, DeliveredCount, EXPORT_ATTEMPT_REQUEST_GOLDEN_JSON,
    EXPORT_COMPLETION_RECEIPT_GOLDEN_JSON, EditedCanonicalBody, ExportAttemptLimits,
    ExportAttemptRequest, ExportCompletionReceipt, FRESHNESS_STATUS_GOLDEN_JSON,
    FreshnessRequirement, NotionScopeKind, ORDERED_EXPORT_ROWS_GOLDEN_JSON,
    OpaqueBootstrapExchangeRequest, OpaqueSessionStatusRequest, OrderedExportRow,
    OrderedSourceGeneration, PROJECTION_VERSION_GOLDEN_JSON, ProjectionVersionContract,
    ProviderSourceScopeSelector, READY_REPLICA_REVISION_GOLDEN_JSON, ReadyReplicaRevision,
    ReplicaFreshnessState, ReplicaFreshnessStatus, SANDBOX_SESSION_STATUS_GOLDEN_JSON,
    SCOPE_AUTHORIZED_COMPONENT_VERSIONS, SCOPE_AUTHORIZED_SESSION_QUERY_GOLDEN_JSON,
    SEALED_EXPORT_OFFER_GOLDEN_JSON, SESSION_PROTOCOL_ERROR_GOLDEN_JSON,
    SOURCE_VERSION_GOLDEN_JSON, SandboxSessionState, SandboxSessionStatus,
    ScopeAuthorizedSessionQuery, ScopeContractError, SealedExportOffer, SessionCapability,
    SessionErrorCode, SessionProtocolError, SessionReplicaRevision, SourceVersionContract,
    StaleSessionBehavior, TAR_EXPORT_METADATA_GOLDEN_JSON, TAR_EXPORT_OFFER_GOLDEN_JSON,
    TarContentEncoding, TarExportMetadata, TarExportOffer, WRITABLE_EXPORT_METADATA_GOLDEN_JSON,
    WritableExportMetadata, WritableMetadataEntry, validate_canonical_export_records,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CanonicalInventoryGolden {
    preimage_hex: String,
    sha256: String,
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
fn scope_authorized_export_contracts_are_exact_golden_bytes() {
    let query = scope_authorized_query();
    query.validate().expect("valid scope query");
    assert_exact_round_trip(SCOPE_AUTHORIZED_SESSION_QUERY_GOLDEN_JSON, &query);

    let request = export_attempt_request();
    request.validate().expect("valid attempt request");
    assert_exact_round_trip(EXPORT_ATTEMPT_REQUEST_GOLDEN_JSON, &request);

    let offer = sealed_export_offer();
    offer.validate().expect("valid sealed offer");
    assert_exact_round_trip(SEALED_EXPORT_OFFER_GOLDEN_JSON, &offer);

    let receipt = export_completion_receipt();
    receipt
        .validate_against(&offer)
        .expect("completion matches sealed metadata selection");
    assert_exact_round_trip(EXPORT_COMPLETION_RECEIPT_GOLDEN_JSON, &receipt);

    let records = canonical_export_records();
    validate_canonical_export_records(&records).expect("canonical record order");
    let preimage = locality_protocol::canonical_export_inventory_preimage(&records)
        .expect("inventory preimage");
    let inventory = CanonicalInventoryGolden {
        preimage_hex: preimage.iter().map(|byte| format!("{byte:02x}")).collect(),
        sha256: locality_protocol::canonical_export_inventory_sha256(&records)
            .expect("inventory digest"),
    };
    offer
        .validate_inventory(&records)
        .expect("sealed offer matches exact canonical inventory");
    assert_exact_round_trip(CANONICAL_EXPORT_INVENTORY_GOLDEN_JSON, &inventory);
    assert_exact_round_trip(CANONICAL_EXPORT_RECORDS_GOLDEN_JSON, &records);
}

#[test]
fn compatibility_decoder_accepts_legacy_and_scope_queries() {
    let legacy = serde_json::from_slice::<CompatibleAuthorizedSessionQuery>(
        AUTHORIZED_SESSION_QUERY_GOLDEN_JSON,
    )
    .expect("legacy query decodes");
    assert_eq!(
        legacy,
        CompatibleAuthorizedSessionQuery::Legacy(authorized_query())
    );

    let scope = serde_json::from_slice::<CompatibleAuthorizedSessionQuery>(
        SCOPE_AUTHORIZED_SESSION_QUERY_GOLDEN_JSON,
    )
    .expect("scope query decodes");
    assert_eq!(
        scope,
        CompatibleAuthorizedSessionQuery::Scope(scope_authorized_query())
    );
}

#[test]
fn scope_contract_validation_rejects_ambiguous_or_inexact_values() {
    assert!(SourceScopeId::new("").is_err());
    assert!(ExportAttemptId::new("").is_err());
    assert!(SourceGenerationId::new("").is_err());
    assert!(serde_json::from_str::<SourceScopeId>("\"\"").is_err());

    let mut query = scope_authorized_query();
    query.authorized_scopes[1].ordinal = 0;
    assert!(matches!(
        query.validate(),
        Err(ScopeContractError::NonCanonicalOrdinal {
            collection: "authorized_scopes",
            ..
        })
    ));

    let mut offer = sealed_export_offer();
    offer.archive_entry_count -= 1;
    assert_eq!(
        offer.validate(),
        Err(ScopeContractError::InconsistentArchiveEntryCount)
    );

    let mut duplicate_generation_offer = sealed_export_offer();
    duplicate_generation_offer
        .source_generations
        .push(OrderedSourceGeneration {
            ordinal: 1,
            source_connection_id: SourceConnectionId::new("source-notion"),
            source_generation_id: SourceGenerationId::new("generation-notion-110")
                .expect("generation id"),
        });
    assert_eq!(
        duplicate_generation_offer.validate(),
        Err(ScopeContractError::DuplicateValue("source_connection_id"))
    );

    let mut receipt = export_completion_receipt();
    receipt.inventory_sha256 = format!("sha256:{}", "A".repeat(64));
    assert_eq!(
        receipt.validate(),
        Err(ScopeContractError::InvalidSha256("inventory_sha256"))
    );

    let mut request = export_attempt_request();
    request.idempotency_key = "x".repeat(ExportAttemptRequest::MAX_IDEMPOTENCY_KEY_BYTES + 1);
    assert!(matches!(
        request.validate(),
        Err(ScopeContractError::ValueTooLong {
            field: "idempotency_key",
            ..
        })
    ));

    let mut records = canonical_export_records();
    records.pop();
    assert_eq!(
        validate_canonical_export_records(&records),
        Err(ScopeContractError::InvalidControlRecordCount { actual: 0 })
    );

    let implicit_control_directory = CanonicalExportRecord::Directory {
        order_key: CanonicalDirectoryOrderKey {
            depth: 1,
            logical_path: LogicalPath::new(".loc").expect("path"),
        },
    };
    assert_eq!(
        implicit_control_directory.validate(),
        Err(ScopeContractError::InvalidControlDirectory)
    );
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
fn freshness_session_and_tar_export_values_are_exact_golden_bytes() {
    assert_exact_round_trip(BOOTSTRAP_EXCHANGE_GOLDEN_JSON, &bootstrap_exchange());
    assert_exact_round_trip(FRESHNESS_STATUS_GOLDEN_JSON, &freshness_status());
    assert_exact_round_trip(TAR_EXPORT_OFFER_GOLDEN_JSON, &tar_export_offer());
    assert_exact_round_trip(TAR_EXPORT_METADATA_GOLDEN_JSON, &tar_export_metadata());
    assert_exact_round_trip(
        SANDBOX_SESSION_STATUS_GOLDEN_JSON,
        &sandbox_session_status(),
    );
    assert_exact_round_trip(SESSION_PROTOCOL_ERROR_GOLDEN_JSON, &needs_update_error());
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
    let status = OpaqueSessionStatusRequest {
        opaque_capability: "status-secret".to_string(),
    };
    let token_only_bootstrap = OpaqueBootstrapExchangeRequest {
        bootstrap_token: "token-only-secret".to_string(),
    };
    let export_attempt = ExportAttemptRequest {
        opaque_session_capability: "attempt-capability-secret".to_string(),
        idempotency_key: "attempt-idempotency-secret".to_string(),
        ..export_attempt_request()
    };

    for (debug, secret) in [
        (format!("{bootstrap:?}"), "bootstrap-secret"),
        (format!("{session:?}"), "session-secret"),
        (format!("{upload:?}"), "upload-secret"),
        (format!("{status:?}"), "status-secret"),
        (format!("{token_only_bootstrap:?}"), "token-only-secret"),
        (format!("{export_attempt:?}"), "attempt-capability-secret"),
        (format!("{export_attempt:?}"), "attempt-idempotency-secret"),
    ] {
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains(secret), "{debug}");
    }
}

#[test]
fn export_v2_pax_wire_contract_is_exact_and_round_trips() {
    assert_eq!(
        locality_protocol::EXPORT_V2_FILE_PAX_KEYS,
        [
            "locality.source_connection_id",
            "locality.projection_id",
            "locality.winning_scope_ordinal",
            "locality.file_kind",
            "locality.effective_actions",
            "locality.content_sha256",
        ]
    );

    for kind in [
        ProjectionFileKind::Markdown,
        ProjectionFileKind::Text,
        ProjectionFileKind::Json,
        ProjectionFileKind::Yaml,
        ProjectionFileKind::Binary,
        ProjectionFileKind::Directory,
    ] {
        let label = locality_protocol::projection_file_kind_wire_label(&kind);
        assert_eq!(
            locality_protocol::projection_file_kind_from_wire_label(label),
            Some(kind)
        );
    }

    for action in [
        SourceAction::Read,
        SourceAction::Search,
        SourceAction::DownloadAttachment,
        SourceAction::Create,
        SourceAction::Update,
        SourceAction::Move,
        SourceAction::Delete,
        SourceAction::Comment,
        SourceAction::UpdateProperties,
        SourceAction::ManageSchema,
    ] {
        let label = locality_protocol::source_action_wire_label(&action);
        assert_eq!(
            locality_protocol::source_action_from_wire_label(label),
            Some(action)
        );
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
        BOOTSTRAP_EXCHANGE_GOLDEN_JSON,
        FRESHNESS_STATUS_GOLDEN_JSON,
        SANDBOX_SESSION_STATUS_GOLDEN_JSON,
        SESSION_PROTOCOL_ERROR_GOLDEN_JSON,
        TAR_EXPORT_OFFER_GOLDEN_JSON,
        TAR_EXPORT_METADATA_GOLDEN_JSON,
        SCOPE_AUTHORIZED_SESSION_QUERY_GOLDEN_JSON,
        EXPORT_ATTEMPT_REQUEST_GOLDEN_JSON,
        SEALED_EXPORT_OFFER_GOLDEN_JSON,
        EXPORT_COMPLETION_RECEIPT_GOLDEN_JSON,
        CANONICAL_EXPORT_RECORDS_GOLDEN_JSON,
        CANONICAL_EXPORT_INVENTORY_GOLDEN_JSON,
    ] {
        let value: serde_json::Value = serde_json::from_slice(golden).expect("valid golden JSON");
        assert_forbidden_physical_keys_absent(&value);
    }
}

fn bootstrap_exchange() -> OpaqueBootstrapExchangeRequest {
    OpaqueBootstrapExchangeRequest {
        bootstrap_token: "opaque-bootstrap-token".to_string(),
    }
}

fn freshness_status() -> ReplicaFreshnessStatus {
    ReplicaFreshnessStatus {
        source_connection_id: SourceConnectionId::new("source-notion"),
        state: ReplicaFreshnessState::Fresh,
        coverage_complete: true,
        provider_observed_through: Some("notion-repair:108".to_string()),
        last_successful_sync_at: Some("2026-07-19T11:58:00Z".to_string()),
        last_repair_at: Some("2026-07-19T11:55:00Z".to_string()),
        pending_events: 0,
        backlog: 0,
        provider_cooldown_until: None,
    }
}

fn tar_export_offer() -> TarExportOffer {
    TarExportOffer {
        media_type: "application/x-tar".to_string(),
        supported_content_encodings: BTreeSet::from([
            TarContentEncoding::Identity,
            TarContentEncoding::Zstd,
        ]),
        selected_entries: 2,
        decoded_bytes: 3072,
        decoded_tar_sha256: "sha256:decoded-tar".to_string(),
    }
}

fn tar_export_metadata() -> TarExportMetadata {
    TarExportMetadata {
        versions: COMPONENT_VERSIONS,
        session_id: SessionId::new("session-7"),
        media_type: "application/x-tar".to_string(),
        content_encoding: TarContentEncoding::Zstd,
        delivered_entries: 2,
        decoded_bytes: 3072,
        wire_bytes: 384,
        decoded_tar_sha256: "sha256:decoded-tar".to_string(),
        inventory_sha256: "sha256:delivered-inventory".to_string(),
    }
}

fn sandbox_session_status() -> SandboxSessionStatus {
    SandboxSessionStatus {
        versions: COMPONENT_VERSIONS,
        session_id: SessionId::new("session-7"),
        state: SandboxSessionState::Ready,
        freshness_requirement: FreshnessRequirement {
            max_age_seconds: 300,
            on_stale: StaleSessionBehavior::WaitThenFail,
            wait_timeout_seconds: 30,
        },
        replicas: vec![freshness_status()],
        export_offer: Some(tar_export_offer()),
        error: None,
        updated_at: "2026-07-19T12:00:00Z".to_string(),
    }
}

fn needs_update_error() -> SessionProtocolError {
    SessionProtocolError {
        code: SessionErrorCode::NeedsUpdate,
        message: "session protocol version 2 requires a newer client".to_string(),
        retriable: false,
        retry_after_seconds: None,
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

fn scope_authorized_query() -> ScopeAuthorizedSessionQuery {
    ScopeAuthorizedSessionQuery {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        tenant_id: TenantId::new("tenant-acme"),
        session_id: SessionId::new("session-scope-7"),
        acting_principal_id: PrincipalId::new("principal-agent"),
        workload_id: "workload-sandbox".to_string(),
        authorization_revision: 43,
        policy_revision: 18,
        profile_revision: 10,
        authorized_scopes: vec![
            AuthorizedSourceScope {
                ordinal: 0,
                source_scope_id: SourceScopeId::new("scope-product").expect("scope id"),
                source_connection_id: SourceConnectionId::new("source-notion"),
                selector: ProviderSourceScopeSelector::Notion {
                    selector_version: 1,
                    scope_kind: NotionScopeKind::Page,
                    provider_scope_id: "e07dd2a2531444aba2f452010314fb87".to_string(),
                },
                effective_actions: BTreeSet::from([
                    SourceAction::Read,
                    SourceAction::DownloadAttachment,
                ]),
                validated_filter_digest: Some(format!("sha256:{}", "1".repeat(64))),
            },
            AuthorizedSourceScope {
                ordinal: 1,
                source_scope_id: SourceScopeId::new("scope-go-to-market").expect("scope id"),
                source_connection_id: SourceConnectionId::new("source-notion"),
                selector: ProviderSourceScopeSelector::Notion {
                    selector_version: 1,
                    scope_kind: NotionScopeKind::Page,
                    provider_scope_id: "f525c3a7db684465a9e1d1082bea6f81".to_string(),
                },
                effective_actions: BTreeSet::from([SourceAction::Read]),
                validated_filter_digest: None,
            },
        ],
        max_files: 10_000,
        max_directories: 10_000,
        max_bytes: 104_857_600,
    }
}

fn attempt_limits() -> ExportAttemptLimits {
    ExportAttemptLimits {
        max_files: 10_000,
        max_directories: 10_000,
        max_content_bytes: 104_857_600,
    }
}

fn export_attempt_request() -> ExportAttemptRequest {
    ExportAttemptRequest {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        opaque_session_capability: "opaque-session-capability".to_string(),
        idempotency_key: "mount-attempt-2026-07-23T19:00:00Z".to_string(),
        content_encoding: TarContentEncoding::Zstd,
        limits: attempt_limits(),
    }
}

fn source_generations() -> Vec<OrderedSourceGeneration> {
    vec![OrderedSourceGeneration {
        ordinal: 0,
        source_connection_id: SourceConnectionId::new("source-notion"),
        source_generation_id: SourceGenerationId::new("generation-notion-109")
            .expect("generation id"),
    }]
}

fn sealed_export_offer() -> SealedExportOffer {
    SealedExportOffer {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        session_id: SessionId::new("session-scope-7"),
        export_attempt_id: ExportAttemptId::new("export-attempt-9").expect("attempt id"),
        source_generations: source_generations(),
        media_type: "application/x-tar".to_string(),
        content_encoding: TarContentEncoding::Zstd,
        limits: attempt_limits(),
        control_entry_count: 1,
        file_count: 2,
        directory_count: 2,
        archive_entry_count: 5,
        selected_content_bytes: 17,
        inventory_sha256: "sha256:025cdbae136931542f7fa881da423e8e1f29a6132cf26ae5f4eea53c53a8ef51"
            .to_string(),
        sealed_at: "2026-07-23T19:00:01Z".to_string(),
        expires_at: "2026-07-23T19:10:01Z".to_string(),
    }
}

fn export_completion_receipt() -> ExportCompletionReceipt {
    let mut body_digest = DeliveredBodyDigestV2::new(2);
    body_digest
        .update_file(&ProjectionId::new("projection-roadmap"), b"# Roadmap\n")
        .expect("body digest");
    body_digest
        .update_file(&ProjectionId::new("projection-readme"), b"Public\n")
        .expect("body digest");
    ExportCompletionReceipt {
        versions: SCOPE_AUTHORIZED_COMPONENT_VERSIONS,
        session_id: SessionId::new("session-scope-7"),
        export_attempt_id: ExportAttemptId::new("export-attempt-9").expect("attempt id"),
        source_generations: source_generations(),
        inventory_sha256: "sha256:025cdbae136931542f7fa881da423e8e1f29a6132cf26ae5f4eea53c53a8ef51"
            .to_string(),
        delivered_control_entry_count: 1,
        delivered_file_count: 2,
        delivered_directory_count: 2,
        delivered_archive_entry_count: 5,
        delivered_content_bytes: 17,
        delivered_body_sha256: body_digest.finish().expect("body digest"),
        completed_at: "2026-07-23T19:00:04Z".to_string(),
    }
}

fn canonical_export_records() -> Vec<CanonicalExportRecord> {
    vec![
        CanonicalExportRecord::Directory {
            order_key: CanonicalDirectoryOrderKey {
                depth: 1,
                logical_path: LogicalPath::new("Projects").expect("path"),
            },
        },
        CanonicalExportRecord::Directory {
            order_key: CanonicalDirectoryOrderKey {
                depth: 2,
                logical_path: LogicalPath::new("Projects/Roadmap").expect("path"),
            },
        },
        CanonicalExportRecord::File {
            order_key: CanonicalFileOrderKey {
                winning_scope_ordinal: 0,
                parent_path: Some(LogicalPath::new("Projects/Roadmap").expect("path")),
                logical_path: LogicalPath::new("Projects/Roadmap/page.md").expect("path"),
                projection_id: ProjectionId::new("projection-roadmap"),
            },
            source_connection_id: SourceConnectionId::new("source-notion"),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: BTreeSet::from([SourceAction::Read, SourceAction::Update]),
            content_sha256: format!("sha256:{}", "5".repeat(64)),
            byte_length: 10,
        },
        CanonicalExportRecord::File {
            order_key: CanonicalFileOrderKey {
                winning_scope_ordinal: 1,
                parent_path: None,
                logical_path: LogicalPath::new("README.md").expect("path"),
                projection_id: ProjectionId::new("projection-readme"),
            },
            source_connection_id: SourceConnectionId::new("source-notion"),
            file_kind: ProjectionFileKind::Markdown,
            effective_actions: BTreeSet::from([SourceAction::Read]),
            content_sha256: format!("sha256:{}", "6".repeat(64)),
            byte_length: 7,
        },
        CanonicalExportRecord::Control {
            order_key: CanonicalControlOrderKey { ordinal: 0 },
            member_path: locality_protocol::RESERVED_EXPORT_METADATA_PATH.to_string(),
        },
    ]
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
                        "scope_root_id"
                            | "export_order"
                            | "content_storage_id"
                            | "mount_id"
                            | "host_path"
                            | "local_root"
                            | "credential"
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
