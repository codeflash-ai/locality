//! Portable workspace placement metadata shared by Desktop, CLI, and the daemon.
//!
//! A binding deliberately excludes the host workspace root. The root is local
//! placement selected by the user; the binding is the portable rule that maps
//! a stable mount identity and logical path below whichever root is active on
//! this host.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::mem::size_of;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
};

use caseless::Caseless;
use locality_core::model::MountId;
use locality_core::portable::LogicalPath;
use locality_core::workspace_layout::MountTarget;
use serde::{Deserialize, Deserializer, Serialize};
use unicode_normalization_v16::UnicodeNormalization;

pub const LEGACY_WORKSPACE_BINDING_VERSION: u16 = 1;
pub const WORKSPACE_BINDING_VERSION: u16 = 2;
pub const WORKSPACE_BINDING_LAYOUT_VERSION: u16 = 1;
pub const WORKSPACE_HOST_BINDING_VERSION: u16 = 1;

/// Stable, host-local identity for one portable workspace namespace.
///
/// This is deliberately distinct from provider workspace IDs and hosted
/// profile IDs. It is opaque, never joined to a path, and remains stable when
/// the trusted host root is relocated by a future owning coordinator.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub const MAX_UTF8_BYTES: usize = 128;
    pub const MAX_UTF16_UNITS: usize = 128;

    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceBindingError> {
        let value = value.into();
        validate_opaque_identity("workspace ID", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Stable operating-system projection/domain identity for one workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkspaceProjectionIdentity(String);

impl WorkspaceProjectionIdentity {
    pub const MAX_UTF8_BYTES: usize = 256;
    pub const MAX_UTF16_UNITS: usize = 256;

    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceBindingError> {
        let value = value.into();
        validate_projection_identity(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WorkspaceProjectionIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Trusted host placement and projection identity for one workspace.
///
/// Absolute paths remain local placement only. They are persisted separately
/// from the portable per-mount target so a mount binding cannot invent or
/// replace a host root by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHostBinding {
    host_binding_version: u16,
    workspace_id: WorkspaceId,
    trusted_workspace_root: PathBuf,
    projection_identity: WorkspaceProjectionIdentity,
    layout_sequence: u64,
}

impl WorkspaceHostBinding {
    pub fn new(
        platform: WorkspaceHostPlatform,
        workspace_id: WorkspaceId,
        trusted_workspace_root: impl Into<PathBuf>,
        projection_identity: WorkspaceProjectionIdentity,
        layout_sequence: u64,
    ) -> Result<Self, WorkspaceHostBindingError> {
        let trusted_workspace_root = trusted_workspace_root.into();
        ParsedHostPath::parse(platform, &trusted_workspace_root)
            .ok_or(WorkspaceHostBindingError::InvalidTrustedWorkspaceRoot)?;
        Ok(Self {
            host_binding_version: WORKSPACE_HOST_BINDING_VERSION,
            workspace_id,
            trusted_workspace_root,
            projection_identity,
            layout_sequence,
        })
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn trusted_workspace_root(&self) -> &Path {
        &self.trusted_workspace_root
    }

    pub fn projection_identity(&self) -> &WorkspaceProjectionIdentity {
        &self.projection_identity
    }

    pub fn layout_sequence(&self) -> u64 {
        self.layout_sequence
    }

    pub fn next_layout_sequence(&self) -> Result<u64, WorkspaceBindingError> {
        self.layout_sequence
            .checked_add(1)
            .ok_or(WorkspaceBindingError::LayoutSequenceOverflow)
    }

    pub fn mount_root(&self, target: &MountTarget) -> PathBuf {
        self.trusted_workspace_root.join(target.as_str())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceHostBindingWire {
    host_binding_version: u16,
    workspace_id: WorkspaceId,
    trusted_workspace_root: PathBuf,
    projection_identity: WorkspaceProjectionIdentity,
    layout_sequence: u64,
}

impl<'de> Deserialize<'de> for WorkspaceHostBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceHostBindingWire::deserialize(deserializer)?;
        if wire.host_binding_version != WORKSPACE_HOST_BINDING_VERSION {
            return Err(serde::de::Error::custom(
                WorkspaceBindingError::UnsupportedHostBindingVersion {
                    actual: wire.host_binding_version,
                },
            ));
        }
        if !wire.trusted_workspace_root.is_absolute()
            || wire
                .trusted_workspace_root
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(serde::de::Error::custom(
                WorkspaceHostBindingError::InvalidTrustedWorkspaceRoot,
            ));
        }
        Ok(Self {
            host_binding_version: wire.host_binding_version,
            workspace_id: wire.workspace_id,
            trusted_workspace_root: wire.trusted_workspace_root,
            projection_identity: wire.projection_identity,
            layout_sequence: wire.layout_sequence,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBinding {
    binding_version: u16,
    layout_version: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<WorkspaceId>,
    mount_target: MountTarget,
}

impl WorkspaceBinding {
    pub fn new(mount_target: MountTarget) -> Self {
        Self {
            binding_version: LEGACY_WORKSPACE_BINDING_VERSION,
            layout_version: WORKSPACE_BINDING_LAYOUT_VERSION,
            workspace_id: None,
            mount_target,
        }
    }

    pub fn for_workspace(workspace_id: WorkspaceId, mount_target: MountTarget) -> Self {
        Self {
            binding_version: WORKSPACE_BINDING_VERSION,
            layout_version: WORKSPACE_BINDING_LAYOUT_VERSION,
            workspace_id: Some(workspace_id),
            mount_target,
        }
    }

    pub fn binding_version(&self) -> u16 {
        self.binding_version
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn mount_target(&self) -> &MountTarget {
        &self.mount_target
    }

    pub fn workspace_id(&self) -> Option<&WorkspaceId> {
        self.workspace_id.as_ref()
    }

    /// Resolve this portable binding beneath one host's selected workspace root.
    pub fn mount_root(&self, workspace_root: &Path) -> PathBuf {
        workspace_root.join(self.mount_target.as_str())
    }

    /// Resolve a validated logical path without making the host root identity.
    pub fn projected_path(&self, workspace_root: &Path, logical_path: &LogicalPath) -> PathBuf {
        self.mount_root(workspace_root)
            .join(logical_path.to_relative_path_buf())
    }

    pub(crate) fn collision_key(&self) -> String {
        self.mount_target.collision_key()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceBindingWire {
    binding_version: u16,
    layout_version: u16,
    #[serde(default)]
    workspace_id: Option<WorkspaceId>,
    mount_target: MountTarget,
}

impl<'de> Deserialize<'de> for WorkspaceBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceBindingWire::deserialize(deserializer)?;
        if !matches!(
            wire.binding_version,
            LEGACY_WORKSPACE_BINDING_VERSION | WORKSPACE_BINDING_VERSION
        ) {
            return Err(serde::de::Error::custom(
                WorkspaceBindingError::UnsupportedBindingVersion {
                    actual: wire.binding_version,
                },
            ));
        }
        if wire.layout_version != WORKSPACE_BINDING_LAYOUT_VERSION {
            return Err(serde::de::Error::custom(
                WorkspaceBindingError::UnsupportedLayoutVersion {
                    actual: wire.layout_version,
                },
            ));
        }
        match (wire.binding_version, wire.workspace_id) {
            (LEGACY_WORKSPACE_BINDING_VERSION, None) => Ok(Self::new(wire.mount_target)),
            (WORKSPACE_BINDING_VERSION, Some(workspace_id)) => {
                Ok(Self::for_workspace(workspace_id, wire.mount_target))
            }
            (LEGACY_WORKSPACE_BINDING_VERSION, Some(_)) => Err(serde::de::Error::custom(
                WorkspaceBindingError::LegacyBindingHasWorkspaceIdentity,
            )),
            (WORKSPACE_BINDING_VERSION, None) => Err(serde::de::Error::custom(
                WorkspaceBindingError::WorkspaceIdentityMissing,
            )),
            _ => unreachable!("binding version was validated above"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceBindingError {
    EmptyIdentity { kind: &'static str },
    IdentityNotNfc { kind: &'static str },
    IdentityTooLong { kind: &'static str },
    IdentityContainsControl { kind: &'static str },
    LayoutSequenceOverflow,
    LegacyBindingHasWorkspaceIdentity,
    UnsupportedBindingVersion { actual: u16 },
    UnsupportedHostBindingVersion { actual: u16 },
    UnsupportedLayoutVersion { actual: u16 },
    WorkspaceIdentityMissing,
}

impl Display for WorkspaceBindingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyIdentity { kind } => write!(formatter, "{kind} is empty"),
            Self::IdentityNotNfc { kind } => write!(formatter, "{kind} is not Unicode NFC"),
            Self::IdentityTooLong { kind } => {
                write!(formatter, "{kind} exceeds its portable bound")
            }
            Self::IdentityContainsControl { kind } => {
                write!(formatter, "{kind} contains NUL or a control character")
            }
            Self::LayoutSequenceOverflow => {
                formatter.write_str("workspace layout sequence is exhausted")
            }
            Self::LegacyBindingHasWorkspaceIdentity => formatter
                .write_str("legacy workspace binding must not contain a workspace identity"),
            Self::UnsupportedBindingVersion { actual } => {
                write!(
                    formatter,
                    "workspace binding version {actual} is unsupported"
                )
            }
            Self::UnsupportedHostBindingVersion { actual } => {
                write!(
                    formatter,
                    "workspace host binding version {actual} is unsupported"
                )
            }
            Self::UnsupportedLayoutVersion { actual } => {
                write!(
                    formatter,
                    "workspace layout version {actual} is unsupported"
                )
            }
            Self::WorkspaceIdentityMissing => {
                formatter.write_str("workspace binding is missing its workspace identity")
            }
        }
    }
}

fn validate_opaque_identity(kind: &'static str, value: &str) -> Result<(), WorkspaceBindingError> {
    if value.is_empty() {
        return Err(WorkspaceBindingError::EmptyIdentity { kind });
    }
    if !value.nfc().eq(value.chars()) {
        return Err(WorkspaceBindingError::IdentityNotNfc { kind });
    }
    if value.len() > WorkspaceId::MAX_UTF8_BYTES
        || value.encode_utf16().count() > WorkspaceId::MAX_UTF16_UNITS
    {
        return Err(WorkspaceBindingError::IdentityTooLong { kind });
    }
    if value
        .chars()
        .any(|character| character == '\0' || character.is_control())
    {
        return Err(WorkspaceBindingError::IdentityContainsControl { kind });
    }
    Ok(())
}

fn validate_projection_identity(value: &str) -> Result<(), WorkspaceBindingError> {
    let kind = "workspace projection identity";
    if value.is_empty() {
        return Err(WorkspaceBindingError::EmptyIdentity { kind });
    }
    if !value.nfc().eq(value.chars()) {
        return Err(WorkspaceBindingError::IdentityNotNfc { kind });
    }
    if value.len() > WorkspaceProjectionIdentity::MAX_UTF8_BYTES
        || value.encode_utf16().count() > WorkspaceProjectionIdentity::MAX_UTF16_UNITS
    {
        return Err(WorkspaceBindingError::IdentityTooLong { kind });
    }
    if value
        .chars()
        .any(|character| character == '\0' || character.is_control())
    {
        return Err(WorkspaceBindingError::IdentityContainsControl { kind });
    }
    Ok(())
}

impl std::error::Error for WorkspaceBindingError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceBindingRecord {
    pub mount_id: MountId,
    pub binding: WorkspaceBinding,
}

/// Metadata-visible reasons that prevent a workspace move from being safe.
///
/// Even when none of the durable blockers are present, this crate never moves
/// files or changes a mount root. An owning Desktop/daemon coordinator must
/// stop observers, move or materialize the destination, update registration,
/// and provide crash recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceRebindBlocker {
    DirtyOrConflictedState,
    UnsettledApplyJournal,
    PendingVirtualMutation,
    ActiveProjection,
    RequiresOwningCoordinator,
}

impl Display for WorkspaceRebindBlocker {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirtyOrConflictedState => {
                formatter.write_str("mount has dirty or conflicted local state")
            }
            Self::UnsettledApplyJournal => {
                formatter.write_str("mount has an unsettled apply journal")
            }
            Self::PendingVirtualMutation => {
                formatter.write_str("mount has a pending virtual mutation")
            }
            Self::ActiveProjection => {
                formatter.write_str("mount has an active platform projection")
            }
            Self::RequiresOwningCoordinator => {
                formatter.write_str("workspace moves require an owning Desktop/daemon coordinator")
            }
        }
    }
}

impl WorkspaceBindingRecord {
    pub fn new(mount_id: MountId, binding: WorkspaceBinding) -> Self {
        Self { mount_id, binding }
    }
}

/// Host path comparison rules used without compiling for the target host.
///
/// This keeps migration and publication checks table-testable on every CI
/// runner. It is deliberately separate from portable layout identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceHostPlatform {
    Macos,
    Linux,
    Windows,
}

impl WorkspaceHostPlatform {
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(target_os = "linux")]
        {
            Self::Linux
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Self::Linux
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyWorkspaceMount {
    pub mount_id: MountId,
    pub root: PathBuf,
}

impl LegacyWorkspaceMount {
    pub fn new(mount_id: MountId, root: impl Into<PathBuf>) -> Self {
        Self {
            mount_id,
            root: root.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyLayout0Reason {
    InvalidHostPath,
    OutsideTrustedWorkspaceRoot,
    InvalidMountTarget,
    MountTargetCollision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyLayout0Mount {
    pub mount_id: MountId,
    pub root: PathBuf,
    pub reason: LegacyLayout0Reason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceBindingMigrationPlan {
    workspace_root: PathBuf,
    host_binding: Option<WorkspaceHostBinding>,
    layout1_bindings: Vec<WorkspaceBindingRecord>,
    layout0_mounts: Vec<LegacyLayout0Mount>,
}

impl WorkspaceBindingMigrationPlan {
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn layout1_bindings(&self) -> &[WorkspaceBindingRecord] {
        &self.layout1_bindings
    }

    pub fn host_binding(&self) -> Option<&WorkspaceHostBinding> {
        self.host_binding.as_ref()
    }

    pub fn layout0_mounts(&self) -> &[LegacyLayout0Mount] {
        &self.layout0_mounts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceHostBindingError {
    InvalidTrustedWorkspaceRoot,
    InvalidBoundMountRoot,
    InvalidPublicationRoot,
    InvalidActiveMountRoot { mount_id: MountId },
    HostPathInspection { path: PathBuf, detail: String },
    PublicationOverlapsActiveMount { mount_id: MountId },
    BoundMountEscapesTrustedWorkspaceRoot,
}

impl Display for WorkspaceHostBindingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTrustedWorkspaceRoot => formatter.write_str(
                "trusted workspace root must be an absolute host path without parent traversal",
            ),
            Self::InvalidBoundMountRoot => formatter.write_str(
                "bound mount root must be the target's direct child of its trusted workspace root",
            ),
            Self::InvalidPublicationRoot => formatter.write_str(
                "sandbox publication root must be an absolute host path without parent traversal",
            ),
            Self::InvalidActiveMountRoot { mount_id } => write!(
                formatter,
                "active mount `{}` has an invalid host root",
                mount_id.as_str()
            ),
            Self::HostPathInspection { path, detail } => write!(
                formatter,
                "could not inspect host path `{}` for filesystem aliases: {detail}",
                path.display()
            ),
            Self::PublicationOverlapsActiveMount { mount_id } => write!(
                formatter,
                "sandbox publication root overlaps active mount `{}`",
                mount_id.as_str()
            ),
            Self::BoundMountEscapesTrustedWorkspaceRoot => formatter.write_str(
                "bound mount root escapes its trusted workspace root through a filesystem alias",
            ),
        }
    }
}

impl std::error::Error for WorkspaceHostBindingError {}

/// Shared, mutation-free host binding contract for CLI and Desktop owners.
///
/// The resolver never sanitizes, suffixes, reparents, renames, or moves a
/// legacy root. A coordinator may persist only `layout1_bindings`; every
/// `layout0_mount` must continue using its exact legacy root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceHostBindingResolver {
    platform: WorkspaceHostPlatform,
}

impl WorkspaceHostBindingResolver {
    pub fn new(platform: WorkspaceHostPlatform) -> Self {
        Self { platform }
    }

    pub fn current() -> Self {
        Self::new(WorkspaceHostPlatform::current())
    }

    pub fn plan_legacy_migration(
        &self,
        trusted_workspace_root: &Path,
        mounts: &[LegacyWorkspaceMount],
    ) -> Result<WorkspaceBindingMigrationPlan, WorkspaceHostBindingError> {
        self.plan_migration(trusted_workspace_root, None, mounts)
    }

    /// Plan a coordinator-owned layout-1 binding against one persisted host
    /// workspace identity. The plan remains mutation free; callers must commit
    /// its host binding and accepted mount bindings atomically.
    pub fn plan_workspace_migration(
        &self,
        host_binding: WorkspaceHostBinding,
        mounts: &[LegacyWorkspaceMount],
    ) -> Result<WorkspaceBindingMigrationPlan, WorkspaceHostBindingError> {
        ParsedHostPath::parse(self.platform, host_binding.trusted_workspace_root())
            .ok_or(WorkspaceHostBindingError::InvalidTrustedWorkspaceRoot)?;
        let trusted_workspace_root = host_binding.trusted_workspace_root().to_path_buf();
        self.plan_migration(&trusted_workspace_root, Some(host_binding), mounts)
    }

    /// Revalidate one persistent mount binding at the commit boundary.
    ///
    /// The lexical check is platform-table-testable. On the running host the
    /// resolver additionally canonicalizes existing symlinks, junctions, and
    /// reparse points (while preserving a missing tail) and requires the
    /// canonical mount to remain the same named direct child of the canonical
    /// trusted root.
    pub fn validate_persistent_mount_root(
        &self,
        host_binding: &WorkspaceHostBinding,
        mount_root: &Path,
        mount_target: &MountTarget,
    ) -> Result<(), WorkspaceHostBindingError> {
        let trusted = ParsedHostPath::parse(self.platform, host_binding.trusted_workspace_root())
            .ok_or(WorkspaceHostBindingError::InvalidTrustedWorkspaceRoot)?;
        let mount = ParsedHostPath::parse(self.platform, mount_root)
            .ok_or(WorkspaceHostBindingError::InvalidBoundMountRoot)?;
        let Some(component) = mount.direct_child_of(self.platform, &trusted) else {
            return Err(WorkspaceHostBindingError::InvalidBoundMountRoot);
        };
        if !path_token_eq(self.platform, component, mount_target.as_str()) {
            return Err(WorkspaceHostBindingError::InvalidBoundMountRoot);
        }

        if self.platform != WorkspaceHostPlatform::current() {
            return Ok(());
        }
        let trusted_aliases = HostFilesystemAliases::inspect(host_binding.trusted_workspace_root())
            .map_err(|source| WorkspaceHostBindingError::HostPathInspection {
                path: host_binding.trusted_workspace_root().to_path_buf(),
                detail: source.to_string(),
            })?;
        let mount_aliases = HostFilesystemAliases::inspect(mount_root).map_err(|source| {
            WorkspaceHostBindingError::HostPathInspection {
                path: mount_root.to_path_buf(),
                detail: source.to_string(),
            }
        })?;
        let Some(canonical_trusted) =
            ParsedHostPath::parse(self.platform, &trusted_aliases.canonical)
        else {
            return Err(WorkspaceHostBindingError::InvalidTrustedWorkspaceRoot);
        };
        let Some(canonical_mount) = ParsedHostPath::parse(self.platform, &mount_aliases.canonical)
        else {
            return Err(WorkspaceHostBindingError::InvalidBoundMountRoot);
        };
        let Some(canonical_component) =
            canonical_mount.direct_child_of(self.platform, &canonical_trusted)
        else {
            return Err(WorkspaceHostBindingError::BoundMountEscapesTrustedWorkspaceRoot);
        };
        if !path_token_eq(self.platform, canonical_component, mount_target.as_str()) {
            return Err(WorkspaceHostBindingError::BoundMountEscapesTrustedWorkspaceRoot);
        }
        Ok(())
    }

    fn plan_migration(
        &self,
        trusted_workspace_root: &Path,
        host_binding: Option<WorkspaceHostBinding>,
        mounts: &[LegacyWorkspaceMount],
    ) -> Result<WorkspaceBindingMigrationPlan, WorkspaceHostBindingError> {
        let trusted = ParsedHostPath::parse(self.platform, trusted_workspace_root)
            .ok_or(WorkspaceHostBindingError::InvalidTrustedWorkspaceRoot)?;
        let mut mounts = mounts.to_vec();
        mounts.sort_by(|left, right| left.mount_id.cmp(&right.mount_id));

        let mut candidates = Vec::with_capacity(mounts.len());
        let mut collision_groups = BTreeMap::<String, Vec<usize>>::new();
        for mount in mounts {
            let outcome = match raw_mount_target(self.platform, &mount.root) {
                Err(reason) => Err(reason),
                Ok(target) => match ParsedHostPath::parse(self.platform, &mount.root) {
                    None => Err(LegacyLayout0Reason::InvalidHostPath),
                    Some(root) => match root.direct_child_of(self.platform, &trusted) {
                        None => Err(LegacyLayout0Reason::OutsideTrustedWorkspaceRoot),
                        Some(_) => Ok(target),
                    },
                },
            };
            let index = candidates.len();
            if let Ok(target) = &outcome {
                collision_groups
                    .entry(target.collision_key())
                    .or_default()
                    .push(index);
            }
            candidates.push((mount, outcome));
        }

        for indexes in collision_groups
            .values()
            .filter(|indexes| indexes.len() > 1)
        {
            for index in indexes {
                candidates[*index].1 = Err(LegacyLayout0Reason::MountTargetCollision);
            }
        }

        let mut layout1_bindings = Vec::new();
        let mut layout0_mounts = Vec::new();
        for (mount, outcome) in candidates {
            match outcome {
                Ok(target) => {
                    let binding = match &host_binding {
                        Some(host_binding) => WorkspaceBinding::for_workspace(
                            host_binding.workspace_id().clone(),
                            target,
                        ),
                        None => WorkspaceBinding::new(target),
                    };
                    layout1_bindings.push(WorkspaceBindingRecord::new(mount.mount_id, binding));
                }
                Err(reason) => layout0_mounts.push(LegacyLayout0Mount {
                    mount_id: mount.mount_id,
                    root: mount.root,
                    reason,
                }),
            }
        }

        Ok(WorkspaceBindingMigrationPlan {
            workspace_root: trusted_workspace_root.to_path_buf(),
            host_binding,
            layout1_bindings,
            layout0_mounts,
        })
    }

    /// Resolve the historical sandbox `--root` as one whole ephemeral
    /// publication unit. No mount target is appended.
    pub fn resolve_ephemeral_publication_root(
        &self,
        requested_root: &Path,
        active_mounts: &[LegacyWorkspaceMount],
    ) -> Result<PathBuf, WorkspaceHostBindingError> {
        let requested = ParsedHostPath::parse(self.platform, requested_root)
            .ok_or(WorkspaceHostBindingError::InvalidPublicationRoot)?;
        for mount in active_mounts {
            let active = ParsedHostPath::parse(self.platform, &mount.root).ok_or_else(|| {
                WorkspaceHostBindingError::InvalidActiveMountRoot {
                    mount_id: mount.mount_id.clone(),
                }
            })?;
            if requested.overlaps(self.platform, &active) {
                return Err(WorkspaceHostBindingError::PublicationOverlapsActiveMount {
                    mount_id: mount.mount_id.clone(),
                });
            }
        }
        Ok(requested_root.to_path_buf())
    }

    /// Resolve an ephemeral root on the running host, including aliases that
    /// only the local filesystem can identify. The lexical platform contract
    /// remains available through [`Self::resolve_ephemeral_publication_root`]
    /// for migration planning and cross-platform tests.
    pub fn resolve_ephemeral_publication_root_on_current_host(
        &self,
        requested_root: &Path,
        active_mounts: &[LegacyWorkspaceMount],
    ) -> Result<PathBuf, WorkspaceHostBindingError> {
        let resolved = self.resolve_ephemeral_publication_root(requested_root, active_mounts)?;
        if self.platform != WorkspaceHostPlatform::current() {
            return Ok(resolved);
        }

        let requested_aliases =
            HostFilesystemAliases::inspect(requested_root).map_err(|source| {
                WorkspaceHostBindingError::HostPathInspection {
                    path: requested_root.to_path_buf(),
                    detail: source.to_string(),
                }
            })?;
        for mount in active_mounts {
            let active_aliases = HostFilesystemAliases::inspect(&mount.root).map_err(|source| {
                WorkspaceHostBindingError::HostPathInspection {
                    path: mount.root.clone(),
                    detail: source.to_string(),
                }
            })?;
            if requested_aliases.overlaps(self.platform, &active_aliases) {
                return Err(WorkspaceHostBindingError::PublicationOverlapsActiveMount {
                    mount_id: mount.mount_id.clone(),
                });
            }
        }
        Ok(resolved)
    }
}

fn raw_mount_target(
    platform: WorkspaceHostPlatform,
    root: &Path,
) -> Result<MountTarget, LegacyLayout0Reason> {
    let value = root.to_str().ok_or(LegacyLayout0Reason::InvalidHostPath)?;
    let leaf = match platform {
        WorkspaceHostPlatform::Macos | WorkspaceHostPlatform::Linux => root
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .ok_or(LegacyLayout0Reason::InvalidHostPath)?,
        WorkspaceHostPlatform::Windows => value
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|leaf| !leaf.is_empty())
            .ok_or(LegacyLayout0Reason::InvalidHostPath)?,
    };
    MountTarget::new(leaf.to_string()).map_err(|_| LegacyLayout0Reason::InvalidMountTarget)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedHostPath {
    prefix: String,
    components: Vec<String>,
}

impl ParsedHostPath {
    fn parse(platform: WorkspaceHostPlatform, path: &Path) -> Option<Self> {
        let value = path.to_str()?;
        match platform {
            WorkspaceHostPlatform::Macos | WorkspaceHostPlatform::Linux => {
                if !value.starts_with('/') {
                    return None;
                }
                let components = parse_components(platform, &value[1..], '/')?;
                Some(Self {
                    prefix: "/".to_string(),
                    components,
                })
            }
            WorkspaceHostPlatform::Windows => Self::parse_windows(value),
        }
    }

    fn parse_windows(value: &str) -> Option<Self> {
        let mut normalized = value.replace('\\', "/");
        if normalized
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("//?/UNC/"))
        {
            normalized = format!("//{}", &normalized[8..]);
        } else if let Some(rest) = normalized.strip_prefix("//?/") {
            normalized = rest.to_string();
        }
        if let Some(rest) = normalized.strip_prefix("//") {
            let mut parts = rest.split('/');
            let server = parts.next().filter(|part| !part.is_empty())?;
            let share = parts.next().filter(|part| !part.is_empty())?;
            let remainder = parts.collect::<Vec<_>>().join("/");
            let components = parse_components(WorkspaceHostPlatform::Windows, &remainder, '/')?;
            return Some(Self {
                prefix: format!("//{server}/{share}"),
                components,
            });
        }
        let bytes = normalized.as_bytes();
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || bytes[2] != b'/'
        {
            return None;
        }
        Some(Self {
            prefix: normalized[..2].to_string(),
            components: parse_components(WorkspaceHostPlatform::Windows, &normalized[3..], '/')?,
        })
    }

    fn direct_child_of<'a>(
        &'a self,
        platform: WorkspaceHostPlatform,
        parent: &Self,
    ) -> Option<&'a str> {
        if self.components.len() != parent.components.len() + 1
            || !path_token_eq(platform, &self.prefix, &parent.prefix)
            || !self
                .components
                .iter()
                .zip(&parent.components)
                .all(|(left, right)| path_token_eq(platform, left, right))
        {
            return None;
        }
        self.components.last().map(String::as_str)
    }

    fn is_ancestor_of(&self, platform: WorkspaceHostPlatform, other: &Self) -> bool {
        self.components.len() <= other.components.len()
            && path_token_eq(platform, &self.prefix, &other.prefix)
            && self
                .components
                .iter()
                .zip(&other.components)
                .all(|(left, right)| path_token_eq(platform, left, right))
    }

    fn overlaps(&self, platform: WorkspaceHostPlatform, other: &Self) -> bool {
        self.is_ancestor_of(platform, other) || other.is_ancestor_of(platform, self)
    }
}

fn parse_components(
    platform: WorkspaceHostPlatform,
    value: &str,
    separator: char,
) -> Option<Vec<String>> {
    let mut components = Vec::new();
    for component in value.split(separator) {
        match component {
            "" | "." => {}
            ".." => return None,
            component => {
                let component = if platform == WorkspaceHostPlatform::Windows {
                    component.trim_end_matches(['.', ' '])
                } else {
                    component
                };
                if component.is_empty() {
                    return None;
                }
                components.push(component.to_string());
            }
        }
    }
    Some(components)
}

#[derive(Clone, Debug)]
struct HostFilesystemAliases {
    canonical: PathBuf,
    #[cfg(any(unix, windows))]
    native_anchors: Vec<NativePathAnchor>,
}

impl HostFilesystemAliases {
    fn inspect(path: &Path) -> io::Result<Self> {
        let canonical = canonicalize_with_missing_tail(path)?;
        #[cfg(unix)]
        let native_anchors = unix_native_anchors(path)?;
        #[cfg(windows)]
        let native_anchors = windows_native_anchors(path)?;
        Ok(Self {
            canonical,
            #[cfg(any(unix, windows))]
            native_anchors,
        })
    }

    fn overlaps(&self, platform: WorkspaceHostPlatform, other: &Self) -> bool {
        if let (Some(left), Some(right)) = (
            ParsedHostPath::parse(platform, &self.canonical),
            ParsedHostPath::parse(platform, &other.canonical),
        ) && left.overlaps(platform, &right)
        {
            return true;
        }

        #[cfg(any(unix, windows))]
        if self.native_anchors.iter().any(|left| {
            other.native_anchors.iter().any(|right| {
                left.identity == right.identity
                    && relative_components_overlap(platform, &left.suffix, &right.suffix)
            })
        }) {
            return true;
        }

        false
    }
}

/// Returns whether two host spellings identify the same filesystem path under
/// the selected platform's path rules. Existing objects use canonical and,
/// on Unix, device/inode anchors; missing tails use normalized canonical
/// components. This is intentionally equality, not ancestor overlap.
pub fn host_paths_equivalent(platform: WorkspaceHostPlatform, left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    // Case folding is useful for collision planning, but it cannot prove host
    // identity: APFS can be case-sensitive and Windows path matching does not
    // implement Unicode default case folding. Off-host comparisons therefore
    // fail closed unless parsing proves the same exact normalized spelling.
    if platform != WorkspaceHostPlatform::current() {
        return matches!(
            (
                ParsedHostPath::parse(platform, left),
                ParsedHostPath::parse(platform, right),
            ),
            (Some(left), Some(right))
                if left.prefix == right.prefix && left.components == right.components
        );
    }

    let (Ok(left), Ok(right)) = (
        HostFilesystemAliases::inspect(left),
        HostFilesystemAliases::inspect(right),
    ) else {
        return false;
    };
    if left.canonical == right.canonical {
        return true;
    }
    #[cfg(any(unix, windows))]
    if left.native_anchors.iter().any(|left| {
        right.native_anchors.iter().any(|right| {
            left.identity == right.identity
                && left.suffix.len() == right.suffix.len()
                && left
                    .suffix
                    .iter()
                    .zip(&right.suffix)
                    .all(|(left, right)| left == right)
        })
    }) {
        return true;
    }
    false
}

fn canonicalize_with_missing_tail(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::<OsString>::new();
    let mut candidate = path;
    loop {
        match fs::canonicalize(candidate) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = candidate.file_name() else {
                    return Err(error);
                };
                missing.push(name.to_os_string());
                candidate = candidate.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(any(unix, windows))]
#[derive(Clone, Debug)]
struct NativePathAnchor {
    identity: NativePathIdentity,
    suffix: Vec<OsString>,
}

#[cfg(unix)]
type NativePathIdentity = (u64, u64);

#[cfg(windows)]
type NativePathIdentity = (u64, u64, u64);

#[cfg(unix)]
fn unix_native_anchors(path: &Path) -> io::Result<Vec<NativePathAnchor>> {
    let prefixes = path.ancestors().collect::<Vec<_>>();
    let mut anchors = Vec::new();
    for (index, prefix) in prefixes.iter().enumerate() {
        match fs::metadata(prefix) {
            Ok(metadata) => {
                let suffix = prefixes[..index]
                    .iter()
                    .rev()
                    .filter_map(|path| path.file_name().map(OsString::from))
                    .collect();
                anchors.push(NativePathAnchor {
                    identity: (metadata.dev(), metadata.ino()),
                    suffix,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(anchors)
}

#[cfg(windows)]
fn windows_native_anchors(path: &Path) -> io::Result<Vec<NativePathAnchor>> {
    let prefixes = path.ancestors().collect::<Vec<_>>();
    let mut anchors = Vec::new();
    for (index, prefix) in prefixes.iter().enumerate() {
        match windows_path_identity(prefix) {
            Ok(identity) => {
                let suffix = prefixes[..index]
                    .iter()
                    .rev()
                    .filter_map(|path| path.file_name().map(OsString::from))
                    .collect();
                anchors.push(NativePathAnchor { identity, suffix });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(anchors)
}

#[cfg(windows)]
fn windows_path_identity(path: &Path) -> io::Result<NativePathIdentity> {
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the handle is live and the output pointer/length describe one
    // initialized FILE_ID_INFO value.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let bytes = info.FileId.Identifier;
    Ok((
        info.VolumeSerialNumber,
        u64::from_le_bytes(bytes[..8].try_into().expect("fixed file ID")),
        u64::from_le_bytes(bytes[8..].try_into().expect("fixed file ID")),
    ))
}

#[cfg(unix)]
fn relative_components_overlap(
    platform: WorkspaceHostPlatform,
    left: &[OsString],
    right: &[OsString],
) -> bool {
    let is_prefix = |prefix: &[OsString], full: &[OsString]| {
        prefix.len() <= full.len()
            && prefix.iter().zip(full).all(|(left, right)| {
                let (Some(left), Some(right)) = (left.to_str(), right.to_str()) else {
                    return left == right;
                };
                path_token_eq(platform, left, right)
            })
    };
    is_prefix(left, right) || is_prefix(right, left)
}

#[cfg(unix)]
fn native_direct_child_component(
    platform: WorkspaceHostPlatform,
    trusted: &HostFilesystemAliases,
    mount: &HostFilesystemAliases,
) -> Option<String> {
    mount.native_anchors.iter().find_map(|mount_anchor| {
        trusted.native_anchors.iter().find_map(|trusted_anchor| {
            if mount_anchor.identity != trusted_anchor.identity
                || mount_anchor.suffix.len() != trusted_anchor.suffix.len() + 1
            {
                return None;
            }
            let same_prefix = trusted_anchor.suffix.iter().zip(&mount_anchor.suffix).all(
                |(trusted, mount)| match (trusted.to_str(), mount.to_str()) {
                    (Some(trusted), Some(mount)) => path_token_eq(platform, trusted, mount),
                    _ => trusted == mount,
                },
            );
            same_prefix
                .then(|| mount_anchor.suffix.last()?.to_str().map(str::to_string))
                .flatten()
        })
    })
}

fn path_token_eq(platform: WorkspaceHostPlatform, left: &str, right: &str) -> bool {
    match platform {
        WorkspaceHostPlatform::Macos | WorkspaceHostPlatform::Windows => {
            host_case_key(left) == host_case_key(right)
        }
        WorkspaceHostPlatform::Linux => left == right,
    }
}

fn host_case_key(value: &str) -> String {
    value.chars().default_case_fold().nfc().collect()
}

pub(crate) fn legacy_mount_collision_key(root: &Path) -> Option<String> {
    let target = root.file_name()?.to_str()?;
    let normalized = target.chars().nfc().collect::<String>();
    MountTarget::new(normalized)
        .ok()
        .map(|target| target.collision_key())
}

pub(crate) fn legacy_mount_collision_key_for_host(
    host_binding: &WorkspaceHostBinding,
    root: &Path,
) -> Option<String> {
    let resolver = WorkspaceHostBindingResolver::current();
    if resolver.platform == WorkspaceHostPlatform::current() {
        let trusted = HostFilesystemAliases::inspect(host_binding.trusted_workspace_root()).ok()?;
        let mount = HostFilesystemAliases::inspect(root).ok()?;
        let canonical_component = ParsedHostPath::parse(resolver.platform, &trusted.canonical)
            .and_then(|trusted| {
                ParsedHostPath::parse(resolver.platform, &mount.canonical).and_then(|mount| {
                    mount
                        .direct_child_of(resolver.platform, &trusted)
                        .map(str::to_string)
                })
            });
        #[cfg(unix)]
        let component = canonical_component
            .or_else(|| native_direct_child_component(resolver.platform, &trusted, &mount))?;
        #[cfg(not(unix))]
        let component = canonical_component?;
        let normalized = component.chars().nfc().collect::<String>();
        return MountTarget::new(normalized)
            .ok()
            .map(|target| target.collision_key());
    }

    let target = root.file_name()?.to_str()?;
    let normalized = target.chars().nfc().collect::<String>();
    let target = MountTarget::new(normalized).ok()?;
    resolver
        .validate_persistent_mount_root(host_binding, root, &target)
        .ok()?;
    Some(target.collision_key())
}

#[cfg(test)]
mod tests {
    use super::{
        LEGACY_WORKSPACE_BINDING_VERSION, LegacyLayout0Reason, LegacyWorkspaceMount,
        WORKSPACE_BINDING_LAYOUT_VERSION, WORKSPACE_BINDING_VERSION, WorkspaceBinding,
        WorkspaceHostBinding, WorkspaceHostBindingResolver, WorkspaceHostPlatform, WorkspaceId,
        WorkspaceProjectionIdentity,
    };
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use locality_core::model::MountId;
    use locality_core::portable::LogicalPath;
    use locality_core::workspace_layout::MountTarget;

    #[test]
    fn legacy_binding_json_has_no_host_path_and_v2_requires_workspace_identity() {
        let binding = WorkspaceBinding::new(MountTarget::new("notion-main").expect("target"));
        assert_eq!(
            serde_json::to_string(&binding).expect("serialize binding"),
            r#"{"binding_version":1,"layout_version":1,"mount_target":"notion-main"}"#
        );
        assert_eq!(binding.binding_version(), LEGACY_WORKSPACE_BINDING_VERSION);
        assert_eq!(binding.layout_version(), WORKSPACE_BINDING_LAYOUT_VERSION);
        assert!(
            serde_json::from_str::<WorkspaceBinding>(
                r#"{"binding_version":2,"layout_version":1,"mount_target":"notion-main"}"#
            )
            .expect_err("v2 identity is required")
            .to_string()
            .contains("missing")
        );
        assert!(
            serde_json::from_str::<WorkspaceBinding>(
                r#"{"binding_version":1,"layout_version":2,"mount_target":"notion-main"}"#
            )
            .expect_err("new layout version")
            .to_string()
            .contains("unsupported")
        );
    }

    #[test]
    fn v2_binding_separates_portable_target_from_trusted_host_placement() {
        let workspace_id = WorkspaceId::new("locality.workspace.linux_fuse").expect("workspace");
        let host = WorkspaceHostBinding::new(
            WorkspaceHostPlatform::Linux,
            workspace_id.clone(),
            "/home/alice/Locality",
            WorkspaceProjectionIdentity::new("linux-fuse:locality-shared-root")
                .expect("projection identity"),
            7,
        )
        .expect("host binding");
        let binding = WorkspaceBinding::for_workspace(
            workspace_id,
            MountTarget::new("notion-main").expect("target"),
        );

        assert_eq!(binding.binding_version(), WORKSPACE_BINDING_VERSION);
        assert_eq!(host.layout_sequence(), 7);
        assert_eq!(
            host.mount_root(binding.mount_target()),
            Path::new("/home/alice/Locality/notion-main")
        );
        assert_eq!(
            serde_json::from_str::<WorkspaceBinding>(
                &serde_json::to_string(&binding).expect("serialize")
            )
            .expect("deserialize"),
            binding
        );
    }

    #[test]
    fn host_roots_differ_while_target_and_logical_path_stay_stable() {
        let binding = WorkspaceBinding::new(MountTarget::new("notion-main").expect("target"));
        let logical = LogicalPath::new("Engineering/Roadmap/page.md").expect("logical path");

        assert_eq!(
            binding.projected_path(
                Path::new("/Users/alice/Library/CloudStorage/Locality"),
                &logical
            ),
            Path::new(
                "/Users/alice/Library/CloudStorage/Locality/notion-main/Engineering/Roadmap/page.md"
            )
        );
        assert_eq!(
            binding.projected_path(Path::new("/home/alice/Locality"), &logical),
            Path::new("/home/alice/Locality/notion-main/Engineering/Roadmap/page.md")
        );
    }

    #[test]
    fn persistent_mount_validation_has_windows_safe_lexical_contract() {
        let workspace_id = WorkspaceId::new("locality.workspace.windows").expect("workspace");
        let host = WorkspaceHostBinding::new(
            WorkspaceHostPlatform::Windows,
            workspace_id,
            r"C:\Locality",
            WorkspaceProjectionIdentity::new(
                "windows-cloud-files:codeflash.ai.loc!default!locality",
            )
            .expect("projection identity"),
            1,
        )
        .expect("host binding");
        let target = MountTarget::new("notion-main").expect("target");
        let resolver = WorkspaceHostBindingResolver::new(WorkspaceHostPlatform::Windows);

        resolver
            .validate_persistent_mount_root(&host, Path::new(r"c:\LOCALITY\Notion-Main"), &target)
            .expect("Windows spelling resolves to the same direct child");
        assert_eq!(
            resolver.validate_persistent_mount_root(
                &host,
                Path::new(r"C:\Locality\notion-main\..\outside"),
                &target,
            ),
            Err(super::WorkspaceHostBindingError::InvalidBoundMountRoot)
        );
        assert_eq!(
            resolver.validate_persistent_mount_root(
                &host,
                Path::new(r"C:\Locality\other"),
                &target,
            ),
            Err(super::WorkspaceHostBindingError::InvalidBoundMountRoot)
        );
    }

    #[test]
    fn lexical_case_folding_is_not_host_identity_proof() {
        assert!(!super::host_paths_equivalent(
            WorkspaceHostPlatform::Windows,
            Path::new(r"C:\Locality\Straße"),
            Path::new(r"c:/LOCALITY/STRASSE"),
        ));
        assert!(!super::host_paths_equivalent(
            WorkspaceHostPlatform::Linux,
            Path::new("/srv/Locality/notion-main"),
            Path::new("/srv/locality/notion-main"),
        ));
    }

    fn unique_host_equivalence_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "locality-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn current_host_equivalence_follows_native_identity_not_casefolding() {
        let root = unique_host_equivalence_root("host-path-identity");
        std::fs::create_dir_all(&root).expect("create identity root");
        let mixed = root.join("Straße");
        let folded = root.join("STRASSE");
        std::fs::create_dir(&mixed).expect("create mixed-case directory");
        match std::fs::create_dir(&folded) {
            Ok(()) => assert!(
                !super::host_paths_equivalent(WorkspaceHostPlatform::current(), &mixed, &folded,),
                "distinct native objects must not become equivalent through Unicode case folding"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => assert!(
                super::host_paths_equivalent(WorkspaceHostPlatform::current(), &mixed, &folded,),
                "a case-insensitive host must prove both spellings resolve to the same object"
            ),
            Err(error) => panic!("create folded directory: {error}"),
        }

        std::fs::remove_dir_all(root).expect("remove identity root");
    }

    #[test]
    fn legacy_invalid_target_remains_layout_zero_without_rewriting() {
        let mount =
            LegacyWorkspaceMount::new(MountId::new("notion-production"), "/tmp/Locality/trailing.");
        let plan = WorkspaceHostBindingResolver::new(WorkspaceHostPlatform::Linux)
            .plan_legacy_migration(Path::new("/tmp/Locality"), std::slice::from_ref(&mount))
            .expect("migration plan");

        assert!(plan.layout1_bindings().is_empty());
        assert_eq!(plan.layout0_mounts()[0].root, mount.root);
        assert_eq!(
            plan.layout0_mounts()[0].reason,
            LegacyLayout0Reason::InvalidMountTarget
        );
    }

    #[test]
    fn windows_plan_validates_raw_leaf_before_alias_normalization() {
        let mount =
            LegacyWorkspaceMount::new(MountId::new("notion-production"), r"C:\Locality\trailing.");
        let plan = WorkspaceHostBindingResolver::new(WorkspaceHostPlatform::Windows)
            .plan_legacy_migration(Path::new(r"C:\Locality"), std::slice::from_ref(&mount))
            .expect("Windows migration plan");

        assert!(plan.layout1_bindings().is_empty());
        assert_eq!(plan.layout0_mounts()[0].root, mount.root);
        assert_eq!(
            plan.layout0_mounts()[0].reason,
            LegacyLayout0Reason::InvalidMountTarget
        );
    }

    #[test]
    fn unicode_collisions_all_remain_layout_zero_without_suffixes() {
        let mounts = [
            LegacyWorkspaceMount::new(MountId::new("first"), "/tmp/Locality/Straße"),
            LegacyWorkspaceMount::new(MountId::new("second"), "/tmp/Locality/STRASSE"),
        ];
        let plan = WorkspaceHostBindingResolver::new(WorkspaceHostPlatform::Linux)
            .plan_legacy_migration(Path::new("/tmp/Locality"), &mounts)
            .expect("migration plan");

        assert!(plan.layout1_bindings().is_empty());
        assert_eq!(plan.layout0_mounts().len(), 2);
        assert!(plan.layout0_mounts().iter().all(|mount| {
            mount.reason == LegacyLayout0Reason::MountTargetCollision
                && (mount.root.ends_with("Straße") || mount.root.ends_with("STRASSE"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn physical_identity_anchor_reserves_direct_child_when_canonical_paths_differ() {
        use std::ffi::OsString;
        use std::path::PathBuf;

        let trusted = super::HostFilesystemAliases {
            canonical: PathBuf::from("/canonical/workspace"),
            native_anchors: vec![super::NativePathAnchor {
                identity: (41, 73),
                suffix: Vec::new(),
            }],
        };
        let mount = super::HostFilesystemAliases {
            canonical: PathBuf::from("/bind-alias/not-canonicalized"),
            native_anchors: vec![super::NativePathAnchor {
                identity: (41, 73),
                suffix: vec![OsString::from("canonical-target")],
            }],
        };

        let component = super::native_direct_child_component(
            WorkspaceHostPlatform::current(),
            &trusted,
            &mount,
        )
        .expect("same physical workspace anchor reserves its direct child");
        assert_eq!(
            MountTarget::new(component).expect("target").collision_key(),
            MountTarget::new("canonical-target")
                .expect("expected target")
                .collision_key()
        );
    }
}
