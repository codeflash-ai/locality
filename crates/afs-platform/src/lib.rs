//! Host integration primitives for AgentFS.
//!
//! This crate owns operating-system decisions that should not leak into the
//! sync engine or connector implementations.

pub mod capabilities;
pub mod paths;

pub use capabilities::{
    PlatformCapabilities, ProjectionModeError, mount_cli_capabilities,
    mount_cli_capabilities_for_target,
};
pub use paths::{
    DefaultHostPaths, HostPaths, ReportPath, default_mount_root, default_state_root,
    logical_path_display, user_home,
};
