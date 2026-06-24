//! Host integration primitives for Locality.
//!
//! This crate owns operating-system decisions that should not leak into the
//! sync engine or connector implementations.

pub mod bundle;
pub mod capabilities;
pub mod cloud_files;
pub mod daemon;
pub mod logs;
pub mod paths;
pub mod process;

pub use bundle::{
    bundled_binary_candidates, bundled_binary_candidates_for_target,
    bundled_binary_next_to_current_exe, executable_filename, executable_filename_for_target,
    find_bundled_binary,
};
pub use capabilities::{
    PlatformCapabilities, ProjectionModeError, mount_cli_capabilities,
    mount_cli_capabilities_for_target,
};
pub use cloud_files::{
    cloud_files_mount_id_component, decode_cloud_files_mount_id_component,
    windows_cloud_files_registration_marker_dir,
};
pub use daemon::{
    DAEMON_METADATA_FILENAME, DAEMON_PID_FILENAME, DAEMON_SOCKET_FILENAME,
    DAEMON_STDERR_LOG_FILENAME, DAEMON_STDOUT_LOG_FILENAME, DaemonManager, DaemonProcessError,
    DaemonProcessManager, DaemonProcessPaths, DaemonProcessStartConfig, DaemonProcessStartReport,
    DaemonProcessStopReport, DaemonStartMode, DefaultDaemonProcessManager, MACOS_LAUNCHD_LABEL,
    daemon_socket_path,
};
pub use logs::{
    DESKTOP_LOG_FILENAME, FILE_PROVIDER_LOG_FILENAME, LOGS_DIR_NAME, append_service_log, logs_dir,
    service_log_path,
};
pub use paths::{
    DefaultHostPaths, HostPaths, ReportPath, default_mount_root, default_state_root,
    host_path_from_logical_path, join_logical_path, logical_path_display, user_home,
};
pub use process::{DefaultSessionProcessManager, ProcessStopCommand, SessionProcessManager};
