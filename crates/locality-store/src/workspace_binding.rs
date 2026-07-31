//! Portable workspace placement metadata shared by Desktop, CLI, and the daemon.
//!
//! A binding deliberately excludes the host workspace root. The root is local
//! placement selected by the user; the binding is the portable rule that maps
//! a stable mount identity and logical path below whichever root is active on
//! this host.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

use locality_core::model::MountId;
use locality_core::portable::LogicalPath;
use locality_core::workspace_layout::MountTarget;
use serde::{Deserialize, Deserializer, Serialize};

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

impl WorkspaceBindingRecord {
    pub fn new(mount_id: MountId, binding: WorkspaceBinding) -> Self {
        Self { mount_id, binding }
    }
}

pub(crate) fn binding_from_legacy_mount(mount_id: &MountId, root: &Path) -> WorkspaceBinding {
    let target = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| MountTarget::new(name.to_string()).ok())
        .unwrap_or_else(|| fallback_mount_target(mount_id));
    WorkspaceBinding::new(target)
}

pub(crate) fn unique_binding(
    preferred: WorkspaceBinding,
    used_collision_keys: &BTreeSet<String>,
) -> WorkspaceBinding {
    if !used_collision_keys.contains(&preferred.collision_key()) {
        return preferred;
    }

    let base = preferred.mount_target().as_str();
    for suffix_number in 2_u64.. {
        let suffix = format!("-{suffix_number}");
        let mut prefix = base;
        while prefix.len() + suffix.len() > MountTarget::MAX_UTF8_BYTES
            || prefix.encode_utf16().count() + suffix.len() > MountTarget::MAX_UTF16_UNITS
        {
            let Some((index, _)) = prefix.char_indices().next_back() else {
                break;
            };
            prefix = &prefix[..index];
        }
        let Ok(target) = MountTarget::new(format!("{prefix}{suffix}")) else {
            continue;
        };
        let candidate = WorkspaceBinding::new(target);
        if !used_collision_keys.contains(&candidate.collision_key()) {
            return candidate;
        }
    }
    unreachable!("u64 mount-target suffix space is inexhaustible")
}

fn fallback_mount_target(mount_id: &MountId) -> MountTarget {
    let mut slug = String::new();
    let mut previous_separator = false;
    for character in mount_id.as_str().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator && !slug.is_empty() {
            slug.push('-');
            previous_separator = true;
        }
        if slug.len() >= 100 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("source");
    }
    MountTarget::new(format!("mount-{slug}"))
        .expect("ASCII legacy mount fallback is always a valid target")
}

#[cfg(test)]
mod tests {
    use super::{
        WORKSPACE_BINDING_LAYOUT_VERSION, WORKSPACE_BINDING_VERSION, WorkspaceBinding,
        binding_from_legacy_mount, unique_binding,
    };
    use std::collections::BTreeSet;
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
    fn legacy_invalid_target_falls_back_without_using_absolute_path() {
        let binding = binding_from_legacy_mount(
            &MountId::new("Notion / Production"),
            Path::new("/tmp/Locality/.."),
        );
        assert_eq!(binding.mount_target().as_str(), "mount-notion-production");
    }

    #[test]
    fn unicode_collisions_receive_deterministic_suffixes() {
        let first = WorkspaceBinding::new(MountTarget::new("Straße").expect("target"));
        let used = BTreeSet::from([first.collision_key()]);
        let second = unique_binding(
            WorkspaceBinding::new(MountTarget::new("STRASSE").expect("target")),
            &used,
        );
        assert_eq!(second.mount_target().as_str(), "STRASSE-2");
        assert_ne!(first.collision_key(), second.collision_key());
    }
}
