use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use locality_core::model::MountId;
use locality_core::{LocalityError, LocalityResult};
use locality_store::{MountConfig, MountRepository, ProjectionMode};

use crate::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, VirtualFsChildrenReport, VirtualFsItem, VirtualFsItemKind,
    mount_point_directory_name, mount_point_identifier, source_root_read_only,
    virtual_projection_root,
};

pub const SHARED_IDENTIFIER_PREFIX: &str = "m:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedProjectionIdentifier {
    pub mount_id: MountId,
    pub daemon_identifier: String,
}

pub fn wrap_identifier(mount_id: &MountId, daemon_identifier: &str) -> String {
    format!(
        "{}{}:{}",
        SHARED_IDENTIFIER_PREFIX,
        URL_SAFE_NO_PAD.encode(mount_id.0.as_bytes()),
        URL_SAFE_NO_PAD.encode(daemon_identifier.as_bytes())
    )
}

pub fn unwrap_identifier(identifier: &str) -> LocalityResult<SharedProjectionIdentifier> {
    let Some(payload) = identifier.strip_prefix(SHARED_IDENTIFIER_PREFIX) else {
        return Err(invalid_identifier(
            "missing shared projection identifier prefix",
        ));
    };
    let Some((mount_segment, daemon_segment)) = payload.split_once(':') else {
        return Err(invalid_identifier(
            "missing shared projection identifier delimiter",
        ));
    };
    if mount_segment.is_empty() || daemon_segment.is_empty() || daemon_segment.contains(':') {
        return Err(invalid_identifier(
            "invalid shared projection identifier segments",
        ));
    }

    let mount_id = decode_segment(mount_segment, "mount id")?;
    let daemon_identifier = decode_segment(daemon_segment, "daemon identifier")?;
    if mount_id.is_empty() || daemon_identifier.is_empty() {
        return Err(invalid_identifier(
            "shared projection identifier segments must not be empty",
        ));
    }

    Ok(SharedProjectionIdentifier {
        mount_id: MountId::new(mount_id),
        daemon_identifier,
    })
}

pub fn wrap_item(mount: &MountConfig, mut item: VirtualFsItem) -> VirtualFsItem {
    let parent_identifier = item.parent_identifier.take();
    item.identifier = wrap_identifier(&mount.mount_id, &item.identifier);
    item.parent_identifier = parent_identifier.map(|parent| {
        if parent == ROOT_CONTAINER_IDENTIFIER {
            ROOT_CONTAINER_IDENTIFIER.to_string()
        } else {
            wrap_identifier(&mount.mount_id, &parent)
        }
    });
    if !item.path.is_empty() {
        item.path = format!(
            "{}/{}",
            mount_point_directory_name(mount),
            item.path.trim_start_matches('/')
        );
    }
    item
}

pub fn virtual_projection_root_children<S>(
    store: &S,
    projection_root: &Path,
    projection: ProjectionMode,
) -> LocalityResult<VirtualFsChildrenReport>
where
    S: MountRepository,
{
    let mut children = store
        .load_mounts()
        .map_err(LocalityError::from)?
        .into_iter()
        .filter(|mount| {
            mount.projection == projection && virtual_projection_root(mount) == projection_root
        })
        .map(|mount| shared_mount_point_item(&mount))
        .collect::<Vec<_>>();

    children.sort_by(|left, right| {
        left.filename
            .to_lowercase()
            .cmp(&right.filename.to_lowercase())
            .then_with(|| left.identifier.cmp(&right.identifier))
    });

    Ok(VirtualFsChildrenReport {
        mount_id: String::new(),
        container_identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
        children,
    })
}

fn shared_mount_point_item(mount: &MountConfig) -> VirtualFsItem {
    let filename = mount_point_directory_name(mount);
    VirtualFsItem {
        identifier: wrap_identifier(&mount.mount_id, &mount_point_identifier(mount)),
        parent_identifier: Some(ROOT_CONTAINER_IDENTIFIER.to_string()),
        filename: filename.clone(),
        kind: VirtualFsItemKind::Folder,
        read_only: source_root_read_only(mount),
        entity_kind: None,
        remote_id: None,
        path: filename,
        hydration: None,
        content_type: "public.folder".to_string(),
        remote_edited_at: None,
        materialized_path: Some(mount.root.display().to_string()),
        byte_size: None,
    }
}

fn decode_segment(segment: &str, name: &str) -> LocalityResult<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(segment.as_bytes())
        .map_err(|error| invalid_identifier(format!("invalid {name} encoding: {error}")))?;
    String::from_utf8(bytes)
        .map_err(|error| invalid_identifier(format!("invalid {name} utf-8: {error}")))
}

fn invalid_identifier(message: impl Into<String>) -> LocalityError {
    LocalityError::InvalidState(message.into())
}

#[cfg(test)]
mod tests {
    use locality_core::model::MountId;
    use locality_store::InMemoryStateStore;

    use super::*;
    use crate::virtual_fs::VirtualFsItemKind;

    #[test]
    fn shared_identifier_round_trips_url_safe_segments() {
        let mount_id = MountId::new("notion-main");

        let wrapped = wrap_identifier(&mount_id, "children:page/root");
        let unwrapped = unwrap_identifier(&wrapped).expect("unwrap identifier");

        assert_eq!(unwrapped.mount_id, mount_id);
        assert_eq!(unwrapped.daemon_identifier, "children:page/root");
    }

    #[test]
    fn unwrap_identifier_rejects_missing_prefix() {
        assert!(unwrap_identifier("notion-main:page-1").is_err());
    }

    #[test]
    fn unwrap_identifier_rejects_missing_delimiter() {
        assert!(unwrap_identifier("m:notion-main").is_err());
    }

    #[test]
    fn unwrap_identifier_rejects_invalid_base64() {
        assert!(unwrap_identifier("m:not-base64*:cGFnZS0x").is_err());
    }

    #[test]
    fn unwrap_identifier_rejects_invalid_utf8() {
        let invalid_utf8 = URL_SAFE_NO_PAD.encode([0xff]);

        assert!(unwrap_identifier(&format!("m:{invalid_utf8}:cGFnZS0x")).is_err());
    }

    #[test]
    fn wrap_item_preserves_shared_root_parent_and_prefixes_path() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            Path::new("/tmp/notion-main"),
        );
        let item = VirtualFsItem {
            identifier: "page-1".to_string(),
            parent_identifier: Some(ROOT_CONTAINER_IDENTIFIER.to_string()),
            filename: "page.md".to_string(),
            kind: VirtualFsItemKind::File,
            read_only: false,
            entity_kind: None,
            remote_id: None,
            path: "Roadmap/page.md".to_string(),
            hydration: None,
            content_type: "net.daringfireball.markdown".to_string(),
            remote_edited_at: None,
            materialized_path: None,
            byte_size: None,
        };

        let wrapped = wrap_item(&mount, item);

        assert!(wrapped.identifier.starts_with(SHARED_IDENTIFIER_PREFIX));
        assert_eq!(
            wrapped.parent_identifier.as_deref(),
            Some(ROOT_CONTAINER_IDENTIFIER)
        );
        assert_eq!(wrapped.path, "notion-main/Roadmap/page.md");
    }

    #[test]
    fn wrap_item_prefixes_path_with_visible_mount_point_name() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            Path::new("/tmp/Work Notion"),
        );
        let item = VirtualFsItem {
            identifier: "page-1".to_string(),
            parent_identifier: Some("children:root".to_string()),
            filename: "page.md".to_string(),
            kind: VirtualFsItemKind::File,
            read_only: false,
            entity_kind: None,
            remote_id: None,
            path: "Roadmap/page.md".to_string(),
            hydration: None,
            content_type: "net.daringfireball.markdown".to_string(),
            remote_edited_at: None,
            materialized_path: None,
            byte_size: None,
        };

        let wrapped = wrap_item(&mount, item);

        assert!(wrapped.identifier.starts_with(SHARED_IDENTIFIER_PREFIX));
        assert!(
            wrapped
                .parent_identifier
                .as_deref()
                .is_some_and(|parent| parent.starts_with(SHARED_IDENTIFIER_PREFIX))
        );
        assert_eq!(wrapped.path, "Work Notion/Roadmap/page.md");
    }

    #[test]
    fn shared_mount_point_item_reflects_read_only_mount() {
        let mount_id = MountId::new("notion-main");
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(mount_id, "notion", "/tmp/Locality/notion-main")
            .projection(ProjectionMode::LinuxFuse)
            .read_only(true);
        store.save_mount(mount).expect("save mount");

        let report = virtual_projection_root_children(
            &store,
            Path::new("/tmp/Locality"),
            ProjectionMode::LinuxFuse,
        )
        .expect("shared root children");

        assert_eq!(report.children.len(), 1);
        assert!(report.children[0].read_only);
    }

    #[test]
    fn shared_mount_point_item_reflects_source_root_write_policy() {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-main"),
                    "notion",
                    "/tmp/Locality/notion-main",
                )
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save notion mount");
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("google-docs-main"),
                    "google-docs",
                    "/tmp/Locality/google-docs-main",
                )
                .with_remote_root_id(locality_core::model::RemoteId::new("workspace-folder"))
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save google docs mount");

        let report = virtual_projection_root_children(
            &store,
            Path::new("/tmp/Locality"),
            ProjectionMode::LinuxFuse,
        )
        .expect("shared root children");

        let notion = report
            .children
            .iter()
            .find(|item| item.filename == "notion-main")
            .expect("notion mount point");
        let google_docs = report
            .children
            .iter()
            .find(|item| item.filename == "google-docs-main")
            .expect("google docs mount point");
        assert!(!notion.read_only);
        assert!(!google_docs.read_only);
    }
}
