//! macOS File Provider compatibility aliases.
//!
//! The daemon-owned virtual filesystem contract lives in `virtual_fs`. macOS
//! File Provider, Linux FUSE, and future platform projections should bind to that
//! generic API instead of growing platform-specific daemon semantics.

use afs_core::AfsResult;
use afs_core::model::MountId;
#[cfg(target_os = "macos")]
use afs_store::ProjectionMode;
use afs_store::{
    EntityRepository, FreshnessStateRepository, MountConfig, MountRepository, ShadowRepository,
    VirtualMutationRepository,
};
use std::path::{Path, PathBuf};

use crate::hydration::HydrationSource;
use crate::virtual_fs;

pub use crate::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, VirtualFsChildrenReport as FileProviderChildrenReport,
    VirtualFsItem as FileProviderItem, VirtualFsItemKind as FileProviderItemKind,
    VirtualFsItemReport as FileProviderItemReport,
    VirtualFsMaterializeOutcome as FileProviderMaterializeOutcome,
    VirtualFsMaterializeReport as FileProviderMaterializeReport,
};

pub fn file_provider_item<S>(
    store: &S,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<FileProviderItemReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    virtual_fs::virtual_fs_item(store, mount_id, identifier)
}

pub fn file_provider_children<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> AfsResult<FileProviderChildrenReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    virtual_fs::virtual_fs_children(store, mount_id, container_identifier)
}

pub fn materialize_file_provider_item<S, Source>(
    store: &mut S,
    source: &Source,
    mount_id: &MountId,
    identifier: &str,
) -> AfsResult<FileProviderMaterializeReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
    Source: HydrationSource + ?Sized,
{
    virtual_fs::materialize_virtual_fs_item(store, source, mount_id, identifier)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountPathMatch {
    pub access_root: PathBuf,
    pub relative_path: PathBuf,
}

pub fn mount_access_roots(mount: &MountConfig) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let mut roots = vec![mount.root.clone()];
    #[cfg(not(target_os = "macos"))]
    let roots = vec![mount.root.clone()];

    #[cfg(target_os = "macos")]
    if mount.projection == ProjectionMode::MacosFileProvider {
        roots.extend(macos_file_provider_access_roots(mount));
    }

    dedupe_paths(roots)
}

pub fn match_mount_path(mount: &MountConfig, path: &Path) -> Option<MountPathMatch> {
    mount_access_roots(mount)
        .into_iter()
        .filter_map(|access_root| {
            relative_to_access_root(path, &access_root).map(|relative_path| MountPathMatch {
                access_root,
                relative_path,
            })
        })
        .max_by_key(|matched| matched.access_root.components().count())
}

pub fn find_mount_for_path<'a>(
    mounts: &'a [MountConfig],
    path: &Path,
) -> Option<(&'a MountConfig, MountPathMatch)> {
    mounts
        .iter()
        .filter_map(|mount| match_mount_path(mount, path).map(|matched| (mount, matched)))
        .max_by_key(|(_, matched)| matched.access_root.components().count())
}

fn relative_to_access_root(path: &Path, access_root: &Path) -> Option<PathBuf> {
    if let Ok(relative_path) = path.strip_prefix(access_root) {
        return Some(relative_path.to_path_buf());
    }

    let canonical_path = canonicalize_existing_prefix(path)?;
    let canonical_root = canonicalize_existing_prefix(access_root)?;
    canonical_path
        .strip_prefix(canonical_root)
        .map(Path::to_path_buf)
        .ok()
}

fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    let mut suffix = PathBuf::new();

    loop {
        if let Ok(canonical_current) = std::fs::canonicalize(current) {
            return Some(canonical_current.join(suffix));
        }

        let file_name = current.file_name()?;
        suffix = PathBuf::from(file_name).join(suffix);
        current = current.parent()?;
    }
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

#[cfg(target_os = "macos")]
fn macos_file_provider_access_roots(mount: &MountConfig) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let display_name = macos_file_provider_display_name(mount);
    let cloud_storage = home.join("Library").join("CloudStorage");
    vec![
        cloud_storage.join(macos_file_provider_directory_name(&display_name)),
        cloud_storage.join(format!("AgentFS-{display_name}")),
    ]
}

#[cfg(target_os = "macos")]
fn macos_file_provider_directory_name(display_name: &str) -> String {
    if display_name == "AFS" || display_name.starts_with("AFS-") {
        display_name.to_string()
    } else {
        format!("AFS-{display_name}")
    }
}

#[cfg(target_os = "macos")]
fn macos_file_provider_display_name(mount: &MountConfig) -> String {
    macos_file_provider_domain_path(&mount.root)
        .file_name()
        .and_then(|name| name.to_str())
        .map(strip_file_provider_directory_prefix)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| mount.mount_id.0.clone())
}

#[cfg(target_os = "macos")]
fn macos_file_provider_domain_path(root: &Path) -> &Path {
    let Some(parent) = root.parent() else {
        return root;
    };
    let Some(grandparent_name) = parent
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    else {
        return root;
    };
    if grandparent_name == "CloudStorage" {
        parent
    } else {
        root
    }
}

#[cfg(target_os = "macos")]
fn strip_file_provider_directory_prefix(name: &str) -> &str {
    name.strip_prefix("AgentFS-")
        .or_else(|| name.strip_prefix("AFS-"))
        .filter(|stripped| !stripped.is_empty())
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use afs_core::model::MountId;

    #[test]
    fn match_mount_path_resolves_relative_path_under_mount_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/AFS/Notion");
        let matched = match_mount_path(&mount, Path::new("/tmp/AFS/Notion/Page.md"))
            .expect("path matches mount");

        assert_eq!(matched.access_root, PathBuf::from("/tmp/AFS/Notion"));
        assert_eq!(matched.relative_path, PathBuf::from("Page.md"));
    }

    #[test]
    fn find_mount_for_path_prefers_longest_access_root() {
        let broad = MountConfig::new(MountId::new("broad"), "notion", "/tmp/AFS");
        let narrow = MountConfig::new(MountId::new("narrow"), "notion", "/tmp/AFS/Notion");
        let mounts = vec![broad, narrow];

        let (mount, matched) = find_mount_for_path(&mounts, Path::new("/tmp/AFS/Notion/Page.md"))
            .expect("path matches mount");

        assert_eq!(mount.mount_id, MountId::new("narrow"));
        assert_eq!(matched.relative_path, PathBuf::from("Page.md"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_access_roots_strip_cloudstorage_domain_prefix() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/Users/test/Library/CloudStorage/AFS/notion",
        )
        .projection(ProjectionMode::MacosFileProvider);
        assert_eq!(macos_file_provider_display_name(&mount), "AFS");
        let roots = mount_access_roots(&mount);
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("home");

        assert!(roots.contains(&PathBuf::from(
            "/Users/test/Library/CloudStorage/AFS/notion"
        )));
        assert!(roots.contains(&home.join("Library").join("CloudStorage").join("AFS")));
        assert!(
            roots.contains(
                &home
                    .join("Library")
                    .join("CloudStorage")
                    .join("AgentFS-AFS")
            )
        );
    }
}
