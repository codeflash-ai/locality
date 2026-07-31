//! HTTP-neutral transport contracts for persistent generation delivery.
//!
//! The private service owns authentication, authorization, routes, and lease
//! persistence. This module owns only bounded portable request/response values.
//! An adapter must authenticate every response before exposing it through the
//! local transport trait.

use std::fmt::{Debug, Display, Formatter};

use locality_core::portable::{SourceConnectionId, SourceGenerationId};
use locality_core::workspace_layout::PortableMountId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::FreshnessEpoch;
use crate::freshness_delivery::{
    GenerationDeltaTerminalReceipt, GenerationFileIdentity, MAX_DELIVERY_ID_BYTES,
    MAX_DELIVERY_TIMESTAMP_BYTES,
};

pub const GENERATION_TRANSPORT_FORMAT_VERSION: u16 = 1;
pub const GENERATION_TRANSPORT_READER_VERSION: u16 = 1;
pub const MAX_GENERATION_TRANSPORT_CAPABILITIES_BYTES: usize = 4 * 1024;
pub const MAX_GENERATION_TRANSPORT_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_GENERATION_BODY_WINDOW_BYTES: u64 = 4 * 1024 * 1024;
pub const MIN_GENERATION_PIN_LEASE_SECONDS: u64 = 60;
pub const MAX_GENERATION_PIN_LEASE_SECONDS: u64 = 24 * 60 * 60;
pub const MAX_GENERATION_PIN_LEASES_PER_DEVICE: u16 = 32;
pub const MAX_GENERATION_PIN_RETRY_AFTER_SECONDS: u64 = 60 * 60;
pub const MAX_OPAQUE_DEVICE_SCOPE_ID_BYTES: usize = 128;
pub const MAX_OPAQUE_PIN_LEASE_ID_BYTES: usize = 256;
pub const MAX_GENERATION_PIN_OPERATION_ID_BYTES: usize = 128;

pub const GENERATION_TRANSPORT_CAPABILITIES_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-transport-capabilities-v1.json");
pub const GENERATION_DELIVERY_REQUEST_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-delivery-request-v1.json");
pub const GENERATION_BODY_WINDOW_REQUEST_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-body-window-request-v1.json");
pub const GENERATION_BODY_WINDOW_METADATA_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-body-window-metadata-v1.json");
pub const GENERATION_DELIVERY_ACKNOWLEDGMENT_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-delivery-acknowledgment-v1.json");
pub const GENERATION_PIN_LEASE_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-pin-lease-v1.json");

const fn v1() -> u16 {
    GENERATION_TRANSPORT_FORMAT_VERSION
}

/// The only safe V1 fallback when an exact retained generation cannot be
/// pinned. `UseLatestRetained` still requires the response to contain a lease;
/// it never authorizes an unpinned read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPinFallbackPolicy {
    RequireExact,
    UseLatestRetained,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationBodyWindowCapability {
    pub max_window_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPinLeaseCapability {
    pub min_lease_seconds: u64,
    pub max_lease_seconds: u64,
    pub max_active_leases_per_device: u16,
    pub fallback_policies: Vec<GenerationPinFallbackPolicy>,
}

/// Client offer or server selection. Missing fields decode as the legacy
/// whole-body transport so old responses remain readable. Unknown additive
/// object fields are ignored by Serde; unknown fallback labels decode but fail
/// validation when selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationTransportCapabilities {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_windows: Option<GenerationBodyWindowCapability>,
    #[serde(default)]
    pub terminal_receipt_acknowledgments: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_pin_leases: Option<GenerationPinLeaseCapability>,
}

impl Default for GenerationTransportCapabilities {
    fn default() -> Self {
        Self::legacy()
    }
}

impl GenerationTransportCapabilities {
    pub const fn legacy() -> Self {
        Self {
            format_version: GENERATION_TRANSPORT_FORMAT_VERSION,
            minimum_reader_version: GENERATION_TRANSPORT_READER_VERSION,
            body_windows: None,
            terminal_receipt_acknowledgments: false,
            generation_pin_leases: None,
        }
    }

    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        if input.len() > MAX_GENERATION_TRANSPORT_CAPABILITIES_BYTES {
            return Err(GenerationTransportContractError::EncodingTooLarge {
                actual: input.len(),
                maximum: MAX_GENERATION_TRANSPORT_CAPABILITIES_BYTES,
            });
        }
        let capabilities: Self = serde_json::from_slice(input)
            .map_err(|error| GenerationTransportContractError::InvalidJson(error.to_string()))?;
        capabilities.validate()?;
        Ok(capabilities)
    }

    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        if let Some(body_windows) = &self.body_windows
            && !(1..=MAX_GENERATION_BODY_WINDOW_BYTES).contains(&body_windows.max_window_bytes)
        {
            return Err(GenerationTransportContractError::InvalidBodyWindowLimit {
                actual: body_windows.max_window_bytes,
            });
        }
        if let Some(pin) = &self.generation_pin_leases {
            validate_pin_capability(pin)?;
        }
        let encoded = serde_json::to_vec(self)
            .expect("serializing typed generation transport capabilities cannot fail");
        if encoded.len() > MAX_GENERATION_TRANSPORT_CAPABILITIES_BYTES {
            return Err(GenerationTransportContractError::EncodingTooLarge {
                actual: encoded.len(),
                maximum: MAX_GENERATION_TRANSPORT_CAPABILITIES_BYTES,
            });
        }
        Ok(())
    }

    pub fn validate_selection(
        &self,
        offered: &Self,
    ) -> Result<(), GenerationTransportContractError> {
        self.validate()?;
        offered.validate()?;
        match (&self.body_windows, &offered.body_windows) {
            (Some(selected), Some(offered))
                if selected.max_window_bytes <= offered.max_window_bytes => {}
            (None, _) => {}
            _ => return Err(GenerationTransportContractError::CapabilityNotOffered),
        }
        if self.terminal_receipt_acknowledgments && !offered.terminal_receipt_acknowledgments {
            return Err(GenerationTransportContractError::CapabilityNotOffered);
        }
        match (&self.generation_pin_leases, &offered.generation_pin_leases) {
            (Some(selected), Some(offered))
                if selected.min_lease_seconds >= offered.min_lease_seconds
                    && selected.max_lease_seconds <= offered.max_lease_seconds
                    && selected.max_active_leases_per_device
                        <= offered.max_active_leases_per_device
                    && selected
                        .fallback_policies
                        .iter()
                        .all(|policy| offered.fallback_policies.contains(policy)) => {}
            (None, _) => {}
            _ => return Err(GenerationTransportContractError::CapabilityNotOffered),
        }
        Ok(())
    }
}

/// Portable metadata request for the next complete generation delta.
/// Capability fields are additive, so a prior reader that ignores unknown
/// fields still sees the original mount/source/observed-generation tuple.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationDeliveryRequest {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub mount_id: PortableMountId,
    pub source_connection_id: SourceConnectionId,
    pub observed_generation_id: SourceGenerationId,
    #[serde(default, skip_serializing_if = "is_legacy_capabilities")]
    pub capabilities: GenerationTransportCapabilities,
}

impl GenerationDeliveryRequest {
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        decode_request(input)
    }

    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_identifier("mount_id", self.mount_id.as_str())?;
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier(
            "observed_generation_id",
            self.observed_generation_id.as_str(),
        )?;
        self.capabilities.validate()
    }
}

fn is_legacy_capabilities(capabilities: &GenerationTransportCapabilities) -> bool {
    capabilities == &GenerationTransportCapabilities::legacy()
}

/// Bounded request for one immutable content body window. The complete content
/// identity is repeated so an authenticated adapter cannot accidentally reuse
/// a range response for another projection or version.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationBodyWindowRequest {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub delta_id: String,
    pub terminal_receipt_sha256: String,
    pub content: GenerationFileIdentity,
    pub offset: u64,
    pub max_bytes: u64,
}

impl GenerationBodyWindowRequest {
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        validate_encoding_length(input.len())?;
        let request: Self = serde_json::from_slice(input)
            .map_err(|error| GenerationTransportContractError::InvalidJson(error.to_string()))?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_identifier("delta_id", &self.delta_id)?;
        validate_sha256(&self.terminal_receipt_sha256)?;
        self.content.validate().map_err(|error| {
            GenerationTransportContractError::ContentIdentity(error.to_string())
        })?;
        if !(1..=MAX_GENERATION_BODY_WINDOW_BYTES).contains(&self.max_bytes) {
            return Err(GenerationTransportContractError::InvalidBodyWindowLimit {
                actual: self.max_bytes,
            });
        }
        if self.offset >= self.content.byte_length {
            return Err(GenerationTransportContractError::InvalidBodyRange);
        }
        Ok(())
    }
}

impl Debug for GenerationBodyWindowRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationBodyWindowRequest")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("delta_id", &"<redacted>")
            .field("terminal_receipt_sha256", &"<redacted>")
            .field("content", &"<redacted>")
            .field("offset", &self.offset)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationBodyRange {
    pub offset: u64,
    pub length: u64,
    pub complete: bool,
}

/// Authenticated metadata accompanying a raw body window. Body bytes are not
/// embedded in JSON. The adapter validates this metadata and the body digest
/// before appending the window to staging.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationBodyWindowMetadata {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub delta_id: String,
    pub terminal_receipt_sha256: String,
    pub content: GenerationFileIdentity,
    pub range: GenerationBodyRange,
    pub window_sha256: String,
}

impl GenerationBodyWindowMetadata {
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        validate_encoding_length(input.len())?;
        serde_json::from_slice(input)
            .map_err(|error| GenerationTransportContractError::InvalidJson(error.to_string()))
    }

    pub fn validate_against(
        &self,
        request: &GenerationBodyWindowRequest,
    ) -> Result<(), GenerationTransportContractError> {
        request.validate()?;
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_sha256(&self.window_sha256)?;
        if self.delta_id != request.delta_id
            || self.terminal_receipt_sha256 != request.terminal_receipt_sha256
            || self.content != request.content
            || self.range.offset != request.offset
            || self.range.length == 0
            || self.range.length > request.max_bytes
        {
            return Err(GenerationTransportContractError::BodyWindowMismatch);
        }
        let end = self
            .range
            .offset
            .checked_add(self.range.length)
            .ok_or(GenerationTransportContractError::InvalidBodyRange)?;
        if end > self.content.byte_length
            || self.range.complete != (end == self.content.byte_length)
        {
            return Err(GenerationTransportContractError::InvalidBodyRange);
        }
        Ok(())
    }

    pub fn validate_body(&self, body: &[u8]) -> Result<(), GenerationTransportContractError> {
        let length = u64::try_from(body.len())
            .map_err(|_| GenerationTransportContractError::InvalidBodyRange)?;
        self.validate_body_digest(length, &sha256_label(body))
    }

    pub fn validate_body_digest(
        &self,
        actual_length: u64,
        actual_sha256: &str,
    ) -> Result<(), GenerationTransportContractError> {
        if actual_length != self.range.length || actual_sha256 != self.window_sha256 {
            return Err(GenerationTransportContractError::BodyIntegrityMismatch);
        }
        Ok(())
    }
}

impl Debug for GenerationBodyWindowMetadata {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationBodyWindowMetadata")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("delta_id", &"<redacted>")
            .field("terminal_receipt_sha256", &"<redacted>")
            .field("content", &"<redacted>")
            .field("range", &self.range)
            .field("window_sha256", &"<redacted>")
            .finish()
    }
}

/// Idempotent client acknowledgment after the terminal receipt has been
/// validated and the local observed generation has advanced.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationDeliveryAcknowledgmentRequest {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub delta_id: String,
    pub mount_id: PortableMountId,
    pub source_connection_id: SourceConnectionId,
    pub target_generation_id: SourceGenerationId,
    pub terminal_receipt_sha256: String,
    pub authorization_epoch: FreshnessEpoch,
}

impl GenerationDeliveryAcknowledgmentRequest {
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        decode_request(input)
    }

    pub fn from_receipt(
        receipt: &GenerationDeltaTerminalReceipt,
    ) -> Result<Self, GenerationTransportContractError> {
        receipt.validate().map_err(|error| {
            GenerationTransportContractError::TerminalReceipt(error.to_string())
        })?;
        Ok(Self {
            format_version: GENERATION_TRANSPORT_FORMAT_VERSION,
            minimum_reader_version: GENERATION_TRANSPORT_READER_VERSION,
            delta_id: receipt.delta_id.clone(),
            mount_id: receipt.mount_id.clone(),
            source_connection_id: receipt.source_connection_id.clone(),
            target_generation_id: receipt.target_generation_id.clone(),
            terminal_receipt_sha256: receipt.canonical_sha256().map_err(|error| {
                GenerationTransportContractError::TerminalReceipt(error.to_string())
            })?,
            authorization_epoch: receipt.authorization_epoch,
        })
    }

    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_identifier("delta_id", &self.delta_id)?;
        validate_identifier("mount_id", self.mount_id.as_str())?;
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier("target_generation_id", self.target_generation_id.as_str())?;
        validate_sha256(&self.terminal_receipt_sha256)
    }

    pub fn validate_against_receipt(
        &self,
        receipt: &GenerationDeltaTerminalReceipt,
    ) -> Result<(), GenerationTransportContractError> {
        if self != &Self::from_receipt(receipt)? {
            return Err(GenerationTransportContractError::AcknowledgmentMismatch);
        }
        Ok(())
    }
}

impl Debug for GenerationDeliveryAcknowledgmentRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationDeliveryAcknowledgmentRequest")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("delta_id", &"<redacted>")
            .field("mount_id", &self.mount_id)
            .field("source_connection_id", &self.source_connection_id)
            .field("target_generation_id", &self.target_generation_id)
            .field("terminal_receipt_sha256", &"<redacted>")
            .field("authorization_epoch", &self.authorization_epoch)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationDeliveryAcknowledgmentStatus {
    Accepted,
    AlreadyAccepted,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationDeliveryAcknowledgment {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub delta_id: String,
    pub terminal_receipt_sha256: String,
    pub status: GenerationDeliveryAcknowledgmentStatus,
    pub acknowledged_at: String,
}

impl GenerationDeliveryAcknowledgment {
    pub fn validate_against(
        &self,
        request: &GenerationDeliveryAcknowledgmentRequest,
    ) -> Result<(), GenerationTransportContractError> {
        request.validate()?;
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_sha256(&self.terminal_receipt_sha256)?;
        validate_timestamp(&self.acknowledged_at)?;
        if self.status == GenerationDeliveryAcknowledgmentStatus::Unknown
            || self.delta_id != request.delta_id
            || self.terminal_receipt_sha256 != request.terminal_receipt_sha256
        {
            return Err(GenerationTransportContractError::AcknowledgmentMismatch);
        }
        Ok(())
    }
}

impl Debug for GenerationDeliveryAcknowledgment {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationDeliveryAcknowledgment")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("delta_id", &"<redacted>")
            .field("terminal_receipt_sha256", &"<redacted>")
            .field("status", &self.status)
            .field("acknowledged_at", &self.acknowledged_at)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPinLeaseAcquireRequest {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub operation_id: String,
    pub device_scope_id: String,
    pub source_connection_id: SourceConnectionId,
    pub generation_id: SourceGenerationId,
    pub requested_lease_seconds: u64,
    pub fallback_policy: GenerationPinFallbackPolicy,
}

impl GenerationPinLeaseAcquireRequest {
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        decode_request(input)
    }

    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_opaque(
            "operation_id",
            &self.operation_id,
            MAX_GENERATION_PIN_OPERATION_ID_BYTES,
        )?;
        validate_opaque(
            "device_scope_id",
            &self.device_scope_id,
            MAX_OPAQUE_DEVICE_SCOPE_ID_BYTES,
        )?;
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier("generation_id", self.generation_id.as_str())?;
        validate_lease_seconds(self.requested_lease_seconds)?;
        validate_fallback(self.fallback_policy)
    }
}

impl Debug for GenerationPinLeaseAcquireRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationPinLeaseAcquireRequest")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("operation_id", &"<redacted>")
            .field("device_scope_id", &"<redacted>")
            .field("source_connection_id", &self.source_connection_id)
            .field("generation_id", &self.generation_id)
            .field("requested_lease_seconds", &self.requested_lease_seconds)
            .field("fallback_policy", &self.fallback_policy)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPinLease {
    pub lease_id: String,
    pub device_scope_id: String,
    pub source_connection_id: SourceConnectionId,
    pub generation_id: SourceGenerationId,
    pub lease_seconds: u64,
    pub issued_at: String,
    pub expires_at: String,
    pub active_leases_for_device: u16,
    pub max_active_leases_per_device: u16,
}

impl GenerationPinLease {
    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_opaque("lease_id", &self.lease_id, MAX_OPAQUE_PIN_LEASE_ID_BYTES)?;
        validate_opaque(
            "device_scope_id",
            &self.device_scope_id,
            MAX_OPAQUE_DEVICE_SCOPE_ID_BYTES,
        )?;
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier("generation_id", self.generation_id.as_str())?;
        validate_lease_seconds(self.lease_seconds)?;
        validate_timestamp(&self.issued_at)?;
        validate_timestamp(&self.expires_at)?;
        let issued_at = canonical_utc_seconds(&self.issued_at)?;
        let expires_at = canonical_utc_seconds(&self.expires_at)?;
        if expires_at.checked_sub(issued_at) != Some(self.lease_seconds as i64) {
            return Err(GenerationTransportContractError::InvalidPinLeaseExpiry);
        }
        if self.active_leases_for_device == 0
            || self.max_active_leases_per_device == 0
            || self.active_leases_for_device > self.max_active_leases_per_device
            || self.max_active_leases_per_device > MAX_GENERATION_PIN_LEASES_PER_DEVICE
        {
            return Err(GenerationTransportContractError::InvalidPinQuota);
        }
        Ok(())
    }

    pub fn validate_at(
        &self,
        trusted_server_time: &str,
    ) -> Result<(), GenerationTransportContractError> {
        self.validate()?;
        let issued_at = canonical_utc_seconds(&self.issued_at)?;
        let server_time = canonical_utc_seconds(trusted_server_time)?;
        let expires_at = canonical_utc_seconds(&self.expires_at)?;
        if server_time < issued_at || server_time >= expires_at {
            return Err(GenerationTransportContractError::ExpiredPinLease);
        }
        Ok(())
    }
}

impl Debug for GenerationPinLease {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationPinLease")
            .field("lease_id", &"<redacted>")
            .field("device_scope_id", &"<redacted>")
            .field("source_connection_id", &self.source_connection_id)
            .field("generation_id", &self.generation_id)
            .field("lease_seconds", &self.lease_seconds)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("active_leases_for_device", &self.active_leases_for_device)
            .field(
                "max_active_leases_per_device",
                &self.max_active_leases_per_device,
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPinLeaseUnavailableReason {
    PinningUnsupported,
    GenerationUnavailable,
    QuotaExceeded,
    TemporarilyUnavailable,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GenerationPinLeaseAcquireResponse {
    Granted {
        #[serde(default = "v1")]
        format_version: u16,
        #[serde(default = "v1")]
        minimum_reader_version: u16,
        operation_id: String,
        device_scope_id: String,
        source_connection_id: SourceConnectionId,
        requested_generation_id: SourceGenerationId,
        requested_lease_seconds: u64,
        fallback_policy: GenerationPinFallbackPolicy,
        server_time: String,
        fallback_applied: bool,
        lease: GenerationPinLease,
    },
    Unavailable {
        #[serde(default = "v1")]
        format_version: u16,
        #[serde(default = "v1")]
        minimum_reader_version: u16,
        operation_id: String,
        device_scope_id: String,
        source_connection_id: SourceConnectionId,
        requested_generation_id: SourceGenerationId,
        requested_lease_seconds: u64,
        fallback_policy: GenerationPinFallbackPolicy,
        server_time: String,
        reason: GenerationPinLeaseUnavailableReason,
        retry_after_seconds: Option<u64>,
    },
}

impl Debug for GenerationPinLeaseAcquireResponse {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Granted {
                format_version,
                minimum_reader_version,
                source_connection_id,
                requested_generation_id,
                requested_lease_seconds,
                fallback_policy,
                server_time,
                fallback_applied,
                lease,
                ..
            } => formatter
                .debug_struct("GenerationPinLeaseAcquireResponse::Granted")
                .field("format_version", format_version)
                .field("minimum_reader_version", minimum_reader_version)
                .field("operation_id", &"<redacted>")
                .field("device_scope_id", &"<redacted>")
                .field("source_connection_id", source_connection_id)
                .field("requested_generation_id", requested_generation_id)
                .field("requested_lease_seconds", requested_lease_seconds)
                .field("fallback_policy", fallback_policy)
                .field("server_time", server_time)
                .field("fallback_applied", fallback_applied)
                .field("lease", lease)
                .finish(),
            Self::Unavailable {
                format_version,
                minimum_reader_version,
                source_connection_id,
                requested_generation_id,
                requested_lease_seconds,
                fallback_policy,
                server_time,
                reason,
                retry_after_seconds,
                ..
            } => formatter
                .debug_struct("GenerationPinLeaseAcquireResponse::Unavailable")
                .field("format_version", format_version)
                .field("minimum_reader_version", minimum_reader_version)
                .field("operation_id", &"<redacted>")
                .field("device_scope_id", &"<redacted>")
                .field("source_connection_id", source_connection_id)
                .field("requested_generation_id", requested_generation_id)
                .field("requested_lease_seconds", requested_lease_seconds)
                .field("fallback_policy", fallback_policy)
                .field("server_time", server_time)
                .field("reason", reason)
                .field("retry_after_seconds", retry_after_seconds)
                .finish(),
        }
    }
}

impl GenerationPinLeaseAcquireResponse {
    pub fn validate_against(
        &self,
        request: &GenerationPinLeaseAcquireRequest,
        selected_capability: &GenerationPinLeaseCapability,
    ) -> Result<(), GenerationTransportContractError> {
        validate_pin_acquire_request_against_capability(request, selected_capability)?;
        match self {
            Self::Granted {
                format_version,
                minimum_reader_version,
                operation_id,
                device_scope_id,
                source_connection_id,
                requested_generation_id,
                requested_lease_seconds,
                fallback_policy,
                server_time,
                fallback_applied,
                lease,
            } => {
                validate_versions(*format_version, *minimum_reader_version)?;
                validate_timestamp(server_time)?;
                lease.validate_at(server_time)?;
                if operation_id != &request.operation_id
                    || device_scope_id != &request.device_scope_id
                    || source_connection_id != &request.source_connection_id
                    || requested_generation_id != &request.generation_id
                    || *requested_lease_seconds != request.requested_lease_seconds
                    || *fallback_policy != request.fallback_policy
                    || lease.device_scope_id != request.device_scope_id
                    || lease.source_connection_id != request.source_connection_id
                    || lease.lease_seconds > request.requested_lease_seconds
                    || lease.lease_seconds < selected_capability.min_lease_seconds
                    || lease.lease_seconds > selected_capability.max_lease_seconds
                    || lease.max_active_leases_per_device
                        > selected_capability.max_active_leases_per_device
                    || lease.active_leases_for_device
                        > selected_capability.max_active_leases_per_device
                    || (*fallback_applied
                        && (request.fallback_policy
                            != GenerationPinFallbackPolicy::UseLatestRetained
                            || lease.generation_id == request.generation_id))
                    || (!*fallback_applied && lease.generation_id != request.generation_id)
                {
                    return Err(GenerationTransportContractError::PinLeaseMismatch);
                }
            }
            Self::Unavailable {
                format_version,
                minimum_reader_version,
                operation_id,
                device_scope_id,
                source_connection_id,
                requested_generation_id,
                requested_lease_seconds,
                fallback_policy,
                server_time,
                reason,
                retry_after_seconds,
            } => {
                validate_versions(*format_version, *minimum_reader_version)?;
                validate_timestamp(server_time)?;
                if operation_id != &request.operation_id
                    || device_scope_id != &request.device_scope_id
                    || source_connection_id != &request.source_connection_id
                    || requested_generation_id != &request.generation_id
                    || *requested_lease_seconds != request.requested_lease_seconds
                    || *fallback_policy != request.fallback_policy
                    || *reason == GenerationPinLeaseUnavailableReason::Unknown
                    || retry_after_seconds.is_some_and(|seconds| {
                        seconds == 0 || seconds > MAX_GENERATION_PIN_RETRY_AFTER_SECONDS
                    })
                {
                    return Err(GenerationTransportContractError::PinLeaseMismatch);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPinLeaseRenewRequest {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub operation_id: String,
    pub lease_id: String,
    pub device_scope_id: String,
    pub source_connection_id: SourceConnectionId,
    pub generation_id: SourceGenerationId,
    pub requested_lease_seconds: u64,
}

impl GenerationPinLeaseRenewRequest {
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        decode_request(input)
    }

    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_opaque(
            "operation_id",
            &self.operation_id,
            MAX_GENERATION_PIN_OPERATION_ID_BYTES,
        )?;
        validate_opaque("lease_id", &self.lease_id, MAX_OPAQUE_PIN_LEASE_ID_BYTES)?;
        validate_opaque(
            "device_scope_id",
            &self.device_scope_id,
            MAX_OPAQUE_DEVICE_SCOPE_ID_BYTES,
        )?;
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier("generation_id", self.generation_id.as_str())?;
        validate_lease_seconds(self.requested_lease_seconds)
    }
}

impl Debug for GenerationPinLeaseRenewRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationPinLeaseRenewRequest")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("operation_id", &"<redacted>")
            .field("lease_id", &"<redacted>")
            .field("device_scope_id", &"<redacted>")
            .field("source_connection_id", &self.source_connection_id)
            .field("generation_id", &self.generation_id)
            .field("requested_lease_seconds", &self.requested_lease_seconds)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPinLeaseRenewal {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub operation_id: String,
    pub requested_lease_seconds: u64,
    pub server_time: String,
    pub lease: GenerationPinLease,
}

impl Debug for GenerationPinLeaseRenewal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationPinLeaseRenewal")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("operation_id", &"<redacted>")
            .field("requested_lease_seconds", &self.requested_lease_seconds)
            .field("server_time", &self.server_time)
            .field("lease", &self.lease)
            .finish()
    }
}

impl GenerationPinLeaseRenewal {
    pub fn validate_against(
        &self,
        request: &GenerationPinLeaseRenewRequest,
        selected_capability: &GenerationPinLeaseCapability,
    ) -> Result<(), GenerationTransportContractError> {
        validate_pin_renew_request_against_capability(request, selected_capability)?;
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_opaque(
            "operation_id",
            &self.operation_id,
            MAX_GENERATION_PIN_OPERATION_ID_BYTES,
        )?;
        validate_timestamp(&self.server_time)?;
        self.lease.validate_at(&self.server_time)?;
        if self.operation_id != request.operation_id
            || self.requested_lease_seconds != request.requested_lease_seconds
            || self.lease.lease_id != request.lease_id
            || self.lease.device_scope_id != request.device_scope_id
            || self.lease.source_connection_id != request.source_connection_id
            || self.lease.generation_id != request.generation_id
            || self.lease.lease_seconds > request.requested_lease_seconds
            || self.lease.lease_seconds < selected_capability.min_lease_seconds
            || self.lease.lease_seconds > selected_capability.max_lease_seconds
            || self.lease.max_active_leases_per_device
                > selected_capability.max_active_leases_per_device
            || self.lease.active_leases_for_device
                > selected_capability.max_active_leases_per_device
        {
            return Err(GenerationTransportContractError::PinLeaseMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPinLeaseReleaseRequest {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub operation_id: String,
    pub lease_id: String,
    pub device_scope_id: String,
}

impl GenerationPinLeaseReleaseRequest {
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationTransportContractError> {
        decode_request(input)
    }

    pub fn validate(&self) -> Result<(), GenerationTransportContractError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_opaque(
            "operation_id",
            &self.operation_id,
            MAX_GENERATION_PIN_OPERATION_ID_BYTES,
        )?;
        validate_opaque("lease_id", &self.lease_id, MAX_OPAQUE_PIN_LEASE_ID_BYTES)?;
        validate_opaque(
            "device_scope_id",
            &self.device_scope_id,
            MAX_OPAQUE_DEVICE_SCOPE_ID_BYTES,
        )
    }
}

impl Debug for GenerationPinLeaseReleaseRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationPinLeaseReleaseRequest")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("operation_id", &"<redacted>")
            .field("lease_id", &"<redacted>")
            .field("device_scope_id", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPinLeaseReleaseStatus {
    Released,
    AlreadyReleased,
    Expired,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationPinLeaseRelease {
    #[serde(default = "v1")]
    pub format_version: u16,
    #[serde(default = "v1")]
    pub minimum_reader_version: u16,
    pub operation_id: String,
    pub lease_id: String,
    pub device_scope_id: String,
    pub status: GenerationPinLeaseReleaseStatus,
    pub released_at: String,
}

impl GenerationPinLeaseRelease {
    pub fn validate_against(
        &self,
        request: &GenerationPinLeaseReleaseRequest,
    ) -> Result<(), GenerationTransportContractError> {
        request.validate()?;
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_opaque(
            "operation_id",
            &self.operation_id,
            MAX_GENERATION_PIN_OPERATION_ID_BYTES,
        )?;
        validate_timestamp(&self.released_at)?;
        if self.status == GenerationPinLeaseReleaseStatus::Unknown
            || self.operation_id != request.operation_id
            || self.lease_id != request.lease_id
            || self.device_scope_id != request.device_scope_id
        {
            return Err(GenerationTransportContractError::PinLeaseMismatch);
        }
        Ok(())
    }
}

impl Debug for GenerationPinLeaseRelease {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationPinLeaseRelease")
            .field("format_version", &self.format_version)
            .field("minimum_reader_version", &self.minimum_reader_version)
            .field("operation_id", &"<redacted>")
            .field("lease_id", &"<redacted>")
            .field("device_scope_id", &"<redacted>")
            .field("status", &self.status)
            .field("released_at", &self.released_at)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerationTransportContractError {
    UpdateRequired { minimum: u16, supported: u16 },
    InvalidVersionEnvelope,
    InvalidJson(String),
    EncodingTooLarge { actual: usize, maximum: usize },
    IdentifierEmpty(&'static str),
    IdentifierTooLong(&'static str),
    InvalidOpaqueValue(&'static str),
    InvalidTimestamp,
    InvalidSha256,
    InvalidBodyWindowLimit { actual: u64 },
    InvalidBodyRange,
    BodyWindowMismatch,
    BodyIntegrityMismatch,
    ContentIdentity(String),
    CapabilityNotOffered,
    InvalidPinCapability,
    InvalidPinLeaseDuration { actual: u64 },
    InvalidPinLeaseExpiry,
    ExpiredPinLease,
    InvalidPinQuota,
    UnknownPinFallback,
    PinLeaseMismatch,
    TerminalReceipt(String),
    AcknowledgmentMismatch,
}

impl Display for GenerationTransportContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpdateRequired { minimum, supported } => write!(
                formatter,
                "generation transport requires reader version {minimum}, supported version is {supported}"
            ),
            Self::InvalidVersionEnvelope => {
                formatter.write_str("invalid transport version envelope")
            }
            Self::InvalidJson(error) => {
                write!(formatter, "invalid generation transport JSON: {error}")
            }
            Self::EncodingTooLarge { actual, maximum } => write!(
                formatter,
                "generation transport encoding is {actual} bytes, exceeding {maximum}"
            ),
            Self::IdentifierEmpty(field) => write!(formatter, "{field} must not be empty"),
            Self::IdentifierTooLong(field) => write!(formatter, "{field} is too long"),
            Self::InvalidOpaqueValue(field) => {
                write!(formatter, "{field} is not a bounded opaque value")
            }
            Self::InvalidTimestamp => formatter.write_str("invalid bounded timestamp"),
            Self::InvalidSha256 => {
                formatter.write_str("digest must be `sha256:` plus 64 lowercase hexadecimal digits")
            }
            Self::InvalidBodyWindowLimit { actual } => write!(
                formatter,
                "body window limit {actual} is outside 1..={MAX_GENERATION_BODY_WINDOW_BYTES}"
            ),
            Self::InvalidBodyRange => formatter.write_str("invalid generation body range"),
            Self::BodyWindowMismatch => {
                formatter.write_str("body window metadata does not match request")
            }
            Self::BodyIntegrityMismatch => {
                formatter.write_str("body window length or digest mismatch")
            }
            Self::ContentIdentity(error) => write!(formatter, "invalid content identity: {error}"),
            Self::CapabilityNotOffered => {
                formatter.write_str("selected transport capability was not offered")
            }
            Self::InvalidPinCapability => {
                formatter.write_str("invalid generation pin capability limits")
            }
            Self::InvalidPinLeaseDuration { actual } => write!(
                formatter,
                "pin lease duration {actual} is outside {MIN_GENERATION_PIN_LEASE_SECONDS}..={MAX_GENERATION_PIN_LEASE_SECONDS}"
            ),
            Self::InvalidPinLeaseExpiry => {
                formatter.write_str("pin lease expiry does not match its issued time and duration")
            }
            Self::ExpiredPinLease => {
                formatter.write_str("pin lease is not live at authenticated server time")
            }
            Self::InvalidPinQuota => formatter.write_str("invalid bounded device pin quota"),
            Self::UnknownPinFallback => {
                formatter.write_str("unknown generation pin fallback policy")
            }
            Self::PinLeaseMismatch => {
                formatter.write_str("generation pin lease response does not match request")
            }
            Self::TerminalReceipt(error) => write!(formatter, "invalid terminal receipt: {error}"),
            Self::AcknowledgmentMismatch => {
                formatter.write_str("delivery acknowledgment does not match terminal receipt")
            }
        }
    }
}

impl std::error::Error for GenerationTransportContractError {}

fn validate_versions(
    format_version: u16,
    minimum_reader_version: u16,
) -> Result<(), GenerationTransportContractError> {
    if format_version == 0 || minimum_reader_version == 0 || minimum_reader_version > format_version
    {
        return Err(GenerationTransportContractError::InvalidVersionEnvelope);
    }
    if minimum_reader_version > GENERATION_TRANSPORT_READER_VERSION {
        return Err(GenerationTransportContractError::UpdateRequired {
            minimum: minimum_reader_version,
            supported: GENERATION_TRANSPORT_READER_VERSION,
        });
    }
    Ok(())
}

fn validate_encoding_length(actual: usize) -> Result<(), GenerationTransportContractError> {
    if actual > MAX_GENERATION_TRANSPORT_REQUEST_BYTES {
        Err(GenerationTransportContractError::EncodingTooLarge {
            actual,
            maximum: MAX_GENERATION_TRANSPORT_REQUEST_BYTES,
        })
    } else {
        Ok(())
    }
}

fn decode_request<T>(input: &[u8]) -> Result<T, GenerationTransportContractError>
where
    T: for<'de> Deserialize<'de> + ValidateGenerationTransportRequest,
{
    validate_encoding_length(input.len())?;
    let request: T = serde_json::from_slice(input)
        .map_err(|error| GenerationTransportContractError::InvalidJson(error.to_string()))?;
    request.validate_request()?;
    Ok(request)
}

trait ValidateGenerationTransportRequest {
    fn validate_request(&self) -> Result<(), GenerationTransportContractError>;
}

impl ValidateGenerationTransportRequest for GenerationDeliveryRequest {
    fn validate_request(&self) -> Result<(), GenerationTransportContractError> {
        self.validate()
    }
}

impl ValidateGenerationTransportRequest for GenerationDeliveryAcknowledgmentRequest {
    fn validate_request(&self) -> Result<(), GenerationTransportContractError> {
        self.validate()
    }
}

impl ValidateGenerationTransportRequest for GenerationPinLeaseAcquireRequest {
    fn validate_request(&self) -> Result<(), GenerationTransportContractError> {
        self.validate()
    }
}

impl ValidateGenerationTransportRequest for GenerationPinLeaseRenewRequest {
    fn validate_request(&self) -> Result<(), GenerationTransportContractError> {
        self.validate()
    }
}

impl ValidateGenerationTransportRequest for GenerationPinLeaseReleaseRequest {
    fn validate_request(&self) -> Result<(), GenerationTransportContractError> {
        self.validate()
    }
}

fn validate_identifier(
    field: &'static str,
    value: &str,
) -> Result<(), GenerationTransportContractError> {
    if value.is_empty() {
        return Err(GenerationTransportContractError::IdentifierEmpty(field));
    }
    if value.len() > MAX_DELIVERY_ID_BYTES {
        return Err(GenerationTransportContractError::IdentifierTooLong(field));
    }
    Ok(())
}

fn validate_opaque(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), GenerationTransportContractError> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(GenerationTransportContractError::InvalidOpaqueValue(field))
    } else {
        Ok(())
    }
}

fn validate_timestamp(value: &str) -> Result<(), GenerationTransportContractError> {
    if value.len() > MAX_DELIVERY_TIMESTAMP_BYTES {
        return Err(GenerationTransportContractError::InvalidTimestamp);
    }
    crate::validate_canonical_utc_timestamp("generation_transport_timestamp", value)
        .map_err(|_| GenerationTransportContractError::InvalidTimestamp)
}

fn canonical_utc_seconds(value: &str) -> Result<i64, GenerationTransportContractError> {
    validate_timestamp(value)?;
    let bytes = value.as_bytes();
    let year = decimal(&bytes[0..4]);
    let month = decimal(&bytes[5..7]);
    let day = decimal(&bytes[8..10]);
    let hour = decimal(&bytes[11..13]);
    let minute = decimal(&bytes[14..16]);
    let second = decimal(&bytes[17..19]);
    let days = days_from_civil(year, month, day);
    Ok(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn decimal(bytes: &[u8]) -> i64 {
    bytes
        .iter()
        .fold(0_i64, |value, byte| value * 10 + i64::from(*byte - b'0'))
}

// Howard Hinnant's civil-calendar conversion, shifted to the Unix epoch. The
// canonical timestamp validator has already bounded every calendar field.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn validate_sha256(value: &str) -> Result<(), GenerationTransportContractError> {
    if value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        Ok(())
    } else {
        Err(GenerationTransportContractError::InvalidSha256)
    }
}

fn validate_lease_seconds(seconds: u64) -> Result<(), GenerationTransportContractError> {
    if (MIN_GENERATION_PIN_LEASE_SECONDS..=MAX_GENERATION_PIN_LEASE_SECONDS).contains(&seconds) {
        Ok(())
    } else {
        Err(GenerationTransportContractError::InvalidPinLeaseDuration { actual: seconds })
    }
}

fn validate_fallback(
    fallback: GenerationPinFallbackPolicy,
) -> Result<(), GenerationTransportContractError> {
    if fallback == GenerationPinFallbackPolicy::Unknown {
        Err(GenerationTransportContractError::UnknownPinFallback)
    } else {
        Ok(())
    }
}

fn validate_pin_capability(
    capability: &GenerationPinLeaseCapability,
) -> Result<(), GenerationTransportContractError> {
    validate_lease_seconds(capability.min_lease_seconds)?;
    validate_lease_seconds(capability.max_lease_seconds)?;
    if capability.min_lease_seconds > capability.max_lease_seconds
        || capability.max_active_leases_per_device == 0
        || capability.max_active_leases_per_device > MAX_GENERATION_PIN_LEASES_PER_DEVICE
        || capability.fallback_policies.is_empty()
        || capability.fallback_policies.len() > 2
    {
        return Err(GenerationTransportContractError::InvalidPinCapability);
    }
    let mut previous = None;
    for policy in &capability.fallback_policies {
        validate_fallback(*policy)?;
        if previous.is_some_and(|previous| previous >= policy) {
            return Err(GenerationTransportContractError::InvalidPinCapability);
        }
        previous = Some(policy);
    }
    Ok(())
}

fn validate_pin_acquire_request_against_capability(
    request: &GenerationPinLeaseAcquireRequest,
    capability: &GenerationPinLeaseCapability,
) -> Result<(), GenerationTransportContractError> {
    request.validate()?;
    validate_pin_capability(capability)?;
    if !(capability.min_lease_seconds..=capability.max_lease_seconds)
        .contains(&request.requested_lease_seconds)
        || !capability
            .fallback_policies
            .contains(&request.fallback_policy)
    {
        return Err(GenerationTransportContractError::PinLeaseMismatch);
    }
    Ok(())
}

fn validate_pin_renew_request_against_capability(
    request: &GenerationPinLeaseRenewRequest,
    capability: &GenerationPinLeaseCapability,
) -> Result<(), GenerationTransportContractError> {
    request.validate()?;
    validate_pin_capability(capability)?;
    if !(capability.min_lease_seconds..=capability.max_lease_seconds)
        .contains(&request.requested_lease_seconds)
    {
        return Err(GenerationTransportContractError::PinLeaseMismatch);
    }
    Ok(())
}

fn sha256_label(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}
