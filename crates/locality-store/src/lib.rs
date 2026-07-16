//! Durable state boundary for Locality.
//!
//! `locality-store` owns the repository contracts used by the daemon and CLI to load
//! mount configuration, locate projected entities, read Synced Tree shadows,
//! and journal pushes. The crate provides a deterministic in-memory
//! implementation for tests and a SQLite implementation for local durable state.

pub mod compatibility;
pub mod credentials;
pub mod error;
pub mod live_mode;
pub mod memory;
pub mod pre_hydration;
pub mod records;
pub mod repository;
pub mod reset;
pub mod sqlite;

pub use compatibility::{
    StateCompatibilityIssue, StateCompatibilityReport, StateCompatibilityStatus,
    StateComponentDefinition, StateComponentRecord,
};
pub use credentials::{
    CredentialError, CredentialResult, CredentialStore, FileCredentialStore,
    InMemoryCredentialStore, open_credential_store,
};
pub use error::{StoreError, StoreResult};
pub use live_mode::{
    LIVE_MODE_STATE_CHANGE_SIGNAL_FILE, MountLiveModeStateChangeError,
    is_live_mode_state_change_signal_path, live_mode_state_change_signal_path,
    publish_live_mode_state_change_signal, save_mount_live_mode_and_publish_signal,
};
pub use memory::InMemoryStateStore;
pub use pre_hydration::{
    MountPreHydrationState, MountPreHydrationStatus, PRE_HYDRATION_MIN_READER_VERSION,
    PRE_HYDRATION_SCOPE_KIND, PRE_HYDRATION_STATE_VERSION, enable_mount_pre_hydration,
    load_mount_pre_hydration_state, mark_mount_pre_hydration_enumerating,
    mark_mount_pre_hydration_error, mark_mount_pre_hydration_hydrating,
    save_mount_pre_hydration_state,
};
pub use records::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveState, ConnectionId, ConnectionRecord,
    ConnectorProfileId, ConnectorProfileRecord, ConnectorStateRecord, EntityRecord,
    FreshnessStateRecord, HydrationJobRecord, MetadataDiscoveryJobRecord,
    MetadataDiscoveryPriority, MountConfig, MountLiveModeRecord, MountLiveModeState,
    ProjectionMode, RemoteObservationRecord, ShadowBlockRecord, ShadowSnapshotRecord,
    VirtualMutationKind, VirtualMutationRecord,
};
pub use repository::{
    AutoSaveRepository, ConnectionRepository, ConnectorProfileRepository, ConnectorStateRepository,
    EntityRepository, EntitySearchCandidate, EntitySearchRepository, FreshnessStateRepository,
    HydrationJobRepository, JournalRepository, MetadataDiscoveryJobRepository,
    MountLiveModeRepository, MountRepository, RemoteObservationRepository, ShadowRepository,
    VirtualMutationRepository,
};
pub use reset::{
    LocalStateResetCredentialError, LocalStateResetError, LocalStateResetStorageReport,
    connection_secret_refs, reset_locality_state_storage,
};
pub use sqlite::SqliteStateStore;
