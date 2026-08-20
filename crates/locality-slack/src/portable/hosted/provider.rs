use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use locality_connector::network::{ConnectorNetworkConfig, RetryConfig};
use locality_protocol::{
    HostedSlackChannelSelector, SlackChannelSharingClassification, SlackInstallationId,
};
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time::Instant as TokioInstant;

use super::checkpoint::{
    HostedSlackPollCheckpointV1, HostedSlackPollError, HostedSlackPollKindV2,
    HostedSlackPollPhaseV1,
};
use super::identity::{
    HostedSlackInstallationBinding, HostedSlackObservedInstallationIdentity,
    HostedSlackPortableError, validate_slack_id,
};
use super::native::{
    HostedSlackConversationKindV1, HostedSlackFileMetadata, HostedSlackUser,
    MAX_HOSTED_SLACK_MESSAGE_FILES, MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES, RawHostedSlackChannel,
    RawHostedSlackFileMetadata, RawHostedSlackMessage, RawHostedSlackUser,
};
use super::poll::{
    HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V1, HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V2,
    HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V3, HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V1,
    HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V2,
    HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V3, HostedSlackHistoryMessageV1,
    HostedSlackHistoryPageV2, HostedSlackPollOutputV1, HostedSlackRepliesPageV2,
    hosted_slack_history_page_reference_closure_v2, hosted_slack_replies_page_reference_closure_v2,
};

pub const HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1: u32 = 15;
pub const MAX_HOSTED_SLACK_PROVIDER_RESPONSE_BYTES_V1: usize = 512 * 1024;
pub const MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1: usize = 256;
pub const MAX_HOSTED_SLACK_PROVIDER_PAGE_APPLICATIONS_V1: usize = 256;
pub const MAX_HOSTED_SLACK_PROVIDER_REQUESTS_V1: usize = 4 * 1024;
pub const MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1: Duration = Duration::from_secs(5 * 60);
const HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER: Duration = Duration::from_secs(6 * 60);
pub const HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1: u32 = 100;
pub const MAX_HOSTED_SLACK_DISCOVERY_PAGES_V1: usize = 10;
pub const MAX_HOSTED_SLACK_DISCOVERY_CHANNELS_V1: usize = 1_000;
pub const MAX_HOSTED_SLACK_DISCOVERY_CURSOR_BYTES_V1: usize = 512;

const HOSTED_SLACK_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const HOSTED_SLACK_MAX_RETRIES: usize = 4;

static HOSTED_SLACK_CRYPTO_PROVIDER: OnceLock<()> = OnceLock::new();
static HOSTED_SLACK_PROVIDER_GATES: OnceLock<
    Mutex<BTreeMap<HostedSlackProviderCoordinationScopeV2, HostedSlackMethodGate>>,
> = OnceLock::new();

pub type HostedSlackProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, HostedSlackProviderError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedSlackProviderOperationV1 {
    VerifyInstallation,
    ConversationsList,
    ConversationsInfo,
    ConversationsHistory,
    ConversationsReplies,
    UsersInfo,
    FilesInfo,
}

/// Legacy team-and-operation key for durable hosted request coordination.
///
/// This V1 shape is retained for existing durable cooldown state. New durable
/// coordinators should use [`HostedSlackProviderCoordinationScopeV2`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackProviderCoordinationScopeV1 {
    pub team_id: String,
    pub operation: HostedSlackProviderOperationV1,
}

/// Exact Slack Web API method used for V2 hosted request coordination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HostedSlackApiMethodV2 {
    #[serde(rename = "auth.test")]
    AuthTest,
    #[serde(rename = "conversations.list")]
    ConversationsList,
    #[serde(rename = "conversations.info")]
    ConversationsInfo,
    #[serde(rename = "conversations.history")]
    ConversationsHistory,
    #[serde(rename = "conversations.replies")]
    ConversationsReplies,
    #[serde(rename = "users.info")]
    UsersInfo,
    #[serde(rename = "files.info")]
    FilesInfo,
}

/// Stable app, team, and exact-method key for V2 hosted request coordination.
///
/// The HTTP provider's built-in gate is process-local only. A hosted backend can
/// persist and coordinate this serializable, non-secret scope outside the public
/// provider.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackProviderCoordinationScopeV2 {
    pub api_app_id: String,
    pub team_id: String,
    pub method: HostedSlackApiMethodV2,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackHistoryRequestV1 {
    pub installation_id: SlackInstallationId,
    pub team_id: String,
    pub channel_id: String,
    pub phase: HostedSlackPollPhaseV1,
    pub oldest: String,
    pub latest: String,
    pub inclusive: bool,
    pub cursor: Option<String>,
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackRepliesRequestV1 {
    pub installation_id: SlackInstallationId,
    pub team_id: String,
    pub channel_id: String,
    pub phase: HostedSlackPollPhaseV1,
    pub root_message_id: String,
    pub latest: String,
    pub inclusive: bool,
    pub cursor: Option<String>,
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackProviderMessageV1 {
    pub message: RawHostedSlackMessage,
    pub reply_count: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackProviderMessagePageV1 {
    pub observed_at: String,
    pub has_more: Option<bool>,
    pub next_cursor: Option<String>,
    pub messages: Vec<HostedSlackProviderMessageV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackObservedChannelAuthorityV1 {
    pub team_id: String,
    pub channel_id: String,
    pub is_private: bool,
    pub is_shared: bool,
    pub is_externally_shared: bool,
    pub is_org_shared: bool,
    pub is_member: bool,
    pub shared_team_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackChannelDiscoveryRequestV1 {
    pub cursor: Option<String>,
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackDiscoveredChannelV1 {
    pub channel: RawHostedSlackChannel,
    pub is_member: bool,
    pub is_archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackChannelDiscoveryPageV1 {
    pub observed_at: String,
    pub next_cursor: Option<String>,
    pub channels: Vec<HostedSlackDiscoveredChannelV1>,
}

pub trait HostedSlackDiscoveryProviderPort: Debug + Send + Sync {
    fn conversations_list(
        &self,
        request: HostedSlackChannelDiscoveryRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackChannelDiscoveryPageV1>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum HostedSlackProviderRequestV1 {
    VerifyInstallation,
    ConversationsInfo {
        channel_id: String,
    },
    ConversationsHistory {
        request: HostedSlackHistoryRequestV1,
    },
    ConversationsReplies {
        request: HostedSlackRepliesRequestV1,
    },
    UsersInfo {
        user_id: String,
    },
    FilesInfo {
        file_id: String,
        channel_id: String,
    },
}

pub trait HostedSlackProviderPort: Debug + Send + Sync {
    fn verify_installation(
        &self,
    ) -> HostedSlackProviderFuture<'_, HostedSlackObservedInstallationIdentity>;

    fn conversations_info(
        &self,
        channel_id: String,
    ) -> HostedSlackProviderFuture<'_, HostedSlackObservedChannelAuthorityV1>;

    fn conversations_history(
        &self,
        request: HostedSlackHistoryRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1>;

    fn conversations_replies(
        &self,
        request: HostedSlackRepliesRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1>;

    fn users_info(&self, user_id: String) -> HostedSlackProviderFuture<'_, RawHostedSlackUser>;

    fn files_info(
        &self,
        file_id: String,
        channel_id: String,
    ) -> HostedSlackProviderFuture<'_, RawHostedSlackFileMetadata>;
}

#[derive(Clone, Debug)]
pub struct HostedSlackCancellationToken {
    cancelled: Arc<watch::Sender<bool>>,
}

impl HostedSlackCancellationToken {
    pub fn new() -> Self {
        let (cancelled, _receiver) = watch::channel(false);
        Self {
            cancelled: Arc::new(cancelled),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    async fn cancelled(&self) {
        let mut receiver = self.cancelled.subscribe();
        if *receiver.borrow() {
            return;
        }
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

impl Default for HostedSlackCancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct HostedSlackDriveControlV1 {
    pub catch_up_cut_at: Option<String>,
    deadline: Instant,
    cancellation: HostedSlackCancellationToken,
    max_page_applications: usize,
    max_provider_requests: usize,
}

impl HostedSlackDriveControlV1 {
    pub fn new(
        deadline: Instant,
        cancellation: HostedSlackCancellationToken,
        catch_up_cut_at: Option<String>,
    ) -> Self {
        Self {
            catch_up_cut_at,
            deadline,
            cancellation,
            max_page_applications: MAX_HOSTED_SLACK_PROVIDER_PAGE_APPLICATIONS_V1,
            max_provider_requests: MAX_HOSTED_SLACK_PROVIDER_REQUESTS_V1,
        }
    }

    pub fn with_budgets(
        mut self,
        max_page_applications: usize,
        max_provider_requests: usize,
    ) -> Result<Self, HostedSlackProviderError> {
        if max_page_applications == 0
            || max_page_applications > MAX_HOSTED_SLACK_PROVIDER_PAGE_APPLICATIONS_V1
        {
            return Err(HostedSlackProviderError::LimitExceeded(
                "page application budget",
            ));
        }
        if max_provider_requests == 0
            || max_provider_requests > MAX_HOSTED_SLACK_PROVIDER_REQUESTS_V1
        {
            return Err(HostedSlackProviderError::LimitExceeded(
                "provider request budget",
            ));
        }
        self.max_page_applications = max_page_applications;
        self.max_provider_requests = max_provider_requests;
        Ok(self)
    }

    pub fn cancellation(&self) -> &HostedSlackCancellationToken {
        &self.cancellation
    }

    fn remaining(&self) -> Option<Duration> {
        self.deadline.checked_duration_since(Instant::now())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedSlackDrivePendingReasonV1 {
    AwaitingCatchUpCut,
    PageBudgetExhausted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedSlackDriveOutcomeV1 {
    Complete(Box<HostedSlackPollOutputV1>),
    Pending {
        phase: HostedSlackPollPhaseV1,
        reason: HostedSlackDrivePendingReasonV1,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackChannelDiscoveryV1 {
    pub installation: HostedSlackObservedInstallationIdentity,
    pub observed_at: String,
    pub channels: Vec<HostedSlackDiscoveredChannelV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedSlackInitialChannelDescriptorV1 {
    pub selector: HostedSlackChannelSelector,
    pub channel: RawHostedSlackChannel,
    pub discovered_at: String,
}

impl HostedSlackInitialChannelDescriptorV1 {
    pub fn new(
        binding: &HostedSlackInstallationBinding,
        discovered: &HostedSlackDiscoveredChannelV1,
        discovered_at: String,
        authorized_history_start_at: String,
    ) -> Result<Self, HostedSlackProviderError> {
        binding.validate()?;
        if discovered.is_archived {
            return Err(HostedSlackProviderError::Revoked);
        }
        if !discovered.is_member {
            return Err(HostedSlackProviderError::NotFound("channel membership"));
        }
        let channel = super::native::HostedSlackChannel::try_from(discovered.channel.clone())?;
        ensure_v1_channel_identity_supported(channel.sharing())?;
        if channel.team_id() != binding.team_id {
            return Err(HostedSlackProviderError::IdentityMismatch(
                "channel team_id",
            ));
        }
        let selector = HostedSlackChannelSelector {
            selector_version: 1,
            installation_id: binding.installation_id.clone(),
            team_id: binding.team_id.clone(),
            channel_id: channel.channel_id().to_owned(),
            authorized_history_start_at,
            sharing: channel.sharing(),
        };
        selector
            .validate()
            .map_err(|_| HostedSlackProviderError::InvalidResponse("history horizon"))?;
        super::checkpoint::parse_canonical_utc_timestamp("discovered_at", &discovered_at)?;
        Ok(Self {
            selector,
            channel: discovered.channel.clone(),
            discovered_at,
        })
    }
}

pub async fn discover_hosted_slack_channels_v1<P>(
    provider: &P,
    binding: &HostedSlackInstallationBinding,
    control: &HostedSlackDriveControlV1,
) -> Result<HostedSlackChannelDiscoveryV1, HostedSlackProviderError>
where
    P: HostedSlackProviderPort + HostedSlackDiscoveryProviderPort,
{
    binding.validate()?;
    ensure_active(control)?;
    let mut request_count = 0;
    let installation = call_with_retry(
        control,
        &mut request_count,
        HostedSlackProviderOperationV1::VerifyInstallation,
        || provider.verify_installation(),
    )
    .await?;
    binding
        .verify_observed_identity(&installation)
        .map_err(|_| HostedSlackProviderError::IdentityMismatch("installation"))?;
    let mut cursor = None;
    let mut channels = Vec::new();
    let mut observed_at = None;
    let mut channel_ids = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    for _ in 0..MAX_HOSTED_SLACK_DISCOVERY_PAGES_V1 {
        let request = HostedSlackChannelDiscoveryRequestV1 {
            cursor: cursor.clone(),
            limit: HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1,
        };
        let page = call_with_retry(
            control,
            &mut request_count,
            HostedSlackProviderOperationV1::ConversationsList,
            || provider.conversations_list(request.clone()),
        )
        .await?;
        super::checkpoint::parse_canonical_utc_timestamp(
            "discovery observed_at",
            &page.observed_at,
        )?;
        if page.channels.len() > HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1 as usize {
            return Err(HostedSlackProviderError::LimitExceeded(
                "discovery page channels",
            ));
        }
        observed_at.get_or_insert(page.observed_at.clone());
        for discovered in page.channels {
            let channel = super::native::HostedSlackChannel::try_from(discovered.channel.clone())?;
            ensure_v1_channel_identity_supported(channel.sharing())?;
            if channel.team_id() != binding.team_id {
                return Err(HostedSlackProviderError::IdentityMismatch(
                    "channel team_id",
                ));
            }
            if !channel_ids.insert(channel.channel_id().to_owned()) {
                return Err(HostedSlackProviderError::InvalidResponse(
                    "duplicate channel",
                ));
            }
            channels.push(discovered);
            if channels.len() > MAX_HOSTED_SLACK_DISCOVERY_CHANNELS_V1 {
                return Err(HostedSlackProviderError::LimitExceeded(
                    "discovery channels",
                ));
            }
        }
        cursor = page.next_cursor.filter(|value| !value.is_empty());
        if let Some(next_cursor) = &cursor {
            if next_cursor.len() > MAX_HOSTED_SLACK_DISCOVERY_CURSOR_BYTES_V1 {
                return Err(HostedSlackProviderError::LimitExceeded("discovery cursor"));
            }
            if !cursors.insert(next_cursor.clone()) {
                return Err(HostedSlackProviderError::InvalidResponse(
                    "repeated discovery cursor",
                ));
            }
        }
        if cursor.is_none() {
            return Ok(HostedSlackChannelDiscoveryV1 {
                installation,
                observed_at: observed_at.unwrap_or_else(current_canonical_utc),
                channels,
            });
        }
    }
    Err(HostedSlackProviderError::LimitExceeded("discovery pages"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostedSlackProviderError {
    Authentication,
    Revoked,
    NotFound(&'static str),
    RateLimited { retry_after: Duration },
    LimitExceeded(&'static str),
    Transient,
    InvalidResponse(&'static str),
    IdentityMismatch(&'static str),
    Unsupported(&'static str),
    ThreadNotFound,
    Cancelled,
    DeadlineExceeded,
    Poll(HostedSlackPollError),
    Portable(HostedSlackPortableError),
}

impl HostedSlackProviderError {
    fn retry_delay(
        &self,
        operation: HostedSlackProviderOperationV1,
        attempt: usize,
    ) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => Some(*retry_after),
            Self::Transient => Some(operation.retry_config().backoff(attempt)),
            _ => None,
        }
    }
}

impl Display for HostedSlackProviderError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication => formatter.write_str("Slack authentication failed"),
            Self::Revoked => formatter.write_str("Slack installation access is revoked"),
            Self::NotFound(resource) => write!(formatter, "Slack {resource} was not found"),
            Self::RateLimited { retry_after } => write!(
                formatter,
                "Slack rate limit requires a {} second retry delay",
                retry_after.as_secs()
            ),
            Self::LimitExceeded(limit) => write!(formatter, "hosted Slack {limit} exceeded"),
            Self::Transient => formatter.write_str("Slack transient provider failure"),
            Self::InvalidResponse(reason) => {
                write!(
                    formatter,
                    "Slack response is incomplete or invalid: {reason}"
                )
            }
            Self::IdentityMismatch(field) => {
                write!(formatter, "Slack installation identity mismatch: {field}")
            }
            Self::Unsupported(feature) => write!(formatter, "unsupported hosted Slack {feature}"),
            Self::ThreadNotFound => formatter.write_str("Slack thread root was not found"),
            Self::Cancelled => formatter.write_str("hosted Slack polling was cancelled"),
            Self::DeadlineExceeded => {
                formatter.write_str("hosted Slack polling deadline was exceeded")
            }
            Self::Poll(error) => Display::fmt(error, formatter),
            Self::Portable(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for HostedSlackProviderError {}

impl From<HostedSlackPollError> for HostedSlackProviderError {
    fn from(value: HostedSlackPollError) -> Self {
        Self::Poll(value)
    }
}

impl From<HostedSlackPortableError> for HostedSlackProviderError {
    fn from(value: HostedSlackPortableError) -> Self {
        Self::Portable(value)
    }
}

impl HostedSlackProviderOperationV1 {
    pub const fn api_method(self) -> HostedSlackApiMethodV2 {
        match self {
            Self::VerifyInstallation => HostedSlackApiMethodV2::AuthTest,
            Self::ConversationsList => HostedSlackApiMethodV2::ConversationsList,
            Self::ConversationsInfo => HostedSlackApiMethodV2::ConversationsInfo,
            Self::ConversationsHistory => HostedSlackApiMethodV2::ConversationsHistory,
            Self::ConversationsReplies => HostedSlackApiMethodV2::ConversationsReplies,
            Self::UsersInfo => HostedSlackApiMethodV2::UsersInfo,
            Self::FilesInfo => HostedSlackApiMethodV2::FilesInfo,
        }
    }

    fn retry_config(self) -> RetryConfig {
        match self {
            Self::VerifyInstallation
            | Self::ConversationsList
            | Self::ConversationsInfo
            | Self::UsersInfo
            | Self::FilesInfo => RetryConfig::exponential(
                HOSTED_SLACK_MAX_RETRIES,
                Duration::from_secs(1),
                Duration::from_secs(16),
            ),
            Self::ConversationsHistory | Self::ConversationsReplies => RetryConfig::exponential(
                HOSTED_SLACK_MAX_RETRIES,
                Duration::from_secs(15),
                Duration::from_secs(60),
            ),
        }
    }
}

impl HostedSlackApiMethodV2 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthTest => "auth.test",
            Self::ConversationsList => "conversations.list",
            Self::ConversationsInfo => "conversations.info",
            Self::ConversationsHistory => "conversations.history",
            Self::ConversationsReplies => "conversations.replies",
            Self::UsersInfo => "users.info",
            Self::FilesInfo => "files.info",
        }
    }
}

pub async fn drive_hosted_slack_poll_v1<P: HostedSlackProviderPort>(
    provider: &P,
    binding: &HostedSlackInstallationBinding,
    selector: &HostedSlackChannelSelector,
    checkpoint: &mut HostedSlackPollCheckpointV1,
    control: &HostedSlackDriveControlV1,
) -> Result<HostedSlackDriveOutcomeV1, HostedSlackProviderError> {
    validate_drive_identity(binding, selector, checkpoint)?;
    ensure_active(control)?;

    let mut request_count = 0;
    let observed = call_with_retry(
        control,
        &mut request_count,
        HostedSlackProviderOperationV1::VerifyInstallation,
        || provider.verify_installation(),
    )
    .await?;
    binding
        .verify_observed_identity(&observed)
        .map_err(|error| match error {
            HostedSlackPortableError::IdentityMismatch(field) => {
                HostedSlackProviderError::IdentityMismatch(field)
            }
            other => HostedSlackProviderError::Portable(other),
        })?;
    let channel_id = selector.channel_id.clone();
    let channel_authority = call_with_retry(
        control,
        &mut request_count,
        HostedSlackProviderOperationV1::ConversationsInfo,
        || provider.conversations_info(channel_id.clone()),
    )
    .await?;
    verify_channel_authority(selector, &observed, &channel_authority)?;

    let mut applied_pages = 0;
    loop {
        ensure_active(control)?;
        match checkpoint.phase() {
            HostedSlackPollPhaseV1::CompleteCandidate => {
                return Ok(HostedSlackDriveOutcomeV1::Complete(Box::new(
                    checkpoint.completed_output()?,
                )));
            }
            HostedSlackPollPhaseV1::AwaitingCatchUpCut => {
                let Some(cut) = &control.catch_up_cut_at else {
                    return Ok(HostedSlackDriveOutcomeV1::Pending {
                        phase: checkpoint.phase(),
                        reason: HostedSlackDrivePendingReasonV1::AwaitingCatchUpCut,
                    });
                };
                ensure_active(control)?;
                checkpoint.begin_catch_up(cut.clone())?;
            }
            HostedSlackPollPhaseV1::HistoricalHistory | HostedSlackPollPhaseV1::CatchUpHistory => {
                if applied_pages >= control.max_page_applications {
                    return Ok(HostedSlackDriveOutcomeV1::Pending {
                        phase: checkpoint.phase(),
                        reason: HostedSlackDrivePendingReasonV1::PageBudgetExhausted,
                    });
                }
                let request = history_request(checkpoint)?;
                let response = call_with_retry(
                    control,
                    &mut request_count,
                    HostedSlackProviderOperationV1::ConversationsHistory,
                    || provider.conversations_history(request.clone()),
                )
                .await?;
                let mut page = history_poll_page(checkpoint, &request, response)?;
                enrich_history_page(provider, checkpoint, control, &mut request_count, &mut page)
                    .await?;
                ensure_active(control)?;
                checkpoint.apply_history_page_v2(&page)?;
                applied_pages += 1;
            }
            HostedSlackPollPhaseV1::HistoricalReplies | HostedSlackPollPhaseV1::CatchUpReplies => {
                if applied_pages >= control.max_page_applications {
                    return Ok(HostedSlackDriveOutcomeV1::Pending {
                        phase: checkpoint.phase(),
                        reason: HostedSlackDrivePendingReasonV1::PageBudgetExhausted,
                    });
                }
                let request = replies_request(checkpoint)?;
                let response = match call_with_retry(
                    control,
                    &mut request_count,
                    HostedSlackProviderOperationV1::ConversationsReplies,
                    || provider.conversations_replies(request.clone()),
                )
                .await
                {
                    Ok(response) => response,
                    Err(HostedSlackProviderError::ThreadNotFound) => {
                        let page = deleted_root_reconciliation_page(checkpoint, &request)?;
                        ensure_active(control)?;
                        checkpoint.apply_replies_page_v2(&page)?;
                        applied_pages += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let mut page = replies_poll_page(checkpoint, &request, response)?;
                enrich_replies_page(provider, checkpoint, control, &mut request_count, &mut page)
                    .await?;
                ensure_active(control)?;
                checkpoint.apply_replies_page_v2(&page)?;
                applied_pages += 1;
            }
        }
    }
}

fn verify_channel_authority(
    selector: &HostedSlackChannelSelector,
    installation: &HostedSlackObservedInstallationIdentity,
    authority: &HostedSlackObservedChannelAuthorityV1,
) -> Result<(), HostedSlackProviderError> {
    validate_slack_id("provider.channel.team_id", &authority.team_id, b"T")?;
    validate_slack_id("provider.channel.channel_id", &authority.channel_id, b"CG")?;
    if authority.shared_team_ids.len() > MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1 {
        return Err(HostedSlackProviderError::LimitExceeded(
            "channel shared team ids",
        ));
    }
    for team_id in &authority.shared_team_ids {
        validate_slack_id("provider.channel.shared_team_id", team_id, b"T")?;
    }
    if authority
        .shared_team_ids
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
    {
        return Err(HostedSlackProviderError::InvalidResponse(
            "channel shared team ids",
        ));
    }
    if authority.team_id != selector.team_id || authority.team_id != installation.team_id {
        return Err(HostedSlackProviderError::IdentityMismatch(
            "channel team_id",
        ));
    }
    if authority.channel_id != selector.channel_id {
        return Err(HostedSlackProviderError::IdentityMismatch("channel_id"));
    }
    if !authority.is_member {
        return Err(HostedSlackProviderError::Revoked);
    }
    if authority.is_org_shared || authority.is_shared != authority.is_externally_shared {
        return Err(HostedSlackProviderError::InvalidResponse(
            "channel sharing facts",
        ));
    }
    if authority.is_externally_shared {
        return Err(HostedSlackProviderError::Unsupported(
            "Slack Connect channel identity in V1",
        ));
    }
    if authority.is_shared
        && authority
            .shared_team_ids
            .binary_search(&selector.team_id)
            .is_err()
    {
        return Err(HostedSlackProviderError::IdentityMismatch(
            "channel shared team_id",
        ));
    }
    if authority.is_externally_shared
        && !authority
            .shared_team_ids
            .iter()
            .any(|team_id| team_id != &selector.team_id)
    {
        return Err(HostedSlackProviderError::InvalidResponse(
            "channel sharing facts",
        ));
    }
    if !authority.is_shared
        && authority
            .shared_team_ids
            .iter()
            .any(|team_id| team_id != &selector.team_id)
    {
        return Err(HostedSlackProviderError::InvalidResponse(
            "channel sharing facts",
        ));
    }
    let sharing = match (authority.is_externally_shared, authority.is_private) {
        (false, false) => SlackChannelSharingClassification::Public,
        (false, true) => SlackChannelSharingClassification::Private,
        (true, false) => SlackChannelSharingClassification::ExternallySharedPublic,
        (true, true) => SlackChannelSharingClassification::ExternallySharedPrivate,
    };
    if sharing != selector.sharing {
        return Err(HostedSlackProviderError::IdentityMismatch(
            "channel sharing",
        ));
    }
    Ok(())
}

fn validate_drive_identity(
    binding: &HostedSlackInstallationBinding,
    selector: &HostedSlackChannelSelector,
    checkpoint: &HostedSlackPollCheckpointV1,
) -> Result<(), HostedSlackProviderError> {
    binding.validate()?;
    selector
        .validate()
        .map_err(|_| HostedSlackProviderError::IdentityMismatch("selector"))?;
    checkpoint.validate()?;
    let checkpoint_selector = checkpoint.selector();
    for (field, matches) in [
        (
            "installation_id",
            binding.installation_id == selector.installation_id
                && checkpoint_selector.installation_id == selector.installation_id,
        ),
        (
            "team_id",
            binding.team_id == selector.team_id && checkpoint_selector.team_id == selector.team_id,
        ),
        (
            "channel_id",
            checkpoint_selector.channel_id == selector.channel_id,
        ),
        (
            "authorized_history_start_at",
            checkpoint_selector.authorized_history_start_at == selector.authorized_history_start_at,
        ),
        ("sharing", checkpoint_selector.sharing == selector.sharing),
    ] {
        if !matches {
            return Err(HostedSlackProviderError::IdentityMismatch(field));
        }
    }
    Ok(())
}

fn history_request(
    checkpoint: &HostedSlackPollCheckpointV1,
) -> Result<HostedSlackHistoryRequestV1, HostedSlackProviderError> {
    let (oldest, latest) = match checkpoint.phase() {
        HostedSlackPollPhaseV1::HistoricalHistory => (
            checkpoint.authorized_history_start_at().to_string(),
            checkpoint.backfill_cut_at().to_string(),
        ),
        HostedSlackPollPhaseV1::CatchUpHistory => (
            checkpoint.poll_overlap_watermark().to_string(),
            checkpoint
                .poll_cut_at()
                .expect("catch-up history has a validated cut")
                .to_string(),
        ),
        _ => unreachable!("history request only constructed in a history phase"),
    };
    let selector = checkpoint.selector();
    Ok(HostedSlackHistoryRequestV1 {
        installation_id: selector.installation_id,
        team_id: selector.team_id,
        channel_id: selector.channel_id,
        phase: checkpoint.phase(),
        oldest: canonical_slack_timestamp(&oldest, true)?,
        latest: canonical_slack_timestamp(&latest, false)?,
        inclusive: false,
        cursor: checkpoint.history_cursor().map(str::to_string),
        limit: HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1,
    })
}

fn replies_request(
    checkpoint: &HostedSlackPollCheckpointV1,
) -> Result<HostedSlackRepliesRequestV1, HostedSlackProviderError> {
    let latest = match checkpoint.phase() {
        HostedSlackPollPhaseV1::HistoricalReplies => checkpoint.backfill_cut_at(),
        HostedSlackPollPhaseV1::CatchUpReplies => checkpoint
            .poll_cut_at()
            .ok_or(HostedSlackProviderError::InvalidResponse("catch-up cut"))?,
        _ => {
            return Err(HostedSlackProviderError::InvalidResponse(
                "replies request phase",
            ));
        }
    };
    let selector = checkpoint.selector();
    Ok(HostedSlackRepliesRequestV1 {
        installation_id: selector.installation_id,
        team_id: selector.team_id,
        channel_id: selector.channel_id,
        phase: checkpoint.phase(),
        root_message_id: checkpoint
            .current_root_message_id()
            .ok_or(HostedSlackProviderError::InvalidResponse("current root"))?
            .to_string(),
        latest: canonical_slack_timestamp(latest, false)?,
        inclusive: false,
        cursor: checkpoint.reply_cursor().map(str::to_string),
        limit: HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1,
    })
}

fn poll_page_version(checkpoint: &HostedSlackPollCheckpointV1) -> (u16, u16) {
    if checkpoint.poll_kind_v2() == HostedSlackPollKindV2::Incremental {
        (
            HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V3,
            HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V3,
        )
    } else {
        (
            HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V1,
            HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V1,
        )
    }
}

fn history_poll_page(
    checkpoint: &HostedSlackPollCheckpointV1,
    request: &HostedSlackHistoryRequestV1,
    response: HostedSlackProviderMessagePageV1,
) -> Result<HostedSlackHistoryPageV2, HostedSlackProviderError> {
    let next_cursor = validate_provider_pagination(request.cursor.as_deref(), &response)?;
    let messages = response
        .messages
        .into_iter()
        .map(|provided| {
            let is_root = normalized_root_id(&provided.message).is_none();
            let reply_count = if is_root {
                provided.reply_count.unwrap_or(0)
            } else {
                if provided.reply_count.is_some_and(|count| count != 0) {
                    return Err(HostedSlackProviderError::InvalidResponse(
                        "reply reply_count",
                    ));
                }
                0
            };
            Ok(HostedSlackHistoryMessageV1 {
                message: provided.message,
                reply_count,
            })
        })
        .collect::<Result<Vec<_>, HostedSlackProviderError>>()?;
    let selector = checkpoint.selector();
    let (page_format_version, minimum_reader_version) = poll_page_version(checkpoint);
    let page = HostedSlackHistoryPageV2 {
        page_format_version,
        minimum_reader_version,
        poll_kind: checkpoint.poll_kind_v2(),
        phase: request.phase,
        installation_id: selector.installation_id,
        team_id: selector.team_id,
        channel_id: selector.channel_id,
        sharing: selector.sharing,
        authorized_history_start_at: selector.authorized_history_start_at,
        backfill_cut_at: checkpoint.backfill_cut_at().to_string(),
        poll_cut_at: checkpoint.poll_cut_at().map(str::to_string),
        poll_overlap_watermark: checkpoint.poll_overlap_watermark().to_string(),
        request_cursor: request.cursor.clone(),
        next_cursor,
        observed_at: response.observed_at,
        messages,
        users: Vec::new(),
        files: Vec::new(),
    };
    page.validate()?;
    Ok(page)
}

fn replies_poll_page(
    checkpoint: &HostedSlackPollCheckpointV1,
    request: &HostedSlackRepliesRequestV1,
    response: HostedSlackProviderMessagePageV1,
) -> Result<HostedSlackRepliesPageV2, HostedSlackProviderError> {
    let next_cursor = validate_provider_pagination(request.cursor.as_deref(), &response)?;
    let root_reply_count = if request.cursor.is_none() {
        response
            .messages
            .iter()
            .find(|provided| {
                provided.message.ts == request.root_message_id
                    && normalized_root_id(&provided.message).is_none()
            })
            .map(|provided| {
                provided
                    .reply_count
                    .ok_or(HostedSlackProviderError::InvalidResponse(
                        "initial replies root reply_count",
                    ))
            })
            .transpose()?
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "initial replies root reply_count",
            ))?
    } else {
        checkpoint
            .root_expected_reply_count(&request.root_message_id)
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "checkpoint root reply_count",
            ))?
    };
    let selector = checkpoint.selector();
    let (page_format_version, minimum_reader_version) = poll_page_version(checkpoint);
    let page = HostedSlackRepliesPageV2 {
        page_format_version,
        minimum_reader_version,
        poll_kind: checkpoint.poll_kind_v2(),
        phase: request.phase,
        installation_id: selector.installation_id,
        team_id: selector.team_id,
        channel_id: selector.channel_id,
        sharing: selector.sharing,
        authorized_history_start_at: selector.authorized_history_start_at,
        backfill_cut_at: checkpoint.backfill_cut_at().to_string(),
        poll_cut_at: checkpoint.poll_cut_at().map(str::to_string),
        poll_overlap_watermark: checkpoint.poll_overlap_watermark().to_string(),
        root_message_id: request.root_message_id.clone(),
        root_reply_count,
        request_cursor: request.cursor.clone(),
        next_cursor,
        observed_at: response.observed_at,
        messages: response
            .messages
            .into_iter()
            .map(|provided| provided.message)
            .collect(),
        users: Vec::new(),
        files: Vec::new(),
    };
    page.validate()?;
    Ok(page)
}

fn deleted_root_reconciliation_page(
    checkpoint: &HostedSlackPollCheckpointV1,
    request: &HostedSlackRepliesRequestV1,
) -> Result<HostedSlackRepliesPageV2, HostedSlackProviderError> {
    let mut root = checkpoint
        .candidate()
        .messages()
        .iter()
        .find(|message| {
            message.ts == request.root_message_id && normalized_root_id(message).is_none()
        })
        .cloned()
        .ok_or_else(|| HostedSlackPollError::MissingRoot(request.root_message_id.clone()))?;
    root.text.clear();
    root.edited_ts = None;
    root.deleted = true;
    root.file_ids.clear();
    let selector = checkpoint.selector();
    let (page_format_version, minimum_reader_version) =
        if checkpoint.poll_kind_v2() == HostedSlackPollKindV2::Incremental {
            (
                HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V3,
                HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V3,
            )
        } else {
            (
                HOSTED_SLACK_POLL_PAGE_FORMAT_VERSION_V2,
                HOSTED_SLACK_POLL_PAGE_MINIMUM_READER_VERSION_V2,
            )
        };
    let page = HostedSlackRepliesPageV2 {
        page_format_version,
        minimum_reader_version,
        poll_kind: checkpoint.poll_kind_v2(),
        phase: request.phase,
        installation_id: selector.installation_id,
        team_id: selector.team_id,
        channel_id: selector.channel_id,
        sharing: selector.sharing,
        authorized_history_start_at: selector.authorized_history_start_at,
        backfill_cut_at: checkpoint.backfill_cut_at().to_string(),
        poll_cut_at: checkpoint.poll_cut_at().map(str::to_string),
        poll_overlap_watermark: checkpoint.poll_overlap_watermark().to_string(),
        root_message_id: request.root_message_id.clone(),
        root_reply_count: 0,
        request_cursor: request.cursor.clone(),
        next_cursor: None,
        observed_at: current_canonical_utc(),
        messages: vec![root],
        users: Vec::new(),
        files: Vec::new(),
    };
    page.validate()?;
    Ok(page)
}

fn validate_provider_pagination(
    request_cursor: Option<&str>,
    response: &HostedSlackProviderMessagePageV1,
) -> Result<Option<String>, HostedSlackProviderError> {
    if response.messages.len() > HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1 as usize {
        return Err(HostedSlackProviderError::LimitExceeded(
            "provider page message limit",
        ));
    }
    let next_cursor = response
        .next_cursor
        .as_deref()
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string);
    match response.has_more {
        Some(true) if next_cursor.is_some() && !response.messages.is_empty() => {}
        Some(false) if next_cursor.is_none() => {}
        Some(_) | None => {
            return Err(HostedSlackProviderError::InvalidResponse(
                "pagination facts",
            ));
        }
    }
    if next_cursor.is_some() && next_cursor.as_deref() == request_cursor {
        return Err(HostedSlackProviderError::InvalidResponse(
            "repeated response cursor",
        ));
    }
    Ok(next_cursor)
}

async fn enrich_history_page<P: HostedSlackProviderPort>(
    provider: &P,
    checkpoint: &HostedSlackPollCheckpointV1,
    control: &HostedSlackDriveControlV1,
    request_count: &mut usize,
    page: &mut HostedSlackHistoryPageV2,
) -> Result<(), HostedSlackProviderError> {
    let closure = hosted_slack_history_page_reference_closure_v2(checkpoint, page)?;
    let (users, files) = fetch_reference_metadata(
        provider,
        checkpoint,
        control,
        request_count,
        closure.user_ids,
        closure.file_ids,
    )
    .await?;
    page.users = users;
    page.files = files;
    page.validate()?;
    Ok(())
}

async fn enrich_replies_page<P: HostedSlackProviderPort>(
    provider: &P,
    checkpoint: &HostedSlackPollCheckpointV1,
    control: &HostedSlackDriveControlV1,
    request_count: &mut usize,
    page: &mut HostedSlackRepliesPageV2,
) -> Result<(), HostedSlackProviderError> {
    let closure = hosted_slack_replies_page_reference_closure_v2(checkpoint, page)?;
    let (users, files) = fetch_reference_metadata(
        provider,
        checkpoint,
        control,
        request_count,
        closure.user_ids,
        closure.file_ids,
    )
    .await?;
    page.users = users;
    page.files = files;
    page.validate()?;
    Ok(())
}

async fn fetch_reference_metadata<P: HostedSlackProviderPort>(
    provider: &P,
    checkpoint: &HostedSlackPollCheckpointV1,
    control: &HostedSlackDriveControlV1,
    request_count: &mut usize,
    mut user_ids: Vec<String>,
    file_ids: Vec<String>,
) -> Result<(Vec<RawHostedSlackUser>, Vec<RawHostedSlackFileMetadata>), HostedSlackProviderError> {
    if user_ids.len() > MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1
        || file_ids.len() > MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1
        || user_ids.len().saturating_add(file_ids.len()) > MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1
    {
        return Err(HostedSlackProviderError::LimitExceeded(
            "metadata reference closure",
        ));
    }

    let existing_files = checkpoint
        .candidate()
        .files()
        .iter()
        .map(|file| (file.id.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut files = Vec::new();
    let mut closure_files = Vec::new();
    for file_id in file_ids {
        if let Some(existing) = existing_files.get(file_id.as_str()) {
            closure_files.push((*existing).clone());
            continue;
        }
        let channel_id = checkpoint.selector().channel_id;
        let file = call_with_retry(
            control,
            request_count,
            HostedSlackProviderOperationV1::FilesInfo,
            || provider.files_info(file_id.clone(), channel_id.clone()),
        )
        .await?;
        if file.id != file_id || file.channel_id != channel_id {
            return Err(HostedSlackProviderError::IdentityMismatch("file"));
        }
        HostedSlackFileMetadata::try_from(file.clone())?;
        closure_files.push(file.clone());
        files.push(file);
    }
    user_ids.extend(closure_files.iter().filter_map(|file| file.user_id.clone()));
    user_ids.sort();
    user_ids.dedup();
    if user_ids.len() > MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1
        || user_ids.len().saturating_add(closure_files.len())
            > MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1
    {
        return Err(HostedSlackProviderError::LimitExceeded(
            "metadata reference closure",
        ));
    }

    let existing_users = checkpoint
        .candidate()
        .users()
        .iter()
        .map(|user| user.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut users = Vec::new();
    for user_id in user_ids {
        if existing_users.contains(user_id.as_str()) {
            continue;
        }
        let user = call_with_retry(
            control,
            request_count,
            HostedSlackProviderOperationV1::UsersInfo,
            || provider.users_info(user_id.clone()),
        )
        .await?;
        if user.id != user_id || user.team_id != checkpoint.selector().team_id {
            return Err(HostedSlackProviderError::IdentityMismatch("user"));
        }
        HostedSlackUser::try_from(user.clone())?;
        users.push(user);
    }
    users.sort_by(|left, right| left.id.cmp(&right.id));
    files.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((users, files))
}

async fn call_with_retry<'a, T, F>(
    control: &'a HostedSlackDriveControlV1,
    request_count: &mut usize,
    operation: HostedSlackProviderOperationV1,
    mut call: F,
) -> Result<T, HostedSlackProviderError>
where
    F: FnMut() -> HostedSlackProviderFuture<'a, T>,
{
    let retry = operation.retry_config();
    for attempt in 0..=retry.max_retries {
        ensure_active(control)?;
        if *request_count >= control.max_provider_requests {
            return Err(HostedSlackProviderError::LimitExceeded(
                "provider request budget",
            ));
        }
        *request_count += 1;
        let result = await_provider_future(control, call()).await;
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                let Some(delay) = error.retry_delay(operation, attempt) else {
                    return Err(error);
                };
                if attempt == retry.max_retries {
                    return Err(error);
                }
                let in_drive_wait = delay.min(MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1);
                if control
                    .remaining()
                    .is_none_or(|remaining| in_drive_wait >= remaining)
                {
                    return Err(error);
                }
                wait_for_retry(control, in_drive_wait).await?;
                if delay > in_drive_wait {
                    return Err(error);
                }
            }
        }
    }
    Err(HostedSlackProviderError::Transient)
}

async fn await_provider_future<T>(
    control: &HostedSlackDriveControlV1,
    future: HostedSlackProviderFuture<'_, T>,
) -> Result<T, HostedSlackProviderError> {
    ensure_active(control)?;
    let deadline = tokio::time::Instant::from_std(control.deadline);
    tokio::select! {
        biased;
        _ = control.cancellation.cancelled() => Err(HostedSlackProviderError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(HostedSlackProviderError::DeadlineExceeded),
        result = future => result,
    }
}

async fn wait_for_retry(
    control: &HostedSlackDriveControlV1,
    delay: Duration,
) -> Result<(), HostedSlackProviderError> {
    let remaining = control
        .remaining()
        .ok_or(HostedSlackProviderError::DeadlineExceeded)?;
    if delay >= remaining {
        return Err(HostedSlackProviderError::DeadlineExceeded);
    }
    let deadline = tokio::time::Instant::from_std(control.deadline);
    tokio::select! {
        biased;
        _ = control.cancellation.cancelled() => Err(HostedSlackProviderError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(HostedSlackProviderError::DeadlineExceeded),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

fn ensure_active(control: &HostedSlackDriveControlV1) -> Result<(), HostedSlackProviderError> {
    if control.cancellation.is_cancelled() {
        return Err(HostedSlackProviderError::Cancelled);
    }
    if control
        .remaining()
        .is_none_or(|remaining| remaining.is_zero())
    {
        return Err(HostedSlackProviderError::DeadlineExceeded);
    }
    Ok(())
}

fn normalized_root_id(message: &RawHostedSlackMessage) -> Option<&str> {
    message
        .thread_ts
        .as_deref()
        .filter(|thread_ts| *thread_ts != message.ts)
}

#[derive(Clone)]
struct HostedSlackProviderGates {
    verify_installation: HostedSlackMethodGate,
    conversations_list: HostedSlackMethodGate,
    conversations_info: HostedSlackMethodGate,
    conversations_history: HostedSlackMethodGate,
    conversations_replies: HostedSlackMethodGate,
    users_info: HostedSlackMethodGate,
    files_info: HostedSlackMethodGate,
}

impl HostedSlackProviderGates {
    fn global(api_app_id: &str, team_id: &str) -> Self {
        let mut gates = HOSTED_SLACK_PROVIDER_GATES
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .expect("hosted Slack gate registry lock");
        let mut gate = |operation: HostedSlackProviderOperationV1| {
            let scope = HostedSlackProviderCoordinationScopeV2 {
                api_app_id: api_app_id.to_string(),
                team_id: team_id.to_string(),
                method: operation.api_method(),
            };
            gates
                .entry(scope)
                .or_insert_with(|| HostedSlackMethodGate::new(operation_network_config(operation)))
                .clone()
        };
        Self {
            verify_installation: gate(HostedSlackProviderOperationV1::VerifyInstallation),
            conversations_list: gate(HostedSlackProviderOperationV1::ConversationsList),
            conversations_info: gate(HostedSlackProviderOperationV1::ConversationsInfo),
            conversations_history: gate(HostedSlackProviderOperationV1::ConversationsHistory),
            conversations_replies: gate(HostedSlackProviderOperationV1::ConversationsReplies),
            users_info: gate(HostedSlackProviderOperationV1::UsersInfo),
            files_info: gate(HostedSlackProviderOperationV1::FilesInfo),
        }
    }

    fn gate(&self, operation: HostedSlackProviderOperationV1) -> &HostedSlackMethodGate {
        match operation {
            HostedSlackProviderOperationV1::VerifyInstallation => &self.verify_installation,
            HostedSlackProviderOperationV1::ConversationsList => &self.conversations_list,
            HostedSlackProviderOperationV1::ConversationsInfo => &self.conversations_info,
            HostedSlackProviderOperationV1::ConversationsHistory => &self.conversations_history,
            HostedSlackProviderOperationV1::ConversationsReplies => &self.conversations_replies,
            HostedSlackProviderOperationV1::UsersInfo => &self.users_info,
            HostedSlackProviderOperationV1::FilesInfo => &self.files_info,
        }
    }
}

#[derive(Clone)]
struct HostedSlackMethodGate {
    inner: Arc<HostedSlackMethodGateInner>,
}

struct HostedSlackMethodGateInner {
    config: ConnectorNetworkConfig,
    state: Mutex<HostedSlackMethodGateState>,
    changed: watch::Sender<u64>,
}

struct HostedSlackMethodGateState {
    waiting: usize,
    in_flight: usize,
    tokens: f64,
    last_refill: TokioInstant,
    cooldown: Option<HostedSlackMethodCooldown>,
}

struct HostedSlackMethodCooldown {
    started_at: TokioInstant,
    duration: Duration,
    checked_until: Option<TokioInstant>,
}

impl HostedSlackMethodCooldown {
    fn new(started_at: TokioInstant, duration: Duration) -> Self {
        Self {
            started_at,
            duration,
            checked_until: started_at.checked_add(duration),
        }
    }

    fn remaining(&self, now: TokioInstant) -> Duration {
        self.checked_until.map_or_else(
            || {
                self.duration
                    .saturating_sub(now.saturating_duration_since(self.started_at))
            },
            |until| until.saturating_duration_since(now),
        )
    }
}

impl HostedSlackMethodGateState {
    fn refill(&mut self, config: &ConnectorNetworkConfig, now: TokioInstant) {
        if let Some(cooldown) = &self.cooldown
            && !cooldown.remaining(now).is_zero()
        {
            return;
        }
        self.cooldown = None;
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * config.requests_per_second).min(config.burst);
        self.last_refill = now;
    }
}

impl HostedSlackMethodGate {
    fn new(config: ConnectorNetworkConfig) -> Self {
        debug_assert!(config.requests_per_second.is_finite());
        debug_assert!(config.requests_per_second > 0.0);
        debug_assert!(config.burst.is_finite());
        debug_assert!(config.burst >= 1.0);
        debug_assert!(config.max_in_flight > 0);
        let (changed, _receiver) = watch::channel(0);
        let tokens = config.burst;
        Self {
            inner: Arc::new(HostedSlackMethodGateInner {
                config,
                state: Mutex::new(HostedSlackMethodGateState {
                    waiting: 0,
                    in_flight: 0,
                    tokens,
                    last_refill: TokioInstant::now(),
                    cooldown: None,
                }),
                changed,
            }),
        }
    }

    async fn acquire(&self) -> HostedSlackMethodPermit {
        let mut changes = self.inner.changed.subscribe();
        let mut reservation = HostedSlackMethodReservation::new(self.inner.clone());
        loop {
            let delay = {
                let mut state = self.inner.state.lock().expect("hosted Slack gate lock");
                let now = TokioInstant::now();
                state.refill(&self.inner.config, now);
                if state.in_flight < self.inner.config.max_in_flight && state.tokens >= 1.0 {
                    state.waiting = state.waiting.saturating_sub(1);
                    state.in_flight += 1;
                    state.tokens -= 1.0;
                    reservation.complete();
                    return HostedSlackMethodPermit {
                        inner: self.inner.clone(),
                    };
                }
                if let Some(cooldown) = &state.cooldown {
                    Some(
                        cooldown
                            .remaining(now)
                            .min(MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1),
                    )
                } else if state.in_flight < self.inner.config.max_in_flight {
                    let missing = (1.0 - state.tokens).max(0.0);
                    Some(Duration::from_secs_f64(
                        missing / self.inner.config.requests_per_second,
                    ))
                } else {
                    None
                }
            };
            match delay {
                Some(delay) => {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        changed = changes.changed() => {
                            debug_assert!(changed.is_ok());
                        }
                    }
                }
                None => {
                    let changed = changes.changed().await;
                    debug_assert!(changed.is_ok());
                }
            }
        }
    }

    fn record_cooldown(&self, delay: Duration) {
        let now = TokioInstant::now();
        let candidate = HostedSlackMethodCooldown::new(now, delay);
        {
            let mut state = self.inner.state.lock().expect("hosted Slack gate lock");
            if state
                .cooldown
                .as_ref()
                .is_none_or(|current| candidate.remaining(now) > current.remaining(now))
            {
                state.cooldown = Some(candidate);
            }
            state.tokens = 0.0;
            state.last_refill = now;
        }
        self.inner.notify();
    }

    #[cfg(test)]
    fn status(&self) -> HostedSlackMethodGateStatus {
        let mut state = self.inner.state.lock().expect("hosted Slack gate lock");
        let now = TokioInstant::now();
        state.refill(&self.inner.config, now);
        HostedSlackMethodGateStatus {
            waiting: state.waiting,
            in_flight: state.in_flight,
            tokens: state.tokens,
            cooldown_remaining: state
                .cooldown
                .as_ref()
                .map(|cooldown| cooldown.remaining(now))
                .filter(|remaining| !remaining.is_zero()),
        }
    }
}

impl HostedSlackMethodGateInner {
    fn notify(&self) {
        self.changed.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
    }
}

struct HostedSlackMethodReservation {
    inner: Arc<HostedSlackMethodGateInner>,
    waiting: bool,
}

impl HostedSlackMethodReservation {
    fn new(inner: Arc<HostedSlackMethodGateInner>) -> Self {
        inner.state.lock().expect("hosted Slack gate lock").waiting += 1;
        Self {
            inner,
            waiting: true,
        }
    }

    fn complete(&mut self) {
        self.waiting = false;
    }
}

impl Drop for HostedSlackMethodReservation {
    fn drop(&mut self) {
        if self.waiting {
            {
                let mut state = self.inner.state.lock().expect("hosted Slack gate lock");
                state.waiting = state.waiting.saturating_sub(1);
            }
            self.inner.notify();
        }
    }
}

struct HostedSlackMethodPermit {
    inner: Arc<HostedSlackMethodGateInner>,
}

impl Drop for HostedSlackMethodPermit {
    fn drop(&mut self) {
        {
            let mut state = self.inner.state.lock().expect("hosted Slack gate lock");
            state.in_flight = state.in_flight.saturating_sub(1);
        }
        self.inner.notify();
    }
}

#[cfg(test)]
struct HostedSlackMethodGateStatus {
    waiting: usize,
    in_flight: usize,
    tokens: f64,
    cooldown_remaining: Option<Duration>,
}

#[derive(Clone)]
pub struct HttpHostedSlackProvider {
    access_token: String,
    credential_identity: HostedSlackObservedInstallationIdentity,
    base_url: String,
    client: Client,
    gates: HostedSlackProviderGates,
}

impl Debug for HttpHostedSlackProvider {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpHostedSlackProvider")
            .field("access_token", &"<redacted>")
            .field("credential_identity", &self.credential_identity)
            .field("base_url", &"<configured>")
            .finish_non_exhaustive()
    }
}

impl HttpHostedSlackProvider {
    pub fn new(
        access_token: impl Into<String>,
        credential_identity: HostedSlackObservedInstallationIdentity,
    ) -> Result<Self, HostedSlackProviderError> {
        Self::with_base_url(
            access_token,
            credential_identity,
            crate::client::DEFAULT_SLACK_API_BASE_URL,
        )
    }

    pub fn with_base_url(
        access_token: impl Into<String>,
        credential_identity: HostedSlackObservedInstallationIdentity,
        base_url: impl Into<String>,
    ) -> Result<Self, HostedSlackProviderError> {
        credential_identity.validate()?;
        ensure_crypto_provider();
        let client = Client::builder()
            .timeout(HOSTED_SLACK_HTTP_TIMEOUT)
            .build()
            .map_err(|_| HostedSlackProviderError::Transient)?;
        let gates = HostedSlackProviderGates::global(
            &credential_identity.api_app_id,
            &credential_identity.team_id,
        );
        Ok(Self {
            access_token: access_token.into(),
            credential_identity,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            gates,
        })
    }

    /// Returns the legacy V1 team-and-operation scope without changing its
    /// source or wire representation.
    pub fn coordination_scope(
        &self,
        operation: HostedSlackProviderOperationV1,
    ) -> HostedSlackProviderCoordinationScopeV1 {
        HostedSlackProviderCoordinationScopeV1 {
            team_id: self.credential_identity.team_id.clone(),
            operation,
        }
    }

    /// Returns the exact V2 scope for new durable coordination.
    ///
    /// Migrating hosts should also consult [`Self::coordination_scope`] until
    /// any unexpired durable V1 cooldowns have aged out.
    pub fn coordination_scope_v2(
        &self,
        operation: HostedSlackProviderOperationV1,
    ) -> HostedSlackProviderCoordinationScopeV2 {
        HostedSlackProviderCoordinationScopeV2 {
            api_app_id: self.credential_identity.api_app_id.clone(),
            team_id: self.credential_identity.team_id.clone(),
            method: operation.api_method(),
        }
    }

    async fn request<T: DeserializeOwned + SlackProviderEnvelope>(
        &self,
        method: Method,
        query: Vec<(&'static str, String)>,
        operation: HostedSlackProviderOperationV1,
    ) -> Result<T, HostedSlackProviderError> {
        let gate = self.gates.gate(operation).clone();
        let _permit = gate.acquire().await;
        let result = async {
            let response = self
                .client
                .request(
                    method,
                    format!("{}/{}", self.base_url, operation.api_method().as_str()),
                )
                .bearer_auth(&self.access_token)
                .query(&query)
                .send()
                .await
                .map_err(|_| HostedSlackProviderError::Transient)?;
            let status = response.status();
            if !status.is_success() {
                if status == StatusCode::TOO_MANY_REQUESTS {
                    let (error, cooldown) = rate_limit_error_and_cooldown(retry_after(&response));
                    gate.record_cooldown(cooldown);
                    return Err(error);
                }
                return Err(http_status_error(status, None));
            }
            let bytes = bounded_response_bytes(response).await?;
            let decoded = serde_json::from_slice::<T>(&bytes)
                .map_err(|_| HostedSlackProviderError::InvalidResponse("JSON"))?;
            if decoded.ok() {
                Ok(decoded)
            } else {
                Err(logical_slack_error(decoded.error(), operation))
            }
        }
        .await;
        if let Err(error) = &result
            && let Some(delay) = shared_cooldown_delay(error, operation)
        {
            gate.record_cooldown(delay);
        }
        result
    }
}

impl HostedSlackProviderPort for HttpHostedSlackProvider {
    fn verify_installation(
        &self,
    ) -> HostedSlackProviderFuture<'_, HostedSlackObservedInstallationIdentity> {
        Box::pin(async move {
            let response = self
                .request::<AuthTestResponse>(
                    Method::POST,
                    Vec::new(),
                    HostedSlackProviderOperationV1::VerifyInstallation,
                )
                .await?;
            let team_id = response
                .team_id
                .ok_or(HostedSlackProviderError::InvalidResponse("auth team_id"))?;
            let bot_user_id = response
                .user_id
                .ok_or(HostedSlackProviderError::InvalidResponse("auth user_id"))?;
            let enterprise_install =
                response
                    .is_enterprise_install
                    .ok_or(HostedSlackProviderError::InvalidResponse(
                        "auth enterprise_install",
                    ))?;
            for (field, matches) in [
                ("team_id", team_id == self.credential_identity.team_id),
                (
                    "enterprise_id",
                    response.enterprise_id == self.credential_identity.enterprise_id,
                ),
                (
                    "enterprise_install",
                    enterprise_install == self.credential_identity.enterprise_install,
                ),
                (
                    "bot_user_id",
                    bot_user_id == self.credential_identity.bot_user_id,
                ),
            ] {
                if !matches {
                    return Err(HostedSlackProviderError::IdentityMismatch(field));
                }
            }
            Ok(self.credential_identity.clone())
        })
    }

    fn conversations_info(
        &self,
        channel_id: String,
    ) -> HostedSlackProviderFuture<'_, HostedSlackObservedChannelAuthorityV1> {
        Box::pin(async move {
            validate_slack_id("provider.channel_id", &channel_id, b"CG")?;
            let response = self
                .request::<ConversationInfoResponse>(
                    Method::GET,
                    vec![("channel", channel_id.clone())],
                    HostedSlackProviderOperationV1::ConversationsInfo,
                )
                .await?;
            provider_channel_authority(response, &channel_id)
        })
    }

    fn conversations_history(
        &self,
        request: HostedSlackHistoryRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1> {
        Box::pin(async move {
            validate_history_request(&request)?;
            if request.team_id != self.credential_identity.team_id {
                return Err(HostedSlackProviderError::IdentityMismatch("team_id"));
            }
            let mut query = vec![
                ("channel", request.channel_id.clone()),
                ("oldest", request.oldest),
                ("latest", request.latest),
                ("inclusive", request.inclusive.to_string()),
                ("limit", request.limit.to_string()),
            ];
            if let Some(cursor) = request.cursor {
                query.push(("cursor", cursor));
            }
            let response = self
                .request::<HistoryResponse>(
                    Method::GET,
                    query,
                    HostedSlackProviderOperationV1::ConversationsHistory,
                )
                .await?;
            provider_page(response, &request.channel_id)
        })
    }

    fn conversations_replies(
        &self,
        request: HostedSlackRepliesRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1> {
        Box::pin(async move {
            validate_replies_request(&request)?;
            if request.team_id != self.credential_identity.team_id {
                return Err(HostedSlackProviderError::IdentityMismatch("team_id"));
            }
            let mut query = vec![
                ("channel", request.channel_id.clone()),
                ("ts", request.root_message_id),
                ("latest", request.latest),
                ("inclusive", request.inclusive.to_string()),
                ("limit", request.limit.to_string()),
            ];
            if let Some(cursor) = request.cursor {
                query.push(("cursor", cursor));
            }
            let response = self
                .request::<HistoryResponse>(
                    Method::GET,
                    query,
                    HostedSlackProviderOperationV1::ConversationsReplies,
                )
                .await?;
            provider_page(response, &request.channel_id)
        })
    }

    fn users_info(&self, user_id: String) -> HostedSlackProviderFuture<'_, RawHostedSlackUser> {
        Box::pin(async move {
            validate_slack_id("provider.user_id", &user_id, b"UW")?;
            let response = self
                .request::<UserInfoResponse>(
                    Method::GET,
                    vec![("user", user_id.clone())],
                    HostedSlackProviderOperationV1::UsersInfo,
                )
                .await?;
            provider_user(response, &user_id, &self.credential_identity.team_id)
        })
    }

    fn files_info(
        &self,
        file_id: String,
        channel_id: String,
    ) -> HostedSlackProviderFuture<'_, RawHostedSlackFileMetadata> {
        Box::pin(async move {
            validate_slack_id("provider.file_id", &file_id, b"F")?;
            let response = self
                .request::<FileInfoResponse>(
                    Method::GET,
                    vec![("file", file_id.clone())],
                    HostedSlackProviderOperationV1::FilesInfo,
                )
                .await?;
            provider_file(response, &file_id, channel_id)
        })
    }
}

impl HostedSlackDiscoveryProviderPort for HttpHostedSlackProvider {
    fn conversations_list(
        &self,
        request: HostedSlackChannelDiscoveryRequestV1,
    ) -> HostedSlackProviderFuture<'_, HostedSlackChannelDiscoveryPageV1> {
        Box::pin(async move {
            if request.limit != HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1 {
                return Err(HostedSlackProviderError::LimitExceeded(
                    "discovery page size",
                ));
            }
            let mut query = vec![
                ("types", "public_channel,private_channel,im,mpim".to_owned()),
                ("exclude_archived", "false".to_owned()),
                ("limit", request.limit.to_string()),
            ];
            if let Some(cursor) = request.cursor {
                if cursor.len() > MAX_HOSTED_SLACK_DISCOVERY_CURSOR_BYTES_V1 {
                    return Err(HostedSlackProviderError::LimitExceeded("discovery cursor"));
                }
                query.push(("cursor", cursor));
            }
            let response = self
                .request::<ConversationsListResponse>(
                    Method::GET,
                    query,
                    HostedSlackProviderOperationV1::ConversationsList,
                )
                .await?;
            let channels = response
                .channels
                .ok_or(HostedSlackProviderError::InvalidResponse("channels"))?
                .into_iter()
                .map(|channel| {
                    provider_discovered_channel(channel, &self.credential_identity.team_id)
                })
                .collect::<Result<Vec<_>, _>>()?;
            if channels.len() > HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1 as usize {
                return Err(HostedSlackProviderError::LimitExceeded(
                    "discovery page channels",
                ));
            }
            Ok(HostedSlackChannelDiscoveryPageV1 {
                observed_at: current_canonical_utc(),
                next_cursor: response
                    .response_metadata
                    .and_then(|metadata| metadata.next_cursor)
                    .filter(|cursor| !cursor.is_empty()),
                channels,
            })
        })
    }
}

#[derive(Deserialize)]
struct ResponseMetadata {
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct AuthTestResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    enterprise_id: Option<String>,
    #[serde(default)]
    is_enterprise_install: Option<bool>,
}

#[derive(Deserialize)]
struct ConversationInfoResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    channel: Option<ChannelAuthorityWire>,
}

#[derive(Deserialize)]
struct ConversationsListResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    channels: Option<Vec<ChannelAuthorityWire>>,
    #[serde(default)]
    response_metadata: Option<ResponseMetadata>,
}

#[derive(Deserialize)]
struct ChannelAuthorityWire {
    id: String,
    #[serde(default)]
    context_team_id: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    is_channel: Option<bool>,
    #[serde(default)]
    is_group: Option<bool>,
    #[serde(default)]
    is_im: Option<bool>,
    #[serde(default)]
    is_mpim: Option<bool>,
    #[serde(default)]
    is_private: Option<bool>,
    #[serde(default)]
    is_shared: Option<bool>,
    #[serde(default)]
    is_ext_shared: Option<bool>,
    #[serde(default)]
    is_org_shared: Option<bool>,
    #[serde(default)]
    is_member: Option<bool>,
    #[serde(default)]
    shared_team_ids: Vec<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    topic: Option<ConversationTextWire>,
    #[serde(default)]
    purpose: Option<ConversationTextWire>,
    #[serde(default)]
    created: Option<u64>,
    #[serde(default)]
    updated: Option<u64>,
    #[serde(default)]
    is_archived: bool,
}

#[derive(Deserialize)]
struct ConversationTextWire {
    #[serde(default)]
    value: String,
}

#[derive(Deserialize)]
struct HistoryResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    messages: Option<Vec<MessageWire>>,
    #[serde(default)]
    has_more: Option<bool>,
    #[serde(default)]
    response_metadata: Option<ResponseMetadata>,
}

#[derive(Clone, Deserialize)]
struct MessageBodyWire {
    ts: String,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    edited: Option<EditedWire>,
    #[serde(default)]
    files: Vec<FileReferenceWire>,
    #[serde(default)]
    reply_count: Option<u32>,
}

#[derive(Deserialize)]
struct MessageWire {
    #[serde(flatten)]
    body: MessageBodyWire,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    deleted_ts: Option<String>,
    #[serde(default)]
    previous_message: Option<MessageBodyWire>,
    #[serde(default)]
    message: Option<MessageBodyWire>,
}

#[derive(Clone, Deserialize)]
struct EditedWire {
    ts: String,
}

#[derive(Clone, Deserialize)]
struct FileReferenceWire {
    id: String,
}

#[derive(Deserialize)]
struct UserInfoResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    user: Option<UserWire>,
}

#[derive(Deserialize)]
struct UserWire {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    real_name: Option<String>,
    #[serde(default)]
    profile: Option<UserProfileWire>,
    #[serde(default)]
    is_bot: bool,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    updated: Option<u64>,
}

#[derive(Deserialize)]
struct UserProfileWire {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    real_name: Option<String>,
}

#[derive(Deserialize)]
struct FileInfoResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    file: Option<FileWire>,
}

#[derive(Deserialize)]
struct FileWire {
    id: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    mimetype: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    created: Option<u64>,
    #[serde(default)]
    deleted: bool,
}

trait SlackProviderEnvelope {
    fn ok(&self) -> bool;
    fn error(&self) -> Option<&str>;
}

macro_rules! impl_envelope {
    ($($response:ty),+ $(,)?) => {
        $(
            impl SlackProviderEnvelope for $response {
                fn ok(&self) -> bool {
                    self.ok
                }

                fn error(&self) -> Option<&str> {
                    self.error.as_deref()
                }
            }
        )+
    };
}

impl_envelope!(
    AuthTestResponse,
    ConversationInfoResponse,
    ConversationsListResponse,
    HistoryResponse,
    UserInfoResponse,
    FileInfoResponse,
);

fn provider_page(
    response: HistoryResponse,
    channel_id: &str,
) -> Result<HostedSlackProviderMessagePageV1, HostedSlackProviderError> {
    let messages = response
        .messages
        .ok_or(HostedSlackProviderError::InvalidResponse("messages"))?
        .into_iter()
        .map(|message| provider_message(message, channel_id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(HostedSlackProviderMessagePageV1 {
        observed_at: current_canonical_utc(),
        has_more: response.has_more,
        next_cursor: response
            .response_metadata
            .and_then(|metadata| metadata.next_cursor)
            .filter(|cursor| !cursor.is_empty()),
        messages,
    })
}

fn validate_history_request(
    request: &HostedSlackHistoryRequestV1,
) -> Result<(), HostedSlackProviderError> {
    if request.limit != HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1 {
        return Err(HostedSlackProviderError::LimitExceeded("history limit"));
    }
    if request.inclusive {
        return Err(HostedSlackProviderError::InvalidResponse(
            "history inclusivity",
        ));
    }
    validate_slack_id("provider.history.team_id", &request.team_id, b"T")?;
    validate_slack_id("provider.history.channel_id", &request.channel_id, b"CG")?;
    let oldest =
        super::checkpoint::parse_slack_timestamp("provider.history.oldest", &request.oldest)?;
    let latest =
        super::checkpoint::parse_slack_timestamp("provider.history.latest", &request.latest)?;
    if oldest >= latest {
        return Err(HostedSlackProviderError::InvalidResponse("history window"));
    }
    super::checkpoint::validate_cursor("provider.history.cursor", request.cursor.as_deref())?;
    Ok(())
}

fn validate_replies_request(
    request: &HostedSlackRepliesRequestV1,
) -> Result<(), HostedSlackProviderError> {
    if request.limit != HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1 {
        return Err(HostedSlackProviderError::LimitExceeded("replies limit"));
    }
    if request.inclusive {
        return Err(HostedSlackProviderError::InvalidResponse(
            "replies inclusivity",
        ));
    }
    validate_slack_id("provider.replies.team_id", &request.team_id, b"T")?;
    validate_slack_id("provider.replies.channel_id", &request.channel_id, b"CG")?;
    super::checkpoint::parse_slack_timestamp(
        "provider.replies.root_message_id",
        &request.root_message_id,
    )?;
    super::checkpoint::parse_slack_timestamp("provider.replies.latest", &request.latest)?;
    super::checkpoint::validate_cursor("provider.replies.cursor", request.cursor.as_deref())?;
    Ok(())
}

fn provider_channel_authority(
    response: ConversationInfoResponse,
    expected_channel_id: &str,
) -> Result<HostedSlackObservedChannelAuthorityV1, HostedSlackProviderError> {
    let channel = response
        .channel
        .ok_or(HostedSlackProviderError::InvalidResponse("channel"))?;
    provider_channel_authority_wire(channel, expected_channel_id)
}

fn provider_channel_authority_wire(
    channel: ChannelAuthorityWire,
    expected_channel_id: &str,
) -> Result<HostedSlackObservedChannelAuthorityV1, HostedSlackProviderError> {
    if channel.id != expected_channel_id {
        return Err(HostedSlackProviderError::IdentityMismatch("channel_id"));
    }
    if channel
        .context_team_id
        .as_ref()
        .zip(channel.team_id.as_ref())
        .is_some_and(|(context, team)| context != team)
    {
        return Err(HostedSlackProviderError::IdentityMismatch(
            "channel team_id",
        ));
    }
    let team_id = channel
        .context_team_id
        .or(channel.team_id)
        .ok_or(HostedSlackProviderError::InvalidResponse("channel team_id"))?;
    let mut shared_team_ids = channel.shared_team_ids;
    shared_team_ids.sort();
    Ok(HostedSlackObservedChannelAuthorityV1 {
        team_id,
        channel_id: channel.id,
        is_private: channel
            .is_private
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "channel is_private",
            ))?,
        is_shared: channel
            .is_shared
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "channel is_shared",
            ))?,
        is_externally_shared: channel.is_ext_shared.ok_or(
            HostedSlackProviderError::InvalidResponse("channel is_ext_shared"),
        )?,
        is_org_shared: channel
            .is_org_shared
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "channel is_org_shared",
            ))?,
        is_member: channel
            .is_member
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "channel is_member",
            ))?,
        shared_team_ids,
    })
}

fn provider_discovered_channel(
    channel: ChannelAuthorityWire,
    expected_team_id: &str,
) -> Result<HostedSlackDiscoveredChannelV1, HostedSlackProviderError> {
    let channel_id = channel.id.clone();
    let conversation_kind = hosted_conversation_kind(
        channel
            .is_channel
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "conversation kind",
            ))?,
        channel
            .is_group
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "conversation kind",
            ))?,
        channel
            .is_im
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "conversation kind",
            ))?,
        channel
            .is_mpim
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "conversation kind",
            ))?,
    )?;
    let name = channel
        .name
        .clone()
        .ok_or(HostedSlackProviderError::InvalidResponse("channel name"))?;
    let topic = channel.topic.as_ref().map(|value| value.value.clone());
    let purpose = channel.purpose.as_ref().map(|value| value.value.clone());
    let created = channel
        .created
        .ok_or(HostedSlackProviderError::InvalidResponse("channel created"))?;
    let updated = channel.updated;
    let is_archived = channel.is_archived;
    let authority = provider_channel_authority_wire(channel, &channel_id)?;
    if authority.team_id != expected_team_id {
        return Err(HostedSlackProviderError::IdentityMismatch(
            "channel team_id",
        ));
    }
    validate_discovered_channel_authority(&authority, expected_team_id)?;
    let sharing = match (authority.is_externally_shared, authority.is_private) {
        (false, false) => SlackChannelSharingClassification::Public,
        (false, true) => SlackChannelSharingClassification::Private,
        (true, false) => SlackChannelSharingClassification::ExternallySharedPublic,
        (true, true) => SlackChannelSharingClassification::ExternallySharedPrivate,
    };
    let raw = RawHostedSlackChannel {
        team_id: authority.team_id,
        id: authority.channel_id,
        conversation_kind,
        name,
        topic,
        purpose,
        created_ts: epoch_seconds_timestamp(created),
        updated_ts: updated.map(epoch_milliseconds_timestamp),
        sharing,
    };
    super::native::HostedSlackChannel::try_from(raw.clone())?;
    Ok(HostedSlackDiscoveredChannelV1 {
        channel: raw,
        is_member: authority.is_member,
        is_archived,
    })
}

fn hosted_conversation_kind(
    is_channel: bool,
    is_group: bool,
    is_im: bool,
    is_mpim: bool,
) -> Result<HostedSlackConversationKindV1, HostedSlackProviderError> {
    match (is_channel, is_group, is_im, is_mpim) {
        (true, false, false, false) => Ok(HostedSlackConversationKindV1::PublicChannel),
        (false, true, false, false) => Ok(HostedSlackConversationKindV1::PrivateChannel),
        (false, false, true, false) => Ok(HostedSlackConversationKindV1::Im),
        (false, false, false, true) => Ok(HostedSlackConversationKindV1::Mpim),
        _ => Err(HostedSlackProviderError::InvalidResponse(
            "conversation kind",
        )),
    }
}

fn validate_discovered_channel_authority(
    authority: &HostedSlackObservedChannelAuthorityV1,
    expected_team_id: &str,
) -> Result<(), HostedSlackProviderError> {
    if authority.shared_team_ids.len() > MAX_HOSTED_SLACK_PROVIDER_METADATA_IDS_V1 {
        return Err(HostedSlackProviderError::LimitExceeded(
            "channel shared team ids",
        ));
    }
    for team_id in &authority.shared_team_ids {
        validate_slack_id("provider.channel.shared_team_id", team_id, b"T")?;
    }
    if authority
        .shared_team_ids
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
        || authority.is_org_shared
        || authority.is_shared != authority.is_externally_shared
    {
        return Err(HostedSlackProviderError::InvalidResponse(
            "channel sharing facts",
        ));
    }
    if authority.is_externally_shared {
        return Err(HostedSlackProviderError::Unsupported(
            "Slack Connect channel identity in V1",
        ));
    }
    if authority.is_shared
        && (authority
            .shared_team_ids
            .binary_search_by(|candidate| candidate.as_str().cmp(expected_team_id))
            .is_err()
            || !authority
                .shared_team_ids
                .iter()
                .any(|team_id| team_id != expected_team_id))
    {
        return Err(HostedSlackProviderError::InvalidResponse(
            "channel sharing facts",
        ));
    }
    if !authority.is_shared
        && authority
            .shared_team_ids
            .iter()
            .any(|team_id| team_id != expected_team_id)
    {
        return Err(HostedSlackProviderError::InvalidResponse(
            "channel sharing facts",
        ));
    }
    Ok(())
}

fn ensure_v1_channel_identity_supported(
    sharing: SlackChannelSharingClassification,
) -> Result<(), HostedSlackProviderError> {
    if matches!(
        sharing,
        SlackChannelSharingClassification::ExternallySharedPublic
            | SlackChannelSharingClassification::ExternallySharedPrivate
    ) {
        return Err(HostedSlackProviderError::Unsupported(
            "Slack Connect channel identity in V1",
        ));
    }
    Ok(())
}

fn provider_user(
    response: UserInfoResponse,
    expected_user_id: &str,
    team_id: &str,
) -> Result<RawHostedSlackUser, HostedSlackProviderError> {
    let user = response
        .user
        .ok_or(HostedSlackProviderError::InvalidResponse("user"))?;
    if user.id != expected_user_id {
        return Err(HostedSlackProviderError::IdentityMismatch("user_id"));
    }
    Ok(RawHostedSlackUser {
        team_id: team_id.to_string(),
        id: user.id,
        name: user.name.unwrap_or_default(),
        display_name: user
            .profile
            .as_ref()
            .and_then(|profile| profile.display_name.clone())
            .unwrap_or_default(),
        real_name: user
            .profile
            .and_then(|profile| profile.real_name)
            .or(user.real_name)
            .unwrap_or_default(),
        is_bot: user.is_bot,
        deleted: user.deleted,
        updated_ts: user.updated.map(epoch_seconds_timestamp),
    })
}

fn provider_file(
    response: FileInfoResponse,
    expected_file_id: &str,
    channel_id: String,
) -> Result<RawHostedSlackFileMetadata, HostedSlackProviderError> {
    let file = response
        .file
        .ok_or(HostedSlackProviderError::InvalidResponse("file"))?;
    if file.id != expected_file_id {
        return Err(HostedSlackProviderError::IdentityMismatch("file_id"));
    }
    Ok(RawHostedSlackFileMetadata {
        channel_id,
        id: file.id,
        user_id: file.user,
        name: file.name.unwrap_or_default(),
        title: file.title.unwrap_or_default(),
        mimetype: file.mimetype.unwrap_or_default(),
        byte_length: file.size.unwrap_or(0),
        created_ts: epoch_seconds_timestamp(
            file.created
                .ok_or(HostedSlackProviderError::InvalidResponse("file created"))?,
        ),
        deleted: file.deleted,
    })
}

fn provider_message(
    wire: MessageWire,
    channel_id: &str,
) -> Result<HostedSlackProviderMessageV1, HostedSlackProviderError> {
    let (body, deleted) = match wire.subtype.as_deref() {
        None | Some("bot_message") | Some("file_share") | Some("thread_broadcast") => {
            (wire.body, false)
        }
        Some("message_changed") => (
            wire.message
                .ok_or(HostedSlackProviderError::InvalidResponse(
                    "changed message body",
                ))?,
            false,
        ),
        Some("message_deleted") => {
            let mut previous =
                wire.previous_message
                    .ok_or(HostedSlackProviderError::InvalidResponse(
                        "deleted message body",
                    ))?;
            previous.ts = wire
                .deleted_ts
                .ok_or(HostedSlackProviderError::InvalidResponse(
                    "deleted message timestamp",
                ))?;
            previous.text.clear();
            previous.files.clear();
            previous.reply_count = Some(0);
            (previous, true)
        }
        Some(subtype) if is_evidence_system_subtype(subtype) => {
            let mut body = wire.body;
            body.text = bounded_system_event_text(subtype, &body.text);
            body.edited = None;
            body.files.clear();
            (body, false)
        }
        Some(_) => {
            return Err(HostedSlackProviderError::InvalidResponse(
                "unsupported message subtype",
            ));
        }
    };
    if body.files.len() > MAX_HOSTED_SLACK_MESSAGE_FILES {
        return Err(HostedSlackProviderError::LimitExceeded(
            "message file references",
        ));
    }
    let mut file_ids = body
        .files
        .into_iter()
        .map(|file| file.id)
        .collect::<Vec<_>>();
    file_ids.sort();
    if file_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(HostedSlackProviderError::InvalidResponse(
            "duplicate message file reference",
        ));
    }
    Ok(HostedSlackProviderMessageV1 {
        reply_count: body.reply_count,
        message: RawHostedSlackMessage {
            channel_id: channel_id.to_string(),
            ts: body.ts,
            thread_ts: body.thread_ts,
            user_id: body.user,
            text: body.text,
            edited_ts: body.edited.map(|edited| edited.ts),
            deleted,
            file_ids,
        },
    })
}

fn is_evidence_system_subtype(subtype: &str) -> bool {
    matches!(
        subtype,
        "bot_add"
            | "bot_remove"
            | "channel_archive"
            | "channel_convert_to_private"
            | "channel_convert_to_public"
            | "channel_join"
            | "channel_leave"
            | "channel_name"
            | "channel_posting_permissions"
            | "channel_purpose"
            | "channel_topic"
            | "channel_unarchive"
            | "ekm_access_denied"
            | "file_comment"
            | "file_mention"
            | "group_archive"
            | "group_join"
            | "group_leave"
            | "group_name"
            | "group_purpose"
            | "group_topic"
            | "group_unarchive"
            | "huddle_thread"
            | "me_message"
            | "pinned_item"
            | "reminder_add"
            | "sh_room_created"
            | "slackbot_response"
            | "tombstone"
            | "unpinned_item"
    )
}

fn bounded_system_event_text(subtype: &str, source: &str) -> String {
    let prefix = format!("[Slack system event: {subtype}]");
    if source.is_empty() {
        return prefix;
    }
    let separator = " ";
    let truncation_marker = " …[truncated]";
    let available =
        MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES.saturating_sub(prefix.len() + separator.len());
    if source.len() <= available {
        return format!("{prefix}{separator}{source}");
    }
    let content_limit = available.saturating_sub(truncation_marker.len());
    let mut boundary = content_limit.min(source.len());
    while !source.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    format!(
        "{prefix}{separator}{}{truncation_marker}",
        &source[..boundary]
    )
}

async fn bounded_response_bytes(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, HostedSlackProviderError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| HostedSlackProviderError::Transient)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_HOSTED_SLACK_PROVIDER_RESPONSE_BYTES_V1 {
            return Err(HostedSlackProviderError::LimitExceeded(
                "provider response bytes",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn http_status_error(
    status: StatusCode,
    retry_after: Option<Duration>,
) -> HostedSlackProviderError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            HostedSlackProviderError::Authentication
        }
        StatusCode::NOT_FOUND => HostedSlackProviderError::NotFound("resource"),
        StatusCode::TOO_MANY_REQUESTS => rate_limit_error_and_cooldown(retry_after).0,
        StatusCode::REQUEST_TIMEOUT
        | StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => HostedSlackProviderError::Transient,
        _ => HostedSlackProviderError::InvalidResponse("HTTP status"),
    }
}

fn rate_limit_error_and_cooldown(
    retry_after: Option<Duration>,
) -> (HostedSlackProviderError, Duration) {
    let error = match retry_after {
        Some(retry_after) if !retry_after.is_zero() => {
            HostedSlackProviderError::RateLimited { retry_after }
        }
        Some(_) | None => HostedSlackProviderError::InvalidResponse("Retry-After"),
    };
    (error, normalized_gate_retry_after(retry_after))
}

fn normalized_gate_retry_after(retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(retry_after)
            if !retry_after.is_zero()
                && retry_after <= MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1 =>
        {
            retry_after
        }
        Some(_) | None => HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
    }
}

fn shared_cooldown_delay(
    error: &HostedSlackProviderError,
    operation: HostedSlackProviderOperationV1,
) -> Option<Duration> {
    match error {
        HostedSlackProviderError::RateLimited { retry_after } => {
            Some(normalized_gate_retry_after(Some(*retry_after)))
        }
        HostedSlackProviderError::Transient => Some(operation.retry_config().initial_backoff),
        _ => None,
    }
}

fn logical_slack_error(
    error: Option<&str>,
    operation: HostedSlackProviderOperationV1,
) -> HostedSlackProviderError {
    match error {
        Some("token_revoked" | "token_expired" | "account_inactive") => {
            HostedSlackProviderError::Revoked
        }
        Some(
            "access_denied"
            | "invalid_auth"
            | "missing_scope"
            | "no_permission"
            | "not_allowed_token_type"
            | "not_authed"
            | "team_access_not_granted",
        ) => HostedSlackProviderError::Authentication,
        Some("not_in_channel")
            if matches!(
                operation,
                HostedSlackProviderOperationV1::ConversationsInfo
                    | HostedSlackProviderOperationV1::ConversationsHistory
                    | HostedSlackProviderOperationV1::ConversationsReplies
            ) =>
        {
            HostedSlackProviderError::Revoked
        }
        Some("not_in_channel") => HostedSlackProviderError::Authentication,
        Some("thread_not_found")
            if operation == HostedSlackProviderOperationV1::ConversationsReplies =>
        {
            HostedSlackProviderError::ThreadNotFound
        }
        Some(
            "channel_not_found" | "file_not_found" | "message_not_found" | "team_not_found"
            | "thread_not_found" | "user_not_found",
        ) => HostedSlackProviderError::NotFound("resource"),
        Some("ratelimited") => HostedSlackProviderError::RateLimited {
            retry_after: Duration::from_secs(1),
        },
        Some("internal_error" | "request_timeout" | "service_unavailable") => {
            HostedSlackProviderError::Transient
        }
        Some("invalid_arguments" | "invalid_cursor" | "too_many_ids") => {
            HostedSlackProviderError::LimitExceeded("provider request")
        }
        Some(_) | None => HostedSlackProviderError::InvalidResponse("logical error"),
    }
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn current_canonical_utc() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn canonical_slack_timestamp(
    canonical_utc: &str,
    exclusive_predecessor: bool,
) -> Result<String, HostedSlackProviderError> {
    let parsed = super::checkpoint::parse_canonical_utc_timestamp(
        "provider request boundary",
        canonical_utc,
    )?;
    let mut micros = parsed.timestamp_micros();
    if exclusive_predecessor {
        micros = micros
            .checked_sub(1)
            .ok_or(HostedSlackProviderError::InvalidResponse(
                "Slack timestamp range",
            ))?;
    }
    if micros < 0 || micros / 1_000_000 > 999_999_999_999 {
        return Err(HostedSlackProviderError::InvalidResponse(
            "Slack timestamp range",
        ));
    }
    Ok(format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000))
}

fn epoch_seconds_timestamp(seconds: u64) -> String {
    format!("{seconds}.000000")
}

fn epoch_milliseconds_timestamp(milliseconds: u64) -> String {
    format!(
        "{}.{:06}",
        milliseconds / 1_000,
        (milliseconds % 1_000) * 1_000
    )
}

fn ensure_crypto_provider() {
    HOSTED_SLACK_CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn operation_network_config(operation: HostedSlackProviderOperationV1) -> ConnectorNetworkConfig {
    let (scope, requests_per_second, burst, max_in_flight) = match operation {
        HostedSlackProviderOperationV1::VerifyInstallation => ("slack-auth-test", 1.0, 2.0, 2),
        HostedSlackProviderOperationV1::ConversationsList => {
            ("slack-hosted-conversations-list-v1", 0.5, 1.0, 1)
        }
        HostedSlackProviderOperationV1::ConversationsInfo => {
            ("slack-conversations-info", 1.0, 2.0, 2)
        }
        HostedSlackProviderOperationV1::ConversationsHistory => {
            ("slack-hosted-conversations-history-v1", 1.0 / 60.0, 1.0, 1)
        }
        HostedSlackProviderOperationV1::ConversationsReplies => {
            ("slack-hosted-conversations-replies-v1", 1.0 / 60.0, 1.0, 1)
        }
        HostedSlackProviderOperationV1::UsersInfo => ("slack-users-info", 1.0, 2.0, 2),
        HostedSlackProviderOperationV1::FilesInfo => ("slack-files-info", 1.0, 2.0, 2),
    };
    ConnectorNetworkConfig::new(scope, requests_per_second, burst)
        .max_in_flight(max_in_flight)
        .request_timeout(HOSTED_SLACK_HTTP_TIMEOUT)
        .retry(operation.retry_config())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};

    use super::super::native::HostedSlackConversationKindV1;
    use super::*;
    use crate::portable::hosted::HostedSlackPollKindV1;

    struct StubResponse {
        status: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
    }

    const CONVERSATIONS_INFO_OK: &str = r#"{
        "ok": true,
        "channel": {
            "id": "C08ENGINEER1",
            "context_team_id": "T08LOCALITY1",
            "is_private": true,
            "is_shared": true,
            "is_ext_shared": true,
            "is_org_shared": false,
            "is_member": true,
            "shared_team_ids": ["T08LOCALITY1"]
        }
    }"#;

    #[derive(Debug)]
    struct StubDiscoveryProvider {
        pages: Mutex<VecDeque<Result<HostedSlackChannelDiscoveryPageV1, HostedSlackProviderError>>>,
        requests: Mutex<Vec<HostedSlackChannelDiscoveryRequestV1>>,
    }

    impl StubDiscoveryProvider {
        fn new(
            pages: impl IntoIterator<
                Item = Result<HostedSlackChannelDiscoveryPageV1, HostedSlackProviderError>,
            >,
        ) -> Self {
            Self {
                pages: Mutex::new(pages.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl HostedSlackProviderPort for StubDiscoveryProvider {
        fn verify_installation(
            &self,
        ) -> HostedSlackProviderFuture<'_, HostedSlackObservedInstallationIdentity> {
            Box::pin(async { Ok(observed_identity()) })
        }

        fn conversations_info(
            &self,
            _channel_id: String,
        ) -> HostedSlackProviderFuture<'_, HostedSlackObservedChannelAuthorityV1> {
            Box::pin(async { Err(HostedSlackProviderError::InvalidResponse("unexpected call")) })
        }

        fn conversations_history(
            &self,
            _request: HostedSlackHistoryRequestV1,
        ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1> {
            Box::pin(async { Err(HostedSlackProviderError::InvalidResponse("unexpected call")) })
        }

        fn conversations_replies(
            &self,
            _request: HostedSlackRepliesRequestV1,
        ) -> HostedSlackProviderFuture<'_, HostedSlackProviderMessagePageV1> {
            Box::pin(async { Err(HostedSlackProviderError::InvalidResponse("unexpected call")) })
        }

        fn users_info(
            &self,
            _user_id: String,
        ) -> HostedSlackProviderFuture<'_, RawHostedSlackUser> {
            Box::pin(async { Err(HostedSlackProviderError::InvalidResponse("unexpected call")) })
        }

        fn files_info(
            &self,
            _file_id: String,
            _channel_id: String,
        ) -> HostedSlackProviderFuture<'_, RawHostedSlackFileMetadata> {
            Box::pin(async { Err(HostedSlackProviderError::InvalidResponse("unexpected call")) })
        }
    }

    impl HostedSlackDiscoveryProviderPort for StubDiscoveryProvider {
        fn conversations_list(
            &self,
            request: HostedSlackChannelDiscoveryRequestV1,
        ) -> HostedSlackProviderFuture<'_, HostedSlackChannelDiscoveryPageV1> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                self.pages
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("missing discovery page")
            })
        }
    }

    fn observed_identity() -> HostedSlackObservedInstallationIdentity {
        HostedSlackObservedInstallationIdentity {
            api_app_id: "A08LOCALITY1".to_string(),
            team_id: "T08LOCALITY1".to_string(),
            enterprise_id: Some("E08LOCALITY1".to_string()),
            enterprise_install: false,
            bot_user_id: "U08LOCALBOT1".to_string(),
            oauth_subject_id: "U08INSTALLER1".to_string(),
        }
    }

    fn installation_binding() -> HostedSlackInstallationBinding {
        HostedSlackInstallationBinding {
            installation_id: serde_json::from_str(r#""0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10""#)
                .unwrap(),
            api_app_id: "A08LOCALITY1".to_string(),
            team_id: "T08LOCALITY1".to_string(),
            enterprise_id: Some("E08LOCALITY1".to_string()),
            enterprise_install: false,
            bot_user_id: "U08LOCALBOT1".to_string(),
            oauth_subject_id: "U08INSTALLER1".to_string(),
        }
    }

    fn discovered_channel(
        team_id: &str,
        is_member: bool,
        is_archived: bool,
    ) -> HostedSlackDiscoveredChannelV1 {
        HostedSlackDiscoveredChannelV1 {
            channel: RawHostedSlackChannel {
                team_id: team_id.to_string(),
                id: "C08ENGINEER1".to_string(),
                conversation_kind: HostedSlackConversationKindV1::PrivateChannel,
                name: "engineering".to_string(),
                topic: Some("Build safely".to_string()),
                purpose: Some("Engineering".to_string()),
                created_ts: "1780000000.000000".to_string(),
                updated_ts: Some("1780000010.000000".to_string()),
                sharing: SlackChannelSharingClassification::Private,
            },
            is_member,
            is_archived,
        }
    }

    fn discovered_channel_wire(
        id: &str,
        is_channel: bool,
        is_group: bool,
        is_im: bool,
        is_mpim: bool,
    ) -> ChannelAuthorityWire {
        ChannelAuthorityWire {
            id: id.to_string(),
            context_team_id: Some("T08LOCALITY1".to_string()),
            team_id: Some("T08LOCALITY1".to_string()),
            is_channel: Some(is_channel),
            is_group: Some(is_group),
            is_im: Some(is_im),
            is_mpim: Some(is_mpim),
            is_private: Some(is_group || is_im || is_mpim),
            is_shared: Some(false),
            is_ext_shared: Some(false),
            is_org_shared: Some(false),
            is_member: Some(true),
            shared_team_ids: Vec::new(),
            name: Some("conversation".to_string()),
            topic: Some(ConversationTextWire {
                value: "topic".to_string(),
            }),
            purpose: Some(ConversationTextWire {
                value: "purpose".to_string(),
            }),
            created: Some(1780000000),
            updated: Some(1780000010123),
            is_archived: false,
        }
    }

    fn indexed_discovered_channel(index: usize) -> HostedSlackDiscoveredChannelV1 {
        HostedSlackDiscoveredChannelV1 {
            channel: RawHostedSlackChannel {
                team_id: "T08LOCALITY1".to_string(),
                id: format!("C{index:011}"),
                conversation_kind: HostedSlackConversationKindV1::PublicChannel,
                name: format!("channel-{index}"),
                topic: None,
                purpose: None,
                created_ts: "1780000000.000000".to_string(),
                updated_ts: Some("1780000010.123000".to_string()),
                sharing: SlackChannelSharingClassification::Public,
            },
            is_member: true,
            is_archived: false,
        }
    }

    fn discovery_page(
        channels: Vec<HostedSlackDiscoveredChannelV1>,
        next_cursor: Option<String>,
    ) -> HostedSlackChannelDiscoveryPageV1 {
        HostedSlackChannelDiscoveryPageV1 {
            observed_at: "2026-06-01T00:00:00Z".to_string(),
            next_cursor,
            channels,
        }
    }

    fn test_gate(
        operation: HostedSlackProviderOperationV1,
        max_in_flight: usize,
    ) -> HostedSlackMethodGate {
        HostedSlackMethodGate::new(
            ConnectorNetworkConfig::new(format!("hosted-slack-test-{operation:?}"), 10_000.0, 16.0)
                .max_in_flight(max_in_flight)
                .retry(operation.retry_config()),
        )
    }

    fn test_gates() -> HostedSlackProviderGates {
        HostedSlackProviderGates {
            verify_installation: test_gate(HostedSlackProviderOperationV1::VerifyInstallation, 2),
            conversations_list: test_gate(HostedSlackProviderOperationV1::ConversationsList, 1),
            conversations_info: test_gate(HostedSlackProviderOperationV1::ConversationsInfo, 1),
            conversations_history: test_gate(
                HostedSlackProviderOperationV1::ConversationsHistory,
                1,
            ),
            conversations_replies: test_gate(
                HostedSlackProviderOperationV1::ConversationsReplies,
                1,
            ),
            users_info: test_gate(HostedSlackProviderOperationV1::UsersInfo, 2),
            files_info: test_gate(HostedSlackProviderOperationV1::FilesInfo, 2),
        }
    }

    fn test_provider(base_url: String) -> HttpHostedSlackProvider {
        ensure_crypto_provider();
        HttpHostedSlackProvider {
            access_token: "test-token".to_string(),
            credential_identity: observed_identity(),
            base_url,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            gates: test_gates(),
        }
    }

    fn spawn_stub_server(
        responses: Vec<StubResponse>,
    ) -> (String, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                request_tx.send(request).unwrap();
                write_http_response(&mut stream, response);
            }
        });
        (format!("http://{address}"), request_rx, server)
    }

    async fn assert_http_retry_after_gate_reopens(
        retry_after: &'static str,
        expected_error: HostedSlackProviderError,
        expected_cooldown: Duration,
    ) {
        let (base_url, requests, server) = spawn_stub_server(vec![
            StubResponse {
                status: "429 Too Many Requests",
                headers: vec![("Retry-After", retry_after)],
                body: r#"{"ok":false,"error":"rate-secret"}"#,
            },
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: CONVERSATIONS_INFO_OK,
            },
        ]);
        let mut provider = test_provider(base_url);
        provider.client = Client::builder().build().unwrap();
        assert_eq!(
            provider
                .conversations_info("C08ENGINEER1".to_string())
                .await,
            Err(expected_error)
        );
        let first_request = requests.recv().unwrap();
        assert!(first_request.starts_with("GET /conversations.info?"));

        let gate = provider
            .gates
            .gate(HostedSlackProviderOperationV1::ConversationsInfo);
        assert_eq!(gate.status().cooldown_remaining, Some(expected_cooldown));

        let next_provider = provider.clone();
        let next = tokio::spawn(async move {
            next_provider
                .conversations_info("C08ENGINEER1".to_string())
                .await
        });
        while gate.status().waiting == 0 {
            tokio::task::yield_now().await;
        }
        assert!(!next.is_finished());
        assert!(requests.try_recv().is_err());

        tokio::time::advance(expected_cooldown - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!next.is_finished());
        assert!(requests.try_recv().is_err());

        tokio::time::advance(Duration::from_secs(1)).await;
        next.await.unwrap().unwrap();
        let second_request = requests.recv().unwrap();
        assert!(second_request.starts_with("GET /conversations.info?"));
        server.join().unwrap();
    }

    fn spawn_stalling_server() -> (String, Receiver<()>, mpsc::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut stream);
            seen_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        (format!("http://{address}"), seen_rx, release_tx, server)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn write_http_response(stream: &mut std::net::TcpStream, response: StubResponse) {
        let mut wire = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            response.body.len()
        );
        for (name, value) in response.headers {
            wire.push_str(name);
            wire.push_str(": ");
            wire.push_str(value);
            wire.push_str("\r\n");
        }
        wire.push_str("\r\n");
        wire.push_str(response.body);
        stream.write_all(wire.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    const HOSTILE_HISTORY: &[u8] = br#"{
        "ok": true,
        "has_more": false,
        "response_metadata": {"next_cursor": ""},
        "headers": {"Authorization": "Bearer history-secret"},
        "cookies": "history-cookie",
        "messages": [{
            "ts": "1780000000.000100",
            "user": "U08ADA00001",
            "text": "safe body",
            "reply_count": 0,
            "permalink": "https://provider.invalid/history-secret",
            "files": [{
                "id": "F08PLAN0001",
                "url_private": "https://provider.invalid/file-secret",
                "permalink": "https://provider.invalid/file-permalink"
            }]
        }]
    }"#;

    const HOSTILE_USER: &[u8] = br#"{
        "ok": true,
        "headers": {"Authorization": "Bearer user-secret"},
        "cookies": "user-cookie",
        "user": {
            "id": "U08ADA00001",
            "name": "ada",
            "real_name": "Ada Lovelace",
            "profile": {
                "display_name": "Ada",
                "real_name": "Ada Lovelace",
                "email": "secret@example.invalid",
                "image_original": "https://provider.invalid/avatar-secret"
            }
        }
    }"#;

    const HOSTILE_FILE: &[u8] = br#"{
        "ok": true,
        "headers": {"Authorization": "Bearer file-secret"},
        "cookies": "file-cookie",
        "file": {
            "id": "F08PLAN0001",
            "user": "U08ADA00001",
            "name": "plan.txt",
            "title": "Plan",
            "mimetype": "text/plain",
            "size": 42,
            "created": 1780000000,
            "url_private": "https://provider.invalid/download-secret",
            "permalink": "https://provider.invalid/file-secret",
            "content": "raw-file-secret"
        }
    }"#;

    const DISCOVERY_PAGE_ONE: &str =
        include_str!("../../../fixtures/hosted-v1/provider-v1/discovery-page-one-v1.json");
    const DISCOVERY_PAGE_TWO: &str =
        include_str!("../../../fixtures/hosted-v1/provider-v1/discovery-page-two-v1.json");
    const DISCOVERY_MALFORMED_UPDATED: &str =
        include_str!("../../../fixtures/hosted-v1/provider-v1/discovery-malformed-updated-v1.json");
    const DISCOVERY_SANITIZED_CHANNEL: &[u8] = include_bytes!(
        "../../../fixtures/hosted-v1/provider-v1/discovery-sanitized-channel-v1.json"
    );
    const DISCOVERY_SLACK_CONNECT: &str =
        include_str!("../../../fixtures/hosted-v1/provider-v1/discovery-slack-connect-v1.json");
    const SYSTEM_SUBTYPES_RESPONSE: &[u8] =
        include_bytes!("../../../fixtures/hosted-v1/provider-v1/system-subtypes-response-v1.json");
    const SYSTEM_SUBTYPES_SANITIZED: &[u8] =
        include_bytes!("../../../fixtures/hosted-v1/provider-v1/system-subtypes-sanitized-v1.json");

    #[test]
    fn system_subtypes_preserve_bounded_evidence_without_private_payloads() {
        let response = serde_json::from_slice::<HistoryResponse>(SYSTEM_SUBTYPES_RESPONSE).unwrap();
        let provider_page = provider_page(response, "C08ENGINEER1").unwrap();
        let messages = provider_page.messages.clone();
        let mut exact = serde_json::to_vec_pretty(&messages).unwrap();
        exact.push(b'\n');
        assert_eq!(exact, SYSTEM_SUBTYPES_SANITIZED);
        let sanitized = String::from_utf8(exact).unwrap();
        for forbidden in ["url_private", "system-file-secret", "system-room-secret"] {
            assert!(!sanitized.contains(forbidden));
        }
        let binding = installation_binding();
        let descriptor = HostedSlackInitialChannelDescriptorV1::new(
            &binding,
            &discovered_channel("T08LOCALITY1", true, false),
            "2026-06-01T00:00:00Z".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .unwrap();
        let mut checkpoint = HostedSlackPollCheckpointV1::new(
            &descriptor.selector,
            descriptor.channel,
            HostedSlackPollKindV1::FullRepair,
            "2026-06-01T00:00:00Z".to_string(),
            "2026-05-28T20:00:00Z".to_string(),
        )
        .unwrap();
        let request = history_request(&checkpoint).unwrap();
        let history = history_poll_page(&checkpoint, &request, provider_page).unwrap();
        checkpoint.apply_history_page_v2(&history).unwrap();
        assert_eq!(
            checkpoint.phase(),
            HostedSlackPollPhaseV1::AwaitingCatchUpCut
        );
        assert_eq!(checkpoint.completed_roots().len(), 3);

        let oversized = serde_json::json!({
            "ts": "1780000003.000100",
            "subtype": "channel_topic",
            "text": "é".repeat(MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES),
            "reply_count": 0
        });
        let bounded = provider_message(
            serde_json::from_value::<MessageWire>(oversized).unwrap(),
            "C08ENGINEER1",
        )
        .unwrap();
        assert!(bounded.message.text.len() <= MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES);
        assert!(bounded.message.text.ends_with(" …[truncated]"));
        assert!(std::str::from_utf8(bounded.message.text.as_bytes()).is_ok());

        let threaded = serde_json::from_str::<MessageWire>(
            r#"{"ts":"1780000004.000100","subtype":"channel_topic","text":"topic","reply_count":2}"#,
        )
        .unwrap();
        assert_eq!(
            provider_message(threaded, "C08ENGINEER1")
                .unwrap()
                .reply_count,
            Some(2)
        );

        let unknown = serde_json::from_str::<MessageWire>(
            r#"{"ts":"1780000005.000100","subtype":"unknown_provider_secret","text":"secret"}"#,
        )
        .unwrap();
        assert_eq!(
            provider_message(unknown, "C08ENGINEER1"),
            Err(HostedSlackProviderError::InvalidResponse(
                "unsupported message subtype"
            ))
        );
    }

    #[test]
    fn deleted_message_tombstones_clear_stale_file_references() {
        let wire = serde_json::from_str::<MessageWire>(
            r#"{
                "ts": "1780000010.000100",
                "subtype": "message_deleted",
                "deleted_ts": "1780000000.000100",
                "previous_message": {
                    "ts": "1780000000.000100",
                    "user": "U08ADA00001",
                    "text": "deleted body",
                    "files": [{"id": "F08PLAN0001"}],
                    "reply_count": 2
                }
            }"#,
        )
        .unwrap();
        let tombstone = provider_message(wire, "C08ENGINEER1").unwrap();
        assert!(tombstone.message.deleted);
        assert!(tombstone.message.text.is_empty());
        assert!(tombstone.message.file_ids.is_empty());
        assert_eq!(tombstone.reply_count, Some(0));
    }

    #[test]
    fn production_wire_boundary_discards_private_urls_email_headers_cookies_and_raw_content() {
        let history = serde_json::from_slice::<HistoryResponse>(HOSTILE_HISTORY).unwrap();
        let user = provider_user(
            serde_json::from_slice::<UserInfoResponse>(HOSTILE_USER).unwrap(),
            "U08ADA00001",
            "T08LOCALITY1",
        )
        .unwrap();
        let file = provider_file(
            serde_json::from_slice::<FileInfoResponse>(HOSTILE_FILE).unwrap(),
            "F08PLAN0001",
            "C08ENGINEER1".to_string(),
        )
        .unwrap();
        let mut discovery_response =
            serde_json::from_str::<ConversationsListResponse>(DISCOVERY_PAGE_ONE).unwrap();
        let discovered = provider_discovered_channel(
            discovery_response.channels.take().unwrap().remove(0),
            "T08LOCALITY1",
        )
        .unwrap();
        let mut exact_discovered = serde_json::to_vec_pretty(&discovered).unwrap();
        exact_discovered.push(b'\n');
        assert_eq!(exact_discovered, DISCOVERY_SANITIZED_CHANNEL);
        let sanitized = serde_json::to_string(&(
            provider_page(history, "C08ENGINEER1").unwrap(),
            user,
            file,
            discovered,
        ))
        .unwrap();
        for forbidden in [
            "authorization",
            "cookie",
            "email",
            "url_private",
            "permalink",
            "history-secret",
            "file-secret",
            "user-secret",
            "raw-file-secret",
            "secret@example.invalid",
            "discovery-secret",
            "discovery-cookie",
            "private-message-secret",
            "discovery-file-secret",
            "discovery-secret@example.invalid",
        ] {
            assert!(!sanitized.to_ascii_lowercase().contains(forbidden));
        }
    }

    #[test]
    fn production_channel_authority_requires_explicit_access_and_sharing_facts() {
        let response = serde_json::from_str::<ConversationInfoResponse>(
            r#"{
                "ok": true,
                "channel": {
                    "id": "C08ENGINEER1",
                    "context_team_id": "T08LOCALITY1",
                    "is_private": true,
                    "is_shared": true,
                    "is_ext_shared": true,
                    "is_org_shared": false,
                    "shared_team_ids": ["T08LOCALITY1", "T08EXTERNAL1"]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            provider_channel_authority(response, "C08ENGINEER1"),
            Err(HostedSlackProviderError::InvalidResponse(
                "channel is_member"
            ))
        );
    }

    #[test]
    fn hosted_conversation_kind_classifies_exactly() {
        assert_eq!(
            hosted_conversation_kind(true, false, false, false),
            Ok(HostedSlackConversationKindV1::PublicChannel)
        );
        assert_eq!(
            hosted_conversation_kind(false, true, false, false),
            Ok(HostedSlackConversationKindV1::PrivateChannel)
        );
        assert_eq!(
            hosted_conversation_kind(false, false, true, false),
            Ok(HostedSlackConversationKindV1::Im)
        );
        assert_eq!(
            hosted_conversation_kind(false, false, false, true),
            Ok(HostedSlackConversationKindV1::Mpim)
        );
        assert_eq!(
            hosted_conversation_kind(false, false, false, false),
            Err(HostedSlackProviderError::InvalidResponse(
                "conversation kind"
            ))
        );
        assert_eq!(
            hosted_conversation_kind(true, true, false, false),
            Err(HostedSlackProviderError::InvalidResponse(
                "conversation kind"
            ))
        );
    }

    #[test]
    fn provider_discovered_channel_classifies_all_hosted_conversation_kinds() {
        for (id, flags, kind) in [
            (
                "C08PUBLIC01",
                (true, false, false, false),
                HostedSlackConversationKindV1::PublicChannel,
            ),
            (
                "G08PRIVATE1",
                (false, true, false, false),
                HostedSlackConversationKindV1::PrivateChannel,
            ),
            (
                "D08DIRECT01",
                (false, false, true, false),
                HostedSlackConversationKindV1::Im,
            ),
            (
                "G08GROUPDM1",
                (false, false, false, true),
                HostedSlackConversationKindV1::Mpim,
            ),
        ] {
            let (is_channel, is_group, is_im, is_mpim) = flags;
            let discovered = provider_discovered_channel(
                discovered_channel_wire(id, is_channel, is_group, is_im, is_mpim),
                "T08LOCALITY1",
            )
            .expect("discovered conversation");
            assert_eq!(discovered.channel.conversation_kind, kind);
        }
    }

    #[test]
    fn initial_channel_descriptor_is_authoritative_and_rejects_unreadable_candidates() {
        let binding = installation_binding();
        let descriptor = HostedSlackInitialChannelDescriptorV1::new(
            &binding,
            &discovered_channel("T08LOCALITY1", true, false),
            "2026-05-28T20:26:40Z".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
        )
        .unwrap();
        assert_eq!(descriptor.selector.installation_id, binding.installation_id);
        assert_eq!(descriptor.selector.team_id, "T08LOCALITY1");
        assert_eq!(descriptor.selector.channel_id, "C08ENGINEER1");
        assert_eq!(
            descriptor.selector.sharing,
            SlackChannelSharingClassification::Private
        );
        assert_eq!(
            descriptor.selector.authorized_history_start_at,
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(
            HostedSlackInitialChannelDescriptorV1::new(
                &binding,
                &discovered_channel("T08LOCALITY1", false, false),
                "2026-05-28T20:26:40Z".to_string(),
                "2026-01-01T00:00:00Z".to_string(),
            ),
            Err(HostedSlackProviderError::NotFound("channel membership"))
        );
        assert_eq!(
            HostedSlackInitialChannelDescriptorV1::new(
                &binding,
                &discovered_channel("T08OTHER001", true, false),
                "2026-05-28T20:26:40Z".to_string(),
                "2026-01-01T00:00:00Z".to_string(),
            ),
            Err(HostedSlackProviderError::IdentityMismatch(
                "channel team_id"
            ))
        );
        assert_eq!(
            HostedSlackInitialChannelDescriptorV1::new(
                &binding,
                &discovered_channel("T08LOCALITY1", true, true),
                "2026-05-28T20:26:40Z".to_string(),
                "2026-01-01T00:00:00Z".to_string(),
            ),
            Err(HostedSlackProviderError::Revoked)
        );
        let mut slack_connect = discovered_channel("T08LOCALITY1", true, false);
        slack_connect.channel.sharing = SlackChannelSharingClassification::ExternallySharedPrivate;
        assert_eq!(
            HostedSlackInitialChannelDescriptorV1::new(
                &binding,
                &slack_connect,
                "2026-05-28T20:26:40Z".to_string(),
                "2026-01-01T00:00:00Z".to_string(),
            ),
            Err(HostedSlackProviderError::Unsupported(
                "Slack Connect channel identity in V1"
            ))
        );
    }

    #[tokio::test]
    async fn discovery_and_readiness_reject_slack_connect_without_composite_identity() {
        let (base_url, _requests, server) = spawn_stub_server(vec![StubResponse {
            status: "200 OK",
            headers: Vec::new(),
            body: DISCOVERY_SLACK_CONNECT,
        }]);
        let provider = test_provider(base_url);
        assert_eq!(
            provider
                .conversations_list(HostedSlackChannelDiscoveryRequestV1 {
                    cursor: None,
                    limit: HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1,
                })
                .await,
            Err(HostedSlackProviderError::Unsupported(
                "Slack Connect channel identity in V1"
            ))
        );
        server.join().unwrap();

        let mut channel = indexed_discovered_channel(1);
        channel.channel.sharing = SlackChannelSharingClassification::ExternallySharedPublic;
        let fake = StubDiscoveryProvider::new([Ok(discovery_page(vec![channel], None))]);
        assert_eq!(
            discover_hosted_slack_channels_v1(
                &fake,
                &installation_binding(),
                &HostedSlackDriveControlV1::new(
                    Instant::now() + Duration::from_secs(10),
                    HostedSlackCancellationToken::new(),
                    None,
                ),
            )
            .await,
            Err(HostedSlackProviderError::Unsupported(
                "Slack Connect channel identity in V1"
            ))
        );

        assert_eq!(
            verify_channel_authority(
                &HostedSlackChannelSelector {
                    sharing: SlackChannelSharingClassification::ExternallySharedPrivate,
                    ..HostedSlackInitialChannelDescriptorV1::new(
                        &installation_binding(),
                        &discovered_channel("T08LOCALITY1", true, false),
                        "2026-05-28T20:26:40Z".to_string(),
                        "2026-01-01T00:00:00Z".to_string(),
                    )
                    .unwrap()
                    .selector
                },
                &observed_identity(),
                &HostedSlackObservedChannelAuthorityV1 {
                    team_id: "T08LOCALITY1".to_string(),
                    channel_id: "C08ENGINEER1".to_string(),
                    is_private: true,
                    is_shared: true,
                    is_externally_shared: true,
                    is_org_shared: false,
                    is_member: true,
                    shared_team_ids: vec!["T08EXTERNAL1".to_string(), "T08LOCALITY1".to_string(),],
                },
            ),
            Err(HostedSlackProviderError::Unsupported(
                "Slack Connect channel identity in V1"
            ))
        );
    }

    #[tokio::test]
    async fn discovery_verifies_identity_pages_with_exact_bounded_queries() {
        let auth = r#"{
            "ok": true,
            "team_id": "T08LOCALITY1",
            "user_id": "U08LOCALBOT1",
            "enterprise_id": "E08LOCALITY1",
            "is_enterprise_install": false
        }"#;
        let (base_url, requests, server) = spawn_stub_server(vec![
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: auth,
            },
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: DISCOVERY_PAGE_ONE,
            },
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: DISCOVERY_PAGE_TWO,
            },
        ]);
        let provider = test_provider(base_url);
        let discovery = discover_hosted_slack_channels_v1(
            &provider,
            &installation_binding(),
            &HostedSlackDriveControlV1::new(
                Instant::now() + Duration::from_secs(10),
                HostedSlackCancellationToken::new(),
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(discovery.installation, observed_identity());
        assert_eq!(discovery.channels.len(), 2);
        assert!(discovery.channels[0].is_member);
        assert!(discovery.channels[1].is_archived);
        assert_eq!(
            discovery.channels[0].channel.updated_ts.as_deref(),
            Some("1780000010.123000")
        );
        assert_eq!(
            requests.recv().unwrap().lines().next(),
            Some("POST /auth.test HTTP/1.1")
        );
        assert_eq!(
            requests.recv().unwrap().lines().next(),
            Some(
                "GET /conversations.list?types=public_channel%2Cprivate_channel%2Cim%2Cmpim&exclude_archived=false&limit=100 HTTP/1.1"
            )
        );
        assert_eq!(
            requests.recv().unwrap().lines().next(),
            Some(
                "GET /conversations.list?types=public_channel%2Cprivate_channel%2Cim%2Cmpim&exclude_archived=false&limit=100&cursor=page-two HTTP/1.1"
            )
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn discovery_accepts_exact_bounds_and_rejects_oversize_pages_cursors_and_cycles() {
        let pages = (0..MAX_HOSTED_SLACK_DISCOVERY_PAGES_V1)
            .map(|page_index| {
                let start = page_index * HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1 as usize;
                let channels = (start..start + HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1 as usize)
                    .map(indexed_discovered_channel)
                    .collect();
                let next_cursor = (page_index + 1 < MAX_HOSTED_SLACK_DISCOVERY_PAGES_V1)
                    .then(|| format!("page-{}", page_index + 2));
                Ok(discovery_page(channels, next_cursor))
            })
            .collect::<Vec<_>>();
        let exact = StubDiscoveryProvider::new(pages);
        let discovery = discover_hosted_slack_channels_v1(
            &exact,
            &installation_binding(),
            &HostedSlackDriveControlV1::new(
                Instant::now() + Duration::from_secs(10),
                HostedSlackCancellationToken::new(),
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            discovery.channels.len(),
            MAX_HOSTED_SLACK_DISCOVERY_CHANNELS_V1
        );
        assert_eq!(
            exact.requests.lock().unwrap().len(),
            MAX_HOSTED_SLACK_DISCOVERY_PAGES_V1
        );

        let oversized_page = StubDiscoveryProvider::new([Ok(discovery_page(
            (0..=HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1 as usize)
                .map(indexed_discovered_channel)
                .collect(),
            None,
        ))]);
        assert_eq!(
            discover_hosted_slack_channels_v1(
                &oversized_page,
                &installation_binding(),
                &HostedSlackDriveControlV1::new(
                    Instant::now() + Duration::from_secs(10),
                    HostedSlackCancellationToken::new(),
                    None,
                ),
            )
            .await,
            Err(HostedSlackProviderError::LimitExceeded(
                "discovery page channels"
            ))
        );

        let oversized_cursor = StubDiscoveryProvider::new([Ok(discovery_page(
            Vec::new(),
            Some("x".repeat(MAX_HOSTED_SLACK_DISCOVERY_CURSOR_BYTES_V1 + 1)),
        ))]);
        assert_eq!(
            discover_hosted_slack_channels_v1(
                &oversized_cursor,
                &installation_binding(),
                &HostedSlackDriveControlV1::new(
                    Instant::now() + Duration::from_secs(10),
                    HostedSlackCancellationToken::new(),
                    None,
                ),
            )
            .await,
            Err(HostedSlackProviderError::LimitExceeded("discovery cursor"))
        );

        let never_ending = StubDiscoveryProvider::new(
            (0..MAX_HOSTED_SLACK_DISCOVERY_PAGES_V1).map(|page_index| {
                Ok(discovery_page(
                    Vec::new(),
                    Some(format!("page-{}", page_index + 2)),
                ))
            }),
        );
        assert_eq!(
            discover_hosted_slack_channels_v1(
                &never_ending,
                &installation_binding(),
                &HostedSlackDriveControlV1::new(
                    Instant::now() + Duration::from_secs(10),
                    HostedSlackCancellationToken::new(),
                    None,
                ),
            )
            .await,
            Err(HostedSlackProviderError::LimitExceeded("discovery pages"))
        );
        assert_eq!(
            never_ending.requests.lock().unwrap().len(),
            MAX_HOSTED_SLACK_DISCOVERY_PAGES_V1
        );

        let repeated_cursor = StubDiscoveryProvider::new([
            Ok(discovery_page(
                vec![indexed_discovered_channel(1)],
                Some("same-cursor".to_string()),
            )),
            Ok(discovery_page(
                vec![indexed_discovered_channel(2)],
                Some("same-cursor".to_string()),
            )),
        ]);
        assert_eq!(
            discover_hosted_slack_channels_v1(
                &repeated_cursor,
                &installation_binding(),
                &HostedSlackDriveControlV1::new(
                    Instant::now() + Duration::from_secs(10),
                    HostedSlackCancellationToken::new(),
                    None,
                ),
            )
            .await,
            Err(HostedSlackProviderError::InvalidResponse(
                "repeated discovery cursor"
            ))
        );
        assert_eq!(
            repeated_cursor.requests.lock().unwrap().as_slice(),
            [
                HostedSlackChannelDiscoveryRequestV1 {
                    cursor: None,
                    limit: HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1,
                },
                HostedSlackChannelDiscoveryRequestV1 {
                    cursor: Some("same-cursor".to_string()),
                    limit: HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1,
                },
            ]
        );
    }

    #[tokio::test]
    async fn discovery_rejects_malformed_and_out_of_range_updated_timestamps_without_leaking() {
        let (base_url, _requests, server) = spawn_stub_server(vec![StubResponse {
            status: "200 OK",
            headers: Vec::new(),
            body: DISCOVERY_MALFORMED_UPDATED,
        }]);
        let provider = test_provider(base_url);
        let error = provider
            .conversations_list(HostedSlackChannelDiscoveryRequestV1 {
                cursor: None,
                limit: HOSTED_SLACK_DISCOVERY_PAGE_LIMIT_V1,
            })
            .await
            .unwrap_err();
        assert_eq!(error, HostedSlackProviderError::InvalidResponse("JSON"));
        assert!(!error.to_string().contains("malformed-timestamp-secret"));
        server.join().unwrap();

        let mut response =
            serde_json::from_str::<ConversationsListResponse>(DISCOVERY_PAGE_ONE).unwrap();
        let mut channel = response.channels.take().unwrap().remove(0);
        channel.updated = Some(u64::MAX);
        assert_eq!(
            provider_discovered_channel(channel, "T08LOCALITY1"),
            Err(HostedSlackProviderError::Portable(
                HostedSlackPortableError::InvalidTimestamp("channel.updated_ts")
            ))
        );
    }

    #[test]
    fn canonical_slack_boundaries_use_exact_microsecond_half_open_semantics() {
        assert_eq!(
            canonical_slack_timestamp("2026-01-01T00:00:00Z", true).unwrap(),
            "1767225599.999999"
        );
        assert_eq!(
            canonical_slack_timestamp("2026-06-01T00:00:00Z", false).unwrap(),
            "1780272000.000000"
        );
        assert!(canonical_slack_timestamp("2026-01-01T00:00:00+00:00", false).is_err());
        assert!(canonical_slack_timestamp("1969-12-31T23:59:59Z", false).is_err());
    }

    #[tokio::test]
    async fn production_http_serializes_exact_history_replies_and_authority_queries() {
        let empty_page =
            r#"{"ok":true,"messages":[],"has_more":false,"response_metadata":{"next_cursor":""}}"#;
        let authority_body = r#"{
            "ok": true,
            "channel": {
                "id": "C08ENGINEER1",
                "context_team_id": "T08LOCALITY1",
                "team_id": "T08LOCALITY1",
                "is_private": true,
                "is_shared": true,
                "is_ext_shared": true,
                "is_org_shared": false,
                "is_member": true,
                "shared_team_ids": ["T08LOCALITY1", "T08EXTERNAL1"]
            }
        }"#;
        let (base_url, requests, server) = spawn_stub_server(vec![
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: empty_page,
            },
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: empty_page,
            },
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: authority_body,
            },
        ]);
        let provider = test_provider(base_url);
        let installation_id: SlackInstallationId =
            serde_json::from_str(r#""0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10""#).unwrap();
        provider
            .conversations_history(HostedSlackHistoryRequestV1 {
                installation_id: installation_id.clone(),
                team_id: "T08LOCALITY1".to_string(),
                channel_id: "C08ENGINEER1".to_string(),
                phase: HostedSlackPollPhaseV1::HistoricalHistory,
                oldest: "1767225599.999999".to_string(),
                latest: "1780272000.000000".to_string(),
                inclusive: false,
                cursor: Some("history-page-2".to_string()),
                limit: HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1,
            })
            .await
            .unwrap();
        provider
            .conversations_replies(HostedSlackRepliesRequestV1 {
                installation_id,
                team_id: "T08LOCALITY1".to_string(),
                channel_id: "C08ENGINEER1".to_string(),
                phase: HostedSlackPollPhaseV1::HistoricalReplies,
                root_message_id: "1780000000.000100".to_string(),
                latest: "1780272000.000000".to_string(),
                inclusive: false,
                cursor: Some("reply-page-2".to_string()),
                limit: HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1,
            })
            .await
            .unwrap();
        let authority = provider
            .conversations_info("C08ENGINEER1".to_string())
            .await
            .unwrap();
        assert_eq!(authority.team_id, "T08LOCALITY1");
        assert_eq!(authority.shared_team_ids, ["T08EXTERNAL1", "T08LOCALITY1"]);

        let history = requests.recv().unwrap();
        let replies = requests.recv().unwrap();
        let info = requests.recv().unwrap();
        assert_eq!(
            history.lines().next(),
            Some(
                "GET /conversations.history?channel=C08ENGINEER1&oldest=1767225599.999999&latest=1780272000.000000&inclusive=false&limit=15&cursor=history-page-2 HTTP/1.1"
            )
        );
        assert_eq!(
            replies.lines().next(),
            Some(
                "GET /conversations.replies?channel=C08ENGINEER1&ts=1780000000.000100&latest=1780272000.000000&inclusive=false&limit=15&cursor=reply-page-2 HTTP/1.1"
            )
        );
        assert_eq!(
            info.lines().next(),
            Some("GET /conversations.info?channel=C08ENGINEER1 HTTP/1.1")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn production_http_rejects_missing_messages_malformed_json_and_ambiguous_rate_limit() {
        let (base_url, _requests, server) = spawn_stub_server(vec![
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: r#"{"ok":true,"has_more":false}"#,
            },
            StubResponse {
                status: "200 OK",
                headers: Vec::new(),
                body: r#"{"ok":true,"messages":"not-a-list","has_more":false}"#,
            },
            StubResponse {
                status: "429 Too Many Requests",
                headers: Vec::new(),
                body: r#"{"ok":false,"error":"provider-body-secret"}"#,
            },
        ]);
        let provider = test_provider(base_url);
        let request = HostedSlackHistoryRequestV1 {
            installation_id: serde_json::from_str(r#""0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10""#)
                .unwrap(),
            team_id: "T08LOCALITY1".to_string(),
            channel_id: "C08ENGINEER1".to_string(),
            phase: HostedSlackPollPhaseV1::HistoricalHistory,
            oldest: "1767225599.999999".to_string(),
            latest: "1780272000.000000".to_string(),
            inclusive: false,
            cursor: None,
            limit: HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1,
        };
        assert_eq!(
            provider.conversations_history(request.clone()).await,
            Err(HostedSlackProviderError::InvalidResponse("messages"))
        );
        assert_eq!(
            provider.conversations_history(request.clone()).await,
            Err(HostedSlackProviderError::InvalidResponse("JSON"))
        );
        let rate_error = provider.conversations_history(request).await.unwrap_err();
        assert_eq!(
            rate_error,
            HostedSlackProviderError::InvalidResponse("Retry-After")
        );
        assert!(!rate_error.to_string().contains("provider-body-secret"));
        server.join().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_and_long_429s_install_the_bounded_fallback_cooldown() {
        let cases = [
            (
                Vec::new(),
                HostedSlackProviderError::InvalidResponse("Retry-After"),
                HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
            ),
            (
                vec![("Retry-After", "not-a-delay")],
                HostedSlackProviderError::InvalidResponse("Retry-After"),
                HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
            ),
            (
                vec![("Retry-After", "0")],
                HostedSlackProviderError::InvalidResponse("Retry-After"),
                HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
            ),
            (
                vec![("Retry-After", "18446744073709551616")],
                HostedSlackProviderError::InvalidResponse("Retry-After"),
                HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
            ),
            (
                vec![("Retry-After", "300")],
                HostedSlackProviderError::RateLimited {
                    retry_after: MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1,
                },
                MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1,
            ),
            (
                vec![("Retry-After", "301")],
                HostedSlackProviderError::RateLimited {
                    retry_after: Duration::from_secs(301),
                },
                HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
            ),
            (
                vec![("Retry-After", "18446744073709551615")],
                HostedSlackProviderError::RateLimited {
                    retry_after: Duration::from_secs(u64::MAX),
                },
                HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
            ),
        ];

        for (headers, expected, expected_cooldown) in cases {
            let (base_url, _requests, server) = spawn_stub_server(vec![StubResponse {
                status: "429 Too Many Requests",
                headers,
                body: r#"{"ok":false,"error":"rate-secret"}"#,
            }]);
            let mut provider = test_provider(base_url);
            provider.client = Client::builder().build().unwrap();
            assert_eq!(
                provider
                    .conversations_info("C08ENGINEER1".to_string())
                    .await,
                Err(expected)
            );
            server.join().unwrap();

            let gate = provider
                .gates
                .gate(HostedSlackProviderOperationV1::ConversationsInfo);
            assert_eq!(gate.status().cooldown_remaining, Some(expected_cooldown));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn huge_retry_after_uses_bounded_gate_fallback_and_same_key_proceeds() {
        assert_http_retry_after_gate_reopens(
            "18446744073709551615",
            HostedSlackProviderError::RateLimited {
                retry_after: Duration::from_secs(u64::MAX),
            },
            HOSTED_SLACK_PROVIDER_GATE_FALLBACK_RETRY_AFTER,
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn valid_retry_after_uses_exact_gate_delay_and_same_key_proceeds() {
        assert_http_retry_after_gate_reopens(
            "7",
            HostedSlackProviderError::RateLimited {
                retry_after: Duration::from_secs(7),
            },
            Duration::from_secs(7),
        )
        .await;
    }

    #[tokio::test]
    async fn production_429_cooldown_is_shared_with_concurrent_method_calls() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (first_seen_tx, first_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut first);
            first_seen_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            write_http_response(
                &mut first,
                StubResponse {
                    status: "429 Too Many Requests",
                    headers: vec![("Retry-After", "1")],
                    body: r#"{"ok":false,"error":"rate-secret"}"#,
                },
            );
            let (mut second, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut second);
            write_http_response(
                &mut second,
                StubResponse {
                    status: "200 OK",
                    headers: Vec::new(),
                    body: r#"{
                        "ok": true,
                        "channel": {
                            "id": "C08ENGINEER1",
                            "context_team_id": "T08LOCALITY1",
                            "is_private": true,
                            "is_shared": true,
                            "is_ext_shared": true,
                            "is_org_shared": false,
                            "is_member": true,
                            "shared_team_ids": ["T08LOCALITY1"]
                        }
                    }"#,
                },
            );
        });
        let provider = test_provider(format!("http://{address}"));
        let first_provider = provider.clone();
        let first = tokio::spawn(async move {
            first_provider
                .conversations_info("C08ENGINEER1".to_string())
                .await
        });
        tokio::task::spawn_blocking(move || first_seen_rx.recv().unwrap())
            .await
            .unwrap();
        let second_provider = provider.clone();
        let started = Instant::now();
        let second = tokio::spawn(async move {
            second_provider
                .conversations_info("C08ENGINEER1".to_string())
                .await
        });
        release_tx.send(()).unwrap();
        let first_error = first.await.unwrap().unwrap_err();
        assert_eq!(
            first_error,
            HostedSlackProviderError::RateLimited {
                retry_after: Duration::from_secs(1)
            }
        );
        assert!(
            provider
                .gates
                .gate(HostedSlackProviderOperationV1::ConversationsInfo)
                .status()
                .cooldown_remaining
                .is_some()
        );
        assert!(
            provider
                .gates
                .gate(HostedSlackProviderOperationV1::UsersInfo)
                .status()
                .cooldown_remaining
                .is_none()
        );
        second.await.unwrap().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(!first_error.to_string().contains("rate-secret"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn production_transients_set_shared_method_cooldown_and_replies_have_no_burst() {
        let (base_url, _requests, server) = spawn_stub_server(vec![StubResponse {
            status: "503 Service Unavailable",
            headers: Vec::new(),
            body: r#"{"ok":false,"error":"transient-secret"}"#,
        }]);
        let provider = test_provider(base_url);
        assert_eq!(
            provider
                .files_info("F08PLAN0001".to_string(), "C08ENGINEER1".to_string())
                .await,
            Err(HostedSlackProviderError::Transient)
        );
        assert!(
            provider
                .gates
                .gate(HostedSlackProviderOperationV1::FilesInfo)
                .status()
                .cooldown_remaining
                .is_some()
        );
        let replies =
            operation_network_config(HostedSlackProviderOperationV1::ConversationsReplies);
        assert_eq!(replies.requests_per_second, 1.0 / 60.0);
        assert_eq!(replies.burst, 1.0);
        assert_eq!(replies.max_in_flight, 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn in_process_gates_are_app_team_and_exact_method_scoped() {
        let first = HostedSlackProviderGates::global("A08SCOPEAPP1", "T08SCOPE001");
        let same_key = HostedSlackProviderGates::global("A08SCOPEAPP1", "T08SCOPE001");
        let other_app = HostedSlackProviderGates::global("A08SCOPEAPP2", "T08SCOPE001");
        let other_team = HostedSlackProviderGates::global("A08SCOPEAPP1", "T08SCOPE002");
        assert!(Arc::ptr_eq(
            &first.conversations_history.inner,
            &same_key.conversations_history.inner,
        ));
        assert!(!Arc::ptr_eq(
            &first.conversations_history.inner,
            &first.conversations_replies.inner,
        ));
        assert!(!Arc::ptr_eq(
            &first.conversations_history.inner,
            &other_app.conversations_history.inner,
        ));
        assert!(!Arc::ptr_eq(
            &first.conversations_history.inner,
            &other_team.conversations_history.inner,
        ));

        let _held = first.conversations_history.acquire().await;
        let same_key_wait = tokio::time::timeout(
            Duration::from_millis(25),
            same_key.conversations_history.acquire(),
        );
        let other_app_wait = tokio::time::timeout(
            Duration::from_secs(1),
            other_app.conversations_history.acquire(),
        );
        let other_method_wait = tokio::time::timeout(
            Duration::from_secs(1),
            first.conversations_replies.acquire(),
        );
        let (same_key_result, other_app_result, other_method_result) =
            tokio::join!(same_key_wait, other_app_wait, other_method_wait);
        assert!(same_key_result.is_err(), "the exact same key must wait");
        assert!(
            other_app_result.is_ok(),
            "another app in the same team must proceed"
        );
        assert!(
            other_method_result.is_ok(),
            "another exact Slack API method must proceed"
        );
    }

    #[test]
    fn provider_exposes_legacy_v1_and_exact_v2_coordination_scopes() {
        let provider = HttpHostedSlackProvider::with_base_url(
            "xoxb-scope-test",
            HostedSlackObservedInstallationIdentity {
                api_app_id: "A08LOCALITY1".to_string(),
                team_id: "T08SCOPE001".to_string(),
                enterprise_id: None,
                enterprise_install: false,
                bot_user_id: "U08LOCALBOT1".to_string(),
                oauth_subject_id: "U08INSTALLER1".to_string(),
            },
            "https://scope.invalid",
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(
                &provider.coordination_scope(HostedSlackProviderOperationV1::ConversationsHistory,)
            )
            .unwrap(),
            r#"{"team_id":"T08SCOPE001","operation":"conversations_history"}"#
        );
        assert_eq!(
            serde_json::to_string(
                &provider
                    .coordination_scope_v2(HostedSlackProviderOperationV1::ConversationsHistory,)
            )
            .unwrap(),
            r#"{"api_app_id":"A08LOCALITY1","team_id":"T08SCOPE001","method":"conversations.history"}"#
        );
    }

    #[tokio::test]
    async fn production_http_calls_obey_orchestrator_cancellation_and_deadline() {
        let request = || HostedSlackHistoryRequestV1 {
            installation_id: serde_json::from_str(r#""0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10""#)
                .unwrap(),
            team_id: "T08LOCALITY1".to_string(),
            channel_id: "C08ENGINEER1".to_string(),
            phase: HostedSlackPollPhaseV1::HistoricalHistory,
            oldest: "1767225599.999999".to_string(),
            latest: "1780272000.000000".to_string(),
            inclusive: false,
            cursor: None,
            limit: HOSTED_SLACK_PROVIDER_PAGE_LIMIT_V1,
        };

        let (base_url, seen, release, server) = spawn_stalling_server();
        let provider = test_provider(base_url);
        let cancellation = HostedSlackCancellationToken::new();
        let control = HostedSlackDriveControlV1::new(
            Instant::now() + Duration::from_secs(5),
            cancellation.clone(),
            None,
        );
        let cancel = async move {
            tokio::task::spawn_blocking(move || seen.recv().unwrap())
                .await
                .unwrap();
            cancellation.cancel();
        };
        let provider_call = provider.conversations_history(request());
        let (result, ()) = tokio::join!(await_provider_future(&control, provider_call), cancel);
        assert_eq!(result, Err(HostedSlackProviderError::Cancelled));
        release.send(()).unwrap();
        server.join().unwrap();

        let (base_url, seen, release, server) = spawn_stalling_server();
        let provider = test_provider(base_url);
        let control = HostedSlackDriveControlV1::new(
            Instant::now() + Duration::from_millis(100),
            HostedSlackCancellationToken::new(),
            None,
        );
        let observed = async move {
            tokio::task::spawn_blocking(move || seen.recv().unwrap())
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            await_provider_future(&control, provider.conversations_history(request())),
            observed,
        );
        assert_eq!(result, Err(HostedSlackProviderError::DeadlineExceeded));
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn production_provider_debug_redacts_credentials_and_configured_endpoint() {
        let identity = HostedSlackObservedInstallationIdentity {
            api_app_id: "A08LOCALITY1".to_string(),
            team_id: "T08LOCALITY1".to_string(),
            enterprise_id: Some("E08LOCALITY1".to_string()),
            enterprise_install: false,
            bot_user_id: "U08LOCALBOT1".to_string(),
            oauth_subject_id: "U08INSTALLER1".to_string(),
        };
        let provider = HttpHostedSlackProvider::with_base_url(
            "xoxb-access-secret",
            identity,
            "https://xoxb-endpoint-secret.invalid",
        )
        .unwrap();
        let debug = format!("{provider:?}");
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("<configured>"));
        for secret in [
            "xoxb-access-secret",
            "xoxb-endpoint-secret",
            "U08INSTALLER1",
        ] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn provider_failures_are_classified_without_loggable_response_bodies() {
        assert_eq!(
            http_status_error(StatusCode::UNAUTHORIZED, None),
            HostedSlackProviderError::Authentication
        );
        assert_eq!(
            http_status_error(StatusCode::TOO_MANY_REQUESTS, Some(Duration::from_secs(7))),
            HostedSlackProviderError::RateLimited {
                retry_after: Duration::from_secs(7)
            }
        );
        assert_eq!(
            http_status_error(StatusCode::TOO_MANY_REQUESTS, None),
            HostedSlackProviderError::InvalidResponse("Retry-After")
        );
        assert_eq!(
            http_status_error(StatusCode::TOO_MANY_REQUESTS, Some(Duration::ZERO)),
            HostedSlackProviderError::InvalidResponse("Retry-After")
        );
        assert_eq!(
            http_status_error(
                StatusCode::TOO_MANY_REQUESTS,
                Some(MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1 + Duration::from_secs(1)),
            ),
            HostedSlackProviderError::RateLimited {
                retry_after: MAX_HOSTED_SLACK_PROVIDER_RETRY_AFTER_V1 + Duration::from_secs(1)
            }
        );
        assert_eq!(
            logical_slack_error(
                Some("token_revoked"),
                HostedSlackProviderOperationV1::VerifyInstallation,
            ),
            HostedSlackProviderError::Revoked
        );
        assert_eq!(
            logical_slack_error(
                Some("thread_not_found"),
                HostedSlackProviderOperationV1::ConversationsReplies,
            ),
            HostedSlackProviderError::ThreadNotFound
        );
        assert_eq!(
            logical_slack_error(
                Some("channel_not_found"),
                HostedSlackProviderOperationV1::ConversationsInfo,
            ),
            HostedSlackProviderError::NotFound("resource")
        );
        assert_eq!(
            logical_slack_error(
                Some("internal_error"),
                HostedSlackProviderOperationV1::ConversationsHistory,
            ),
            HostedSlackProviderError::Transient
        );
        assert_eq!(
            logical_slack_error(
                Some("provider-body-secret"),
                HostedSlackProviderOperationV1::ConversationsInfo,
            ),
            HostedSlackProviderError::InvalidResponse("logical error")
        );
        assert!(
            !logical_slack_error(
                Some("provider-body-secret"),
                HostedSlackProviderOperationV1::ConversationsInfo,
            )
            .to_string()
            .contains("provider-body-secret")
        );
        assert_eq!(
            logical_slack_error(
                Some("not_in_channel"),
                HostedSlackProviderOperationV1::ConversationsInfo,
            ),
            HostedSlackProviderError::Revoked
        );
    }

    #[tokio::test]
    async fn cancelled_and_expired_gate_reservations_never_consume_a_token() {
        let gate = || {
            HostedSlackMethodGate::new(
                ConnectorNetworkConfig::new("hosted-slack-cancel-test", 0.000_001, 2.0)
                    .max_in_flight(1),
            )
        };

        let cancellation_gate = gate();
        let first = cancellation_gate.acquire().await;
        let cancellation = HostedSlackCancellationToken::new();
        let control = HostedSlackDriveControlV1::new(
            Instant::now() + Duration::from_secs(5),
            cancellation.clone(),
            None,
        );
        let blocked_gate = cancellation_gate.clone();
        let blocked: HostedSlackProviderFuture<'_, ()> = Box::pin(async move {
            let _permit = blocked_gate.acquire().await;
            Ok(())
        });
        let observed_gate = cancellation_gate.clone();
        let cancel = async move {
            tokio::time::timeout(Duration::from_secs(1), async {
                while observed_gate.status().waiting != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(await_provider_future(&control, blocked), cancel);
        assert_eq!(result, Err(HostedSlackProviderError::Cancelled));
        let status = cancellation_gate.status();
        assert_eq!(status.waiting, 0);
        assert_eq!(status.in_flight, 1);
        assert!(status.tokens >= 1.0);
        drop(first);
        let next = tokio::time::timeout(Duration::from_millis(50), cancellation_gate.acquire())
            .await
            .expect("cancelled reservation must not consume the remaining token");
        drop(next);

        let deadline_gate = gate();
        let first = deadline_gate.acquire().await;
        let control = HostedSlackDriveControlV1::new(
            Instant::now() + Duration::from_millis(20),
            HostedSlackCancellationToken::new(),
            None,
        );
        let blocked_gate = deadline_gate.clone();
        let blocked: HostedSlackProviderFuture<'_, ()> = Box::pin(async move {
            let _permit = blocked_gate.acquire().await;
            Ok(())
        });
        assert_eq!(
            await_provider_future(&control, blocked).await,
            Err(HostedSlackProviderError::DeadlineExceeded)
        );
        let status = deadline_gate.status();
        assert_eq!(status.waiting, 0);
        assert_eq!(status.in_flight, 1);
        assert!(status.tokens >= 1.0);
        drop(first);
        let next = tokio::time::timeout(Duration::from_millis(50), deadline_gate.acquire())
            .await
            .expect("expired reservation must not consume the remaining token");
        drop(next);
    }

    #[tokio::test]
    async fn cancellation_is_race_safe_when_cancelled_before_waiter_registration() {
        let cancellation = HostedSlackCancellationToken::new();
        cancellation.cancel();
        tokio::time::timeout(Duration::from_millis(10), cancellation.cancelled())
            .await
            .unwrap();
    }
}
