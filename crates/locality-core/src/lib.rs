//! Connector-agnostic Locality correctness layer.
//!
//! This crate owns the state machines and value types that must stay independent
//! of Notion, SQLite, file watchers, and process orchestration. `plan.md` is the
//! product authority; this crate encodes its core guarantees as deterministic
//! Rust APIs so higher layers can be thin adapters.
//!
//! The design keeps I/O out of the core. Daemon code supplies observed local and
//! remote state, connector code supplies rendered canonical documents, and store
//! code persists snapshots and journals. The core decides what those facts mean.

pub mod canonical;
pub mod conflict;
pub mod diff;
pub mod error;
pub mod explain;
pub mod freshness;
pub mod hydration;
pub mod journal;
pub mod model;
pub mod path_projection;
pub mod planner;
pub mod portable;
pub mod pull;
pub mod push;
pub mod readable_diff;
pub mod shadow;
pub mod sync;
pub mod undo;
pub mod validation;

pub use error::{LocalityError, LocalityResult};
