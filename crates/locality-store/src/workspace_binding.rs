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

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use caseless::Caseless;
use locality_core::model::MountId;
use locality_core::portable::LogicalPath;
use locality_core::workspace_layout::MountTarget;
use serde::{Deserialize, Deserializer, Serialize};
use unicode_normalization_v16::UnicodeNormalization;

pub const WORKSPACE_BINDING_VERSION: u16 = 1;
pub const WORKSPACE_BINDING_LAYOUT_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBinding {
    binding_version: u16,
    layout_version: u16,
    mount_target: MountTarget,
}

impl WorkspaceBinding {
    pub fn new(mount_target: MountTarget) -> Self {
        Self {
            binding_version: WORKSPACE_BINDING_VERSION,
            layout_version: WORKSPACE_BINDING_LAYOUT_VERSION,
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
    mount_target: MountTarget,
}

impl<'de> Deserialize<'de> for WorkspaceBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceBindingWire::deserialize(deserializer)?;
        if wire.binding_version != WORKSPACE_BINDING_VERSION {
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
        Ok(Self::new(wire.mount_target))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceBindingError {
    UnsupportedBindingVersion { actual: u16 },
    UnsupportedLayoutVersion { actual: u16 },
}

impl Display for WorkspaceBindingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedBindingVersion { actual } => {
                write!(
                    formatter,
                    "workspace binding version {actual} is unsupported"
                )
            }
            Self::UnsupportedLayoutVersion { actual } => {
                write!(
                    formatter,
                    "workspace layout version {actual} is unsupported"
                )
            }
        }
    }
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

    pub fn layout0_mounts(&self) -> &[LegacyLayout0Mount] {
        &self.layout0_mounts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceHostBindingError {
    InvalidTrustedWorkspaceRoot,
    InvalidPublicationRoot,
    InvalidActiveMountRoot { mount_id: MountId },
    HostPathInspection { path: PathBuf, detail: String },
    PublicationOverlapsActiveMount { mount_id: MountId },
}

impl Display for WorkspaceHostBindingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTrustedWorkspaceRoot => formatter.write_str(
                "trusted workspace root must be an absolute host path without parent traversal",
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
        let trusted = ParsedHostPath::parse(self.platform, trusted_workspace_root)
            .ok_or(WorkspaceHostBindingError::InvalidTrustedWorkspaceRoot)?;
        let mut mounts = mounts.to_vec();
        mounts.sort_by(|left, right| left.mount_id.cmp(&right.mount_id));

        let mut candidates = Vec::with_capacity(mounts.len());
        let mut collision_groups = BTreeMap::<String, Vec<usize>>::new();
        for mount in mounts {
            let outcome = match ParsedHostPath::parse(self.platform, &mount.root) {
                None => Err(LegacyLayout0Reason::InvalidHostPath),
                Some(root) => match root.direct_child_of(self.platform, &trusted) {
                    None => Err(LegacyLayout0Reason::OutsideTrustedWorkspaceRoot),
                    Some(component) => MountTarget::new(component.to_string())
                        .map_err(|_| LegacyLayout0Reason::InvalidMountTarget),
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
                Ok(target) => layout1_bindings.push(WorkspaceBindingRecord::new(
                    mount.mount_id,
                    WorkspaceBinding::new(target),
                )),
                Err(reason) => layout0_mounts.push(LegacyLayout0Mount {
                    mount_id: mount.mount_id,
                    root: mount.root,
                    reason,
                }),
            }
        }

        Ok(WorkspaceBindingMigrationPlan {
            workspace_root: trusted_workspace_root.to_path_buf(),
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

    pub(crate) fn same_host_path(&self, left: &Path, right: &Path) -> bool {
        match (
            ParsedHostPath::parse(self.platform, left),
            ParsedHostPath::parse(self.platform, right),
        ) {
            (Some(left), Some(right)) => left.equals(self.platform, &right),
            _ => false,
        }
    }
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

    fn equals(&self, platform: WorkspaceHostPlatform, other: &Self) -> bool {
        self.components.len() == other.components.len()
            && path_token_eq(platform, &self.prefix, &other.prefix)
            && self
                .components
                .iter()
                .zip(&other.components)
                .all(|(left, right)| path_token_eq(platform, left, right))
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
    #[cfg(unix)]
    native_anchors: Vec<NativePathAnchor>,
}

impl HostFilesystemAliases {
    fn inspect(path: &Path) -> io::Result<Self> {
        let canonical = canonicalize_with_missing_tail(path)?;
        #[cfg(unix)]
        let native_anchors = unix_native_anchors(path)?;
        Ok(Self {
            canonical,
            #[cfg(unix)]
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

        #[cfg(unix)]
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

#[cfg(unix)]
#[derive(Clone, Debug)]
struct NativePathAnchor {
    identity: (u64, u64),
    suffix: Vec<OsString>,
}

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

pub(crate) fn plan_legacy_migration_from_common_parent(
    mounts: &[LegacyWorkspaceMount],
) -> Option<WorkspaceBindingMigrationPlan> {
    let resolver = WorkspaceHostBindingResolver::current();
    let workspace_root = mounts.first()?.root.parent()?.to_path_buf();
    if mounts.iter().any(|mount| match mount.root.parent() {
        Some(parent) => !resolver.same_host_path(parent, &workspace_root),
        None => true,
    }) {
        return None;
    }
    resolver.plan_legacy_migration(&workspace_root, mounts).ok()
}

pub(crate) fn legacy_mount_collision_key(root: &Path) -> Option<String> {
    let target = root.file_name()?.to_str()?;
    let normalized = target.chars().nfc().collect::<String>();
    MountTarget::new(normalized)
        .ok()
        .map(|target| target.collision_key())
}

#[cfg(test)]
mod tests {
    use super::{
        LegacyLayout0Reason, LegacyWorkspaceMount, WORKSPACE_BINDING_LAYOUT_VERSION,
        WORKSPACE_BINDING_VERSION, WorkspaceBinding, WorkspaceHostBindingResolver,
        WorkspaceHostPlatform,
    };
    use std::path::Path;

    use locality_core::model::MountId;
    use locality_core::portable::LogicalPath;
    use locality_core::workspace_layout::MountTarget;

    #[test]
    fn binding_json_has_no_host_path_and_rejects_newer_versions() {
        let binding = WorkspaceBinding::new(MountTarget::new("notion-main").expect("target"));
        assert_eq!(
            serde_json::to_string(&binding).expect("serialize binding"),
            r#"{"binding_version":1,"layout_version":1,"mount_target":"notion-main"}"#
        );
        assert_eq!(binding.binding_version(), WORKSPACE_BINDING_VERSION);
        assert_eq!(binding.layout_version(), WORKSPACE_BINDING_LAYOUT_VERSION);
        assert!(
            serde_json::from_str::<WorkspaceBinding>(
                r#"{"binding_version":2,"layout_version":1,"mount_target":"notion-main"}"#
            )
            .expect_err("new binding version")
            .to_string()
            .contains("unsupported")
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
}
