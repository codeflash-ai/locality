//! macOS File Provider compatibility aliases.
//!
//! The daemon-owned virtual filesystem contract lives in `virtual_fs`. macOS
//! File Provider, Linux FUSE, and future platform projections should bind to that
//! generic API instead of growing platform-specific daemon semantics.

use afs_core::AfsResult;
use afs_core::model::MountId;
use afs_store::{EntityRepository, MountRepository, ShadowRepository};

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
    S: MountRepository + EntityRepository,
{
    virtual_fs::virtual_fs_item(store, mount_id, identifier)
}

pub fn file_provider_children<S>(
    store: &S,
    mount_id: &MountId,
    container_identifier: &str,
) -> AfsResult<FileProviderChildrenReport>
where
    S: MountRepository + EntityRepository,
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
    S: MountRepository + EntityRepository + ShadowRepository,
    Source: HydrationSource + ?Sized,
{
    virtual_fs::materialize_virtual_fs_item(store, source, mount_id, identifier)
}
