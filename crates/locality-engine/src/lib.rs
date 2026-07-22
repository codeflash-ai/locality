//! Shared Locality application workflows.
//!
//! The engine composes portable connector and core behavior. Persistence,
//! filesystems, transports, schedulers, and cloud infrastructure stay in host
//! adapters.

pub mod apply_reconcile;
pub mod prepare_changeset;
pub mod synchronize_project;
