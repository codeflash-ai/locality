//! Durable state boundary for AgentFS.
//!
//! `afs-store` owns the repository contracts used by the daemon and CLI to load
//! mount configuration, locate projected entities, read last-synced shadows, and
//! journal pushes. The first concrete implementation is an in-memory store for
//! deterministic tests and early orchestration work. SQLite remains an explicit
//! placeholder until the repository surface settles.

pub mod error;
pub mod memory;
pub mod records;
pub mod repository;
pub mod sqlite;

pub use error::{StoreError, StoreResult};
pub use memory::InMemoryStateStore;
pub use records::{EntityRecord, MountConfig, ShadowBlockRecord, ShadowSnapshotRecord};
pub use repository::{EntityRepository, JournalRepository, MountRepository, ShadowRepository};
pub use sqlite::SqliteStateStore;
