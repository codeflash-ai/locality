use std::cell::Cell;
use std::collections::BTreeSet;
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
use locality_core::portable::{ChangesetId, PrincipalId, SessionId, SourceAction, TenantId};
use locality_protocol::{
    CHANGESET_ENVELOPE_GOLDEN_JSON, COMPONENT_VERSIONS, ChangesetEnvelope, ChangesetReceipt,
    ChangesetState, ChangesetStatus, ChangesetStatusRequest, FreshnessRequirement,
    OpaqueBootstrapExchangeRequest, OpaqueSessionStatusRequest, ReplicaExportFrame,
    ReplicaExportRequest, SandboxSessionState, SandboxSessionStatus, SessionCapability,
    SessionGrant, SessionRequest, StaleSessionBehavior,
};
use localityd::remote_truth::{
    BackendReplica, DirectSourceReplica, OpaqueReplicaSessionService, RemoteTruthAuthority,
    RemoteTruthProvider, ReplicaService,
};

#[derive(Clone)]
struct RecordingDirectConnector {
    execution_policy: ConnectorExecutionPolicy,
    enumerations: Cell<usize>,
}

impl Connector for RecordingDirectConnector {
    fn with_execution_policy(&self, execution_policy: ConnectorExecutionPolicy) -> Self {
        Self {
            execution_policy,
            enumerations: Cell::new(self.enumerations.get()),
        }
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind("recording-direct")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::default()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        self.enumerations.set(self.enumerations.get() + 1);
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
        enumerations: Cell::new(0),
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
    assert_eq!(connector.enumerations.get(), 1);
}

struct RecordingReplicaService {
    calls: Cell<usize>,
}

impl ReplicaService for RecordingReplicaService {
    type Error = &'static str;
    type Export = std::vec::IntoIter<Result<ReplicaExportFrame, Self::Error>>;

    fn create_session(&self, request: SessionRequest) -> Result<SessionGrant, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Ok(SessionGrant {
            versions: request.versions,
            session_id: SessionId::new("backend-session"),
            capability: SessionCapability {
                session_id: SessionId::new("backend-session"),
                opaque_capability: "secret-capability".to_string(),
                expires_at: "2026-07-19T13:00:00Z".to_string(),
            },
            authorization_revision: 42,
            policy_revision: 17,
            profile_revision: request.profile_revision,
            replica_revisions: Vec::new(),
            effective_actions: request.requested_actions,
        })
    }

    fn open_export(&self, _request: ReplicaExportRequest) -> Result<Self::Export, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Ok(Vec::new().into_iter())
    }

    fn submit_changeset(
        &self,
        changeset: ChangesetEnvelope,
    ) -> Result<ChangesetReceipt, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Ok(ChangesetReceipt {
            versions: changeset.versions,
            changeset_id: changeset.changeset_id,
            state: ChangesetState::Received,
            received_at: "2026-07-19T12:00:01Z".to_string(),
        })
    }

    fn changeset_status(
        &self,
        request: ChangesetStatusRequest,
    ) -> Result<ChangesetStatus, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Ok(ChangesetStatus {
            versions: request.versions,
            changeset_id: request.changeset_id,
            state: ChangesetState::Reconciled,
            updated_at: "2026-07-19T12:00:02Z".to_string(),
            detail: None,
        })
    }
}

impl OpaqueReplicaSessionService for RecordingReplicaService {
    type Error = &'static str;

    fn exchange_bootstrap(
        &self,
        _request: OpaqueBootstrapExchangeRequest,
    ) -> Result<SessionCapability, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Ok(SessionCapability {
            session_id: SessionId::new("backend-session"),
            opaque_capability: "secret-capability".to_string(),
            expires_at: "2026-07-19T13:00:00Z".to_string(),
        })
    }

    fn session_status(
        &self,
        _request: OpaqueSessionStatusRequest,
    ) -> Result<SandboxSessionStatus, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Ok(SandboxSessionStatus {
            versions: COMPONENT_VERSIONS,
            session_id: SessionId::new("backend-session"),
            state: SandboxSessionState::Ready,
            freshness_requirement: FreshnessRequirement {
                max_age_seconds: 300,
                on_stale: StaleSessionBehavior::WaitThenFail,
                wait_timeout_seconds: 30,
            },
            replicas: Vec::new(),
            export_offer: None,
            error: None,
            updated_at: "2026-07-19T12:00:00Z".to_string(),
        })
    }
}

#[test]
fn backend_replica_uses_only_replica_service_authority() {
    let connector = RecordingDirectConnector {
        execution_policy: ConnectorExecutionPolicy::Inline,
        enumerations: Cell::new(0),
    };
    let service = RecordingReplicaService {
        calls: Cell::new(0),
    };
    let provider = BackendReplica::new(&service);

    assert_eq!(provider.authority(), RemoteTruthAuthority::BackendReplica);
    let grant = provider
        .service()
        .create_session(SessionRequest {
            versions: COMPONENT_VERSIONS,
            tenant_id: TenantId::new("tenant-acme"),
            profile_revision: 9,
            acting_principal_id: PrincipalId::new("principal-agent"),
            workload_id: "workload-sandbox".to_string(),
            requested_actions: BTreeSet::from([SourceAction::Read, SourceAction::Update]),
            narrowing_filter_digest: None,
        })
        .expect("backend session");
    assert_eq!(grant.session_id, SessionId::new("backend-session"));

    provider
        .service()
        .open_export(ReplicaExportRequest {
            versions: COMPONENT_VERSIONS,
            capability: grant.capability,
        })
        .expect("backend export");
    let envelope = serde_json::from_slice::<ChangesetEnvelope>(CHANGESET_ENVELOPE_GOLDEN_JSON)
        .expect("changeset golden");
    let receipt = provider
        .service()
        .submit_changeset(envelope)
        .expect("backend changeset");
    provider
        .service()
        .changeset_status(ChangesetStatusRequest {
            versions: COMPONENT_VERSIONS,
            changeset_id: ChangesetId::new("changeset-3"),
        })
        .expect("backend status");

    assert_eq!(receipt.state, ChangesetState::Received);
    assert_eq!(service.calls.get(), 4);
    assert_eq!(
        connector.enumerations.get(),
        0,
        "backend must not fall back"
    );
}

#[test]
fn backend_replica_exchanges_only_opaque_session_authority() {
    let connector = RecordingDirectConnector {
        execution_policy: ConnectorExecutionPolicy::Inline,
        enumerations: Cell::new(0),
    };
    let service = RecordingReplicaService {
        calls: Cell::new(0),
    };
    let provider = BackendReplica::new(&service);

    let capability = provider
        .service()
        .exchange_bootstrap(OpaqueBootstrapExchangeRequest {
            bootstrap_token: "one-time-secret".to_string(),
        })
        .expect("token-only exchange");
    let status = provider
        .service()
        .session_status(OpaqueSessionStatusRequest {
            opaque_capability: capability.opaque_capability,
        })
        .expect("capability-only status");

    assert_eq!(status.session_id, SessionId::new("backend-session"));
    assert_eq!(status.state, SandboxSessionState::Ready);
    assert_eq!(service.calls.get(), 2);
    assert_eq!(
        connector.enumerations.get(),
        0,
        "backend must not fall back"
    );
}
