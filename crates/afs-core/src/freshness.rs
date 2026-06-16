//! Connector-neutral freshness and remote observation types.
//!
//! Freshness is intentionally distinct from hydration. A connector can cheaply
//! observe metadata and version tokens without fetching full document bodies,
//! while hydration still owns rendered content and shadows.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{EntityKind, HydrationState, MountId, RemoteId};

/// Opaque connector-owned token for a remote entity version.
///
/// AFS core only compares versions for equality. Timestamps, etags, revision
/// IDs, sequence numbers, and content hashes all fit behind this type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RemoteVersion(pub String);

impl RemoteVersion {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Cheap source metadata for one remote entity.
///
/// Observations are advisory and must not be used as the final authority before
/// remote writes. Push preflight still re-checks connector state immediately
/// before applying mutations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteObservation {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub kind: EntityKind,
    pub title: String,
    pub parent_remote_id: Option<RemoteId>,
    pub projected_path: PathBuf,
    pub remote_version: Option<RemoteVersion>,
    pub deleted: bool,
    pub raw_metadata_json: String,
}

impl RemoteObservation {
    pub fn new(
        mount_id: MountId,
        remote_id: RemoteId,
        kind: EntityKind,
        title: impl Into<String>,
        projected_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mount_id,
            remote_id,
            kind,
            title: title.into(),
            parent_remote_id: None,
            projected_path: projected_path.into(),
            remote_version: None,
            deleted: false,
            raw_metadata_json: "{}".to_string(),
        }
    }

    pub fn with_parent(mut self, parent_remote_id: RemoteId) -> Self {
        self.parent_remote_id = Some(parent_remote_id);
        self
    }

    pub fn with_remote_version(mut self, remote_version: RemoteVersion) -> Self {
        self.remote_version = Some(remote_version);
        self
    }

    pub fn deleted(mut self, deleted: bool) -> Self {
        self.deleted = deleted;
        self
    }

    pub fn with_raw_metadata_json(mut self, raw_metadata_json: impl Into<String>) -> Self {
        self.raw_metadata_json = raw_metadata_json.into();
        self
    }
}

/// Scheduling class for how aggressively AFS should refresh an entity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessTier {
    Immediate,
    Hot,
    Warm,
    Cold,
    Dormant,
}

impl FreshnessTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
            Self::Dormant => "dormant",
        }
    }

    pub fn is_more_urgent_than(&self, other: &Self) -> bool {
        self.priority() < other.priority()
    }

    fn priority(&self) -> u8 {
        match self {
            Self::Immediate => 0,
            Self::Hot => 1,
            Self::Warm => 2,
            Self::Cold => 3,
            Self::Dormant => 4,
        }
    }
}

/// Local-only policy knobs for decaying freshness tiers.
///
/// The scheduler should spend budget where humans and agents are active, while
/// large unused subtrees should naturally cool down. Durations are in
/// milliseconds so callers can use monotonic or wall-clock sources without
/// bringing time handling into core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessOptimizationPolicy {
    pub hot_after_activity_ms: u64,
    pub warm_after_activity_ms: u64,
    pub cold_after_check_ms: u64,
    pub dormant_subtree_depth: usize,
}

impl Default for FreshnessOptimizationPolicy {
    fn default() -> Self {
        Self {
            hot_after_activity_ms: 5 * 60 * 1000,
            warm_after_activity_ms: 60 * 60 * 1000,
            cold_after_check_ms: 24 * 60 * 60 * 1000,
            dormant_subtree_depth: 4,
        }
    }
}

/// Connector-neutral activity facts used to score sync priority.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessActivity {
    pub hydration: Option<HydrationState>,
    pub local_pending: bool,
    pub remote_hint_pending: bool,
    pub last_opened_age_ms: Option<u64>,
    pub last_local_change_age_ms: Option<u64>,
    pub last_checked_age_ms: Option<u64>,
    pub subtree_depth: usize,
}

impl FreshnessActivity {
    pub fn for_hydration(hydration: HydrationState) -> Self {
        Self {
            hydration: Some(hydration),
            ..Self::default()
        }
    }

    pub fn score(&self, policy: &FreshnessOptimizationPolicy) -> u16 {
        match self.recommended_tier(policy) {
            FreshnessTier::Immediate => 100,
            FreshnessTier::Hot => 80,
            FreshnessTier::Warm => 50,
            FreshnessTier::Cold => 20,
            FreshnessTier::Dormant => 0,
        }
    }

    pub fn recommended_tier(&self, policy: &FreshnessOptimizationPolicy) -> FreshnessTier {
        if self.local_pending || self.remote_hint_pending {
            return FreshnessTier::Hot;
        }

        let newest_activity = newest_age(self.last_opened_age_ms, self.last_local_change_age_ms);
        if let Some(age) = newest_activity {
            if age <= policy.hot_after_activity_ms {
                return FreshnessTier::Hot;
            }
            if age <= policy.warm_after_activity_ms {
                return FreshnessTier::Warm;
            }
        }

        if matches!(self.hydration, Some(HydrationState::Hydrated)) {
            return FreshnessTier::Warm;
        }

        if self.subtree_depth >= policy.dormant_subtree_depth
            && self
                .last_checked_age_ms
                .is_some_and(|age| age > policy.cold_after_check_ms)
        {
            return FreshnessTier::Dormant;
        }

        FreshnessTier::Cold
    }
}

/// Result of applying the optimization policy to one entity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessDecision {
    pub tier: FreshnessTier,
    pub activity_score: u16,
}

impl FreshnessDecision {
    pub fn from_activity(
        activity: &FreshnessActivity,
        policy: &FreshnessOptimizationPolicy,
    ) -> Self {
        Self {
            tier: activity.recommended_tier(policy),
            activity_score: activity.score(policy),
        }
    }
}

fn newest_age(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

/// Advisory reason for scheduling freshness work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeHintKind {
    BackgroundPoll,
    DirectoryListed,
    ExplicitRefresh,
    FileOpened,
    LocalEdited,
    PushRequested,
    RemoteMaybeChanged,
    UrlLocated,
    Webhook,
}

impl ChangeHintKind {
    pub fn recommended_tier(&self) -> FreshnessTier {
        match self {
            Self::PushRequested => FreshnessTier::Immediate,
            Self::FileOpened
            | Self::LocalEdited
            | Self::RemoteMaybeChanged
            | Self::UrlLocated
            | Self::Webhook => FreshnessTier::Hot,
            Self::DirectoryListed | Self::ExplicitRefresh => FreshnessTier::Warm,
            Self::BackgroundPoll => FreshnessTier::Cold,
        }
    }
}

/// Advisory signal that an entity or container may need observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeHint {
    pub mount_id: MountId,
    pub remote_id: Option<RemoteId>,
    pub kind: ChangeHintKind,
    pub observed_at: String,
}

/// User- and agent-facing state derived from Local Tree and Remote Tree facts
/// relative to the Synced Tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingCopyState {
    Clean,
    RemoteChanged,
    LocalPending,
    Diverged,
}

pub fn classify_working_copy(local_changed: bool, remote_changed: bool) -> WorkingCopyState {
    match (local_changed, remote_changed) {
        (false, false) => WorkingCopyState::Clean,
        (false, true) => WorkingCopyState::RemoteChanged,
        (true, false) => WorkingCopyState::LocalPending,
        (true, true) => WorkingCopyState::Diverged,
    }
}

/// Bounded daemon work type for freshness scheduling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncJobKind {
    ObserveEntity,
    EnumerateChildren,
    HydrateEntity,
    FetchAsset,
    PushPreflight,
    ExplainRemoteChange,
}

impl SyncJobKind {
    pub fn estimated_cost(&self) -> SyncJobCost {
        match self {
            Self::ObserveEntity | Self::PushPreflight => SyncJobCost::Cheap,
            Self::EnumerateChildren | Self::ExplainRemoteChange => SyncJobCost::Medium,
            Self::HydrateEntity | Self::FetchAsset => SyncJobCost::Expensive,
        }
    }
}

/// Coarse cost class used by the daemon to spend bounded sync budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncJobCost {
    Cheap,
    Medium,
    Expensive,
}

impl SyncJobCost {
    pub fn budget_units(self) -> u16 {
        match self {
            Self::Cheap => 1,
            Self::Medium => 5,
            Self::Expensive => 20,
        }
    }
}

/// Connector-neutral freshness work item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncJob {
    pub mount_id: MountId,
    pub remote_id: Option<RemoteId>,
    pub kind: SyncJobKind,
    pub tier: FreshnessTier,
    pub reason: ChangeHintKind,
    pub estimated_cost: SyncJobCost,
    pub next_eligible_at: Option<String>,
}

impl SyncJob {
    pub fn new(
        mount_id: MountId,
        remote_id: Option<RemoteId>,
        kind: SyncJobKind,
        reason: ChangeHintKind,
    ) -> Self {
        let tier = reason.recommended_tier();
        let estimated_cost = kind.estimated_cost();
        Self {
            mount_id,
            remote_id,
            kind,
            tier,
            reason,
            estimated_cost,
            next_eligible_at: None,
        }
    }

    pub fn with_tier(mut self, tier: FreshnessTier) -> Self {
        self.tier = tier;
        self
    }

    pub fn next_eligible_at(mut self, next_eligible_at: impl Into<String>) -> Self {
        self.next_eligible_at = Some(next_eligible_at.into());
        self
    }

    pub fn dedupe_key(&self) -> String {
        let remote_id = self.remote_id.as_ref().map_or("-", RemoteId::as_str);
        format!("{}:{remote_id}:{:?}", self.mount_id.as_str(), self.kind)
    }
}
