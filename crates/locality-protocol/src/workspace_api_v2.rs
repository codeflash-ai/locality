//! Inert HTTP API generation-2 workspace-session contracts.
//!
//! These DTOs describe negotiation and authenticated pre-stream responses.
//! They do not implement routes, authorize scopes, select rows, or bind a
//! portable workspace to an absolute host path.

use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};

use locality_core::portable::{ExportAttemptId, SessionId, SourceConnectionId, SourceGenerationId};
use serde::{Deserialize, Deserializer, Serialize};

use crate::workspace_layout::{
    LayoutDigest, SessionLayout, WORKSPACE_LAYOUT_VERSION, WorkspaceLayoutError, WorkspaceProfileId,
};
use crate::{
    ComponentVersions, ExportAttemptLimits, FreshnessRequirement, OrderedSourceGeneration,
    ReplicaFreshnessState, ReplicaFreshnessStatus, SandboxSessionState, ScopeContractError,
    SealedExportOffer, SessionErrorCode, SessionProtocolError, StaleSessionBehavior,
    TarContentEncoding, VersionCompatibilityError,
};

pub const WORKSPACE_HTTP_API_GENERATION_V2: u16 = 2;
pub const WORKSPACE_CAPABILITY_VERSION_V1: u16 = 1;
pub const REQUIRED_MAX_COMPONENT_UTF8_BYTES: u16 = 255;
pub const REQUIRED_MAX_COMPONENT_UTF16_UNITS: u16 = 255;
pub const REQUIRED_MAX_PATH_UTF8_BYTES: u16 = 1024;
pub const REQUIRED_MAX_PATH_UTF16_UNITS: u16 = 1024;
pub const MIN_WORKSPACE_CLIENT_CAPABILITIES_V2: usize = 4;
pub const MAX_WORKSPACE_CLIENT_CAPABILITIES_V2: usize = 5;
pub const MAX_WORKSPACE_CLIENT_CAPABILITIES_V2_ENCODED_BYTES: usize = 1024;
pub const MAX_WORKSPACE_SESSION_REQUEST_V2_BYTES: usize = 4096;

pub const WORKSPACE_PROFILE_SESSION_REQUEST_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-profile-session-request-v2.json");
pub const WORKSPACE_PROFILE_SESSION_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-profile-session-v2.json");
pub const WORKSPACE_SESSION_STATUS_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-session-status-v2.json");
pub const WORKSPACE_EXPORT_OFFER_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-export-offer-v2.json");
pub const WORKSPACE_UPDATE_REQUIRED_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-update-required-v2.json");
pub const WORKSPACE_INCOMPATIBLE_CAPABILITIES_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-incompatible-capabilities-v2.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkspaceClientCapabilityKindV2 {
    WorkspaceLayout,
    AtomicRootPublication,
    PathCeilings,
    TarEncodings,
    FreshnessWait,
}

impl Display for WorkspaceClientCapabilityKindV2 {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::WorkspaceLayout => "workspace_layout",
            Self::AtomicRootPublication => "atomic_root_publication",
            Self::PathCeilings => "path_ceilings",
            Self::TarEncodings => "tar_encodings",
            Self::FreshnessWait => "freshness_wait",
        })
    }
}

/// One closed generation-2 client capability advertisement.
///
/// The capability set validates every value against the layout-1 requirements.
/// Higher client path ceilings do not grant authority or alter the frozen
/// server path ceilings, and the server may select only an advertised tar
/// encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceClientCapabilityV2 {
    WorkspaceLayout {
        version: u16,
    },
    AtomicRootPublication {
        version: u16,
    },
    PathCeilings {
        version: u16,
        max_component_utf8_bytes: u16,
        max_component_utf16_units: u16,
        max_path_utf8_bytes: u16,
        max_path_utf16_units: u16,
    },
    TarEncodings {
        version: u16,
        encodings: Vec<TarContentEncoding>,
    },
    FreshnessWait {
        version: u16,
    },
}

impl WorkspaceClientCapabilityV2 {
    pub fn kind(&self) -> WorkspaceClientCapabilityKindV2 {
        match self {
            Self::WorkspaceLayout { .. } => WorkspaceClientCapabilityKindV2::WorkspaceLayout,
            Self::AtomicRootPublication { .. } => {
                WorkspaceClientCapabilityKindV2::AtomicRootPublication
            }
            Self::PathCeilings { .. } => WorkspaceClientCapabilityKindV2::PathCeilings,
            Self::TarEncodings { .. } => WorkspaceClientCapabilityKindV2::TarEncodings,
            Self::FreshnessWait { .. } => WorkspaceClientCapabilityKindV2::FreshnessWait,
        }
    }
}

/// Validated, bounded capability set for a generation-2 session request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkspaceClientCapabilitiesV2(Vec<WorkspaceClientCapabilityV2>);

impl WorkspaceClientCapabilitiesV2 {
    pub fn new(
        capabilities: Vec<WorkspaceClientCapabilityV2>,
    ) -> Result<Self, WorkspaceApiV2ValidationError> {
        validate_capability_count(capabilities.len())?;
        let encoded_length = serde_json::to_vec(&capabilities)
            .expect("serializing typed capabilities cannot fail")
            .len();
        if encoded_length > MAX_WORKSPACE_CLIENT_CAPABILITIES_V2_ENCODED_BYTES {
            return Err(WorkspaceApiV2ValidationError::CapabilityEncodingTooLarge {
                actual: encoded_length,
            });
        }

        let mut seen = BTreeSet::new();
        for capability in &capabilities {
            let kind = capability.kind();
            if !seen.insert(kind) {
                return Err(WorkspaceApiV2ValidationError::DuplicateCapability { kind });
            }
            validate_capability(capability)?;
        }
        for kind in [
            WorkspaceClientCapabilityKindV2::WorkspaceLayout,
            WorkspaceClientCapabilityKindV2::AtomicRootPublication,
            WorkspaceClientCapabilityKindV2::PathCeilings,
            WorkspaceClientCapabilityKindV2::TarEncodings,
        ] {
            if !seen.contains(&kind) {
                return Err(WorkspaceApiV2ValidationError::MissingCapability { kind });
            }
        }
        Ok(Self(capabilities))
    }

    pub fn workspace_layout_v1(include_freshness_wait: bool) -> Self {
        let mut capabilities = vec![
            WorkspaceClientCapabilityV2::WorkspaceLayout {
                version: WORKSPACE_CAPABILITY_VERSION_V1,
            },
            WorkspaceClientCapabilityV2::AtomicRootPublication {
                version: WORKSPACE_CAPABILITY_VERSION_V1,
            },
            WorkspaceClientCapabilityV2::PathCeilings {
                version: WORKSPACE_CAPABILITY_VERSION_V1,
                max_component_utf8_bytes: REQUIRED_MAX_COMPONENT_UTF8_BYTES,
                max_component_utf16_units: REQUIRED_MAX_COMPONENT_UTF16_UNITS,
                max_path_utf8_bytes: REQUIRED_MAX_PATH_UTF8_BYTES,
                max_path_utf16_units: REQUIRED_MAX_PATH_UTF16_UNITS,
            },
            WorkspaceClientCapabilityV2::TarEncodings {
                version: WORKSPACE_CAPABILITY_VERSION_V1,
                encodings: vec![TarContentEncoding::Identity, TarContentEncoding::Zstd],
            },
        ];
        if include_freshness_wait {
            capabilities.push(WorkspaceClientCapabilityV2::FreshnessWait {
                version: WORKSPACE_CAPABILITY_VERSION_V1,
            });
        }
        Self::new(capabilities).expect("the frozen layout-1 capabilities are valid")
    }

    pub fn capabilities(&self) -> &[WorkspaceClientCapabilityV2] {
        &self.0
    }

    pub fn supports_freshness_wait(&self) -> bool {
        self.freshness_wait_version().is_some()
    }

    pub fn freshness_wait_version(&self) -> Option<u16> {
        self.0.iter().find_map(|capability| match capability {
            WorkspaceClientCapabilityV2::FreshnessWait { version } => Some(*version),
            _ => None,
        })
    }

    pub fn supports_tar_encoding(&self, encoding: TarContentEncoding) -> bool {
        self.0.iter().any(|capability| match capability {
            WorkspaceClientCapabilityV2::TarEncodings { encodings, .. } => {
                encodings.contains(&encoding)
            }
            _ => false,
        })
    }

    pub fn validate_for_freshness(
        &self,
        freshness_requirement: &FreshnessRequirement,
    ) -> Result<(), WorkspaceApiV2ValidationError> {
        if freshness_requirement.on_stale == StaleSessionBehavior::WaitThenFail
            && !self.supports_freshness_wait()
        {
            return Err(WorkspaceApiV2ValidationError::FreshnessWaitRequired);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for WorkspaceClientCapabilitiesV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let capabilities = Vec::<WorkspaceClientCapabilityV2>::deserialize(deserializer)?;
        Self::new(capabilities).map_err(serde::de::Error::custom)
    }
}

/// Body for `POST /v2/workspace-profile-sessions`.
///
/// Profile mapping and absolute-root fields are intentionally absent and
/// rejected by strict deserialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceProfileSessionRequestV2 {
    api_generation: u16,
    capabilities: WorkspaceClientCapabilitiesV2,
}

impl WorkspaceProfileSessionRequestV2 {
    pub fn new(capabilities: WorkspaceClientCapabilitiesV2) -> Self {
        Self {
            api_generation: WORKSPACE_HTTP_API_GENERATION_V2,
            capabilities,
        }
    }

    pub fn decode_json(input: &[u8]) -> Result<Self, WorkspaceApiV2DecodeError> {
        if input.len() > MAX_WORKSPACE_SESSION_REQUEST_V2_BYTES {
            return Err(WorkspaceApiV2DecodeError::RequestEncodingTooLarge {
                actual: input.len(),
            });
        }
        serde_json::from_slice(input)
            .map_err(|error| WorkspaceApiV2DecodeError::InvalidJson(error.to_string()))
    }

    pub fn api_generation(&self) -> u16 {
        self.api_generation
    }

    pub fn capabilities(&self) -> &WorkspaceClientCapabilitiesV2 {
        &self.capabilities
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceProfileSessionRequestV2Wire {
    api_generation: u16,
    capabilities: WorkspaceClientCapabilitiesV2,
}

impl<'de> Deserialize<'de> for WorkspaceProfileSessionRequestV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceProfileSessionRequestV2Wire::deserialize(deserializer)?;
        validate_api_generation(wire.api_generation).map_err(serde::de::Error::custom)?;
        Ok(Self::new(wire.capabilities))
    }
}

/// Authenticated generation-2 session response with its complete sealed map.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceProfileSessionV2 {
    api_generation: u16,
    session_id: SessionId,
    opaque_capability: String,
    expires_at: String,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    session_layout: SessionLayout,
}

impl WorkspaceProfileSessionV2 {
    pub fn new(
        session_id: SessionId,
        opaque_capability: impl Into<String>,
        expires_at: impl Into<String>,
        profile_id: WorkspaceProfileId,
        profile_revision: u64,
        session_layout: SessionLayout,
    ) -> Result<Self, WorkspaceApiV2ValidationError> {
        let session = Self {
            api_generation: WORKSPACE_HTTP_API_GENERATION_V2,
            session_id,
            opaque_capability: opaque_capability.into(),
            expires_at: expires_at.into(),
            profile_id,
            profile_revision,
            session_layout,
        };
        session.validate()?;
        Ok(session)
    }

    pub fn decode_json(input: &[u8]) -> Result<Self, WorkspaceApiV2DecodeError> {
        serde_json::from_slice(input)
            .map_err(|error| WorkspaceApiV2DecodeError::InvalidJson(error.to_string()))
    }

    pub fn validate(&self) -> Result<(), WorkspaceApiV2ValidationError> {
        validate_api_generation(self.api_generation)?;
        validate_nonempty("session_id", self.session_id.as_str())?;
        validate_nonempty("opaque_capability", &self.opaque_capability)?;
        crate::validate_canonical_utc_timestamp("expires_at", &self.expires_at)?;
        validate_profile_revision(self.profile_revision)?;
        self.session_layout
            .verify_profile_context(&self.profile_id, self.profile_revision)?;
        Ok(())
    }

    pub fn api_generation(&self) -> u16 {
        self.api_generation
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn opaque_capability(&self) -> &str {
        &self.opaque_capability
    }

    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    pub fn profile_id(&self) -> &WorkspaceProfileId {
        &self.profile_id
    }

    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    pub fn session_layout(&self) -> &SessionLayout {
        &self.session_layout
    }
}

impl Debug for WorkspaceProfileSessionV2 {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceProfileSessionV2")
            .field("api_generation", &self.api_generation)
            .field("session_id", &self.session_id)
            .field("opaque_capability", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("profile_id", &self.profile_id)
            .field("profile_revision", &self.profile_revision)
            .field("session_layout", &self.session_layout)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceProfileSessionV2Wire {
    api_generation: u16,
    session_id: SessionId,
    opaque_capability: String,
    expires_at: String,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    session_layout: SessionLayout,
}

impl<'de> Deserialize<'de> for WorkspaceProfileSessionV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceProfileSessionV2Wire::deserialize(deserializer)?;
        validate_api_generation(wire.api_generation).map_err(serde::de::Error::custom)?;
        Self::new(
            wire.session_id,
            wire.opaque_capability,
            wire.expires_at,
            wire.profile_id,
            wire.profile_revision,
            wire.session_layout,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Authenticated generation-2 status. The layout reference is available
/// before any export body is opened. Replicas retain the server-configured
/// source order; they are never sorted or deduplicated during validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceSessionStatusV2 {
    api_generation: u16,
    versions: ComponentVersions,
    session_id: SessionId,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    state: SandboxSessionState,
    freshness_requirement: FreshnessRequirement,
    replicas: Vec<ReplicaFreshnessStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    export_attempt_limits: Option<ExportAttemptLimits>,
    error: Option<SessionProtocolError>,
    updated_at: String,
}

impl WorkspaceSessionStatusV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: &WorkspaceProfileSessionV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
        versions: ComponentVersions,
        state: SandboxSessionState,
        freshness_requirement: FreshnessRequirement,
        replicas: Vec<ReplicaFreshnessStatus>,
        export_attempt_limits: Option<ExportAttemptLimits>,
        error: Option<SessionProtocolError>,
        updated_at: impl Into<String>,
    ) -> Result<Self, WorkspaceApiV2ValidationError> {
        let status = Self {
            api_generation: WORKSPACE_HTTP_API_GENERATION_V2,
            versions,
            session_id: session.session_id.clone(),
            profile_id: session.profile_id.clone(),
            profile_revision: session.profile_revision,
            layout_version: session.session_layout.layout_version(),
            layout_digest: session.session_layout.layout_digest().clone(),
            state,
            freshness_requirement,
            replicas,
            export_attempt_limits,
            error,
            updated_at: updated_at.into(),
        };
        status.validate_against(session, capabilities)?;
        Ok(status)
    }

    pub fn decode_json(
        input: &[u8],
        session: &WorkspaceProfileSessionV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<Self, WorkspaceApiV2DecodeError> {
        let status: Self = serde_json::from_slice(input)
            .map_err(|error| WorkspaceApiV2DecodeError::InvalidJson(error.to_string()))?;
        status
            .validate_against(session, capabilities)
            .map_err(WorkspaceApiV2DecodeError::Contract)?;
        Ok(status)
    }

    pub fn validate(&self) -> Result<(), WorkspaceApiV2ValidationError> {
        validate_api_generation(self.api_generation)?;
        self.versions.validate_required()?;
        validate_nonempty("session_id", self.session_id.as_str())?;
        validate_profile_revision(self.profile_revision)?;
        validate_layout_reference(self.layout_version, &self.layout_digest)?;
        validate_status_replicas(&self.replicas)?;
        if let Some(limits) = &self.export_attempt_limits {
            limits.validate()?;
        }
        if let Some(error) = &self.error {
            validate_nonempty("error.message", &error.message)?;
        }
        crate::validate_canonical_utc_timestamp("updated_at", &self.updated_at)?;
        Ok(())
    }

    pub fn validate_against(
        &self,
        session: &WorkspaceProfileSessionV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<(), WorkspaceApiV2ValidationError> {
        self.validate()?;
        session.validate()?;
        if self.session_id != session.session_id
            || self.profile_id != session.profile_id
            || self.profile_revision != session.profile_revision
            || self.layout_version != session.session_layout.layout_version()
            || self.layout_digest != *session.session_layout.layout_digest()
        {
            return Err(WorkspaceApiV2ValidationError::SessionBindingMismatch);
        }
        capabilities.validate_for_freshness(&self.freshness_requirement)
    }

    pub fn versions(&self) -> ComponentVersions {
        self.versions
    }

    pub fn api_generation(&self) -> u16 {
        self.api_generation
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn profile_id(&self) -> &WorkspaceProfileId {
        &self.profile_id
    }

    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn freshness_requirement(&self) -> &FreshnessRequirement {
        &self.freshness_requirement
    }

    pub fn state(&self) -> SandboxSessionState {
        self.state
    }

    pub fn replicas(&self) -> &[ReplicaFreshnessStatus] {
        &self.replicas
    }

    pub fn export_attempt_limits(&self) -> Option<&ExportAttemptLimits> {
        self.export_attempt_limits.as_ref()
    }

    pub fn error(&self) -> Option<&SessionProtocolError> {
        self.error.as_ref()
    }

    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceSessionStatusV2Wire {
    api_generation: u16,
    versions: StrictComponentVersionsWire,
    session_id: SessionId,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    state: SandboxSessionState,
    freshness_requirement: StrictFreshnessRequirementWire,
    replicas: Vec<StrictReplicaFreshnessStatusWire>,
    #[serde(default)]
    export_attempt_limits: Option<StrictExportAttemptLimitsWire>,
    error: Option<StrictSessionProtocolErrorWire>,
    updated_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictComponentVersionsWire {
    session: u16,
    replica: u16,
    export_metadata: u16,
    writable_session_store: u16,
    canonical: u16,
    path: u16,
    changeset: u16,
}

impl From<StrictComponentVersionsWire> for ComponentVersions {
    fn from(wire: StrictComponentVersionsWire) -> Self {
        Self {
            session: wire.session,
            replica: wire.replica,
            export_metadata: wire.export_metadata,
            writable_session_store: wire.writable_session_store,
            canonical: wire.canonical,
            path: wire.path,
            changeset: wire.changeset,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictFreshnessRequirementWire {
    max_age_seconds: u64,
    on_stale: StaleSessionBehavior,
    wait_timeout_seconds: u64,
}

impl From<StrictFreshnessRequirementWire> for FreshnessRequirement {
    fn from(wire: StrictFreshnessRequirementWire) -> Self {
        Self {
            max_age_seconds: wire.max_age_seconds,
            on_stale: wire.on_stale,
            wait_timeout_seconds: wire.wait_timeout_seconds,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictReplicaFreshnessStatusWire {
    source_connection_id: SourceConnectionId,
    state: ReplicaFreshnessState,
    coverage_complete: bool,
    provider_observed_through: Option<String>,
    last_successful_sync_at: Option<String>,
    last_repair_at: Option<String>,
    pending_events: u64,
    backlog: u64,
    provider_cooldown_until: Option<String>,
}

impl From<StrictReplicaFreshnessStatusWire> for ReplicaFreshnessStatus {
    fn from(wire: StrictReplicaFreshnessStatusWire) -> Self {
        Self {
            source_connection_id: wire.source_connection_id,
            state: wire.state,
            coverage_complete: wire.coverage_complete,
            provider_observed_through: wire.provider_observed_through,
            last_successful_sync_at: wire.last_successful_sync_at,
            last_repair_at: wire.last_repair_at,
            pending_events: wire.pending_events,
            backlog: wire.backlog,
            provider_cooldown_until: wire.provider_cooldown_until,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictExportAttemptLimitsWire {
    max_files: u64,
    max_directories: u64,
    max_content_bytes: u64,
}

impl From<StrictExportAttemptLimitsWire> for ExportAttemptLimits {
    fn from(wire: StrictExportAttemptLimitsWire) -> Self {
        Self {
            max_files: wire.max_files,
            max_directories: wire.max_directories,
            max_content_bytes: wire.max_content_bytes,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictSessionProtocolErrorWire {
    code: SessionErrorCode,
    message: String,
    retriable: bool,
    retry_after_seconds: Option<u64>,
}

impl From<StrictSessionProtocolErrorWire> for SessionProtocolError {
    fn from(wire: StrictSessionProtocolErrorWire) -> Self {
        Self {
            code: wire.code,
            message: wire.message,
            retriable: wire.retriable,
            retry_after_seconds: wire.retry_after_seconds,
        }
    }
}

impl<'de> Deserialize<'de> for WorkspaceSessionStatusV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceSessionStatusV2Wire::deserialize(deserializer)?;
        let status = Self {
            api_generation: wire.api_generation,
            versions: wire.versions.into(),
            session_id: wire.session_id,
            profile_id: wire.profile_id,
            profile_revision: wire.profile_revision,
            layout_version: wire.layout_version,
            layout_digest: wire.layout_digest,
            state: wire.state,
            freshness_requirement: wire.freshness_requirement.into(),
            replicas: wire.replicas.into_iter().map(Into::into).collect(),
            export_attempt_limits: wire.export_attempt_limits.map(Into::into),
            error: wire.error.map(Into::into),
            updated_at: wire.updated_at,
        };
        status.validate().map_err(serde::de::Error::custom)?;
        Ok(status)
    }
}

/// Authenticated generation-2 export offer. The existing immutable offer facts
/// remain intact beneath a profile and layout binding. Its contiguous source
/// generation order must exactly match the status replica order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceExportOfferV2 {
    api_generation: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    offer: SealedExportOffer,
}

impl WorkspaceExportOfferV2 {
    pub fn new(
        session: &WorkspaceProfileSessionV2,
        status: &WorkspaceSessionStatusV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
        offer: SealedExportOffer,
    ) -> Result<Self, WorkspaceApiV2ValidationError> {
        let workspace_offer = Self {
            api_generation: WORKSPACE_HTTP_API_GENERATION_V2,
            profile_id: session.profile_id.clone(),
            profile_revision: session.profile_revision,
            layout_version: session.session_layout.layout_version(),
            layout_digest: session.session_layout.layout_digest().clone(),
            offer,
        };
        workspace_offer.validate_against(session, status, capabilities)?;
        Ok(workspace_offer)
    }

    pub fn decode_json(
        input: &[u8],
        session: &WorkspaceProfileSessionV2,
        status: &WorkspaceSessionStatusV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<Self, WorkspaceApiV2DecodeError> {
        let offer: Self = serde_json::from_slice(input)
            .map_err(|error| WorkspaceApiV2DecodeError::InvalidJson(error.to_string()))?;
        offer
            .validate_against(session, status, capabilities)
            .map_err(WorkspaceApiV2DecodeError::Contract)?;
        Ok(offer)
    }

    pub fn validate(&self) -> Result<(), WorkspaceApiV2ValidationError> {
        validate_api_generation(self.api_generation)?;
        validate_profile_revision(self.profile_revision)?;
        validate_layout_reference(self.layout_version, &self.layout_digest)?;
        self.offer.validate()?;
        validate_offer_generation_ids(&self.offer.source_generations)?;
        Ok(())
    }

    pub fn validate_against(
        &self,
        session: &WorkspaceProfileSessionV2,
        status: &WorkspaceSessionStatusV2,
        capabilities: &WorkspaceClientCapabilitiesV2,
    ) -> Result<(), WorkspaceApiV2ValidationError> {
        self.validate()?;
        status.validate_against(session, capabilities)?;
        if self.profile_id != session.profile_id
            || self.profile_revision != session.profile_revision
            || self.layout_version != session.session_layout.layout_version()
            || self.layout_digest != *session.session_layout.layout_digest()
            || self.offer.session_id != session.session_id
            || self.offer.session_id != status.session_id
            || self.offer.versions != status.versions
        {
            return Err(WorkspaceApiV2ValidationError::OfferBindingMismatch);
        }
        if !capabilities.supports_tar_encoding(self.offer.content_encoding) {
            return Err(WorkspaceApiV2ValidationError::UnsupportedOfferEncoding);
        }
        validate_offer_sources_against_status(status, &self.offer)?;
        Ok(())
    }

    pub fn profile_id(&self) -> &WorkspaceProfileId {
        &self.profile_id
    }

    pub fn api_generation(&self) -> u16 {
        self.api_generation
    }

    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn offer(&self) -> &SealedExportOffer {
        &self.offer
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceExportOfferV2Wire {
    api_generation: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    offer: StrictSealedExportOfferWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictSealedExportOfferWire {
    versions: StrictComponentVersionsWire,
    session_id: SessionId,
    export_attempt_id: ExportAttemptId,
    source_generations: Vec<StrictOrderedSourceGenerationWire>,
    media_type: String,
    content_encoding: TarContentEncoding,
    limits: StrictExportAttemptLimitsWire,
    control_entry_count: u64,
    file_count: u64,
    directory_count: u64,
    archive_entry_count: u64,
    selected_content_bytes: u64,
    inventory_sha256: String,
    writable_metadata_sha256: String,
    sealed_at: String,
    expires_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictOrderedSourceGenerationWire {
    ordinal: u32,
    source_connection_id: SourceConnectionId,
    source_generation_id: SourceGenerationId,
}

impl From<StrictOrderedSourceGenerationWire> for OrderedSourceGeneration {
    fn from(wire: StrictOrderedSourceGenerationWire) -> Self {
        Self {
            ordinal: wire.ordinal,
            source_connection_id: wire.source_connection_id,
            source_generation_id: wire.source_generation_id,
        }
    }
}

impl From<StrictSealedExportOfferWire> for SealedExportOffer {
    fn from(wire: StrictSealedExportOfferWire) -> Self {
        Self {
            versions: wire.versions.into(),
            session_id: wire.session_id,
            export_attempt_id: wire.export_attempt_id,
            source_generations: wire
                .source_generations
                .into_iter()
                .map(Into::into)
                .collect(),
            media_type: wire.media_type,
            content_encoding: wire.content_encoding,
            limits: wire.limits.into(),
            control_entry_count: wire.control_entry_count,
            file_count: wire.file_count,
            directory_count: wire.directory_count,
            archive_entry_count: wire.archive_entry_count,
            selected_content_bytes: wire.selected_content_bytes,
            inventory_sha256: wire.inventory_sha256,
            writable_metadata_sha256: wire.writable_metadata_sha256,
            sealed_at: wire.sealed_at,
            expires_at: wire.expires_at,
        }
    }
}

impl<'de> Deserialize<'de> for WorkspaceExportOfferV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceExportOfferV2Wire::deserialize(deserializer)?;
        let offer = Self {
            api_generation: wire.api_generation,
            profile_id: wire.profile_id,
            profile_revision: wire.profile_revision,
            layout_version: wire.layout_version,
            layout_digest: wire.layout_digest,
            offer: wire.offer.into(),
        };
        offer.validate().map_err(serde::de::Error::custom)?;
        Ok(offer)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCompatibilityErrorCodeV2 {
    UpdateRequired,
    IncompatibleCapabilities,
}

/// Stable post-authentication compatibility failure for a profile session.
///
/// It contains no profile, scope, mount, target, or host-root disclosure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceCompatibilityErrorV2 {
    code: WorkspaceCompatibilityErrorCodeV2,
    message: String,
    retriable: bool,
    required_api_generation: u16,
    minimum_layout_version: u16,
    required_capabilities: WorkspaceClientCapabilitiesV2,
}

impl WorkspaceCompatibilityErrorV2 {
    pub fn update_required(
        message: impl Into<String>,
    ) -> Result<Self, WorkspaceApiV2ValidationError> {
        Self::new(
            WorkspaceCompatibilityErrorCodeV2::UpdateRequired,
            message,
            false,
        )
    }

    pub fn incompatible_capabilities(
        message: impl Into<String>,
        require_freshness_wait: bool,
    ) -> Result<Self, WorkspaceApiV2ValidationError> {
        Self::new(
            WorkspaceCompatibilityErrorCodeV2::IncompatibleCapabilities,
            message,
            require_freshness_wait,
        )
    }

    fn new(
        code: WorkspaceCompatibilityErrorCodeV2,
        message: impl Into<String>,
        require_freshness_wait: bool,
    ) -> Result<Self, WorkspaceApiV2ValidationError> {
        let error = Self {
            code,
            message: message.into(),
            retriable: false,
            required_api_generation: WORKSPACE_HTTP_API_GENERATION_V2,
            minimum_layout_version: WORKSPACE_LAYOUT_VERSION,
            required_capabilities: WorkspaceClientCapabilitiesV2::workspace_layout_v1(
                require_freshness_wait,
            ),
        };
        error.validate()?;
        Ok(error)
    }

    pub fn validate(&self) -> Result<(), WorkspaceApiV2ValidationError> {
        validate_nonempty("message", &self.message)?;
        if self.retriable {
            return Err(WorkspaceApiV2ValidationError::CompatibilityErrorRetriable);
        }
        validate_api_generation(self.required_api_generation)?;
        if self.minimum_layout_version != WORKSPACE_LAYOUT_VERSION {
            return Err(WorkspaceApiV2ValidationError::UnsupportedLayoutVersion {
                actual: self.minimum_layout_version,
            });
        }
        WorkspaceClientCapabilitiesV2::new(self.required_capabilities.0.clone())?;
        Ok(())
    }

    pub fn code(&self) -> WorkspaceCompatibilityErrorCodeV2 {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retriable(&self) -> bool {
        self.retriable
    }

    pub fn required_api_generation(&self) -> u16 {
        self.required_api_generation
    }

    pub fn minimum_layout_version(&self) -> u16 {
        self.minimum_layout_version
    }

    pub fn required_capabilities(&self) -> &WorkspaceClientCapabilitiesV2 {
        &self.required_capabilities
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCompatibilityErrorV2Wire {
    code: WorkspaceCompatibilityErrorCodeV2,
    message: String,
    retriable: bool,
    required_api_generation: u16,
    minimum_layout_version: u16,
    required_capabilities: WorkspaceClientCapabilitiesV2,
}

impl<'de> Deserialize<'de> for WorkspaceCompatibilityErrorV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceCompatibilityErrorV2Wire::deserialize(deserializer)?;
        let error = Self {
            code: wire.code,
            message: wire.message,
            retriable: wire.retriable,
            required_api_generation: wire.required_api_generation,
            minimum_layout_version: wire.minimum_layout_version,
            required_capabilities: wire.required_capabilities,
        };
        error.validate().map_err(serde::de::Error::custom)?;
        Ok(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceApiV2ValidationError {
    UnsupportedApiGeneration {
        actual: u16,
    },
    UnsupportedLayoutVersion {
        actual: u16,
    },
    CapabilityCount {
        actual: usize,
    },
    CapabilityEncodingTooLarge {
        actual: usize,
    },
    MissingCapability {
        kind: WorkspaceClientCapabilityKindV2,
    },
    DuplicateCapability {
        kind: WorkspaceClientCapabilityKindV2,
    },
    IncompatibleCapability {
        kind: WorkspaceClientCapabilityKindV2,
    },
    FreshnessWaitRequired,
    EmptyField(&'static str),
    ZeroProfileRevision,
    SessionBindingMismatch,
    OfferBindingMismatch,
    DuplicateStatusSourceId {
        index: usize,
    },
    DuplicateOfferSourceGenerationId {
        index: usize,
    },
    SourceSetMismatch,
    SourceOrderMismatch,
    UnsupportedOfferEncoding,
    CompatibilityErrorRetriable,
    Layout(WorkspaceLayoutError),
    Scope(ScopeContractError),
    Versions(VersionCompatibilityError),
}

impl Display for WorkspaceApiV2ValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedApiGeneration { actual } => write!(
                formatter,
                "workspace HTTP API generation {actual} is unsupported"
            ),
            Self::UnsupportedLayoutVersion { actual } => {
                write!(
                    formatter,
                    "workspace layout version {actual} is unsupported"
                )
            }
            Self::CapabilityCount { actual } => write!(
                formatter,
                "workspace client capability count is {actual}; expected {MIN_WORKSPACE_CLIENT_CAPABILITIES_V2} through {MAX_WORKSPACE_CLIENT_CAPABILITIES_V2}"
            ),
            Self::CapabilityEncodingTooLarge { actual } => write!(
                formatter,
                "workspace client capabilities encode to {actual} bytes; maximum is {MAX_WORKSPACE_CLIENT_CAPABILITIES_V2_ENCODED_BYTES}"
            ),
            Self::MissingCapability { kind } => {
                write!(
                    formatter,
                    "required workspace client capability {kind} is missing"
                )
            }
            Self::DuplicateCapability { kind } => {
                write!(
                    formatter,
                    "workspace client capability {kind} is duplicated"
                )
            }
            Self::IncompatibleCapability { kind } => write!(
                formatter,
                "workspace client capability {kind} is incompatible with layout 1"
            ),
            Self::FreshnessWaitRequired => {
                formatter.write_str("freshness_wait capability version 1 is required")
            }
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::ZeroProfileRevision => formatter.write_str("profile revision must be positive"),
            Self::SessionBindingMismatch => formatter
                .write_str("workspace status does not match the sealed profile session and layout"),
            Self::OfferBindingMismatch => formatter.write_str(
                "workspace export offer does not match the sealed session, status, and layout",
            ),
            Self::DuplicateStatusSourceId { index } => write!(
                formatter,
                "workspace status replica at index {index} duplicates a source connection ID"
            ),
            Self::DuplicateOfferSourceGenerationId { index } => write!(
                formatter,
                "workspace export source generation at index {index} duplicates a source generation ID"
            ),
            Self::SourceSetMismatch => formatter.write_str(
                "workspace export source set does not match the status replica source set",
            ),
            Self::SourceOrderMismatch => formatter.write_str(
                "workspace export source order does not match the status replica source order",
            ),
            Self::UnsupportedOfferEncoding => {
                formatter.write_str("workspace export offer selects an unsupported tar encoding")
            }
            Self::CompatibilityErrorRetriable => {
                formatter.write_str("workspace compatibility errors must not be retriable")
            }
            Self::Layout(error) => Display::fmt(error, formatter),
            Self::Scope(error) => Display::fmt(error, formatter),
            Self::Versions(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for WorkspaceApiV2ValidationError {}

impl From<WorkspaceLayoutError> for WorkspaceApiV2ValidationError {
    fn from(error: WorkspaceLayoutError) -> Self {
        Self::Layout(error)
    }
}

impl From<ScopeContractError> for WorkspaceApiV2ValidationError {
    fn from(error: ScopeContractError) -> Self {
        Self::Scope(error)
    }
}

impl From<VersionCompatibilityError> for WorkspaceApiV2ValidationError {
    fn from(error: VersionCompatibilityError) -> Self {
        Self::Versions(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceApiV2DecodeError {
    RequestEncodingTooLarge { actual: usize },
    InvalidJson(String),
    Contract(WorkspaceApiV2ValidationError),
}

impl Display for WorkspaceApiV2DecodeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestEncodingTooLarge { actual } => write!(
                formatter,
                "workspace session request is {actual} bytes; maximum is {MAX_WORKSPACE_SESSION_REQUEST_V2_BYTES}"
            ),
            Self::InvalidJson(error) => write!(formatter, "invalid workspace API v2 JSON: {error}"),
            Self::Contract(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for WorkspaceApiV2DecodeError {}

fn validate_capability_count(actual: usize) -> Result<(), WorkspaceApiV2ValidationError> {
    if !(MIN_WORKSPACE_CLIENT_CAPABILITIES_V2..=MAX_WORKSPACE_CLIENT_CAPABILITIES_V2)
        .contains(&actual)
    {
        return Err(WorkspaceApiV2ValidationError::CapabilityCount { actual });
    }
    Ok(())
}

fn validate_capability(
    capability: &WorkspaceClientCapabilityV2,
) -> Result<(), WorkspaceApiV2ValidationError> {
    let compatible = match capability {
        WorkspaceClientCapabilityV2::WorkspaceLayout { version }
        | WorkspaceClientCapabilityV2::AtomicRootPublication { version }
        | WorkspaceClientCapabilityV2::FreshnessWait { version } => {
            *version == WORKSPACE_CAPABILITY_VERSION_V1
        }
        WorkspaceClientCapabilityV2::PathCeilings {
            version,
            max_component_utf8_bytes,
            max_component_utf16_units,
            max_path_utf8_bytes,
            max_path_utf16_units,
        } => {
            *version == WORKSPACE_CAPABILITY_VERSION_V1
                && *max_component_utf8_bytes >= REQUIRED_MAX_COMPONENT_UTF8_BYTES
                && *max_component_utf16_units >= REQUIRED_MAX_COMPONENT_UTF16_UNITS
                && *max_path_utf8_bytes >= REQUIRED_MAX_PATH_UTF8_BYTES
                && *max_path_utf16_units >= REQUIRED_MAX_PATH_UTF16_UNITS
        }
        WorkspaceClientCapabilityV2::TarEncodings { version, encodings } => {
            *version == WORKSPACE_CAPABILITY_VERSION_V1
                && !encodings.is_empty()
                && encodings.len() <= 2
                && encodings.windows(2).all(|pair| pair[0] < pair[1])
        }
    };
    if compatible {
        Ok(())
    } else {
        Err(WorkspaceApiV2ValidationError::IncompatibleCapability {
            kind: capability.kind(),
        })
    }
}

fn validate_api_generation(actual: u16) -> Result<(), WorkspaceApiV2ValidationError> {
    if actual != WORKSPACE_HTTP_API_GENERATION_V2 {
        return Err(WorkspaceApiV2ValidationError::UnsupportedApiGeneration { actual });
    }
    Ok(())
}

fn validate_profile_revision(actual: u64) -> Result<(), WorkspaceApiV2ValidationError> {
    if actual == 0 {
        return Err(WorkspaceApiV2ValidationError::ZeroProfileRevision);
    }
    Ok(())
}

fn validate_layout_reference(
    layout_version: u16,
    _layout_digest: &LayoutDigest,
) -> Result<(), WorkspaceApiV2ValidationError> {
    if layout_version != WORKSPACE_LAYOUT_VERSION {
        return Err(WorkspaceApiV2ValidationError::UnsupportedLayoutVersion {
            actual: layout_version,
        });
    }
    Ok(())
}

fn validate_nonempty(
    field: &'static str,
    value: &str,
) -> Result<(), WorkspaceApiV2ValidationError> {
    if value.is_empty() {
        return Err(WorkspaceApiV2ValidationError::EmptyField(field));
    }
    Ok(())
}

fn validate_status_replicas(
    replicas: &[ReplicaFreshnessStatus],
) -> Result<(), WorkspaceApiV2ValidationError> {
    let mut source_ids = BTreeSet::new();
    for (index, replica) in replicas.iter().enumerate() {
        validate_nonempty(
            "source_connection_id",
            replica.source_connection_id.as_str(),
        )?;
        if !source_ids.insert(&replica.source_connection_id) {
            return Err(WorkspaceApiV2ValidationError::DuplicateStatusSourceId { index });
        }
    }
    Ok(())
}

fn validate_offer_sources_against_status(
    status: &WorkspaceSessionStatusV2,
    offer: &SealedExportOffer,
) -> Result<(), WorkspaceApiV2ValidationError> {
    let status_source_ids = status
        .replicas
        .iter()
        .map(|replica| &replica.source_connection_id)
        .collect::<Vec<_>>();
    let offer_source_ids = offer
        .source_generations
        .iter()
        .map(|generation| &generation.source_connection_id)
        .collect::<Vec<_>>();

    let status_source_set = status_source_ids.iter().copied().collect::<BTreeSet<_>>();
    let offer_source_set = offer_source_ids.iter().copied().collect::<BTreeSet<_>>();
    if status_source_set != offer_source_set {
        return Err(WorkspaceApiV2ValidationError::SourceSetMismatch);
    }
    if status_source_ids != offer_source_ids {
        return Err(WorkspaceApiV2ValidationError::SourceOrderMismatch);
    }
    Ok(())
}

fn validate_offer_generation_ids(
    source_generations: &[OrderedSourceGeneration],
) -> Result<(), WorkspaceApiV2ValidationError> {
    let mut generation_ids = BTreeSet::new();
    for (index, generation) in source_generations.iter().enumerate() {
        if !generation_ids.insert(&generation.source_generation_id) {
            return Err(WorkspaceApiV2ValidationError::DuplicateOfferSourceGenerationId { index });
        }
    }
    Ok(())
}
