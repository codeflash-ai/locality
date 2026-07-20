//! Synchronize/project workflow boundary.
//!
//! Phase 0 defines the portable edge while connector scheduling, checkpoints,
//! and persistence remain host responsibilities. Orchestration is added when
//! Phase 1 ingestion requires it.

use locality_connector::{PortableEnumerateRequest, PortableEnumerateResult};
use locality_core::LocalityResult;

pub trait SynchronizeAndProjectWorkflow {
    fn synchronize_and_project(
        &self,
        request: PortableEnumerateRequest,
    ) -> LocalityResult<PortableEnumerateResult>;
}
