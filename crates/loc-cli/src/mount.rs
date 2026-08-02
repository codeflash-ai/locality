//! `loc mount` orchestration.
//!
//! This first real mount command records enough connector configuration for the
//! pull path to build a filesystem projection from a Notion root page and drops
//! concise agent guidance into the mount root.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use locality_core::model::{MountId, RemoteId};
use locality_platform::DaemonManager;
use locality_store::{
    ConnectionId, LegacyLayout0Reason, LegacyWorkspaceMount, MountConfig, MountRepository,
    ProjectionMode, StoreError, WorkspaceBindingRepository, WorkspaceHostBinding,
    WorkspaceHostBindingError, WorkspaceHostBindingResolver, WorkspaceHostPlatform, WorkspaceId,
    WorkspaceProjectionIdentity, host_paths_equivalent,
};
use localityd::durable_fs::{remove_path_durable, write_new_file_durable};
use localityd::source::source_descriptor;
use serde::{Deserialize, Serialize};

pub const WORKSPACE_REMOUNT_RECOVERY_VERSION: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemountFilesystemIdentity {
    device: u64,
    inode: u64,
    #[serde(default)]
    inode_high: u64,
}

impl RemountFilesystemIdentity {
    pub fn inspect(path: &Path) -> io::Result<Self> {
        #[cfg(windows)]
        {
            let (device, inode, inode_high) =
                localityd::durable_fs::windows_path_identity_no_follow(path)?;
            return Ok(Self {
                device,
                inode,
                inode_high,
            });
        }
        #[cfg(not(windows))]
        let metadata = fs::symlink_metadata(path)?;
        #[cfg(not(windows))]
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("remount path `{}` is a symlink", path.display()),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            return Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                inode_high: 0,
            });
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = metadata;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remount filesystem identity is unsupported on this platform",
            ))
        }
    }

    #[cfg(unix)]
    pub fn unix_device_inode(self) -> (u64, u64) {
        (self.device, self.inode)
    }

    #[cfg(windows)]
    pub fn windows_volume_file_id(self) -> (u64, u64, u64) {
        (self.device, self.inode, self.inode_high)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum WorkspaceRemountRecoveryRecord {
    Header {
        version: u32,
        recovery_id: String,
        previous_mount: Box<MountConfig>,
        intended_mount: Box<MountConfig>,
        preserved_directory: Option<PathBuf>,
    },
    StagingDirectory {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<RemountFilesystemIdentity>,
    },
    StagedPath {
        original: PathBuf,
        staged: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<RemountFilesystemIdentity>,
    },
}

const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountOptions {
    pub mount_id: MountId,
    pub connector: String,
    pub root: PathBuf,
    pub remote_root_id: Option<RemoteId>,
    pub connection_id: Option<ConnectionId>,
    pub read_only: bool,
    pub projection: ProjectionMode,
    pub settings_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MountReport {
    pub ok: bool,
    pub command: &'static str,
    pub mount_id: String,
    pub connector: String,
    pub root: String,
    pub remote_root_id: Option<String>,
    pub connection_id: Option<String>,
    pub read_only: bool,
    pub projection: String,
    pub settings_json: String,
    pub guidance: MountGuidanceReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MountGuidanceReport {
    pub agents_md: GuidanceFileReport,
    pub claude_md: GuidanceFileReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GuidanceFileReport {
    pub path: String,
    pub action: GuidanceFileAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuidanceFileAction {
    Created,
    Preserved,
    Symlinked,
    Copied,
    Virtual,
}

impl GuidanceFileAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Preserved => "preserved",
            Self::Symlinked => "symlinked",
            Self::Copied => "copied",
            Self::Virtual => "virtual",
        }
    }
}

pub fn run_mount<S>(store: &mut S, options: MountOptions) -> Result<MountReport, MountError>
where
    S: MountRepository + WorkspaceBindingRepository,
{
    run_mount_inner(store, options, WorkspaceMountCommit::Standard)
}

/// Run a virtual mount while retaining the durable mount/binding transaction
/// through one coordinator-owned cleanup step.
pub fn run_mount_with_workspace_cleanup<S, F>(
    store: &mut S,
    options: MountOptions,
    mut cleanup: F,
) -> Result<MountReport, MountError>
where
    S: MountRepository + WorkspaceBindingRepository,
    F: FnMut() -> Result<(), StoreError>,
{
    run_mount_inner(
        store,
        options,
        WorkspaceMountCommit::WithCleanup(&mut cleanup),
    )
}

/// Surface-independent lifecycle used whenever an existing workspace source
/// is rebound. Desktop and the CLI provide their platform-specific drain,
/// projection cleanup, and restart operations, while this coordinator owns the
/// crash-sensitive ordering between them.
pub trait QuiescedWorkspaceRemountRuntime {
    type SupervisionFence;

    fn persist_fence(&mut self) -> Result<(), String>;
    fn clear_fence(&mut self) -> Result<(), String>;
    fn suspend_supervision(&mut self) -> Result<Self::SupervisionFence, String>;
    fn restore_supervision(&mut self, fence: &mut Self::SupervisionFence) -> Result<(), String>;
    fn remain_suspended(&mut self, fence: &mut Self::SupervisionFence);
    fn drain(&mut self) -> Result<(), String>;
    fn reconcile_cleanup(&mut self) -> Result<(), String>;
    fn ensure_running(&mut self) -> Result<(), String>;

    fn reactivate_after_failed_commit(&mut self) -> Result<(), String> {
        Ok(())
    }
}

pub fn run_quiesced_workspace_remount<R, T>(
    runtime: &mut R,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String>
where
    R: QuiescedWorkspaceRemountRuntime,
{
    runtime.persist_fence()?;
    let mut supervision = match runtime.suspend_supervision() {
        Ok(supervision) => supervision,
        Err(error) => {
            return Err(format!(
                "{error}; durable remount recovery remains fenced until supervision policy can be restored"
            ));
        }
    };

    if let Err(drain_error) = runtime.drain() {
        if let Err(restart_error) = runtime.ensure_running() {
            runtime.remain_suspended(&mut supervision);
            return Err(format!(
                "{drain_error}; restarting after the cancelled remount also failed and recovery remains fenced: {restart_error}"
            ));
        }
        if let Err(restore_error) = runtime.restore_supervision(&mut supervision) {
            runtime.remain_suspended(&mut supervision);
            return Err(format!(
                "{drain_error}; restoring supervision after restart also failed and recovery remains fenced: {restore_error}"
            ));
        }
        runtime.clear_fence()?;
        return Err(drain_error);
    }

    let operation_result = operation();
    if let Err(recovery_error) = runtime.reconcile_cleanup() {
        runtime.remain_suspended(&mut supervision);
        return Err(match operation_result {
            Ok(_) => format!(
                "the remount committed, but durable cleanup recovery did not resolve; supervision remains fenced: {recovery_error}"
            ),
            Err(error) => format!(
                "{error}; durable cleanup recovery also failed and supervision remains fenced: {recovery_error}"
            ),
        });
    }

    if let Err(error) = runtime.ensure_running() {
        runtime.remain_suspended(&mut supervision);
        return Err(format!(
            "restarting the exact pre-remount daemon manager failed; recovery remains fenced: {error}"
        ));
    }
    if let Err(error) = runtime.restore_supervision(&mut supervision) {
        runtime.remain_suspended(&mut supervision);
        return Err(error);
    }
    runtime.clear_fence()?;

    match operation_result {
        Ok(value) => Ok(value),
        Err(error) => match runtime.reactivate_after_failed_commit() {
            Ok(()) => Err(error),
            Err(reactivation) => Err(format!(
                "{error}; restoring the previous projection runtime also failed: {reactivation}"
            )),
        },
    }
}

const WORKSPACE_REMOUNT_FENCE_VERSION: u32 = 4;
const WORKSPACE_REMOUNT_FENCE_MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRemountFenceRecord {
    version: u32,
    owner: String,
    generation: String,
    mount_id: String,
    created_at: String,
    #[serde(default)]
    supervision_was_enabled: Option<bool>,
    #[serde(default)]
    daemon_was_ready: bool,
    #[serde(default)]
    daemon_manager: Option<DaemonManager>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceRemountDaemonState {
    pub was_ready: bool,
    pub manager: Option<DaemonManager>,
    pub supervision_was_enabled: Option<bool>,
}

impl WorkspaceRemountDaemonState {
    pub fn stopped(supervision_was_enabled: Option<bool>) -> Self {
        Self {
            was_ready: false,
            manager: None,
            supervision_was_enabled,
        }
    }
}

/// Exclusive cross-process ownership for one remount or recovery pass.
///
/// The sidecar OS lock is retained while recovery artifacts are inspected and
/// while the exact owner/generation fence is deleted. A stale coordinator can
/// therefore neither recover through an active remount nor clear a successor's
/// fence generation.
#[must_use = "dropping remount ownership releases cross-process exclusion"]
pub struct WorkspaceRemountOwnership {
    state_root: PathBuf,
    _lock: locality_platform::DaemonRemountCoordinatorLock,
    expected_fence: Option<Vec<u8>>,
    owner: Option<String>,
    generation: Option<String>,
    supervision_was_enabled: Option<bool>,
    daemon_was_ready: bool,
    daemon_manager: Option<DaemonManager>,
}

impl WorkspaceRemountOwnership {
    pub fn begin(
        state_root: &Path,
        mount_id: &MountId,
        owner: &str,
        created_at: &str,
    ) -> Result<Self, String> {
        Self::begin_capturing_daemon_state(state_root, mount_id, owner, created_at, || {
            Ok(WorkspaceRemountDaemonState::stopped(None))
        })
    }

    /// Acquires exclusive remount ownership before inspecting process-manager
    /// state, then persists that exact observation in the durable fence.
    pub fn begin_capturing_supervision(
        state_root: &Path,
        mount_id: &MountId,
        owner: &str,
        created_at: &str,
        capture_supervision: impl FnOnce() -> Result<Option<bool>, String>,
    ) -> Result<Self, String> {
        Self::begin_capturing_daemon_state(state_root, mount_id, owner, created_at, || {
            capture_supervision().map(WorkspaceRemountDaemonState::stopped)
        })
    }

    /// Acquires exclusive ownership, captures exact daemon readiness, manager,
    /// and launchd policy, then persists all three before any drain occurs.
    pub fn begin_capturing_daemon_state(
        state_root: &Path,
        mount_id: &MountId,
        owner: &str,
        created_at: &str,
        capture_daemon_state: impl FnOnce() -> Result<WorkspaceRemountDaemonState, String>,
    ) -> Result<Self, String> {
        let lock = locality_platform::DaemonRemountCoordinatorLock::try_acquire(state_root)
            .map_err(|error| format!("Could not acquire remount coordinator ownership: {error}"))?;
        let path = locality_platform::daemon_remount_fence_path(state_root);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Could not inspect daemon remount fence `{}`: {error}",
                    path.display()
                ));
            }
            Ok(_) => {
                return Err(format!(
                    "An interrupted remount fence already exists at `{}`; recover it before starting another remount",
                    path.display()
                ));
            }
        }
        let daemon_state = capture_daemon_state()?;
        if daemon_state.was_ready
            && !matches!(
                daemon_state.manager,
                Some(DaemonManager::Launchd | DaemonManager::Session)
            )
        {
            return Err("Could not capture the live daemon's exact process manager".to_string());
        }
        let generation = remount_fence_generation()?;
        let owner = format!("{owner}:{}", std::process::id());
        let record = WorkspaceRemountFenceRecord {
            version: WORKSPACE_REMOUNT_FENCE_VERSION,
            owner: owner.clone(),
            generation: generation.clone(),
            mount_id: mount_id.0.clone(),
            created_at: created_at.to_string(),
            supervision_was_enabled: daemon_state.supervision_was_enabled,
            daemon_was_ready: daemon_state.was_ready,
            daemon_manager: daemon_state.manager,
        };
        let mut contents = serde_json::to_vec(&record)
            .map_err(|error| format!("Could not encode daemon remount fence: {error}"))?;
        contents.push(b'\n');
        write_new_file_durable(state_root, &path, &contents).map_err(|error| {
            format!(
                "Could not persist daemon remount fence `{}`: {error}",
                path.display()
            )
        })?;
        Ok(Self {
            state_root: state_root.to_path_buf(),
            _lock: lock,
            expected_fence: Some(contents),
            owner: Some(owner),
            generation: Some(generation),
            supervision_was_enabled: daemon_state.supervision_was_enabled,
            daemon_was_ready: daemon_state.was_ready,
            daemon_manager: daemon_state.manager,
        })
    }

    pub fn recover(state_root: &Path) -> Result<Self, String> {
        let lock = locality_platform::DaemonRemountCoordinatorLock::try_acquire(state_root)
            .map_err(|error| format!("Could not acquire remount recovery ownership: {error}"))?;
        let path = locality_platform::daemon_remount_fence_path(state_root);
        let contents = match fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "Could not read daemon remount fence `{}`: {error}",
                    path.display()
                ));
            }
        };
        if contents
            .as_ref()
            .is_some_and(|contents| contents.len() > WORKSPACE_REMOUNT_FENCE_MAX_BYTES)
        {
            return Err(format!(
                "Daemon remount fence `{}` exceeds its size limit",
                path.display()
            ));
        }
        let (
            expected_fence,
            owner,
            generation,
            supervision_was_enabled,
            daemon_was_ready,
            daemon_manager,
        ) = if contents.is_none() {
            (None, None, None, None, false, None)
        } else if let Ok(record) = serde_json::from_slice::<WorkspaceRemountFenceRecord>(
            contents.as_deref().expect("checked persisted fence"),
        ) {
            if !matches!(record.version, 2 | 3 | WORKSPACE_REMOUNT_FENCE_VERSION)
                || record.owner.is_empty()
                || record.generation.is_empty()
                || (record.version >= WORKSPACE_REMOUNT_FENCE_VERSION
                    && record.daemon_was_ready
                    && !matches!(
                        record.daemon_manager,
                        Some(DaemonManager::Launchd | DaemonManager::Session)
                    ))
            {
                return Err(format!(
                    "Daemon remount fence `{}` has invalid owner/generation metadata",
                    path.display()
                ));
            }
            let supervision_was_enabled = if record.version == 2 {
                // V2 always disabled launchd and therefore always restored it.
                Some(true)
            } else {
                record.supervision_was_enabled
            };
            (
                contents,
                Some(record.owner),
                Some(record.generation),
                supervision_was_enabled,
                record.version >= WORKSPACE_REMOUNT_FENCE_VERSION && record.daemon_was_ready,
                (record.version >= WORKSPACE_REMOUNT_FENCE_VERSION)
                    .then_some(record.daemon_manager)
                    .flatten(),
            )
        } else if contents
            .as_deref()
            .is_some_and(|contents| contents.starts_with(b"version=1\n"))
        {
            // Released v1 fences had no owner token. The exact observed bytes
            // are their recovery generation and are rechecked before removal.
            (
                contents.clone(),
                Some("legacy-v1".to_string()),
                Some(hex_generation(
                    contents.as_deref().expect("checked legacy fence"),
                )),
                Some(true),
                false,
                None,
            )
        } else {
            return Err(format!(
                "Daemon remount fence `{}` has an unsupported format",
                path.display()
            ));
        };
        Ok(Self {
            state_root: state_root.to_path_buf(),
            _lock: lock,
            expected_fence,
            owner,
            generation,
            supervision_was_enabled,
            daemon_was_ready,
            daemon_manager,
        })
    }

    pub fn has_fence(&self) -> bool {
        self.expected_fence.is_some()
    }

    pub fn owner_generation(&self) -> Option<(&str, &str)> {
        self.owner.as_deref().zip(self.generation.as_deref())
    }

    pub fn supervision_was_enabled(&self) -> Option<bool> {
        self.supervision_was_enabled
    }

    pub fn daemon_state(&self) -> WorkspaceRemountDaemonState {
        WorkspaceRemountDaemonState {
            was_ready: self.daemon_was_ready,
            manager: self.daemon_manager,
            supervision_was_enabled: self.supervision_was_enabled,
        }
    }

    pub fn coordinator_lock(&self) -> &locality_platform::DaemonRemountCoordinatorLock {
        &self._lock
    }

    pub fn clear(&mut self) -> Result<(), String> {
        let Some(expected) = self.expected_fence.as_ref() else {
            return Ok(());
        };
        let path = locality_platform::daemon_remount_fence_path(&self.state_root);
        let current = fs::read(&path).map_err(|error| {
            format!(
                "Could not verify daemon remount fence `{}` before removal: {error}",
                path.display()
            )
        })?;
        if &current != expected {
            return Err(format!(
                "Daemon remount fence `{}` changed owner or generation; refusing stale removal",
                path.display()
            ));
        }
        remove_path_durable(&self.state_root, &path).map_err(|error| {
            format!(
                "Could not durably remove daemon remount fence `{}`: {error}",
                path.display()
            )
        })?;
        self.expected_fence = None;
        Ok(())
    }
}

fn remount_fence_generation() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Could not create remount fence generation: {error}"))?;
    Ok(hex_generation(&bytes))
}

fn hex_generation(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

enum WorkspaceMountCommit<'a> {
    Standard,
    WithCleanup(&'a mut dyn FnMut() -> Result<(), StoreError>),
}

fn run_mount_inner<S>(
    store: &mut S,
    options: MountOptions,
    workspace_commit: WorkspaceMountCommit<'_>,
) -> Result<MountReport, MountError>
where
    S: MountRepository + WorkspaceBindingRepository,
{
    let mut root = absolute_path(&options.root)?;
    let existing_mount = store
        .get_mount(&options.mount_id)
        .map_err(MountError::Store)?;
    if let Some(existing) = &existing_mount
        && host_paths_equivalent(WorkspaceHostPlatform::current(), &existing.root, &root)
    {
        root = existing.root.clone();
    }
    if existing_mount
        .as_ref()
        .is_some_and(|mount| mount.projection.uses_virtual_filesystem())
        && !options.projection.uses_virtual_filesystem()
    {
        return Err(MountError::ProjectionMigrationRequired {
            mount_id: options.mount_id.clone(),
        });
    }
    if options.projection.uses_virtual_filesystem() {
        let proposed = MountConfig::new(options.mount_id.clone(), "pending", &root)
            .projection(options.projection.clone());
        localityd::virtual_fs::validate_virtual_projection_root(&proposed).map_err(|error| {
            MountError::UnsafeVirtualProjectionRoot {
                root: root.clone(),
                projection_root: localityd::virtual_fs::virtual_projection_root(&proposed),
                message: error.to_string(),
            }
        })?;
        reject_duplicate_virtual_mount_point(store, &options.mount_id, &root, &options.projection)?;
    }

    let guidance = if options.projection.uses_virtual_filesystem() {
        virtual_mount_guidance(&root)
    } else {
        std::fs::create_dir_all(&root).map_err(|error| MountError::CreateRoot {
            path: root.clone(),
            message: error.to_string(),
        })?;
        install_mount_guidance(&root, &options.connector)?
    };

    let mut mount = MountConfig::new(options.mount_id.clone(), options.connector.clone(), &root)
        .read_only(options.read_only)
        .projection(options.projection.clone())
        .with_settings_json(options.settings_json.clone());
    if let Some(remote_root_id) = options.remote_root_id.clone() {
        mount = mount.with_remote_root_id(remote_root_id);
    }
    if let Some(connection_id) = options.connection_id.clone() {
        mount = mount.with_connection_id(connection_id);
    }

    if mount.projection.uses_virtual_filesystem() {
        // The virtual projection root is an explicit coordinator-owned host
        // binding, validated above. Generic stores must never infer this trust
        // from a common parent during save_mount.
        let trusted_workspace_root = localityd::virtual_fs::virtual_projection_root(&mount);
        let workspace_id = virtual_workspace_id(&mount.projection);
        let existing_host = store
            .get_workspace_host_binding(&workspace_id)
            .map_err(MountError::Store)?;
        let existing_binding = store
            .get_workspace_binding(&mount.mount_id)
            .map_err(MountError::Store)?;
        let layout_sequence = match &existing_host {
            Some(host)
                if existing_binding
                    .as_ref()
                    .and_then(|binding| binding.workspace_id())
                    == Some(&workspace_id) =>
            {
                host.layout_sequence()
            }
            Some(host) => host
                .next_layout_sequence()
                .map_err(|error| MountError::Store(StoreError::InvalidState(error.to_string())))?,
            None => 1,
        };
        let host_binding = WorkspaceHostBinding::new(
            WorkspaceHostPlatform::current(),
            workspace_id,
            trusted_workspace_root,
            virtual_projection_identity(&mount.projection),
            layout_sequence,
        )
        .map_err(MountError::HostBinding)?;
        let legacy = LegacyWorkspaceMount::new(mount.mount_id.clone(), mount.root.clone());
        let plan = WorkspaceHostBindingResolver::current()
            .plan_workspace_migration(host_binding, std::slice::from_ref(&legacy))
            .map_err(MountError::HostBinding)?;
        if let Some(layout0) = plan.layout0_mounts().first() {
            return Err(MountError::WorkspaceBindingUnavailable {
                mount_id: layout0.mount_id.clone(),
                reason: layout0.reason,
            });
        }
        if let (Some(host_binding), Some(binding)) =
            (plan.host_binding(), plan.layout1_bindings().first())
        {
            let result = match workspace_commit {
                WorkspaceMountCommit::Standard => store.save_mount_with_workspace_binding(
                    mount.clone(),
                    host_binding.clone(),
                    binding.clone(),
                ),
                WorkspaceMountCommit::WithCleanup(cleanup) => store
                    .save_mount_with_workspace_binding_and_cleanup(
                        mount.clone(),
                        host_binding.clone(),
                        binding.clone(),
                        cleanup,
                    ),
            };
            result.map_err(MountError::Store)?;
        }
    } else {
        store.save_mount(mount.clone()).map_err(MountError::Store)?;
    }

    Ok(MountReport {
        ok: true,
        command: "mount",
        mount_id: options.mount_id.0,
        connector: options.connector,
        root: root.display().to_string(),
        remote_root_id: options.remote_root_id.map(|remote_id| remote_id.0),
        connection_id: options.connection_id.map(|connection_id| connection_id.0),
        read_only: options.read_only,
        projection: options.projection.as_str().to_string(),
        settings_json: options.settings_json,
        guidance,
    })
}

/// Shared CLI/Desktop mount-root resolver. Layout-1 bindings resolve through
/// their persisted trusted workspace root; v1 and unbound mounts retain the
/// exact legacy root.
pub fn resolve_workspace_mount_root<S>(
    store: &S,
    mount: &MountConfig,
) -> Result<PathBuf, StoreError>
where
    S: WorkspaceBindingRepository,
{
    store.resolve_workspace_mount_root(mount)
}

fn virtual_workspace_id(projection: &ProjectionMode) -> WorkspaceId {
    WorkspaceId::new(format!("locality.workspace.{}", projection.as_str()))
        .expect("static virtual workspace identity is valid")
}

fn virtual_projection_identity(projection: &ProjectionMode) -> WorkspaceProjectionIdentity {
    let identity = match projection {
        ProjectionMode::MacosFileProvider => "macos-file-provider:loc",
        ProjectionMode::LinuxFuse => "linux-fuse:locality-shared-root",
        ProjectionMode::WindowsCloudFiles => {
            "windows-cloud-files:codeflash.ai.loc!default!locality"
        }
        ProjectionMode::PlainFiles => "plain-files:locality",
    };
    WorkspaceProjectionIdentity::new(identity).expect("static virtual projection identity is valid")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountError {
    CreateRoot {
        path: PathBuf,
        message: String,
    },
    CurrentDir(String),
    HostBinding(WorkspaceHostBindingError),
    WorkspaceBindingUnavailable {
        mount_id: MountId,
        reason: LegacyLayout0Reason,
    },
    MountPointConflict {
        root: PathBuf,
        mount_point: String,
        existing_mount_id: MountId,
    },
    ProjectionMigrationRequired {
        mount_id: MountId,
    },
    UnsafeVirtualProjectionRoot {
        root: PathBuf,
        projection_root: PathBuf,
        message: String,
    },
    ReadGuidance {
        path: PathBuf,
        message: String,
    },
    Store(StoreError),
    WriteGuidance {
        path: PathBuf,
        message: String,
    },
}

impl MountError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CreateRoot { .. } => "create_mount_root_failed",
            Self::CurrentDir(_) => "current_dir_failed",
            Self::HostBinding(_) => "invalid_mount_binding",
            Self::WorkspaceBindingUnavailable { .. } => "invalid_mount_binding",
            Self::MountPointConflict { .. } => "mount_point_conflict",
            Self::ProjectionMigrationRequired { .. } => "projection_migration_required",
            Self::UnsafeVirtualProjectionRoot { .. } => "unsafe_virtual_projection_root",
            Self::ReadGuidance { .. } => "read_mount_guidance_failed",
            Self::Store(_) => "store_error",
            Self::WriteGuidance { .. } => "write_mount_guidance_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CreateRoot { path, message } => {
                format!(
                    "failed to create mount root `{}`: {message}",
                    path.display()
                )
            }
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::HostBinding(error) => format!("invalid virtual mount host binding: {error}"),
            Self::WorkspaceBindingUnavailable { mount_id, reason } => format!(
                "virtual mount `{}` cannot be persisted as a portable workspace binding: {}",
                mount_id.as_str(),
                layout0_reason_message(*reason)
            ),
            Self::MountPointConflict {
                root,
                mount_point,
                existing_mount_id,
            } => format!(
                "mount `{}` already uses mount point `{mount_point}` under `{}`",
                existing_mount_id.0,
                root.display()
            ),
            Self::ProjectionMigrationRequired { mount_id } => format!(
                "mount `{}` uses a virtual layout-1 workspace binding; changing it to plain files requires an explicit projection migration",
                mount_id.as_str()
            ),
            Self::UnsafeVirtualProjectionRoot {
                root,
                projection_root,
                message,
            } => format!(
                "virtual mount `{}` would register unsafe shared provider root `{}`: {message}",
                root.display(),
                projection_root.display()
            ),
            Self::ReadGuidance { path, message } => {
                format!(
                    "failed to read mount guidance `{}`: {message}",
                    path.display()
                )
            }
            Self::Store(error) => error.to_string(),
            Self::WriteGuidance { path, message } => {
                format!(
                    "failed to write mount guidance `{}`: {message}",
                    path.display()
                )
            }
        }
    }
}

fn layout0_reason_message(reason: LegacyLayout0Reason) -> &'static str {
    match reason {
        LegacyLayout0Reason::InvalidHostPath => "its host path is invalid",
        LegacyLayout0Reason::OutsideTrustedWorkspaceRoot => {
            "it is not a direct child of the trusted workspace root"
        }
        LegacyLayout0Reason::InvalidMountTarget => "its mount target is not portable",
        LegacyLayout0Reason::MountTargetCollision => {
            "its mount target collides with another mount in the workspace"
        }
    }
}

fn reject_duplicate_virtual_mount_point<S>(
    store: &S,
    mount_id: &MountId,
    root: &Path,
    projection: &ProjectionMode,
) -> Result<(), MountError>
where
    S: MountRepository,
{
    let proposed =
        MountConfig::new(mount_id.clone(), "pending", root).projection(projection.clone());
    let proposed_root = localityd::virtual_fs::virtual_projection_root(&proposed);
    let proposed_mount_point = localityd::virtual_fs::mount_point_directory_name(&proposed);

    for existing in store.load_mounts().map_err(MountError::Store)? {
        if existing.mount_id == *mount_id || existing.projection != *projection {
            continue;
        }
        if !existing.projection.uses_virtual_filesystem() {
            continue;
        }
        if localityd::virtual_fs::virtual_projection_root(&existing) == proposed_root
            && localityd::virtual_fs::mount_point_directory_name(&existing) == proposed_mount_point
        {
            return Err(MountError::MountPointConflict {
                root: proposed_root,
                mount_point: proposed_mount_point,
                existing_mount_id: existing.mount_id,
            });
        }
    }

    Ok(())
}

fn install_mount_guidance(root: &Path, connector: &str) -> Result<MountGuidanceReport, MountError> {
    let agents_path = root.join(AGENTS_FILE);
    let claude_path = root.join(CLAUDE_FILE);
    let descriptor = source_descriptor(connector);
    let agents_action = write_guidance_if_absent(&agents_path, descriptor.mount_guidance())?;
    let claude_action = install_claude_guidance(&agents_path, &claude_path)?;

    Ok(MountGuidanceReport {
        agents_md: GuidanceFileReport {
            path: agents_path.display().to_string(),
            action: agents_action,
        },
        claude_md: GuidanceFileReport {
            path: claude_path.display().to_string(),
            action: claude_action,
        },
    })
}

fn virtual_mount_guidance(root: &Path) -> MountGuidanceReport {
    MountGuidanceReport {
        agents_md: GuidanceFileReport {
            path: root.join(AGENTS_FILE).display().to_string(),
            action: GuidanceFileAction::Virtual,
        },
        claude_md: GuidanceFileReport {
            path: root.join(CLAUDE_FILE).display().to_string(),
            action: GuidanceFileAction::Virtual,
        },
    }
}

fn write_guidance_if_absent(path: &Path, contents: &str) -> Result<GuidanceFileAction, MountError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())
                .map_err(|error| MountError::WriteGuidance {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                })?;
            Ok(GuidanceFileAction::Created)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Ok(GuidanceFileAction::Preserved)
        }
        Err(error) => Err(MountError::WriteGuidance {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

fn install_claude_guidance(
    agents_path: &Path,
    claude_path: &Path,
) -> Result<GuidanceFileAction, MountError> {
    if claude_path
        .try_exists()
        .map_err(|error| MountError::WriteGuidance {
            path: claude_path.to_path_buf(),
            message: error.to_string(),
        })?
    {
        return Ok(GuidanceFileAction::Preserved);
    }

    symlink_agents_guidance(claude_path).or_else(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            Ok(GuidanceFileAction::Preserved)
        } else {
            copy_agents_guidance(agents_path, claude_path)
        }
    })
}

#[cfg(unix)]
fn symlink_agents_guidance(claude_path: &Path) -> io::Result<GuidanceFileAction> {
    std::os::unix::fs::symlink(AGENTS_FILE, claude_path)?;
    Ok(GuidanceFileAction::Symlinked)
}

#[cfg(not(unix))]
fn symlink_agents_guidance(_claude_path: &Path) -> io::Result<GuidanceFileAction> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are not used on this platform",
    ))
}

fn copy_agents_guidance(
    agents_path: &Path,
    claude_path: &Path,
) -> Result<GuidanceFileAction, MountError> {
    let contents = fs::read_to_string(agents_path).map_err(|error| MountError::ReadGuidance {
        path: agents_path.to_path_buf(),
        message: error.to_string(),
    })?;

    match write_guidance_if_absent(claude_path, &contents)? {
        GuidanceFileAction::Created => Ok(GuidanceFileAction::Copied),
        action => Ok(action),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, MountError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| MountError::CurrentDir(error.to_string()))
    }
}

#[cfg(test)]
mod remount_coordinator_tests {
    use super::{
        QuiescedWorkspaceRemountRuntime, WorkspaceRemountDaemonState, WorkspaceRemountOwnership,
        run_quiesced_workspace_remount,
    };

    #[cfg(windows)]
    #[test]
    fn remount_identity_uses_native_attributes_only_file_id() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let path = std::env::temp_dir().join(format!(
            "locality-remount-identity-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir(&path).expect("create identity test directory");
        let identity =
            super::RemountFilesystemIdentity::inspect(&path).expect("inspect remount identity");
        let (device, inode, inode_high) =
            localityd::durable_fs::windows_path_identity_no_follow(&path)
                .expect("inspect native identity");
        assert_eq!(
            (identity.device, identity.inode, identity.inode_high),
            (device, inode, inode_high)
        );
        std::fs::remove_dir(path).expect("remove identity test directory");
    }

    struct Runtime {
        surface: &'static str,
        events: Vec<String>,
        drain_error: bool,
        ensure_error: bool,
        restore_error: bool,
        panic_while_ensuring: bool,
        panic_while_restoring: bool,
    }

    impl Runtime {
        fn record(&mut self, event: &str) {
            self.events.push(format!("{}:{event}", self.surface));
        }
    }

    impl QuiescedWorkspaceRemountRuntime for Runtime {
        type SupervisionFence = ();

        fn persist_fence(&mut self) -> Result<(), String> {
            self.record("persist_fence");
            Ok(())
        }
        fn clear_fence(&mut self) -> Result<(), String> {
            self.record("clear_fence");
            Ok(())
        }
        fn suspend_supervision(&mut self) -> Result<(), String> {
            self.record("suspend");
            Ok(())
        }
        fn restore_supervision(&mut self, _: &mut ()) -> Result<(), String> {
            self.record("restore");
            if self.panic_while_restoring {
                panic!("injected crash while restoring supervision");
            }
            if self.restore_error {
                Err("injected supervision policy restore failure".to_string())
            } else {
                Ok(())
            }
        }
        fn remain_suspended(&mut self, _: &mut ()) {
            self.record("remain_suspended");
        }
        fn drain(&mut self) -> Result<(), String> {
            self.record("drain");
            if self.drain_error {
                Err("injected drain failure".to_string())
            } else {
                Ok(())
            }
        }
        fn reconcile_cleanup(&mut self) -> Result<(), String> {
            self.record("reconcile_cleanup");
            Ok(())
        }
        fn ensure_running(&mut self) -> Result<(), String> {
            self.record("ensure_running");
            if self.panic_while_ensuring {
                panic!("injected crash while restoring exact-manager readiness");
            }
            if self.ensure_error {
                Err("injected exact-manager restart failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn cli_and_desktop_use_the_same_quiesced_remount_ordering() {
        for surface in ["cli", "desktop"] {
            let mut runtime = Runtime {
                surface,
                events: Vec::new(),
                drain_error: false,
                ensure_error: false,
                restore_error: false,
                panic_while_ensuring: false,
                panic_while_restoring: false,
            };
            run_quiesced_workspace_remount(&mut runtime, || Ok::<_, String>(()))
                .expect("coordinated remount");
            assert_eq!(
                runtime.events,
                [
                    "persist_fence",
                    "suspend",
                    "drain",
                    "reconcile_cleanup",
                    "ensure_running",
                    "restore",
                    "clear_fence",
                ]
                .map(|event| format!("{surface}:{event}"))
            );
        }
    }

    #[test]
    fn drain_failure_restores_supervision_before_clearing_fence() {
        let mut runtime = Runtime {
            surface: "shared",
            events: Vec::new(),
            drain_error: true,
            ensure_error: false,
            restore_error: false,
            panic_while_ensuring: false,
            panic_while_restoring: false,
        };
        run_quiesced_workspace_remount(&mut runtime, || Ok::<_, String>(()))
            .expect_err("drain failure cancels remount");
        assert_eq!(
            runtime.events,
            [
                "persist_fence",
                "suspend",
                "drain",
                "ensure_running",
                "restore",
                "clear_fence",
            ]
            .map(|event| format!("shared:{event}"))
        );
    }

    #[test]
    fn drain_failure_crash_during_restore_never_reaches_fence_removal() {
        let mut runtime = Runtime {
            surface: "shared",
            events: Vec::new(),
            drain_error: true,
            ensure_error: false,
            restore_error: false,
            panic_while_ensuring: false,
            panic_while_restoring: true,
        };
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run_quiesced_workspace_remount(&mut runtime, || Ok::<_, String>(()));
        }));
        assert!(crashed.is_err());
        assert_eq!(
            runtime.events,
            [
                "persist_fence",
                "suspend",
                "drain",
                "ensure_running",
                "restore"
            ]
            .map(|event| format!("shared:{event}"))
        );
    }

    #[test]
    fn restart_failure_keeps_recovery_fenced_before_policy_restore() {
        let mut runtime = Runtime {
            surface: "shared",
            events: Vec::new(),
            drain_error: false,
            ensure_error: true,
            restore_error: false,
            panic_while_ensuring: false,
            panic_while_restoring: false,
        };

        let error = run_quiesced_workspace_remount(&mut runtime, || Ok::<_, String>(()))
            .expect_err("restart failure must remain recoverable");

        assert!(error.contains("recovery remains fenced"));
        assert_eq!(
            runtime.events,
            [
                "persist_fence",
                "suspend",
                "drain",
                "reconcile_cleanup",
                "ensure_running",
                "remain_suspended"
            ]
            .map(|event| format!("shared:{event}"))
        );
    }

    #[test]
    fn disabled_policy_restore_failure_keeps_recovery_fenced_after_readiness() {
        let mut runtime = Runtime {
            surface: "shared",
            events: Vec::new(),
            drain_error: false,
            ensure_error: false,
            restore_error: true,
            panic_while_ensuring: false,
            panic_while_restoring: false,
        };

        run_quiesced_workspace_remount(&mut runtime, || Ok::<_, String>(()))
            .expect_err("policy restore failure must remain recoverable");

        assert_eq!(
            runtime.events,
            [
                "persist_fence",
                "suspend",
                "drain",
                "reconcile_cleanup",
                "ensure_running",
                "restore",
                "remain_suspended"
            ]
            .map(|event| format!("shared:{event}"))
        );
    }

    #[test]
    fn crash_during_exact_manager_restart_never_clears_durable_fence() {
        let mut runtime = Runtime {
            surface: "shared",
            events: Vec::new(),
            drain_error: false,
            ensure_error: false,
            restore_error: false,
            panic_while_ensuring: true,
            panic_while_restoring: false,
        };

        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run_quiesced_workspace_remount(&mut runtime, || Ok::<_, String>(()));
        }));

        assert!(crashed.is_err());
        assert_eq!(
            runtime.events,
            [
                "persist_fence",
                "suspend",
                "drain",
                "reconcile_cleanup",
                "ensure_running"
            ]
            .map(|event| format!("shared:{event}"))
        );
    }

    #[test]
    fn competing_process_cannot_recover_an_owned_remount_generation() {
        let root = std::env::temp_dir().join(format!(
            "locality-remount-owner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut owner = WorkspaceRemountOwnership::begin(
            &root,
            &locality_core::model::MountId::new("notion-main"),
            "cli",
            "1",
        )
        .unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg("mount::remount_coordinator_tests::remount_lock_contender_helper")
            .arg("--nocapture")
            .env("LOCALITY_REMOUNT_LOCK_CONTENDER_ROOT", &root)
            .status()
            .expect("run competing remount coordinator");
        assert!(
            status.success(),
            "contender unexpectedly acquired ownership"
        );
        owner.clear().unwrap();
        drop(owner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore]
    fn remount_lock_contender_helper() {
        let Some(root) = std::env::var_os("LOCALITY_REMOUNT_LOCK_CONTENDER_ROOT") else {
            return;
        };
        let error = WorkspaceRemountOwnership::recover(std::path::Path::new(&root))
            .err()
            .expect("active owner must exclude competing recovery");
        assert!(error.contains("another Locality coordinator"), "{error}");
    }

    #[test]
    fn owner_cannot_clear_a_replaced_fence_generation() {
        let root = std::env::temp_dir().join(format!(
            "locality-remount-generation-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut owner = WorkspaceRemountOwnership::begin(
            &root,
            &locality_core::model::MountId::new("notion-main"),
            "desktop",
            "1",
        )
        .unwrap();
        std::fs::write(
            locality_platform::daemon_remount_fence_path(&root),
            b"{\"version\":2,\"owner\":\"successor\",\"generation\":\"2\",\"mount_id\":\"notion-main\",\"created_at\":\"2\"}\n",
        )
        .unwrap();
        owner
            .clear()
            .expect_err("stale owner must not clear successor generation");
        assert!(locality_platform::daemon_remount_fence_path(&root).exists());
        drop(owner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remount_fence_preserves_explicitly_disabled_supervision_across_recovery() {
        let root = std::env::temp_dir().join(format!(
            "locality-remount-disabled-supervision-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let owner = WorkspaceRemountOwnership::begin_capturing_supervision(
            &root,
            &locality_core::model::MountId::new("notion-main"),
            "desktop",
            "1",
            || Ok(Some(false)),
        )
        .unwrap();
        assert_eq!(owner.supervision_was_enabled(), Some(false));
        drop(owner);

        let mut recovered = WorkspaceRemountOwnership::recover(&root).unwrap();
        assert_eq!(recovered.supervision_was_enabled(), Some(false));
        recovered.clear().unwrap();
        drop(recovered);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remount_fence_durably_preserves_exact_ready_manager_and_policy() {
        let root = std::env::temp_dir().join(format!(
            "locality-remount-daemon-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let owner = WorkspaceRemountOwnership::begin_capturing_daemon_state(
            &root,
            &locality_core::model::MountId::new("notion-main"),
            "desktop",
            "1",
            || {
                Ok(WorkspaceRemountDaemonState {
                    was_ready: true,
                    manager: Some(locality_platform::DaemonManager::Launchd),
                    supervision_was_enabled: Some(false),
                })
            },
        )
        .unwrap();
        drop(owner);

        let mut recovered = WorkspaceRemountOwnership::recover(&root).unwrap();
        assert_eq!(
            recovered.daemon_state(),
            WorkspaceRemountDaemonState {
                was_ready: true,
                manager: Some(locality_platform::DaemonManager::Launchd),
                supervision_was_enabled: Some(false),
            }
        );
        recovered.clear().unwrap();
        drop(recovered);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn supervision_capture_runs_after_exclusive_remount_ownership() {
        let root = std::env::temp_dir().join(format!(
            "locality-remount-supervision-capture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let paths = locality_platform::DaemonProcessPaths::new(root.clone());
        let mut owner = WorkspaceRemountOwnership::begin_capturing_supervision(
            &root,
            &locality_core::model::MountId::new("notion-main"),
            "cli",
            "1",
            || {
                let error = locality_platform::DaemonStartupCoordinatorLock::try_acquire(&paths)
                    .err()
                    .expect("startup must be excluded before supervision is captured");
                assert_eq!(error.code(), "remount_in_progress");
                Ok(Some(true))
            },
        )
        .expect("capture supervision under remount ownership");

        assert_eq!(owner.supervision_was_enabled(), Some(true));
        owner.clear().expect("clear fence");
        drop(owner);
        std::fs::remove_dir_all(root).expect("remove state root");
    }
}
