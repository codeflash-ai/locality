//! Durable state boundary for Locality.
//!
//! `locality-store` owns the repository contracts used by the daemon and CLI to load
//! mount configuration, locate projected entities, read Synced Tree shadows,
//! and journal pushes. The crate provides a deterministic in-memory
//! implementation for tests and a SQLite implementation for local durable state.

pub mod compatibility;
pub mod credentials;
pub mod error;
pub mod memory;
pub mod records;
pub mod repository;
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
pub use memory::InMemoryStateStore;
pub use records::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveState, ConnectionId, ConnectionRecord,
    ConnectorProfileId, ConnectorProfileRecord, EntityRecord, FreshnessStateRecord,
    HydrationJobRecord, MountConfig, ProjectionMode, RemoteObservationRecord, ShadowBlockRecord,
    ShadowSnapshotRecord, VirtualMutationKind, VirtualMutationRecord,
};
pub use repository::{
    AutoSaveRepository, ConnectionRepository, ConnectorProfileRepository, EntityRepository,
    EntitySearchCandidate, EntitySearchRepository, FreshnessStateRepository,
    HydrationJobRepository, JournalRepository, MountRepository, RemoteObservationRepository,
    ShadowRepository, VirtualMutationRepository,
};
pub use sqlite::SqliteStateStore;
