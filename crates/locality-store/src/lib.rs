//! Durable state boundary for Locality.
//!
//! `locality-store` owns the repository contracts used by the daemon and CLI to load
//! mount configuration, locate projected entities, read Synced Tree shadows,
//! and journal pushes. The crate provides a deterministic in-memory
//! implementation for tests and a SQLite implementation for local durable state.

pub mod compatibility;
pub mod credentials;
pub mod discovery;
pub mod error;
pub mod generation_delivery;
pub mod live_mode;
pub mod memory;
pub mod records;
pub mod repository;
pub mod reset;
pub mod sqlite;
pub mod workspace_binding;

pub use compatibility::{
    StateCompatibilityIssue, StateCompatibilityReport, StateCompatibilityStatus,
    StateComponentDefinition, StateComponentRecord,
};
pub use credentials::{
    CredentialError, CredentialResult, CredentialStore, FileCredentialStore,
    InMemoryCredentialStore, open_credential_store,
};
pub use discovery::{
    DiscoveryCommit, DiscoveryRepository, DiscoveryReservation, DiscoveryTransactionEnvelope,
    DiscoveryTransactionId, DiscoveryTransactionRecord, DiscoveryTransactionStatus,
    PreparedDiscoveryTransaction, TransactionalDiscoveryCommit, discovery_auto_save_candidate,
};
pub use error::{StoreError, StoreResult};
pub use generation_delivery::{
    GenerationApplyJournalRecord, GenerationApplyOutcome, GenerationApplyStatus,
    GenerationDeliveryRepository, GenerationInodeEvidenceConflictUpdate,
    GenerationInodeEvidenceRecord, GenerationInodeEvidenceResolution,
    GenerationInodeEvidenceTombstoneRefresh, GenerationPathRecord, GenerationPathState,
    GenerationRetainedInodeRecord, ObservedGenerationRecord, PreparedGenerationApply,
};
pub use live_mode::{
    LIVE_MODE_STATE_CHANGE_SIGNAL_FILE, MountLiveModeStateChangeError,
    is_live_mode_state_change_signal_path, live_mode_state_change_signal_path,
    publish_live_mode_state_change_signal, save_mount_live_mode_and_publish_signal,
};
pub use memory::InMemoryStateStore;
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
    EntityRepository, EntitySearchCandidate, EntitySearchDocument, EntitySearchRepository,
    FreshnessStateRepository, HydrationJobRepository, JournalRepository,
    MetadataDiscoveryJobRepository, MountLiveModeRepository, MountRepository,
    RemoteObservationRepository, ShadowRepository, VirtualMoveRepository, VirtualMoveTransition,
    VirtualMutationRepository, WorkspaceBindingRepository,
};
pub use reset::{
    LocalStateResetCredentialError, LocalStateResetError, LocalStateResetStorageReport,
    connection_secret_refs, reset_locality_state_storage,
};
pub use sqlite::SqliteStateStore;
pub use workspace_binding::{
    WORKSPACE_BINDING_LAYOUT_VERSION, WORKSPACE_BINDING_VERSION, WorkspaceBinding,
    WorkspaceBindingError, WorkspaceBindingRecord, WorkspaceRebindBlocker,
};
