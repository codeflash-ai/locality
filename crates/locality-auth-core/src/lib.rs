//! Shared OAuth connector auth contracts for Locality runtimes.
//!
//! This crate owns stable connector IDs, OAuth callback paths, authority modes,
//! and scope profiles. It does not own token storage, tenant authorization,
//! broker route handling, or hosted source finalization.

pub mod oauth;
