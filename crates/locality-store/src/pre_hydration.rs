//! Durable mount pre-hydration state helpers.
//!
//! Pre-hydration progress is intentionally summary-only so mount startup can
//! update state without scanning entities, files, or hydration jobs.

use locality_core::model::MountId;
use serde::{Deserialize, Serialize};

use crate::error::{StoreError, StoreResult};
use crate::records::ConnectorStateRecord;
use crate::repository::ConnectorStateRepository;

pub const PRE_HYDRATION_SCOPE_KIND: &str = "mount_pre_hydration";
pub const PRE_HYDRATION_STATE_VERSION: i64 = 1;
pub const PRE_HYDRATION_MIN_READER_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountPreHydrationStatus {
    Requested,
    Enumerating,
    Hydrating,
    Complete,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountPreHydrationState {
    pub enabled: bool,
    pub status: MountPreHydrationStatus,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub last_error: Option<String>,
    pub discovered_pages: usize,
    pub queued_pages: usize,
}

impl MountPreHydrationState {
    pub fn requested(now: impl Into<String>) -> Self {
        Self {
            enabled: true,
            status: MountPreHydrationStatus::Requested,
            requested_at: now.into(),
            started_at: None,
            completed_at: None,
            last_error: None,
            discovered_pages: 0,
            queued_pages: 0,
        }
    }
}

pub fn enable_mount_pre_hydration(
    store: &mut impl ConnectorStateRepository,
    connector: &str,
    mount_id: &MountId,
    now: &str,
) -> StoreResult<MountPreHydrationState> {
    let state = MountPreHydrationState::requested(now);
    save_mount_pre_hydration_state(store, connector, mount_id, &state)?;
    Ok(state)
}

pub fn load_mount_pre_hydration_state(
    store: &impl ConnectorStateRepository,
    connector: &str,
    mount_id: &MountId,
) -> StoreResult<Option<MountPreHydrationState>> {
    let Some(record) =
        store.get_connector_state(connector, PRE_HYDRATION_SCOPE_KIND, mount_id.as_str())?
    else {
        return Ok(None);
    };

    if record.min_reader_version > PRE_HYDRATION_STATE_VERSION {
        return Err(StoreError::Database(format!(
            "mount pre-hydration state for connector `{connector}` mount `{}` requires reader version {}, but supported version is {}; update Locality to read this state",
            mount_id.as_str(),
            record.min_reader_version,
            PRE_HYDRATION_STATE_VERSION
        )));
    }

    serde_json::from_str(&record.state_json).map(Some).map_err(|error| {
        StoreError::Database(format!(
            "failed to decode mount pre-hydration state for connector `{connector}` mount `{}`: {error}",
            mount_id.as_str()
        ))
    })
}

pub fn save_mount_pre_hydration_state(
    store: &mut impl ConnectorStateRepository,
    connector: &str,
    mount_id: &MountId,
    state: &MountPreHydrationState,
) -> StoreResult<()> {
    let state_json = serde_json::to_string(state).map_err(|error| {
        StoreError::Database(format!(
            "failed to encode mount pre-hydration state for connector `{connector}` mount `{}`: {error}",
            mount_id.as_str()
        ))
    })?;
    let updated_at = state
        .completed_at
        .as_ref()
        .or(state.started_at.as_ref())
        .unwrap_or(&state.requested_at)
        .clone();

    store.save_connector_state(ConnectorStateRecord {
        connector: connector.to_string(),
        scope_kind: PRE_HYDRATION_SCOPE_KIND.to_string(),
        scope_id: mount_id.as_str().to_string(),
        state_version: PRE_HYDRATION_STATE_VERSION,
        min_reader_version: PRE_HYDRATION_MIN_READER_VERSION,
        state_json,
        updated_at,
    })
}

pub fn mark_mount_pre_hydration_enumerating(
    store: &mut impl ConnectorStateRepository,
    connector: &str,
    mount_id: &MountId,
    now: &str,
) -> StoreResult<MountPreHydrationState> {
    let mut state = load_mount_pre_hydration_state(store, connector, mount_id)?
        .unwrap_or_else(|| MountPreHydrationState::requested(now));
    state.enabled = true;
    state.status = MountPreHydrationStatus::Enumerating;
    state.started_at = Some(now.to_string());
    state.completed_at = None;
    state.last_error = None;
    save_mount_pre_hydration_state(store, connector, mount_id, &state)?;
    Ok(state)
}

pub fn mark_mount_pre_hydration_hydrating(
    store: &mut impl ConnectorStateRepository,
    connector: &str,
    mount_id: &MountId,
    discovered_pages: usize,
    queued_pages: usize,
    now: &str,
) -> StoreResult<MountPreHydrationState> {
    let mut state = load_mount_pre_hydration_state(store, connector, mount_id)?
        .unwrap_or_else(|| MountPreHydrationState::requested(now));
    state.enabled = true;
    state.status = if queued_pages == 0 {
        state.completed_at = Some(now.to_string());
        MountPreHydrationStatus::Complete
    } else {
        state.completed_at = None;
        MountPreHydrationStatus::Hydrating
    };
    state.started_at.get_or_insert_with(|| now.to_string());
    state.last_error = None;
    state.discovered_pages = discovered_pages;
    state.queued_pages = queued_pages;
    save_mount_pre_hydration_state(store, connector, mount_id, &state)?;
    Ok(state)
}

pub fn mark_mount_pre_hydration_error(
    store: &mut impl ConnectorStateRepository,
    connector: &str,
    mount_id: &MountId,
    message: &str,
    now: &str,
) -> StoreResult<MountPreHydrationState> {
    let mut state = load_mount_pre_hydration_state(store, connector, mount_id)?
        .unwrap_or_else(|| MountPreHydrationState::requested(now));
    state.enabled = true;
    state.status = MountPreHydrationStatus::Error;
    state.started_at.get_or_insert_with(|| now.to_string());
    state.completed_at = Some(now.to_string());
    state.last_error = Some(message.to_string());
    save_mount_pre_hydration_state(store, connector, mount_id, &state)?;
    Ok(state)
}
