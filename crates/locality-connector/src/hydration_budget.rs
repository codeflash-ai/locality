//! Job-scoped limits for connector initial hydration.
//!
//! A host creates one [`InitialHydrationBudget`] per job and passes clones to
//! every stage. Clones share only counters and a deadline; they never contain
//! credentials, provider payloads, checkpoints, or connector-global state.

use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use locality_core::LocalityError;
use serde::{Deserialize, Serialize};

/// Hard limits for one initial-hydration attempt.
///
/// Every field is required to be nonzero. Byte values describe decoded bytes
/// unless the field explicitly says `encoded`. `provider_deadline_ms` starts
/// when the budget is constructed, not when the first request is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialHydrationLimits {
    pub max_response_body_bytes: u64,
    pub max_provider_calls: u64,
    pub provider_deadline_ms: u64,
    pub max_inventory_items: u64,
    pub max_inventory_encoded_bytes: u64,
    pub max_traversal_nodes: u64,
    pub max_traversal_depth: u64,
    pub max_native_bytes: u64,
    pub max_media_assets: u64,
    pub max_media_decoded_bytes: u64,
    pub max_rendered_content_bytes: u64,
    pub max_projections: u64,
    pub max_changes: u64,
    pub max_retained_bytes: u64,
}

impl InitialHydrationLimits {
    pub fn validate(self) -> Result<Self, InitialHydrationError> {
        for (resource, value) in [
            (
                HydrationResource::ResponseBodyBytes,
                self.max_response_body_bytes,
            ),
            (HydrationResource::ProviderCalls, self.max_provider_calls),
            (
                HydrationResource::ProviderDeadline,
                self.provider_deadline_ms,
            ),
            (HydrationResource::InventoryItems, self.max_inventory_items),
            (
                HydrationResource::InventoryEncodedBytes,
                self.max_inventory_encoded_bytes,
            ),
            (HydrationResource::TraversalNodes, self.max_traversal_nodes),
            (HydrationResource::TraversalDepth, self.max_traversal_depth),
            (HydrationResource::NativeBytes, self.max_native_bytes),
            (HydrationResource::MediaAssets, self.max_media_assets),
            (
                HydrationResource::MediaDecodedBytes,
                self.max_media_decoded_bytes,
            ),
            (
                HydrationResource::RenderedContentBytes,
                self.max_rendered_content_bytes,
            ),
            (HydrationResource::Projections, self.max_projections),
            (HydrationResource::Changes, self.max_changes),
            (HydrationResource::RetainedBytes, self.max_retained_bytes),
        ] {
            if value == 0 {
                return Err(InitialHydrationError::InvalidLimit { resource });
            }
        }
        Ok(self)
    }

    fn limit(self, resource: HydrationResource) -> u64 {
        match resource {
            HydrationResource::ResponseBodyBytes => self.max_response_body_bytes,
            HydrationResource::ProviderCalls => self.max_provider_calls,
            HydrationResource::ProviderDeadline => self.provider_deadline_ms,
            HydrationResource::InventoryItems => self.max_inventory_items,
            HydrationResource::InventoryEncodedBytes => self.max_inventory_encoded_bytes,
            HydrationResource::TraversalNodes => self.max_traversal_nodes,
            HydrationResource::TraversalDepth => self.max_traversal_depth,
            HydrationResource::NativeBytes => self.max_native_bytes,
            HydrationResource::MediaAssets => self.max_media_assets,
            HydrationResource::MediaDecodedBytes => self.max_media_decoded_bytes,
            HydrationResource::RenderedContentBytes => self.max_rendered_content_bytes,
            HydrationResource::Projections => self.max_projections,
            HydrationResource::Changes => self.max_changes,
            HydrationResource::RetainedBytes => self.max_retained_bytes,
        }
    }
}

/// A stable, redaction-safe budget dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HydrationResource {
    ResponseBodyBytes,
    ProviderCalls,
    ProviderDeadline,
    InventoryItems,
    InventoryEncodedBytes,
    TraversalNodes,
    TraversalDepth,
    NativeBytes,
    MediaAssets,
    MediaDecodedBytes,
    RenderedContentBytes,
    Projections,
    Changes,
    RetainedBytes,
}

/// Typed error for the opt-in bounded hydration path.
///
/// Provider response bodies and transport messages are deliberately discarded.
/// Rate-limit provider identity and `Retry-After` remain structured so a
/// scheduler can park the job without parsing an error string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitialHydrationError {
    InvalidLimit {
        resource: HydrationResource,
    },
    LimitExceeded {
        resource: HydrationResource,
    },
    ProviderRateLimited {
        provider: String,
        retry_after: Duration,
    },
    ProviderNotFound,
    ProviderUnavailable,
    ProviderResponseInvalid,
    InvalidScope {
        reason: InitialHydrationScopeFailure,
    },
}

/// A stable, redaction-safe reason that connector scope cannot be hydrated.
///
/// Scope failures describe trusted configuration rather than provider payloads.
/// Keeping them separate from [`InitialHydrationError::ProviderResponseInvalid`]
/// lets a host offer recovery without logging root identities or content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialHydrationScopeFailure {
    OverlappingRoots,
}

impl InitialHydrationScopeFailure {
    pub const fn code(self) -> &'static str {
        match self {
            Self::OverlappingRoots => "overlapping_roots",
        }
    }
}

impl InitialHydrationError {
    pub fn from_connector_error(error: LocalityError) -> Self {
        match error {
            LocalityError::RateLimited {
                provider,
                retry_after,
                ..
            } => Self::ProviderRateLimited {
                provider: sanitize_provider_name(&provider),
                retry_after,
            },
            LocalityError::RemoteNotFound(_) => Self::ProviderNotFound,
            LocalityError::Io(_) => Self::ProviderUnavailable,
            _ => Self::ProviderResponseInvalid,
        }
    }

    pub fn is_permanent(&self) -> bool {
        !matches!(
            self,
            Self::ProviderRateLimited { .. } | Self::ProviderUnavailable
        )
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::ProviderRateLimited { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

impl Display for InitialHydrationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLimit { resource } => {
                write!(f, "initial hydration limit is invalid: {resource:?}")
            }
            Self::LimitExceeded { resource } => {
                write!(f, "initial hydration limit exceeded: {resource:?}")
            }
            Self::ProviderRateLimited {
                provider,
                retry_after,
            } => write!(
                f,
                "{provider} rate limited initial hydration for {}ms",
                retry_after.as_millis()
            ),
            Self::ProviderNotFound => write!(f, "initial hydration object was not found"),
            Self::ProviderUnavailable => write!(f, "initial hydration provider is unavailable"),
            Self::ProviderResponseInvalid => {
                write!(f, "initial hydration provider response is invalid")
            }
            Self::InvalidScope { reason } => {
                write!(f, "initial hydration scope is invalid: {}", reason.code())
            }
        }
    }
}

impl std::error::Error for InitialHydrationError {}

pub type InitialHydrationResult<T> = Result<T, InitialHydrationError>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HydrationUsage {
    response_body_bytes: u64,
    provider_calls: u64,
    inventory_items: u64,
    inventory_encoded_bytes: u64,
    traversal_nodes: u64,
    native_bytes: u64,
    media_assets: u64,
    media_decoded_bytes: u64,
    rendered_content_bytes: u64,
    projections: u64,
    changes: u64,
    retained_bytes: u64,
}

impl HydrationUsage {
    fn get(self, resource: HydrationResource) -> u64 {
        match resource {
            HydrationResource::ResponseBodyBytes => self.response_body_bytes,
            HydrationResource::ProviderCalls => self.provider_calls,
            HydrationResource::InventoryItems => self.inventory_items,
            HydrationResource::InventoryEncodedBytes => self.inventory_encoded_bytes,
            HydrationResource::TraversalNodes => self.traversal_nodes,
            HydrationResource::NativeBytes => self.native_bytes,
            HydrationResource::MediaAssets => self.media_assets,
            HydrationResource::MediaDecodedBytes => self.media_decoded_bytes,
            HydrationResource::RenderedContentBytes => self.rendered_content_bytes,
            HydrationResource::Projections => self.projections,
            HydrationResource::Changes => self.changes,
            HydrationResource::RetainedBytes => self.retained_bytes,
            HydrationResource::ProviderDeadline | HydrationResource::TraversalDepth => 0,
        }
    }

    fn set(&mut self, resource: HydrationResource, value: u64) {
        match resource {
            HydrationResource::ResponseBodyBytes => self.response_body_bytes = value,
            HydrationResource::ProviderCalls => self.provider_calls = value,
            HydrationResource::InventoryItems => self.inventory_items = value,
            HydrationResource::InventoryEncodedBytes => self.inventory_encoded_bytes = value,
            HydrationResource::TraversalNodes => self.traversal_nodes = value,
            HydrationResource::NativeBytes => self.native_bytes = value,
            HydrationResource::MediaAssets => self.media_assets = value,
            HydrationResource::MediaDecodedBytes => self.media_decoded_bytes = value,
            HydrationResource::RenderedContentBytes => self.rendered_content_bytes = value,
            HydrationResource::Projections => self.projections = value,
            HydrationResource::Changes => self.changes = value,
            HydrationResource::RetainedBytes => self.retained_bytes = value,
            HydrationResource::ProviderDeadline | HydrationResource::TraversalDepth => {}
        }
    }
}

#[derive(Debug)]
struct BudgetState {
    started: Instant,
    usage: Mutex<HydrationUsage>,
}

/// Shared accounting handle for exactly one hydration job.
#[derive(Clone, Debug)]
pub struct InitialHydrationBudget {
    limits: InitialHydrationLimits,
    state: Arc<BudgetState>,
}

/// Exclusive decoded-media and retained-byte capacity for one pending fetch.
///
/// The reservation is intentionally not cloneable. Dropping it releases all
/// uncommitted capacity, including during unwinding or task cancellation.
#[derive(Debug)]
#[must_use = "dropping the media reservation releases its byte allowance"]
pub struct InitialHydrationMediaReservation {
    budget: InitialHydrationBudget,
    reserved_bytes: u64,
}

impl InitialHydrationMediaReservation {
    pub fn maximum_bytes(&self) -> usize {
        usize::try_from(self.reserved_bytes)
            .expect("media reservation originated from a usize request")
    }

    /// Convert the exclusive allowance into the actual returned capture.
    /// Unused body capacity is released atomically, while `extra_retained_bytes`
    /// accounts returned metadata such as the sanitized media type.
    pub fn commit(
        mut self,
        actual_body_bytes: usize,
        extra_retained_bytes: usize,
    ) -> InitialHydrationResult<()> {
        let actual = usize_to_u64(actual_body_bytes, HydrationResource::MediaDecodedBytes)?;
        let extra = usize_to_u64(extra_retained_bytes, HydrationResource::RetainedBytes)?;
        if actual > self.reserved_bytes {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        }
        let mut usage = self
            .budget
            .state
            .usage
            .lock()
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        let media_without_reservation = usage
            .media_decoded_bytes
            .checked_sub(self.reserved_bytes)
            .ok_or(InitialHydrationError::ProviderResponseInvalid)?;
        let retained_without_reservation = usage
            .retained_bytes
            .checked_sub(self.reserved_bytes)
            .ok_or(InitialHydrationError::ProviderResponseInvalid)?;
        let next_media = media_without_reservation.checked_add(actual).ok_or(
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::MediaDecodedBytes,
            },
        )?;
        let next_retained = retained_without_reservation
            .checked_add(actual)
            .and_then(|value| value.checked_add(extra))
            .ok_or(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes,
            })?;
        if next_media > self.budget.limits.max_media_decoded_bytes {
            return Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::MediaDecodedBytes,
            });
        }
        if next_retained > self.budget.limits.max_retained_bytes {
            return Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes,
            });
        }
        usage.media_decoded_bytes = next_media;
        usage.retained_bytes = next_retained;
        self.reserved_bytes = 0;
        Ok(())
    }
}

impl Drop for InitialHydrationMediaReservation {
    fn drop(&mut self) {
        if self.reserved_bytes == 0 {
            return;
        }
        let mut usage = match self.budget.state.usage.lock() {
            Ok(usage) => usage,
            Err(poisoned) => poisoned.into_inner(),
        };
        usage.media_decoded_bytes = usage
            .media_decoded_bytes
            .saturating_sub(self.reserved_bytes);
        usage.retained_bytes = usage.retained_bytes.saturating_sub(self.reserved_bytes);
        self.reserved_bytes = 0;
    }
}

impl InitialHydrationBudget {
    pub fn new(limits: InitialHydrationLimits) -> InitialHydrationResult<Self> {
        let limits = limits.validate()?;
        Ok(Self {
            limits,
            state: Arc::new(BudgetState {
                started: Instant::now(),
                usage: Mutex::new(HydrationUsage::default()),
            }),
        })
    }

    pub fn limits(&self) -> InitialHydrationLimits {
        self.limits
    }

    /// Reserve one provider attempt before acquiring a token or sending bytes.
    pub fn reserve_provider_call(&self) -> InitialHydrationResult<()> {
        self.check_deadline()?;
        self.account(&[(HydrationResource::ProviderCalls, 1)])
    }

    /// Atomically reserve the provider attempt and media slot for one asset.
    pub fn reserve_media_fetch(&self) -> InitialHydrationResult<()> {
        self.check_deadline()?;
        self.account(&[
            (HydrationResource::ProviderCalls, 1),
            (HydrationResource::MediaAssets, 1),
        ])
    }

    pub fn check_deadline(&self) -> InitialHydrationResult<()> {
        let deadline = Duration::from_millis(self.limits.provider_deadline_ms);
        if self.state.started.elapsed() >= deadline {
            return Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::ProviderDeadline,
            });
        }
        Ok(())
    }

    pub fn remaining_provider_time(&self) -> InitialHydrationResult<Duration> {
        self.check_deadline()?;
        Ok(Duration::from_millis(self.limits.provider_deadline_ms)
            .saturating_sub(self.state.started.elapsed()))
    }

    /// Check a declared response length before allocating its body buffer.
    /// Streaming chunks must still be accounted with [`Self::account_response_chunk`].
    pub fn preflight_response_length(&self, length: u64) -> InitialHydrationResult<()> {
        self.ensure(&[(HydrationResource::ResponseBodyBytes, length)])
    }

    pub fn account_response_chunk(&self, bytes: usize) -> InitialHydrationResult<()> {
        self.account(&[(
            HydrationResource::ResponseBodyBytes,
            usize_to_u64(bytes, HydrationResource::ResponseBodyBytes)?,
        )])
    }

    pub fn account_inventory(
        &self,
        items: usize,
        encoded_bytes: usize,
    ) -> InitialHydrationResult<()> {
        let items = usize_to_u64(items, HydrationResource::InventoryItems)?;
        let encoded = usize_to_u64(encoded_bytes, HydrationResource::InventoryEncodedBytes)?;
        self.account(&[
            (HydrationResource::InventoryItems, items),
            (HydrationResource::InventoryEncodedBytes, encoded),
            (HydrationResource::RetainedBytes, encoded),
        ])
    }

    pub fn preflight_inventory(
        &self,
        items: usize,
        encoded_bytes: usize,
    ) -> InitialHydrationResult<()> {
        let items = usize_to_u64(items, HydrationResource::InventoryItems)?;
        let encoded = usize_to_u64(encoded_bytes, HydrationResource::InventoryEncodedBytes)?;
        self.ensure(&[
            (HydrationResource::InventoryItems, items),
            (HydrationResource::InventoryEncodedBytes, encoded),
            (HydrationResource::RetainedBytes, encoded),
        ])
    }

    pub fn visit_traversal_node(&self, depth: usize) -> InitialHydrationResult<()> {
        self.preflight_traversal_node(depth)?;
        self.account(&[(HydrationResource::TraversalNodes, 1)])
    }

    /// Check node and depth capacity before issuing the provider call that
    /// could discover the node.
    pub fn preflight_traversal_node(&self, depth: usize) -> InitialHydrationResult<()> {
        let depth = usize_to_u64(depth, HydrationResource::TraversalDepth)?;
        if depth > self.limits.max_traversal_depth {
            return Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::TraversalDepth,
            });
        }
        self.ensure(&[(HydrationResource::TraversalNodes, 1)])
    }

    pub fn account_native_bytes(&self, bytes: usize) -> InitialHydrationResult<()> {
        let bytes = usize_to_u64(bytes, HydrationResource::NativeBytes)?;
        self.account(&[
            (HydrationResource::NativeBytes, bytes),
            (HydrationResource::RetainedBytes, bytes),
        ])
    }

    pub fn preflight_native_bytes(&self, bytes: u64) -> InitialHydrationResult<()> {
        self.ensure(&[
            (HydrationResource::NativeBytes, bytes),
            (HydrationResource::RetainedBytes, bytes),
        ])
    }

    /// Validate an already-accounted native input before decoding it again.
    /// Unlike `preflight_native_bytes`, this does not require capacity for a
    /// second retained copy.
    pub fn validate_native_input_bytes(&self, bytes: usize) -> InitialHydrationResult<()> {
        let bytes = usize_to_u64(bytes, HydrationResource::NativeBytes)?;
        if bytes > self.limits.max_native_bytes || bytes > self.limits.max_retained_bytes {
            return Err(InitialHydrationError::LimitExceeded {
                resource: if bytes > self.limits.max_native_bytes {
                    HydrationResource::NativeBytes
                } else {
                    HydrationResource::RetainedBytes
                },
            });
        }
        Ok(())
    }

    /// Reserve an asset slot before its fetch and account decoded bytes before
    /// retaining them. These are separate operations because the byte length is
    /// normally unknown until response headers or the first body chunk arrive.
    pub fn reserve_media_asset(&self) -> InitialHydrationResult<()> {
        self.account(&[(HydrationResource::MediaAssets, 1)])
    }

    /// Atomically claim the largest available per-fetch body allowance across
    /// decoded media and retained bytes. No provider attempt should begin until
    /// this succeeds. The returned guard releases uncommitted capacity.
    pub fn reserve_media_bytes(
        &self,
        requested_bytes: usize,
    ) -> InitialHydrationResult<InitialHydrationMediaReservation> {
        if requested_bytes == 0 {
            return Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::MediaDecodedBytes,
            });
        }
        let requested = usize_to_u64(requested_bytes, HydrationResource::MediaDecodedBytes)?;
        let mut usage = self
            .state
            .usage
            .lock()
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        let remaining_media = self
            .limits
            .max_media_decoded_bytes
            .saturating_sub(usage.media_decoded_bytes);
        let remaining_retained = self
            .limits
            .max_retained_bytes
            .saturating_sub(usage.retained_bytes);
        let reserved = requested.min(remaining_media).min(remaining_retained);
        if reserved == 0 {
            return Err(InitialHydrationError::LimitExceeded {
                resource: if remaining_media == 0 {
                    HydrationResource::MediaDecodedBytes
                } else {
                    HydrationResource::RetainedBytes
                },
            });
        }
        let next_media = usage.media_decoded_bytes.checked_add(reserved).ok_or(
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::MediaDecodedBytes,
            },
        )?;
        let next_retained = usage.retained_bytes.checked_add(reserved).ok_or(
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes,
            },
        )?;
        usage.media_decoded_bytes = next_media;
        usage.retained_bytes = next_retained;
        Ok(InitialHydrationMediaReservation {
            budget: self.clone(),
            reserved_bytes: reserved,
        })
    }

    pub fn preflight_media_bytes(&self, bytes: u64) -> InitialHydrationResult<()> {
        self.ensure(&[
            (HydrationResource::MediaDecodedBytes, bytes),
            (HydrationResource::RetainedBytes, bytes),
        ])
    }

    pub fn account_media_bytes(&self, bytes: usize) -> InitialHydrationResult<()> {
        let bytes = usize_to_u64(bytes, HydrationResource::MediaDecodedBytes)?;
        self.account(&[
            (HydrationResource::MediaDecodedBytes, bytes),
            (HydrationResource::RetainedBytes, bytes),
        ])
    }

    pub fn account_rendered_content(&self, bytes: usize) -> InitialHydrationResult<()> {
        let bytes = usize_to_u64(bytes, HydrationResource::RenderedContentBytes)?;
        self.account(&[(HydrationResource::RenderedContentBytes, bytes)])
    }

    pub fn preflight_rendered_content(&self, bytes: u64) -> InitialHydrationResult<()> {
        self.ensure(&[(HydrationResource::RenderedContentBytes, bytes)])
    }

    /// Atomically retain one complete render result and its content/count
    /// dimensions. `retained_bytes` is the canonical serialized size of the
    /// whole returned representation, not a sum of its body fields.
    pub fn account_render_output(
        &self,
        content_bytes: usize,
        projection_count: usize,
        retained_bytes: usize,
    ) -> InitialHydrationResult<()> {
        self.account(&[
            (
                HydrationResource::RenderedContentBytes,
                usize_to_u64(content_bytes, HydrationResource::RenderedContentBytes)?,
            ),
            (
                HydrationResource::Projections,
                usize_to_u64(projection_count, HydrationResource::Projections)?,
            ),
            (
                HydrationResource::RetainedBytes,
                usize_to_u64(retained_bytes, HydrationResource::RetainedBytes)?,
            ),
        ])
    }

    /// Account a complete logical buffer retained by the caller or a live
    /// intermediate. Callers may release only reservations they own after the
    /// corresponding buffer has been dropped.
    pub fn account_retained_bytes(&self, bytes: usize) -> InitialHydrationResult<()> {
        self.account(&[(
            HydrationResource::RetainedBytes,
            usize_to_u64(bytes, HydrationResource::RetainedBytes)?,
        )])
    }

    pub fn release_retained_bytes(&self, bytes: usize) -> InitialHydrationResult<()> {
        let bytes = usize_to_u64(bytes, HydrationResource::RetainedBytes)?;
        let mut usage = self
            .state
            .usage
            .lock()
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        usage.retained_bytes = usage
            .retained_bytes
            .checked_sub(bytes)
            .ok_or(InitialHydrationError::ProviderResponseInvalid)?;
        Ok(())
    }

    /// Replace an owned live reservation without exposing a transient double
    /// charge while one logical representation is transformed into another.
    pub fn replace_retained_bytes(
        &self,
        released_bytes: usize,
        added_bytes: usize,
    ) -> InitialHydrationResult<()> {
        let released = usize_to_u64(released_bytes, HydrationResource::RetainedBytes)?;
        let added = usize_to_u64(added_bytes, HydrationResource::RetainedBytes)?;
        let mut usage = self
            .state
            .usage
            .lock()
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        let retained_without_owned = usage
            .retained_bytes
            .checked_sub(released)
            .ok_or(InitialHydrationError::ProviderResponseInvalid)?;
        let next = retained_without_owned.checked_add(added).ok_or(
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes,
            },
        )?;
        if next > self.limits.max_retained_bytes {
            return Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes,
            });
        }
        usage.retained_bytes = next;
        Ok(())
    }

    pub fn account_projections(
        &self,
        count: usize,
        retained_bytes: usize,
    ) -> InitialHydrationResult<()> {
        self.account(&[
            (
                HydrationResource::Projections,
                usize_to_u64(count, HydrationResource::Projections)?,
            ),
            (
                HydrationResource::RetainedBytes,
                usize_to_u64(retained_bytes, HydrationResource::RetainedBytes)?,
            ),
        ])
    }

    pub fn preflight_projections(
        &self,
        count: usize,
        retained_bytes: usize,
    ) -> InitialHydrationResult<()> {
        self.ensure(&[
            (
                HydrationResource::Projections,
                usize_to_u64(count, HydrationResource::Projections)?,
            ),
            (
                HydrationResource::RetainedBytes,
                usize_to_u64(retained_bytes, HydrationResource::RetainedBytes)?,
            ),
        ])
    }

    pub fn account_changes(
        &self,
        count: usize,
        retained_bytes: usize,
    ) -> InitialHydrationResult<()> {
        self.account(&[
            (
                HydrationResource::Changes,
                usize_to_u64(count, HydrationResource::Changes)?,
            ),
            (
                HydrationResource::RetainedBytes,
                usize_to_u64(retained_bytes, HydrationResource::RetainedBytes)?,
            ),
        ])
    }

    pub fn preflight_changes(
        &self,
        count: usize,
        retained_bytes: usize,
    ) -> InitialHydrationResult<()> {
        self.ensure(&[
            (
                HydrationResource::Changes,
                usize_to_u64(count, HydrationResource::Changes)?,
            ),
            (
                HydrationResource::RetainedBytes,
                usize_to_u64(retained_bytes, HydrationResource::RetainedBytes)?,
            ),
        ])
    }

    pub fn remaining(&self, resource: HydrationResource) -> InitialHydrationResult<u64> {
        if matches!(resource, HydrationResource::ProviderDeadline) {
            return Ok(self
                .remaining_provider_time()?
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX));
        }
        if matches!(resource, HydrationResource::TraversalDepth) {
            return Ok(self.limits.max_traversal_depth);
        }
        let usage = self
            .state
            .usage
            .lock()
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        Ok(self
            .limits
            .limit(resource)
            .saturating_sub(usage.get(resource)))
    }

    fn ensure(&self, reservations: &[(HydrationResource, u64)]) -> InitialHydrationResult<()> {
        let usage = self
            .state
            .usage
            .lock()
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        validate_reservations(self.limits, *usage, reservations).map(|_| ())
    }

    /// Validate the complete multi-resource reservation before changing any
    /// counter. This prevents partial accounting when the shared retained-byte
    /// limit is the dimension that fails.
    fn account(&self, reservations: &[(HydrationResource, u64)]) -> InitialHydrationResult<()> {
        let mut usage = self
            .state
            .usage
            .lock()
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        let next = validate_reservations(self.limits, *usage, reservations)?;
        *usage = next;
        Ok(())
    }
}

fn validate_reservations(
    limits: InitialHydrationLimits,
    mut usage: HydrationUsage,
    reservations: &[(HydrationResource, u64)],
) -> InitialHydrationResult<HydrationUsage> {
    for &(resource, amount) in reservations {
        if matches!(
            resource,
            HydrationResource::ProviderDeadline | HydrationResource::TraversalDepth
        ) {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        }
        let next = usage
            .get(resource)
            .checked_add(amount)
            .ok_or(InitialHydrationError::LimitExceeded { resource })?;
        if next > limits.limit(resource) {
            return Err(InitialHydrationError::LimitExceeded { resource });
        }
        usage.set(resource, next);
    }
    Ok(usage)
}

fn usize_to_u64(value: usize, resource: HydrationResource) -> InitialHydrationResult<u64> {
    value
        .try_into()
        .map_err(|_| InitialHydrationError::LimitExceeded { resource })
}

fn sanitize_provider_name(provider: &str) -> String {
    let sanitized = provider
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(64)
        .collect::<String>();
    if sanitized.is_empty() {
        "provider".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(cap: u64) -> InitialHydrationLimits {
        InitialHydrationLimits {
            max_response_body_bytes: cap,
            max_provider_calls: cap,
            provider_deadline_ms: 60_000,
            max_inventory_items: cap,
            max_inventory_encoded_bytes: cap,
            max_traversal_nodes: cap,
            max_traversal_depth: cap,
            max_native_bytes: cap,
            max_media_assets: cap,
            max_media_decoded_bytes: cap,
            max_rendered_content_bytes: cap,
            max_projections: cap,
            max_changes: cap,
            max_retained_bytes: cap,
        }
    }

    #[test]
    fn every_limit_must_be_nonzero() {
        let mut invalid = limits(1);
        invalid.max_changes = 0;
        assert_eq!(
            InitialHydrationBudget::new(invalid).unwrap_err(),
            InitialHydrationError::InvalidLimit {
                resource: HydrationResource::Changes
            }
        );
    }

    #[test]
    fn cap_succeeds_and_cap_plus_one_fails_without_changing_usage() {
        let budget = InitialHydrationBudget::new(limits(3)).unwrap();
        budget.account_response_chunk(3).unwrap();
        assert_eq!(
            budget.remaining(HydrationResource::ResponseBodyBytes),
            Ok(0)
        );
        assert_eq!(
            budget.account_response_chunk(1),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::ResponseBodyBytes
            })
        );
        assert_eq!(
            budget.remaining(HydrationResource::ResponseBodyBytes),
            Ok(0)
        );
    }

    #[test]
    fn failed_shared_retained_reservation_is_atomic() {
        let mut configured = limits(10);
        configured.max_retained_bytes = 2;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        assert_eq!(
            budget.account_native_bytes(3),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes
            })
        );
        assert_eq!(budget.remaining(HydrationResource::NativeBytes), Ok(10));
    }

    #[test]
    fn failed_media_commit_releases_reserved_media_and_retained_bytes() {
        let mut configured = limits(10);
        configured.max_media_decoded_bytes = 7;
        configured.max_retained_bytes = 7;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let reservation = budget.reserve_media_bytes(7).unwrap();
        assert_eq!(reservation.maximum_bytes(), 7);
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(0)
        );
        assert_eq!(budget.remaining(HydrationResource::RetainedBytes), Ok(0));
        assert_eq!(
            reservation.commit(7, 1),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes
            })
        );
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(7)
        );
        assert_eq!(budget.remaining(HydrationResource::RetainedBytes), Ok(7));
    }

    #[test]
    fn inventory_content_projection_and_change_caps_are_independent() {
        let mut configured = limits(10);
        configured.max_inventory_items = 1;
        configured.max_inventory_encoded_bytes = 3;
        configured.max_rendered_content_bytes = 3;
        configured.max_projections = 1;
        configured.max_changes = 1;
        configured.max_retained_bytes = 100;
        let budget = InitialHydrationBudget::new(configured).unwrap();

        budget.account_inventory(1, 3).unwrap();
        assert_eq!(
            budget.account_inventory(1, 0),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::InventoryItems
            })
        );
        budget.account_rendered_content(3).unwrap();
        assert_eq!(
            budget.account_rendered_content(1),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RenderedContentBytes
            })
        );
        budget.account_projections(1, 0).unwrap();
        assert_eq!(
            budget.account_projections(1, 0),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::Projections
            })
        );
        budget.account_changes(1, 2).unwrap();
        assert_eq!(
            budget.account_changes(1, 0),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::Changes
            })
        );
    }

    #[test]
    fn clones_share_provider_call_accounting() {
        let mut configured = limits(10);
        configured.max_provider_calls = 1;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        budget.clone().reserve_provider_call().unwrap();
        assert_eq!(
            budget.reserve_provider_call(),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::ProviderCalls
            })
        );
    }

    #[test]
    fn elapsed_deadline_stops_before_reserving_a_provider_call() {
        let mut configured = limits(10);
        configured.provider_deadline_ms = 1;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            budget.reserve_provider_call(),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::ProviderDeadline
            })
        );
        assert_eq!(budget.remaining(HydrationResource::ProviderCalls), Ok(10));
    }

    #[test]
    fn depth_and_node_limits_are_independent() {
        let mut configured = limits(10);
        configured.max_traversal_depth = 2;
        configured.max_traversal_nodes = 2;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        budget.visit_traversal_node(0).unwrap();
        budget.visit_traversal_node(2).unwrap();
        assert_eq!(
            budget.visit_traversal_node(3),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::TraversalDepth
            })
        );
        assert_eq!(
            budget.visit_traversal_node(2),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::TraversalNodes
            })
        );
    }

    #[test]
    fn limit_errors_are_permanent_and_provider_details_are_redacted() {
        let error = InitialHydrationError::from_connector_error(LocalityError::Io(
            "bearer secret-token at signed URL".to_string(),
        ));
        assert_eq!(
            error.to_string(),
            "initial hydration provider is unavailable"
        );
        assert!(!error.to_string().contains("secret-token"));
        assert!(!error.is_permanent());
        assert!(
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::NativeBytes
            }
            .is_permanent()
        );
    }

    #[test]
    fn rate_limit_keeps_retry_after_without_body_message() {
        let error = InitialHydrationError::from_connector_error(LocalityError::RateLimited {
            provider: "notion".to_string(),
            retry_after: Duration::from_secs(7),
            message: "signed-url-secret".to_string(),
        });
        assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(
            error.to_string(),
            "notion rate limited initial hydration for 7000ms"
        );
        assert!(!format!("{error:?}").contains("signed-url-secret"));
    }
}
