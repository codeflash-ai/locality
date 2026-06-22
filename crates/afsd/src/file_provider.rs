//! macOS File Provider compatibility aliases.
//!
//! The daemon-owned virtual filesystem contract lives in `virtual_fs`. macOS
//! File Provider, Linux FUSE, and future platform projections should bind to that
//! generic API instead of growing platform-specific daemon semantics.

use afs_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId};
use afs_core::shadow::ShadowDocument;
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, FreshnessStateRepository, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository, VirtualMutationRepository,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hydration::HydrationSource;
use crate::virtual_fs;
use crate::virtual_fs::source_root_directory_name;

pub use crate::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, VirtualFsChildrenReport as FileProviderChildrenReport,
    VirtualFsItem as FileProviderItem, VirtualFsItemKind as FileProviderItemKind,
    VirtualFsItemReport as FileProviderItemReport,
    VirtualFsMaterializeOutcome as FileProviderMaterializeOutcome,
    VirtualFsMaterializeReport as FileProviderMaterializeReport,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderReadReport {
    pub mount_id: String,
    pub identifier: String,
    pub remote_id: String,
    pub path: String,
    pub outcome: FileProviderMaterializeOutcome,
    pub hydration: HydrationState,
    pub item: FileProviderItem,
    pub contents_base64: String,
}

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
    let mut roots = vec![mount.root.clone()];

    if matches!(
        mount.projection,
        ProjectionMode::LinuxFuse | ProjectionMode::WindowsCloudFiles
    ) {
        roots.push(
            mount
                .root
                .join(source_root_directory_name(&mount.connector)),
        );
    }

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionReconcileReport {
    pub checked: usize,
    pub reconciled: usize,
    pub skipped_unchanged: usize,
}

/// Imports macOS File Provider replica edits that did not arrive through
/// `modifyItem`.
///
/// This is intentionally a narrow command-boundary fallback, not a background
/// scanner: it reads only an explicit target. The daemon cache remains the
/// durable source used by diff and push after this reconciliation step.
pub fn reconcile_macos_file_provider_projection<S>(
    store: &mut S,
    state_root: &Path,
    target: Option<&Path>,
) -> AfsResult<ProjectionReconcileReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    let Some(target) = target.map(absolute_reconcile_path).transpose()? else {
        return Ok(ProjectionReconcileReport::default());
    };
    let mounts = store.load_mounts().map_err(AfsError::from)?;
    let mut report = ProjectionReconcileReport::default();

    for mount in mounts {
        if mount.projection != ProjectionMode::MacosFileProvider {
            continue;
        }

        let Some(target_match) = match_mount_path(&mount, &target) else {
            continue;
        };

        let content_root = virtual_fs::virtual_fs_content_root(state_root, &mount.mount_id);
        let entities = scoped_page_entities(store, &mount, Some(&target_match))?;
        for entity in entities {
            let Some(candidate) =
                reconcile_candidate_path(&mount, &entity, Some(&target), Some(&target_match))
            else {
                continue;
            };

            match reconcile_projection_candidate(store, &mount, &entity, &content_root, candidate)?
            {
                ProjectionCandidateOutcome::Skipped => {}
                ProjectionCandidateOutcome::Unchanged => {
                    report.checked += 1;
                    report.skipped_unchanged += 1;
                }
                ProjectionCandidateOutcome::Reconciled => {
                    report.checked += 1;
                    report.reconciled += 1;
                }
            }
        }
    }

    Ok(report)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectionCandidate {
    path: PathBuf,
    force_read: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionCandidateOutcome {
    Skipped,
    Unchanged,
    Reconciled,
}

fn scoped_page_entities<S>(
    store: &S,
    mount: &MountConfig,
    target_match: Option<&MountPathMatch>,
) -> AfsResult<Vec<EntityRecord>>
where
    S: EntityRepository,
{
    let target_relative = target_match.map(|matched| matched.relative_path.as_path());
    Ok(store
        .list_entities(&mount.mount_id)
        .map_err(AfsError::from)?
        .into_iter()
        .filter(|entity| entity.kind == EntityKind::Page)
        .filter(|entity| match target_relative {
            None => true,
            Some(relative) if relative.as_os_str().is_empty() => true,
            Some(relative) => entity.path == relative || entity.path.starts_with(relative),
        })
        .collect())
}

fn reconcile_candidate_path(
    mount: &MountConfig,
    entity: &EntityRecord,
    target: Option<&Path>,
    target_match: Option<&MountPathMatch>,
) -> Option<ProjectionCandidate> {
    if let (Some(target), Some(target_match)) = (target, target_match)
        && target_match.relative_path == entity.path
        && target.is_file()
    {
        return Some(ProjectionCandidate {
            path: target.to_path_buf(),
            force_read: true,
        });
    }

    newest_existing_projection_path(mount, &entity.path).map(|path| ProjectionCandidate {
        path,
        force_read: false,
    })
}

fn newest_existing_projection_path(mount: &MountConfig, relative_path: &Path) -> Option<PathBuf> {
    source_projection_roots(mount)
        .into_iter()
        .filter_map(|root| {
            let path = root.join(relative_path);
            let metadata = std::fs::metadata(&path).ok()?;
            metadata
                .is_file()
                .then_some((path, metadata_modified(&metadata)))
        })
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)
}

fn source_projection_roots(mount: &MountConfig) -> Vec<PathBuf> {
    let source_dir = source_root_directory_name(&mount.connector);
    mount_access_roots(mount)
        .into_iter()
        .filter(|root| {
            root.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == source_dir.as_str())
        })
        .collect()
}

fn reconcile_projection_candidate<S>(
    store: &mut S,
    mount: &MountConfig,
    entity: &EntityRecord,
    content_root: &Path,
    candidate: ProjectionCandidate,
) -> AfsResult<ProjectionCandidateOutcome>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + FreshnessStateRepository,
{
    let content_path = content_cache_path(content_root, &entity.path)?;
    if !projection_needs_read(&candidate.path, &content_path, candidate.force_read) {
        return Ok(ProjectionCandidateOutcome::Skipped);
    }

    let projection_contents = std::fs::read_to_string(&candidate.path).map_err(AfsError::from)?;
    let commit_contents =
        projection_contents_for_existing_page(store, mount, entity, &projection_contents)?;

    if std::fs::read(&content_path).is_ok_and(|existing| existing == commit_contents) {
        return Ok(ProjectionCandidateOutcome::Unchanged);
    }

    virtual_fs::commit_virtual_fs_write(
        store,
        content_root,
        &mount.mount_id,
        &entity.remote_id.0,
        &commit_contents,
    )?;
    Ok(ProjectionCandidateOutcome::Reconciled)
}

fn projection_needs_read(projection_path: &Path, content_path: &Path, force_read: bool) -> bool {
    if force_read {
        return true;
    }

    let Ok(projection_metadata) = std::fs::metadata(projection_path) else {
        return false;
    };
    if !projection_metadata.is_file() {
        return false;
    }

    let Ok(content_metadata) = std::fs::metadata(content_path) else {
        return true;
    };

    metadata_modified(&projection_metadata) > metadata_modified(&content_metadata)
}

fn projection_contents_for_existing_page<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    contents: &str,
) -> AfsResult<Vec<u8>>
where
    S: ShadowRepository,
{
    let Ok(parsed) = parse_canonical_markdown(contents) else {
        return Ok(contents.as_bytes().to_vec());
    };
    if parsed.frontmatter.afs.is_some() {
        return Ok(contents.as_bytes().to_vec());
    }

    let shadow = store
        .load_shadow(&mount.mount_id, &entity.remote_id)
        .map_err(AfsError::from)?;
    let frontmatter = merge_identity_frontmatter(entity, &shadow, &parsed.document.frontmatter);
    Ok(
        render_canonical_markdown(&CanonicalDocument::new(frontmatter, parsed.document.body))
            .into_bytes(),
    )
}

fn merge_identity_frontmatter(
    entity: &EntityRecord,
    shadow: &ShadowDocument,
    visible_frontmatter: &str,
) -> String {
    let mut merged = afs_identity_frontmatter(entity, shadow);
    let visible = visible_frontmatter.trim_start_matches('\n');
    if !visible.trim().is_empty() {
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str(visible);
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
    }
    merged
}

fn afs_identity_frontmatter(entity: &EntityRecord, shadow: &ShadowDocument) -> String {
    let shadow_parsed = parse_canonical_markdown(&render_canonical_markdown(
        &CanonicalDocument::new(shadow.frontmatter.clone(), ""),
    ))
    .ok();
    let shadow_afs = shadow_parsed
        .as_ref()
        .and_then(|parsed| parsed.frontmatter.afs.as_ref());

    let id = shadow_afs
        .and_then(|afs| afs.id.as_ref())
        .unwrap_or(&entity.remote_id);
    let entity_type = shadow_afs
        .and_then(|afs| afs.raw_entity_type.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| entity_kind_frontmatter_name(&entity.kind));
    let synced_at = shadow_afs
        .and_then(|afs| afs.synced_at.as_deref())
        .or(entity.remote_edited_at.as_deref())
        .unwrap_or("unknown");
    let remote_edited_at = shadow_afs
        .and_then(|afs| afs.remote_edited_at.as_deref())
        .or(entity.remote_edited_at.as_deref())
        .unwrap_or("unknown");

    let mut frontmatter = String::new();
    frontmatter.push_str("afs:\n");
    frontmatter.push_str(&format!("  id: {}\n", yaml_quoted(&id.0)));
    frontmatter.push_str(&format!("  type: {}\n", yaml_quoted(&entity_type)));
    if let Some(parent) = shadow_afs.and_then(|afs| afs.parent.as_ref()) {
        frontmatter.push_str(&format!("  parent: {}\n", yaml_quoted(&parent.0)));
    }
    frontmatter.push_str(&format!("  synced_at: {}\n", yaml_quoted(synced_at)));
    frontmatter.push_str(&format!(
        "  remote_edited_at: {}\n",
        yaml_quoted(remote_edited_at)
    ));
    frontmatter
}

fn entity_kind_frontmatter_name(kind: &EntityKind) -> String {
    match kind {
        EntityKind::Page => "page".to_string(),
        EntityKind::Database => "database".to_string(),
        EntityKind::Directory => "directory".to_string(),
        EntityKind::Asset => "asset".to_string(),
        EntityKind::Unknown(value) => value.clone(),
    }
}

fn yaml_quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn metadata_modified(metadata: &std::fs::Metadata) -> SystemTime {
    metadata.modified().unwrap_or(UNIX_EPOCH)
}

fn content_cache_path(content_root: &Path, relative_path: &Path) -> AfsResult<PathBuf> {
    let mut path = content_root.to_path_buf();
    for component in relative_path.components() {
        match component {
            std::path::Component::Normal(part) => path.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(AfsError::InvalidState(format!(
                    "virtual content path `{}` escapes the mount root",
                    relative_path.display()
                )));
            }
        }
    }
    Ok(path)
}

fn absolute_reconcile_path(path: &Path) -> AfsResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(AfsError::from)
    }
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
    let domain_roots = vec![
        cloud_storage.join(macos_file_provider_directory_name(&display_name)),
        cloud_storage.join(format!("AFS-{display_name}")),
        cloud_storage.join(format!("AgentFS-{display_name}")),
    ];
    let source_directory = source_root_directory_name(&mount.connector);
    domain_roots
        .into_iter()
        .flat_map(|domain_root| [domain_root.join(&source_directory), domain_root])
        .collect()
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
    use afs_core::canonical::{parse_canonical_markdown, render_canonical_markdown};
    use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
    use afs_core::shadow::ShadowDocument;
    use afs_store::{
        EntityRecord, EntityRepository, InMemoryStateStore, MountRepository, ProjectionMode,
        ShadowRepository,
    };
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    #[test]
    fn linux_fuse_source_directory_is_an_access_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/AFS")
            .projection(ProjectionMode::LinuxFuse);

        let matched = match_mount_path(&mount, Path::new("/tmp/AFS/notion/roadmap/page.md"))
            .expect("path matches source directory");

        assert_eq!(matched.access_root, PathBuf::from("/tmp/AFS/notion"));
        assert_eq!(matched.relative_path, PathBuf::from("roadmap/page.md"));
    }

    #[test]
    fn windows_cloud_files_source_directory_is_an_access_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/AFS")
            .projection(ProjectionMode::WindowsCloudFiles);

        let matched = match_mount_path(&mount, Path::new("/tmp/AFS/notion/roadmap/page.md"))
            .expect("path matches source directory");

        assert_eq!(matched.access_root, PathBuf::from("/tmp/AFS/notion"));
        assert_eq!(matched.relative_path, PathBuf::from("roadmap/page.md"));
    }

    #[test]
    fn plain_mount_keeps_source_named_directory_in_relative_path() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/AFS");

        let matched = match_mount_path(&mount, Path::new("/tmp/AFS/notion/roadmap/page.md"))
            .expect("path matches mount root");

        assert_eq!(matched.access_root, PathBuf::from("/tmp/AFS"));
        assert_eq!(
            matched.relative_path,
            PathBuf::from("notion/roadmap/page.md")
        );
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
                    .join("AFS")
                    .join("notion")
            )
        );
        assert!(
            roots.contains(
                &home
                    .join("Library")
                    .join("CloudStorage")
                    .join("AFS-AFS")
                    .join("notion")
            )
        );
        assert!(
            roots.contains(
                &home
                    .join("Library")
                    .join("CloudStorage")
                    .join("AgentFS-AFS")
            )
        );
        let matched = match_mount_path(
            &mount,
            &home
                .join("Library")
                .join("CloudStorage")
                .join("AFS-AFS")
                .join("notion")
                .join("Page.md"),
        )
        .expect("AFS-AFS connector path matches");
        assert_eq!(matched.relative_path, PathBuf::from("Page.md"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reconcile_macos_projection_without_target_is_noop() {
        let fixture = ProjectionFixture::new("no-target");
        fixture.write_cache("Original body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_projection_without_identity("Original body.\n\nLocal edit.\n");

        let mut store = fixture.store();
        let report =
            reconcile_macos_file_provider_projection(&mut store, &fixture.state_root, None)
                .expect("reconcile projection");

        assert_eq!(report, ProjectionReconcileReport::default());
        let cached = fs::read_to_string(fixture.content_path()).expect("read cache");
        assert!(!cached.contains("Local edit."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reconcile_macos_projection_imports_explicit_visible_file_with_missing_identity() {
        let fixture = ProjectionFixture::new("newer-visible");
        fixture.write_cache("Original body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_projection_without_identity("Original body.\n\nLocal edit.\n");

        let mut store = fixture.store();
        let report = reconcile_macos_file_provider_projection(
            &mut store,
            &fixture.state_root,
            Some(&fixture.projection_path()),
        )
        .expect("reconcile projection");

        assert_eq!(report.reconciled, 1);
        let cached = fs::read_to_string(fixture.content_path()).expect("read cache");
        let parsed = parse_canonical_markdown(&cached).expect("canonical cache");
        assert_eq!(parsed.remote_id(), Some(&fixture.remote_id));
        assert!(cached.contains("Local edit."));
        assert!(cached.contains("afs:"));
        let entity = store
            .get_entity(&fixture.mount_id, &fixture.remote_id)
            .expect("read entity")
            .expect("entity");
        assert_eq!(entity.hydration, HydrationState::Dirty);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reconcile_macos_projection_explicit_target_reads_even_when_cache_is_newer() {
        let fixture = ProjectionFixture::new("explicit-target");
        fixture.write_projection_without_identity("Edited body.\n");
        std::thread::sleep(Duration::from_millis(5));
        fixture.write_cache("Original body.\n");

        let mut store = fixture.store();
        let report = reconcile_macos_file_provider_projection(
            &mut store,
            &fixture.state_root,
            Some(&fixture.projection_path()),
        )
        .expect("reconcile projection");

        assert_eq!(report.reconciled, 1);
        let cached = fs::read_to_string(fixture.content_path()).expect("read cache");
        assert!(cached.contains("Edited body."));
        assert!(cached.contains("afs:"));
    }

    #[cfg(target_os = "macos")]
    struct ProjectionFixture {
        root: PathBuf,
        state_root: PathBuf,
        mount_id: MountId,
        remote_id: RemoteId,
    }

    #[cfg(target_os = "macos")]
    impl ProjectionFixture {
        fn new(name: &str) -> Self {
            let root = temp_root(&format!("afs-file-provider-reconcile-{name}"));
            let state_root = temp_root(&format!("afs-file-provider-reconcile-state-{name}"));
            let source_root = root.join("notion");
            fs::create_dir_all(source_root.join("go-to-market/afs-launch"))
                .expect("projection directories");
            fs::create_dir_all(
                crate::virtual_fs::virtual_fs_content_root(
                    &state_root,
                    &MountId::new("notion-main"),
                )
                .join("go-to-market/afs-launch"),
            )
            .expect("content directories");
            Self {
                root,
                state_root,
                mount_id: MountId::new("notion-main"),
                remote_id: RemoteId::new("page-1"),
            }
        }

        fn store(&self) -> InMemoryStateStore {
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(
                    MountConfig::new(self.mount_id.clone(), "notion", self.root.join("notion"))
                        .projection(ProjectionMode::MacosFileProvider),
                )
                .expect("save mount");
            store
                .save_entity(
                    EntityRecord::new(
                        self.mount_id.clone(),
                        self.remote_id.clone(),
                        EntityKind::Page,
                        "AFS Launch",
                        "go-to-market/afs-launch/page.md",
                    )
                    .with_hydration(HydrationState::Hydrated)
                    .with_remote_edited_at("remote-v1"),
                )
                .expect("save entity");
            store
                .save_shadow(
                    &self.mount_id,
                    ShadowDocument::from_synced_body(
                        self.remote_id.clone(),
                        "Original body.\n",
                        8,
                        [RemoteId::new("block-1")],
                    )
                    .expect("shadow")
                    .with_frontmatter(frontmatter(&self.remote_id)),
                )
                .expect("save shadow");
            store
        }

        fn projection_path(&self) -> PathBuf {
            self.root
                .join("notion")
                .join("go-to-market/afs-launch/page.md")
        }

        fn content_path(&self) -> PathBuf {
            crate::virtual_fs::virtual_fs_content_root(&self.state_root, &self.mount_id)
                .join("go-to-market/afs-launch/page.md")
        }

        fn write_projection_without_identity(&self, body: &str) {
            fs::write(
                self.projection_path(),
                format!("---\ntitle: \"AFS Launch\"\n---\n{body}"),
            )
            .expect("write projection");
        }

        fn write_cache(&self, body: &str) {
            fs::write(
                self.content_path(),
                render_canonical_markdown(&CanonicalDocument::new(
                    frontmatter(&self.remote_id),
                    body,
                )),
            )
            .expect("write cache");
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for ProjectionFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.state_root);
        }
    }

    #[cfg(target_os = "macos")]
    fn frontmatter(remote_id: &RemoteId) -> String {
        format!(
            "afs:\n  id: {}\n  type: page\n  synced_at: remote-v1\n  remote_edited_at: remote-v1\ntitle: \"AFS Launch\"\n",
            remote_id.0
        )
    }

    #[cfg(target_os = "macos")]
    fn temp_root(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
    }
}
