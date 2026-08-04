//! Strict filesystem durability primitives shared by recovery paths.

#[cfg(any(test, not(unix), windows))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

/// Opens a Windows path without following reparse points and returns its
/// volume plus full 128-bit FILE_ID_INFO identity. The handle requests only
/// attributes and synchronization access, so restrictive data ACLs and cloud
/// placeholders do not require FILE_READ_DATA.
#[cfg(windows)]
pub fn windows_path_identity_no_follow(path: &Path) -> io::Result<(u64, u64, u64)> {
    let identity = crate::windows_workspace_fs::inspect_path_identity_no_follow(path)?;
    Ok((identity.device, identity.inode, identity.inode_high))
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_vendor = "apple",
    target_os = "redox"
)))]
fn unsupported_descriptor_relative_operation<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-relative durable filesystem operations are unavailable on this platform",
    ))
}

pub fn create_dir_all_durable(trusted_root: &Path, path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        return create_dir_all_durable_unix(trusted_root, path);
    }
    #[cfg(windows)]
    return create_dir_all_durable_windows(trusted_root, path);
    #[cfg(not(any(unix, windows)))]
    unsupported_descriptor_relative_operation()
}

#[cfg(windows)]
fn create_dir_all_durable_windows(trusted_root: &Path, path: &Path) -> io::Result<()> {
    create_dir_all_durable_windows_with_post_validation_hook(trusted_root, path, || {})
}

#[cfg(windows)]
fn create_dir_all_durable_windows_with_post_validation_hook(
    trusted_root: &Path,
    path: &Path,
    after_validation: impl FnOnce(),
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    after_validation();

    let relative = path.strip_prefix(trusted_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is outside trusted root", path.display()),
        )
    })?;
    let mut directories = vec![
        crate::windows_workspace_fs::WindowsDirectory::open_absolute_mutation_anchor(trusted_root)?,
    ];
    for component in relative.components() {
        let component = match component {
            std::path::Component::Normal(component) => component,
            std::path::Component::CurDir => continue,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("path `{}` contains unsafe components", path.display()),
                ));
            }
        };
        let parent = directories.last().expect("anchored directory parent");
        let child = match parent.open_directory_mutation_anchor(component) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match parent.create_directory_anchored(component) {
                    Ok(child) => {
                        parent.sync()?;
                        child.sync()?;
                        child
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        parent.open_directory_mutation_anchor(component)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        directories.push(child);
    }
    Ok(())
}

#[cfg(any(test, not(any(unix, windows))))]
pub(crate) fn create_dir_all_durable_with_sync(
    trusted_root: &Path,
    path: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || metadata_is_windows_reparse_point(&metadata)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("directory `{}` is a symlink", cursor.display()),
                    ));
                }
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("ancestor `{}` is not a directory", cursor.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor.to_path_buf());
                cursor = cursor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("directory `{}` has no existing ancestor", path.display()),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    for directory in missing.into_iter().rev() {
        fs::create_dir(&directory)?;
        sync(directory.parent().expect("new directory has parent"))?;
        sync(&directory)?;
    }
    Ok(())
}

pub fn write_new_file_durable(trusted_root: &Path, path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        return write_new_file_durable_unix(trusted_root, path, contents);
    }
    #[cfg(windows)]
    return write_new_file_durable_windows(trusted_root, path, contents);
    #[cfg(not(any(unix, windows)))]
    unsupported_descriptor_relative_operation()
}

pub(crate) fn create_new_file(trusted_root: &Path, path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        let (_, file) =
            create_new_file_windows_with_post_validation_hook(trusted_root, path, || {})?;
        return Ok(file);
    }
    #[cfg(not(windows))]
    {
        let _ = trusted_root;
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
    }
}

#[cfg(windows)]
fn write_new_file_durable_windows(
    trusted_root: &Path,
    path: &Path,
    contents: &[u8],
) -> io::Result<()> {
    let (parents, mut file) =
        create_new_file_windows_with_post_validation_hook(trusted_root, path, || {})?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    parents.last().expect("anchored file parent").sync()
}

#[cfg(windows)]
fn create_new_file_windows_with_post_validation_hook(
    trusted_root: &Path,
    path: &Path,
    after_validation: impl FnOnce(),
) -> io::Result<(Vec<crate::windows_workspace_fs::WindowsDirectory>, File)> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    after_validation();
    let (parents, name) = open_windows_parent_anchored_after_validation(trusted_root, path, true)?;
    let file = parents
        .last()
        .expect("anchored file parent")
        .create_file_anchored(&name)?;
    Ok((parents, file))
}

pub fn copy_new_file_durable(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
) -> io::Result<u64> {
    #[cfg(windows)]
    {
        return copy_new_file_durable_windows(
            source_root,
            source,
            destination_root,
            destination,
            false,
        );
    }
    #[cfg(unix)]
    {
        return copy_new_file_durable_unix(source_root, source, destination_root, destination);
    }
    #[cfg(not(any(unix, windows)))]
    unsupported_descriptor_relative_operation()
}

pub fn copy_new_file_durable_allow_cloud_placeholder(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
) -> io::Result<u64> {
    #[cfg(windows)]
    {
        return copy_new_file_durable_windows(
            source_root,
            source,
            destination_root,
            destination,
            true,
        );
    }
    #[cfg(not(windows))]
    copy_new_file_durable(source_root, source, destination_root, destination)
}

#[cfg(any(test, not(any(unix, windows))))]
pub(crate) fn copy_new_file_durable_with_sync(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    sync_file: impl FnOnce(&File) -> io::Result<()>,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<u64> {
    validate_no_symlink_or_reparse_ancestors(source_root, source)?;
    validate_no_symlink_or_reparse_ancestors(destination_root, destination)?;
    let mut source = File::open(source)?;
    let mut destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let copied = io::copy(&mut source, &mut destination_file)?;
    sync_file(&destination_file)?;
    drop(destination_file);
    sync_parent(destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("file `{}` has no parent", destination.display()),
        )
    })?)?;
    Ok(copied)
}

#[cfg(any(test, not(any(unix, windows))))]
pub(crate) fn write_new_file_durable_with_sync(
    trusted_root: &Path,
    path: &Path,
    contents: &[u8],
    sync_file: impl FnOnce(&File) -> io::Result<()>,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(contents)?;
    sync_file(&file)?;
    drop(file);
    sync_parent(path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("file `{}` has no parent", path.display()),
        )
    })?)
}

pub fn rename_noreplace_durable(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
) -> io::Result<()> {
    #[cfg(windows)]
    validate_no_symlink_or_reparse_ancestors_allow_final(source_root, source)?;
    #[cfg(windows)]
    validate_no_symlink_or_reparse_ancestors(destination_root, destination)?;
    #[cfg(all(
        unix,
        any(target_os = "linux", target_vendor = "apple", target_os = "redox")
    ))]
    {
        return rename_noreplace_durable_unix(source_root, destination_root, source, destination);
    }
    #[cfg(not(all(
        unix,
        any(target_os = "linux", target_vendor = "apple", target_os = "redox")
    )))]
    {
        #[cfg(windows)]
        return rename_noreplace_durable_windows_anchored(
            source_root,
            source,
            destination_root,
            destination,
        );
        #[cfg(not(windows))]
        return unsupported_descriptor_relative_operation();
    }
}

pub fn rename_noreplace_durable_anchored(
    trusted_root: &Path,
    source: &Path,
    destination: &Path,
) -> io::Result<()> {
    rename_noreplace_durable(trusted_root, source, trusted_root, destination)
}

#[cfg(windows)]
fn rename_noreplace_durable_windows_anchored(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
) -> io::Result<()> {
    let (source_parents, source_name) = open_windows_parent_anchored(source_root, source, true)?;
    let (destination_parents, destination_name) =
        open_windows_parent_anchored(destination_root, destination, true)?;
    let source_parent = source_parents.last().expect("anchored source parent");
    let destination_parent = destination_parents
        .last()
        .expect("anchored destination parent");
    source_parent.rename_child_no_replace_allow_final_reparse(
        &source_name,
        destination_parent,
        &destination_name,
    )?;
    destination_parent.sync()?;
    if source.parent() != destination.parent() {
        source_parent.sync()?;
    }
    Ok(())
}

#[cfg(windows)]
fn copy_new_file_durable_windows(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    allow_cloud_placeholder: bool,
) -> io::Result<u64> {
    let (source_parents, source_name) = open_windows_parent_anchored(source_root, source, false)?;
    let (destination_parents, destination_name) =
        open_windows_parent_anchored(destination_root, destination, true)?;
    let source_parent = source_parents.last().expect("anchored source parent");
    let destination_parent = destination_parents
        .last()
        .expect("anchored destination parent");
    let mut source_file = if allow_cloud_placeholder {
        source_parent.open_file_for_durable_copy_allow_cloud_placeholder(&source_name)?
    } else {
        source_parent.open_file_for_durable_copy(&source_name)?
    };
    let mut destination_file = destination_parent.create_file_anchored(&destination_name)?;
    let copied = io::copy(&mut source_file, &mut destination_file)?;
    destination_file.sync_all()?;
    drop(destination_file);
    destination_parent.sync()?;
    Ok(copied)
}

#[cfg(windows)]
fn open_windows_parent_anchored(
    trusted_root: &Path,
    path: &Path,
    writable_final: bool,
) -> io::Result<(
    Vec<crate::windows_workspace_fs::WindowsDirectory>,
    std::ffi::OsString,
)> {
    validate_no_symlink_or_reparse_ancestors_allow_final(trusted_root, path)?;
    open_windows_parent_anchored_after_validation(trusted_root, path, writable_final)
}

#[cfg(windows)]
fn open_windows_parent_anchored_after_validation(
    trusted_root: &Path,
    path: &Path,
    writable_final: bool,
) -> io::Result<(
    Vec<crate::windows_workspace_fs::WindowsDirectory>,
    std::ffi::OsString,
)> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` has no parent", path.display()),
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` has no filename", path.display()),
        )
    })?;
    let relative = parent.strip_prefix(trusted_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is outside trusted root", path.display()),
        )
    })?;
    let components = relative.components().collect::<Vec<_>>();
    let root = if writable_final && components.is_empty() {
        crate::windows_workspace_fs::WindowsDirectory::open_absolute_sync_anchor(trusted_root)?
    } else {
        crate::windows_workspace_fs::WindowsDirectory::open_absolute_anchor(trusted_root)?
    };
    let mut parents = vec![root];
    let component_count = components.len();
    for (index, component) in components.into_iter().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path `{}` contains unsafe components", path.display()),
            ));
        };
        let parent = parents.last().expect("anchored parent");
        let child = if writable_final && index + 1 == component_count {
            parent.open_directory_sync_anchor(component)?
        } else {
            parent.open_directory_anchor(component)?
        };
        parents.push(child);
    }
    Ok((parents, name.to_os_string()))
}

#[cfg(windows)]
fn open_windows_directory_anchored(
    trusted_root: &Path,
    path: &Path,
) -> io::Result<Vec<crate::windows_workspace_fs::WindowsDirectory>> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    let relative = path.strip_prefix(trusted_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is outside trusted root", path.display()),
        )
    })?;
    let components = relative.components().collect::<Vec<_>>();
    let root = if components.is_empty() {
        crate::windows_workspace_fs::WindowsDirectory::open_absolute_sync_anchor(trusted_root)?
    } else {
        crate::windows_workspace_fs::WindowsDirectory::open_absolute_anchor(trusted_root)?
    };
    let mut directories = vec![root];
    let component_count = components.len();
    for (index, component) in components.into_iter().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path `{}` contains unsafe components", path.display()),
            ));
        };
        let parent = directories.last().expect("anchored directory");
        directories.push(if index + 1 == component_count {
            parent.open_directory_sync_anchor(component)?
        } else {
            parent.open_directory_anchor(component)?
        });
    }
    Ok(directories)
}

#[cfg(all(
    unix,
    any(target_os = "linux", target_vendor = "apple", target_os = "redox")
))]
fn rename_noreplace_durable_unix(
    source_root: &Path,
    destination_root: &Path,
    source: &Path,
    destination: &Path,
) -> io::Result<()> {
    use rustix::fs::RenameFlags;

    if source_root != destination_root {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable rename requires one shared trusted root",
        ));
    }
    let root = open_unix_root(source_root)?;
    let (source_parent, source_name) = open_unix_parent_from_root(&root, source_root, source)?;
    let (destination_parent, destination_name) =
        open_unix_parent_from_root(&root, source_root, destination)?;
    rustix::fs::renameat_with(
        &source_parent,
        &source_name,
        &destination_parent,
        &destination_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)?;
    rustix::fs::fsync(&destination_parent).map_err(io::Error::from)?;
    if source.parent() != destination.parent() {
        rustix::fs::fsync(&source_parent).map_err(io::Error::from)?;
    }
    Ok(())
}

#[cfg(unix)]
fn validate_root_relative_path(trusted_root: &Path, path: &Path) -> io::Result<()> {
    if !trusted_root.is_absolute() || !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trusted root and durable path must be absolute",
        ));
    }
    let relative = path.strip_prefix(trusted_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path `{}` is outside trusted root `{}`",
                path.display(),
                trusted_root.display()
            ),
        )
    })?;
    if relative.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` contains unsafe components", path.display()),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_unix_root(trusted_root: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags};

    if !trusted_root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("trusted root `{}` must be absolute", trusted_root.display()),
        ));
    }
    rustix::fs::open(
        trusted_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)
}

#[cfg(unix)]
fn open_unix_child_directory(
    parent: &std::os::fd::OwnedFd,
    component: &std::ffi::OsStr,
) -> io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::openat(
        parent,
        component,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "trusted path component `{}` is a symlink or not a directory",
                    component.to_string_lossy()
                ),
            )
        } else {
            error.into()
        }
    })
}

#[cfg(unix)]
fn open_unix_directory_without_symlinks(
    trusted_root: &Path,
    path: &Path,
) -> io::Result<std::os::fd::OwnedFd> {
    validate_root_relative_path(trusted_root, path)?;
    let mut directory = open_unix_root(trusted_root)?;
    for component in path
        .strip_prefix(trusted_root)
        .expect("validated root-relative path")
        .components()
    {
        let std::path::Component::Normal(component) = component else {
            continue;
        };
        directory = open_unix_child_directory(&directory, component)?;
    }
    Ok(directory)
}

#[cfg(unix)]
fn create_dir_all_durable_unix(trusted_root: &Path, path: &Path) -> io::Result<()> {
    use rustix::fs::Mode;

    validate_root_relative_path(trusted_root, path)?;
    let mut directory = open_unix_root(trusted_root)?;
    for component in path
        .strip_prefix(trusted_root)
        .expect("validated root-relative path")
        .components()
    {
        let std::path::Component::Normal(component) = component else {
            continue;
        };
        match open_unix_child_directory(&directory, component) {
            Ok(child) => directory = child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                rustix::fs::mkdirat(&directory, component, Mode::from_raw_mode(0o777))
                    .map_err(io::Error::from)?;
                rustix::fs::fsync(&directory).map_err(io::Error::from)?;
                directory = open_unix_child_directory(&directory, component)?;
                rustix::fs::fsync(&directory).map_err(io::Error::from)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn write_new_file_durable_unix(
    trusted_root: &Path,
    path: &Path,
    contents: &[u8],
) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags};

    let (parent, name) = open_unix_parent_without_symlinks(trusted_root, path)?;
    let descriptor = rustix::fs::openat(
        &parent,
        &name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o666),
    )
    .map_err(io::Error::from)?;
    let mut file = File::from(descriptor);
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    rustix::fs::fsync(&parent).map_err(io::Error::from)
}

#[cfg(unix)]
fn copy_new_file_durable_unix(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
) -> io::Result<u64> {
    use rustix::fs::{Mode, OFlags};

    let (source_parent, source_name) = open_unix_parent_without_symlinks(source_root, source)?;
    let source_descriptor = rustix::fs::openat(
        &source_parent,
        &source_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let (destination_parent, destination_name) =
        open_unix_parent_without_symlinks(destination_root, destination)?;
    let destination_descriptor = rustix::fs::openat(
        &destination_parent,
        &destination_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o666),
    )
    .map_err(io::Error::from)?;
    let mut source_file = File::from(source_descriptor);
    let mut destination_file = File::from(destination_descriptor);
    let copied = io::copy(&mut source_file, &mut destination_file)?;
    destination_file.sync_all()?;
    drop(destination_file);
    rustix::fs::fsync(&destination_parent).map_err(io::Error::from)?;
    Ok(copied)
}

#[cfg(unix)]
fn remove_private_directory_contents_unix(directory: &std::os::fd::OwnedFd) -> io::Result<()> {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

    for entry in Dir::read_from(directory).map_err(io::Error::from)? {
        let entry = entry.map_err(io::Error::from)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let metadata = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(io::Error::from)?;
        if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
            let child = rustix::fs::openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            remove_private_directory_contents_unix(&child)?;
            rustix::fs::unlinkat(directory, name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        } else {
            rustix::fs::unlinkat(directory, name, AtFlags::empty()).map_err(io::Error::from)?;
        }
    }
    rustix::fs::fsync(directory).map_err(io::Error::from)
}

#[cfg(unix)]
fn remove_dir_all_durable_unix(trusted_root: &Path, path: &Path) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags};

    recover_unix_identity_quarantines(trusted_root)?;
    let root = open_unix_root(trusted_root)?;
    let (parent, name) = open_unix_parent_from_root(&root, trusted_root, path)?;
    let directory = rustix::fs::openat(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let opened = rustix::fs::fstat(&directory).map_err(io::Error::from)?;
    drop(directory);
    quarantine_directory_if_identity_unix(
        &root,
        &parent,
        &name,
        opened.st_dev as u64,
        opened.st_ino as u64,
        UnixQuarantineKind::Directory,
        || Ok(()),
    )
}

#[cfg(unix)]
fn remove_path_durable_unix(trusted_root: &Path, path: &Path) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};

    recover_unix_identity_quarantines(trusted_root)?;
    let root = open_unix_root(trusted_root)?;
    let (parent, name) = open_unix_parent_from_root(&root, trusted_root, path)?;
    let metadata =
        rustix::fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    match FileType::from_raw_mode(metadata.st_mode) {
        FileType::Directory => {
            let directory = rustix::fs::openat(
                &parent,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let opened = rustix::fs::fstat(&directory).map_err(io::Error::from)?;
            drop(directory);
            quarantine_directory_if_identity_unix(
                &root,
                &parent,
                &name,
                opened.st_dev as u64,
                opened.st_ino as u64,
                UnixQuarantineKind::Directory,
                || Ok(()),
            )?;
        }
        FileType::RegularFile => {
            let descriptor = rustix::fs::openat(
                &parent,
                &name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let opened = rustix::fs::fstat(&descriptor).map_err(io::Error::from)?;
            drop(descriptor);
            quarantine_regular_file_if_identity_unix(
                &root,
                &parent,
                &name,
                opened.st_dev as u64,
                opened.st_ino as u64,
                || Ok(()),
            )?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "path `{}` is not an ordinary file or directory",
                    path.display()
                ),
            ));
        }
    }
    rustix::fs::fsync(&parent).map_err(io::Error::from)
}

/// Atomically removes a regular file from its public pathname only while that
/// pathname still names the expected Unix object.
///
/// POSIX has no conditional-unlink primitive. The checked object is therefore
/// renamed without replacement into an unpredictable mode-0700 quarantine next
/// to the trusted root. The descriptor-held quarantine is identity-checked and
/// unlinked immediately; its durable manifest lets bounded recovery complete
/// the unlink after a crash. Keeping the quarantine outside the trusted tree
/// prevents internal tombstones from becoming projected or recovery content.
#[cfg(unix)]
pub(crate) fn remove_regular_file_durable_if_identity_unix(
    trusted_root: &Path,
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
) -> io::Result<()> {
    remove_regular_file_durable_if_identity_unix_with_hook(
        trusted_root,
        path,
        expected_device,
        expected_inode,
        || Ok(()),
    )
}

/// Removes a directory tree only if the final public pathname still names the
/// expected Unix object at the atomic quarantine boundary.
#[cfg(unix)]
pub fn remove_dir_all_durable_if_identity_unix(
    trusted_root: &Path,
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
) -> io::Result<()> {
    recover_unix_identity_quarantines(trusted_root)?;
    let root = open_unix_root(trusted_root)?;
    let (parent, name) = open_unix_parent_from_root(&root, trusted_root, path)?;
    quarantine_directory_if_identity_unix(
        &root,
        &parent,
        &name,
        expected_device,
        expected_inode,
        UnixQuarantineKind::Directory,
        || Ok(()),
    )
}

/// Removes an empty directory only if the final public pathname still names
/// the expected Unix object. Unexpected children make collection fail closed.
#[cfg(unix)]
pub fn remove_empty_dir_durable_if_identity_unix(
    trusted_root: &Path,
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
) -> io::Result<()> {
    recover_unix_identity_quarantines(trusted_root)?;
    let root = open_unix_root(trusted_root)?;
    let (parent, name) = open_unix_parent_from_root(&root, trusted_root, path)?;
    quarantine_directory_if_identity_unix(
        &root,
        &parent,
        &name,
        expected_device,
        expected_inode,
        UnixQuarantineKind::EmptyDirectory,
        || Ok(()),
    )
}

#[cfg(unix)]
pub(crate) fn remove_regular_file_durable_if_identity_unix_with_hook(
    trusted_root: &Path,
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
    before_remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    recover_unix_identity_quarantines(trusted_root)?;
    let root = open_unix_root(trusted_root)?;
    let (parent, name) = open_unix_parent_from_root(&root, trusted_root, path)?;
    quarantine_regular_file_if_identity_unix(
        &root,
        &parent,
        &name,
        expected_device,
        expected_inode,
        before_remove,
    )
}

#[cfg(unix)]
fn quarantine_regular_file_if_identity_unix(
    trusted_root: &std::os::fd::OwnedFd,
    parent: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    expected_device: u64,
    expected_inode: u64,
    before_remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};

    let descriptor = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let opened = rustix::fs::fstat(&descriptor).map_err(io::Error::from)?;
    if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
        || opened.st_dev as u64 != expected_device
        || opened.st_ino as u64 != expected_inode
    {
        return Err(io::Error::other(
            "file identity changed before descriptor-relative removal",
        ));
    }

    before_remove()?;
    let (quarantine_parent, quarantine, quarantine_name, quarantine_identity) =
        create_unix_identity_quarantine(
            trusted_root,
            UnixQuarantineKind::File,
            expected_device,
            expected_inode,
        )?;
    rustix::fs::renameat_with(parent, name, &quarantine, "object", RenameFlags::NOREPLACE)
        .map_err(io::Error::from)?;
    rustix::fs::fsync(parent).map_err(io::Error::from)?;
    rustix::fs::fsync(&quarantine).map_err(io::Error::from)?;
    rustix::fs::fsync(&quarantine_parent).map_err(io::Error::from)?;

    let quarantined = rustix::fs::statat(&quarantine, "object", AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)?;
    if FileType::from_raw_mode(quarantined.st_mode) != FileType::RegularFile
        || quarantined.st_dev as u64 != expected_device
        || quarantined.st_ino as u64 != expected_inode
    {
        return Err(io::Error::other(format!(
            "file identity changed at the atomic removal boundary; replacement preserved in `{quarantine_name}`"
        )));
    }
    finalize_unix_identity_quarantine(
        &quarantine_parent,
        &quarantine,
        &quarantine_name,
        quarantine_identity,
        UnixQuarantineKind::File,
        expected_device,
        expected_inode,
    )
}

#[cfg(unix)]
fn quarantine_directory_if_identity_unix(
    trusted_root: &std::os::fd::OwnedFd,
    parent: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    expected_device: u64,
    expected_inode: u64,
    kind: UnixQuarantineKind,
    before_remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};

    let descriptor = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let opened = rustix::fs::fstat(&descriptor).map_err(io::Error::from)?;
    if FileType::from_raw_mode(opened.st_mode) != FileType::Directory
        || opened.st_dev as u64 != expected_device
        || opened.st_ino as u64 != expected_inode
    {
        return Err(io::Error::other(
            "directory identity changed before descriptor-relative removal",
        ));
    }
    before_remove()?;
    let (quarantine_parent, quarantine, quarantine_name, quarantine_identity) =
        create_unix_identity_quarantine(trusted_root, kind, expected_device, expected_inode)?;
    rustix::fs::renameat_with(parent, name, &quarantine, "object", RenameFlags::NOREPLACE)
        .map_err(io::Error::from)?;
    rustix::fs::fsync(parent).map_err(io::Error::from)?;
    rustix::fs::fsync(&quarantine).map_err(io::Error::from)?;
    rustix::fs::fsync(&quarantine_parent).map_err(io::Error::from)?;
    let quarantined = rustix::fs::statat(&quarantine, "object", AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)?;
    if FileType::from_raw_mode(quarantined.st_mode) != FileType::Directory
        || quarantined.st_dev as u64 != expected_device
        || quarantined.st_ino as u64 != expected_inode
    {
        return Err(io::Error::other(format!(
            "directory identity changed at the atomic removal boundary; replacement preserved in `{quarantine_name}`"
        )));
    }
    finalize_unix_identity_quarantine(
        &quarantine_parent,
        &quarantine,
        &quarantine_name,
        quarantine_identity,
        kind,
        expected_device,
        expected_inode,
    )
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnixQuarantineKind {
    File,
    Directory,
    EmptyDirectory,
}

#[cfg(unix)]
fn create_unix_identity_quarantine(
    trusted_root: &std::os::fd::OwnedFd,
    kind: UnixQuarantineKind,
    expected_device: u64,
    expected_inode: u64,
) -> io::Result<(
    std::os::fd::OwnedFd,
    std::os::fd::OwnedFd,
    String,
    (u64, u64),
)> {
    use rustix::fs::{Mode, OFlags};

    let quarantine_parent = rustix::fs::openat(
        trusted_root,
        "..",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let root_metadata = rustix::fs::fstat(trusted_root).map_err(io::Error::from)?;
    let root_identity = (root_metadata.st_dev as u64, root_metadata.st_ino as u64);
    recover_unix_identity_quarantines_in_parent(&quarantine_parent, root_identity)?;
    let quarantine_name = unix_quarantine_name(root_identity)?;
    rustix::fs::mkdirat(
        &quarantine_parent,
        quarantine_name.as_str(),
        Mode::from_raw_mode(0o700),
    )
    .map_err(io::Error::from)?;
    rustix::fs::fsync(&quarantine_parent).map_err(io::Error::from)?;
    let quarantine = open_unix_child_directory(&quarantine_parent, quarantine_name.as_ref())?;
    let metadata = rustix::fs::fstat(&quarantine).map_err(io::Error::from)?;
    let quarantine_identity = (metadata.st_dev as u64, metadata.st_ino as u64);
    write_unix_quarantine_manifest(
        &quarantine,
        quarantine_identity,
        root_identity,
        kind,
        expected_device,
        expected_inode,
    )?;
    rustix::fs::fsync(&quarantine).map_err(io::Error::from)?;
    Ok((
        quarantine_parent,
        quarantine,
        quarantine_name,
        quarantine_identity,
    ))
}

#[cfg(unix)]
fn write_unix_quarantine_manifest(
    quarantine: &std::os::fd::OwnedFd,
    quarantine_identity: (u64, u64),
    root_identity: (u64, u64),
    kind: UnixQuarantineKind,
    expected_device: u64,
    expected_inode: u64,
) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        quarantine,
        "manifest",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(io::Error::from)?;
    let mut manifest = File::from(descriptor);
    write!(
        manifest,
        "version=2\nquarantine_device={}\nquarantine_inode={}\nroot_device={}\nroot_inode={}\nkind={}\nobject_device={expected_device}\nobject_inode={expected_inode}\n",
        quarantine_identity.0,
        quarantine_identity.1,
        root_identity.0,
        root_identity.1,
        match kind {
            UnixQuarantineKind::File => "file",
            UnixQuarantineKind::Directory => "directory",
            UnixQuarantineKind::EmptyDirectory => "empty_directory",
        }
    )?;
    manifest.sync_all()
}

#[cfg(unix)]
fn read_unix_quarantine_manifest(
    quarantine: &std::os::fd::OwnedFd,
) -> io::Result<((u64, u64), (u64, u64), UnixQuarantineKind, u64, u64)> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        quarantine,
        "manifest",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let mut contents = String::new();
    File::from(descriptor)
        .take(1025)
        .read_to_string(&mut contents)?;
    if contents.len() > 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unix quarantine manifest exceeds its fixed limit",
        ));
    }
    let value = |key: &str| {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unix quarantine manifest is missing `{key}`"),
                )
            })
    };
    if value("version")? != "2" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unix quarantine manifest version is unsupported",
        ));
    }
    let parse = |key: &str| -> io::Result<u64> {
        value(key)?.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unix quarantine manifest has invalid `{key}`"),
            )
        })
    };
    let kind = match value("kind")? {
        "file" => UnixQuarantineKind::File,
        "directory" => UnixQuarantineKind::Directory,
        "empty_directory" => UnixQuarantineKind::EmptyDirectory,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unix quarantine manifest has invalid `kind`",
            ));
        }
    };
    Ok((
        (parse("quarantine_device")?, parse("quarantine_inode")?),
        (parse("root_device")?, parse("root_inode")?),
        kind,
        parse("object_device")?,
        parse("object_inode")?,
    ))
}

#[cfg(unix)]
fn finalize_unix_identity_quarantine(
    quarantine_parent: &std::os::fd::OwnedFd,
    quarantine: &std::os::fd::OwnedFd,
    quarantine_name: &str,
    quarantine_identity: (u64, u64),
    kind: UnixQuarantineKind,
    expected_device: u64,
    expected_inode: u64,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType};

    let directory = rustix::fs::fstat(quarantine).map_err(io::Error::from)?;
    if (directory.st_dev as u64, directory.st_ino as u64) != quarantine_identity {
        return Err(io::Error::other(
            "Unix identity quarantine directory changed before collection",
        ));
    }
    let object = match rustix::fs::statat(quarantine, "object", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(object) => Some(object),
        Err(rustix::io::Errno::NOENT) => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(object) = object {
        let expected_kind = match kind {
            UnixQuarantineKind::File => FileType::RegularFile,
            UnixQuarantineKind::Directory | UnixQuarantineKind::EmptyDirectory => {
                FileType::Directory
            }
        };
        if FileType::from_raw_mode(object.st_mode) != expected_kind
            || object.st_dev as u64 != expected_device
            || object.st_ino as u64 != expected_inode
        {
            return Err(io::Error::other(format!(
                "Unix identity quarantine `{quarantine_name}` contains a replacement; preserving it"
            )));
        }
        match kind {
            UnixQuarantineKind::File => {
                rustix::fs::unlinkat(quarantine, "object", AtFlags::empty())
                    .map_err(io::Error::from)?;
            }
            UnixQuarantineKind::Directory => {
                let object = open_unix_child_directory(quarantine, "object".as_ref())?;
                remove_private_directory_contents_unix(&object)?;
                rustix::fs::unlinkat(quarantine, "object", AtFlags::REMOVEDIR)
                    .map_err(io::Error::from)?;
            }
            UnixQuarantineKind::EmptyDirectory => {
                rustix::fs::unlinkat(quarantine, "object", AtFlags::REMOVEDIR)
                    .map_err(io::Error::from)?;
            }
        }
    }
    rustix::fs::unlinkat(quarantine, "manifest", AtFlags::empty()).map_err(io::Error::from)?;
    rustix::fs::fsync(quarantine).map_err(io::Error::from)?;
    rustix::fs::unlinkat(quarantine_parent, quarantine_name, AtFlags::REMOVEDIR)
        .map_err(io::Error::from)?;
    rustix::fs::fsync(quarantine_parent).map_err(io::Error::from)
}

#[cfg(unix)]
fn recover_unix_identity_quarantines_in_parent(
    quarantine_parent: &std::os::fd::OwnedFd,
    root_identity: (u64, u64),
) -> io::Result<()> {
    use rustix::fs::{AtFlags, Dir};

    let owned_prefix = unix_quarantine_prefix(root_identity);
    let mut quarantines = Vec::new();
    for entry in Dir::read_from(quarantine_parent).map_err(io::Error::from)? {
        let entry = entry.map_err(io::Error::from)?;
        let name = entry.file_name().to_bytes();
        if name.starts_with(owned_prefix.as_bytes()) {
            quarantines.push(name.to_vec());
            if quarantines.len() > 64 {
                return Err(io::Error::other(
                    "Unix identity quarantine recovery exceeded its bounded entry limit",
                ));
            }
        }
    }
    use std::os::unix::ffi::OsStrExt;
    for name in quarantines {
        let name = std::ffi::OsStr::from_bytes(&name);
        let quarantine = open_unix_child_directory(quarantine_parent, name)?;
        let metadata = rustix::fs::fstat(&quarantine).map_err(io::Error::from)?;
        let manifest = match read_unix_quarantine_manifest(&quarantine) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if rustix::fs::statat(&quarantine, "object", AtFlags::SYMLINK_NOFOLLOW)
                    .is_err_and(|error| error == rustix::io::Errno::NOENT)
                {
                    rustix::fs::unlinkat(quarantine_parent, name, AtFlags::REMOVEDIR)
                        .map_err(io::Error::from)?;
                    continue;
                }
                return Err(io::Error::other(format!(
                    "unbound Unix identity quarantine `{}` was preserved",
                    name.to_string_lossy()
                )));
            }
            Err(error) => return Err(error),
        };
        let (quarantine_identity, manifest_root_identity, kind, device, inode) = manifest;
        if manifest_root_identity != root_identity {
            return Err(io::Error::other(format!(
                "Unix identity quarantine `{}` belongs to a different trusted root",
                name.to_string_lossy()
            )));
        }
        if quarantine_identity != (metadata.st_dev as u64, metadata.st_ino as u64) {
            return Err(io::Error::other(format!(
                "Unix identity quarantine `{}` was replaced",
                name.to_string_lossy()
            )));
        }
        finalize_unix_identity_quarantine(
            quarantine_parent,
            &quarantine,
            &name.to_string_lossy(),
            quarantine_identity,
            kind,
            device,
            inode,
        )?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn recover_unix_identity_quarantines(trusted_root: &Path) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags};

    let root = open_unix_root(trusted_root)?;
    let parent = rustix::fs::openat(
        &root,
        "..",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let root_metadata = rustix::fs::fstat(&root).map_err(io::Error::from)?;
    recover_unix_identity_quarantines_in_parent(
        &parent,
        (root_metadata.st_dev as u64, root_metadata.st_ino as u64),
    )
}

#[cfg(unix)]
fn unix_quarantine_prefix(root_identity: (u64, u64)) -> String {
    format!(
        ".locality-identity-delete-{:016x}-{:016x}-",
        root_identity.0, root_identity.1
    )
}

#[cfg(unix)]
fn unix_quarantine_name(root_identity: (u64, u64)) -> io::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        io::Error::other(format!("could not allocate quarantine name: {error}"))
    })?;
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "{}{}-{random}",
        unix_quarantine_prefix(root_identity),
        std::process::id()
    ))
}

#[cfg(unix)]
fn open_unix_parent_without_symlinks(
    trusted_root: &Path,
    path: &Path,
) -> io::Result<(std::os::fd::OwnedFd, std::ffi::OsString)> {
    let root = open_unix_root(trusted_root)?;
    open_unix_parent_from_root(&root, trusted_root, path)
}

#[cfg(unix)]
fn open_unix_parent_from_root(
    root: &std::os::fd::OwnedFd,
    trusted_root: &Path,
    path: &Path,
) -> io::Result<(std::os::fd::OwnedFd, std::ffi::OsString)> {
    validate_root_relative_path(trusted_root, path)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` has no parent", path.display()),
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` has no filename", path.display()),
        )
    })?;
    let mut directory = root.try_clone()?;
    let relative_parent = parent.strip_prefix(trusted_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path `{}` is outside trusted root `{}`",
                path.display(),
                trusted_root.display()
            ),
        )
    })?;
    for component in relative_parent.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path `{}` contains unsafe components", path.display()),
            ));
        };
        directory = open_unix_child_directory(&directory, component)?;
    }
    Ok((directory, name.to_os_string()))
}

#[cfg(any(
    test,
    not(all(
        unix,
        any(target_os = "linux", target_vendor = "apple", target_os = "redox")
    ))
))]
pub(crate) fn rename_noreplace_durable_with_sync(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors(source_root, source)?;
    validate_no_symlink_or_reparse_ancestors(destination_root, destination)?;
    rename_noreplace(source, destination)?;
    let source_parent = source.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source `{}` has no parent", source.display()),
        )
    })?;
    let destination_parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("destination `{}` has no parent", destination.display()),
        )
    })?;
    sync(destination_parent)?;
    if source_parent != destination_parent {
        sync(source_parent)?;
    }
    Ok(())
}

pub fn remove_dir_all_durable(trusted_root: &Path, path: &Path) -> io::Result<()> {
    remove_dir_all_durable_anchored(trusted_root, path)
}

/// Removes a Windows directory tree only when the descriptor opened at the
/// cleanup boundary has the expected volume and full 128-bit file identity.
/// The retained handle denies delete sharing, so the validated root cannot be
/// replaced before traversal and deletion complete.
#[cfg(windows)]
pub fn remove_dir_all_durable_if_identity_windows(
    trusted_root: &Path,
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
    expected_inode_high: u64,
) -> io::Result<()> {
    remove_dir_durable_if_identity_windows(
        trusted_root,
        path,
        crate::replica_materializer::WorkspaceGenerationIdentity {
            device: expected_device,
            inode: expected_inode,
            inode_high: expected_inode_high,
        },
        true,
    )
}

/// Removes an empty Windows directory only when the cleanup handle has the
/// expected identity. Unexpected children fail closed at `mark_delete`.
#[cfg(windows)]
pub fn remove_empty_dir_durable_if_identity_windows(
    trusted_root: &Path,
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
    expected_inode_high: u64,
) -> io::Result<()> {
    remove_dir_durable_if_identity_windows(
        trusted_root,
        path,
        crate::replica_materializer::WorkspaceGenerationIdentity {
            device: expected_device,
            inode: expected_inode,
            inode_high: expected_inode_high,
        },
        false,
    )
}

#[cfg(windows)]
fn remove_dir_durable_if_identity_windows(
    trusted_root: &Path,
    path: &Path,
    expected: crate::replica_materializer::WorkspaceGenerationIdentity,
    recursive: bool,
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    let (parents, name) = open_windows_parent_anchored(trusted_root, path, true)?;
    let parent = parents.last().expect("anchored cleanup parent");
    let (directory, directory_sync) =
        parent.open_directory_for_anchored_cleanup_with_sync(&name)?;
    let actual = directory.identity()?;
    if actual != expected {
        return Err(io::Error::other(
            "directory identity changed before anchored cleanup",
        ));
    }
    if recursive {
        directory.remove_contents(path, actual.device)?;
    }
    directory_sync.sync()?;
    drop(directory_sync);
    directory.mark_delete()?;
    parent.sync()
}

pub fn remove_dir_all_durable_anchored(trusted_root: &Path, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
        let (parents, name) = open_windows_parent_anchored(trusted_root, path, true)?;
        let parent = parents.last().expect("anchored cleanup parent");
        let (directory, directory_sync) =
            parent.open_directory_for_anchored_cleanup_with_sync(&name)?;
        let device = directory.identity()?.device;
        directory.remove_contents(path, device)?;
        directory_sync.sync()?;
        drop(directory_sync);
        directory.mark_delete()?;
        parent.sync()?;
        return Ok(());
    }
    #[cfg(unix)]
    {
        remove_dir_all_durable_unix(trusted_root, path)
    }
    #[cfg(not(any(unix, windows)))]
    unsupported_descriptor_relative_operation()
}

pub fn remove_path_durable(trusted_root: &Path, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            return remove_dir_all_durable_anchored(trusted_root, path);
        }
        if !metadata.is_file() || metadata_is_windows_reparse_point(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path `{}` is not an ordinary file", path.display()),
            ));
        }
        let (parents, name) = open_windows_parent_anchored(trusted_root, path, true)?;
        let parent = parents.last().expect("anchored cleanup parent");
        parent.remove_file_for_anchored_cleanup(&name)?;
        parent.sync()?;
        return Ok(());
    }
    #[cfg(unix)]
    {
        remove_path_durable_unix(trusted_root, path)
    }
    #[cfg(not(any(unix, windows)))]
    unsupported_descriptor_relative_operation()
}

#[cfg(any(test, not(any(unix, windows))))]
pub(crate) fn remove_path_durable_with_sync(
    trusted_root: &Path,
    path: &Path,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is a symlink", path.display()),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` has no parent", path.display()),
        )
    })?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else if metadata.is_file() {
        fs::remove_file(path)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path `{}` is not a regular file or directory",
                path.display()
            ),
        ));
    }
    sync_parent(parent)
}

#[cfg(unix)]
pub(crate) fn same_volume(left: &Path, right: &Path) -> io::Result<bool> {
    let left = rustix::fs::stat(left).map_err(io::Error::from)?;
    let right = rustix::fs::stat(right).map_err(io::Error::from)?;
    Ok(left.st_dev == right.st_dev)
}

#[cfg(windows)]
pub(crate) fn same_volume(left: &Path, right: &Path) -> io::Result<bool> {
    Ok(windows_volume_path(left)?.eq_ignore_ascii_case(&windows_volume_path(right)?))
}

#[cfg(windows)]
fn windows_volume_path(path: &Path) -> io::Result<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut volume = vec![0_u16; 32_768];
    let found = unsafe {
        GetVolumePathNameW(
            path.as_ptr(),
            volume.as_mut_ptr(),
            volume
                .len()
                .try_into()
                .expect("volume buffer length fits u32"),
        )
    };
    if found == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = volume
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(volume.len());
    String::from_utf16(&volume[..length])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn same_volume(_left: &Path, _right: &Path) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "volume identity is unavailable on this platform",
    ))
}

pub fn validate_no_symlink_or_reparse_ancestors(
    trusted_root: &Path,
    path: &Path,
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors_impl(trusted_root, path, false)
}

pub fn validate_no_symlink_or_reparse_ancestors_allow_final(
    trusted_root: &Path,
    path: &Path,
) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors_impl(trusted_root, path, true)
}

fn validate_no_symlink_or_reparse_ancestors_impl(
    trusted_root: &Path,
    path: &Path,
    allow_final_reparse: bool,
) -> io::Result<()> {
    if !trusted_root.is_absolute() || !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "trusted root `{}` and path `{}` must be absolute",
                trusted_root.display(),
                path.display()
            ),
        ));
    }
    let root_metadata = fs::symlink_metadata(trusted_root)?;
    if root_metadata.file_type().is_symlink()
        || metadata_is_windows_reparse_point(&root_metadata)
        || !root_metadata.is_dir()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "trusted root `{}` is not an ordinary directory",
                trusted_root.display()
            ),
        ));
    }
    let relative = path.strip_prefix(trusted_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path `{}` is outside trusted root `{}`",
                path.display(),
                trusted_root.display()
            ),
        )
    })?;
    if relative.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` contains unsafe components", path.display()),
        ));
    }
    let mut reached_root = false;
    for (index, ancestor) in path.ancestors().enumerate() {
        if ancestor == trusted_root {
            reached_root = true;
        }
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if reached_root {
                    break;
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        if (metadata.file_type().is_symlink() || metadata_is_windows_reparse_point(&metadata))
            && !(allow_final_reparse && index == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "path `{}` traverses symlink or reparse-point ancestor `{}`",
                    path.display(),
                    ancestor.display()
                ),
            ));
        }
        if reached_root {
            break;
        }
    }
    if !reached_root {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path `{}` does not reach trusted root `{}` without unsafe components",
                path.display(),
                trusted_root.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
pub fn sync_directory(trusted_root: &Path, path: &Path) -> io::Result<()> {
    let directory = open_unix_directory_without_symlinks(trusted_root, path)?;
    rustix::fs::fsync(&directory).map_err(io::Error::from)
}

#[cfg(windows)]
pub fn sync_directory(trusted_root: &Path, path: &Path) -> io::Result<()> {
    let directories = open_windows_directory_anchored(trusted_root, path)?;
    directories.last().expect("anchored directory").sync()
}

#[cfg(windows)]
fn sync_windows_directory_with(
    trusted_root: &Path,
    path: &Path,
    flush: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    let directory = OpenOptions::new()
        .access_mode(windows_directory_sync_access())
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    flush(&directory)
}

#[cfg(windows)]
fn windows_directory_sync_access() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, SYNCHRONIZE,
    };
    FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE
}

#[cfg(not(any(unix, windows)))]
pub fn sync_directory(trusted_root: &Path, path: &Path) -> io::Result<()> {
    let _ = (trusted_root, path);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-relative durable filesystem operations are unavailable on this platform",
    ))
}

#[cfg(all(
    unix,
    any(target_os = "linux", target_vendor = "apple", target_os = "redox"),
    test
))]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_vendor = "apple", target_os = "redox"))
))]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let _ = (source, destination);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic rename without replacement is unavailable on this platform",
    ))
}

#[cfg(windows)]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    rename_noreplace_windows_with(
        source,
        destination,
        |source, destination, flags| unsafe { MoveFileExW(source, destination, flags) },
        MOVEFILE_WRITE_THROUGH,
    )
}

#[cfg(windows)]
fn rename_noreplace_windows_with(
    source: &Path,
    destination: &Path,
    move_file: impl FnOnce(*const u16, *const u16, u32) -> i32,
    flags: u32,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = move_file(source.as_ptr(), destination.as_ptr(), flags);
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let _ = (source, destination);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic rename without replacement is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_directory_create_rejects_post_validation_junction_substitution() {
        let container = temp_root("windows-anchored-directory-create-race");
        let root = container.join("trusted");
        let ancestor = root.join("ancestor");
        let displaced = root.join("displaced-ancestor");
        let outside = container.join("outside");
        let target = ancestor.join("created");
        fs::create_dir_all(&ancestor).expect("create trusted ancestor");
        fs::create_dir_all(&outside).expect("create outside directory");

        let error =
            create_dir_all_durable_windows_with_post_validation_hook(&root, &target, || {
                fs::rename(&ancestor, &displaced)
                    .expect("replace validated ancestor before directory create");
                create_windows_junction(&ancestor, &outside);
            })
            .expect_err("anchored directory create must reject the substituted junction");

        assert!(error.to_string().contains("reparse point"));
        assert!(!outside.join("created").exists());
        assert!(!displaced.join("created").exists());
        fs::remove_dir(&ancestor).expect("remove injected junction");
        fs::remove_dir_all(container).expect("remove directory race fixture");
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_create_rejects_post_validation_junction_substitution() {
        let container = temp_root("windows-anchored-file-create-race");
        let root = container.join("trusted");
        let ancestor = root.join("ancestor");
        let displaced = root.join("displaced-ancestor");
        let outside = container.join("outside");
        let target = ancestor.join("record");
        fs::create_dir_all(&ancestor).expect("create trusted ancestor");
        fs::create_dir_all(&outside).expect("create outside directory");

        let result = create_new_file_windows_with_post_validation_hook(&root, &target, || {
            fs::rename(&ancestor, &displaced)
                .expect("replace validated ancestor before file create");
            create_windows_junction(&ancestor, &outside);
        });
        let error = match result {
            Ok(_) => panic!("anchored file create must reject the substituted junction"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("reparse point"));
        assert!(!outside.join("record").exists());
        assert!(!displaced.join("record").exists());
        fs::remove_dir(&ancestor).expect("remove injected junction");
        fs::remove_dir_all(container).expect("remove file race fixture");
    }

    #[cfg(windows)]
    fn create_windows_junction(alias: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(alias)
            .arg(target)
            .status()
            .expect("create ancestor junction");
        assert!(status.success(), "mklink /J failed with {status}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_bound_tree_removal_refuses_racing_replacement() {
        let container = temp_root("windows-identity-tree-remove-race");
        let root = container.join("trusted");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("staging");
        let displaced = root.join("original-staging");
        fs::create_dir(&path).expect("create original staging");
        fs::write(path.join("old.txt"), b"original").expect("write original bytes");
        let (device, inode, inode_high) =
            windows_path_identity_no_follow(&path).expect("inspect original identity");
        let expected = crate::replica_materializer::WorkspaceGenerationIdentity {
            device,
            inode,
            inode_high,
        };

        fs::rename(&path, &displaced).expect("displace original staging");
        fs::create_dir(&path).expect("create replacement staging");
        fs::write(path.join("keep.txt"), b"replacement").expect("write replacement bytes");

        remove_dir_durable_if_identity_windows(&root, &path, expected, true)
            .expect_err("cleanup handle must reject the racing replacement");

        assert_eq!(fs::read(path.join("keep.txt")).unwrap(), b"replacement");
        assert_eq!(fs::read(displaced.join("old.txt")).unwrap(), b"original");
        fs::remove_dir_all(container).expect("remove test tree");
    }

    #[cfg(unix)]
    #[test]
    fn identity_stable_removal_never_unlinks_a_racing_replacement() {
        let container = temp_root("identity-stable-remove-race");
        let root = container.join("trusted");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("record");
        fs::write(&path, b"original").expect("write original");
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(&path).expect("inspect original");

        remove_regular_file_durable_if_identity_unix_with_hook(
            &root,
            &path,
            metadata.dev(),
            metadata.ino(),
            || {
                fs::remove_file(&path)?;
                fs::write(&path, b"replacement")
            },
        )
        .expect_err("atomic removal boundary rejects a replacement");

        let quarantined = fs::read_dir(&container)
            .expect("read quarantine parent")
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".locality-identity-delete-")
                    && matches!(
                        fs::read(entry.path().join("object")),
                        Ok(contents) if contents == b"replacement"
                    )
            })
            .expect("replacement remains quarantined");
        assert_eq!(
            fs::read(quarantined.path().join("object")).unwrap(),
            b"replacement"
        );
        fs::remove_dir_all(quarantined.path()).expect("remove test quarantine");
        let _ = fs::remove_dir_all(container);
    }

    #[cfg(unix)]
    #[test]
    fn identity_stable_tree_removal_never_unlinks_a_racing_replacement() {
        let container = temp_root("identity-stable-tree-remove-race");
        let root = container.join("trusted");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("tree");
        fs::create_dir(&path).expect("create original tree");
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(&path).expect("inspect original tree");
        let root_descriptor = open_unix_root(&root).unwrap();
        let (parent, name) = open_unix_parent_from_root(&root_descriptor, &root, &path).unwrap();

        quarantine_directory_if_identity_unix(
            &root_descriptor,
            &parent,
            &name,
            metadata.dev(),
            metadata.ino(),
            UnixQuarantineKind::Directory,
            || {
                fs::remove_dir(&path)?;
                fs::create_dir(&path)?;
                fs::write(path.join("keep.txt"), b"replacement tree")
            },
        )
        .expect_err("atomic tree removal boundary rejects a replacement");

        let quarantined = fs::read_dir(&container)
            .expect("read quarantine parent")
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".locality-identity-delete-")
                    && entry.path().join("object/keep.txt").is_file()
            })
            .expect("replacement tree remains quarantined");
        assert_eq!(
            fs::read(quarantined.path().join("object/keep.txt")).unwrap(),
            b"replacement tree"
        );
        fs::remove_dir_all(quarantined.path()).expect("remove test quarantine");
        let _ = fs::remove_dir_all(container);
    }

    #[cfg(unix)]
    #[test]
    fn identity_quarantine_unlinks_validated_bytes_without_accumulation() {
        let container = temp_root("identity-quarantine-unlink");
        let root = container.join("trusted");
        fs::create_dir_all(&root).expect("create trusted root");
        let path = root.join("sensitive");
        fs::write(&path, b"sensitive bytes").expect("write sensitive bytes");
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(&path).expect("inspect sensitive file");

        remove_regular_file_durable_if_identity_unix(&root, &path, metadata.dev(), metadata.ino())
            .expect("identity-bound removal");

        assert!(!path.exists());
        assert!(
            fs::read_dir(&container)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".locality-identity-delete-")),
            "successful cleanup leaves no sensitive quarantine"
        );
        let _ = fs::remove_dir_all(container);
    }

    #[cfg(unix)]
    #[test]
    fn identity_empty_quarantine_preserves_unexpected_children() {
        use std::os::unix::fs::MetadataExt;

        let container = temp_root("identity-empty-quarantine-child");
        let root = container.join("trusted");
        let path = root.join("staging");
        fs::create_dir_all(&path).expect("create staging directory");
        let metadata = fs::metadata(&path).expect("inspect staging directory");
        fs::write(path.join("unexpected.txt"), b"do not delete").expect("inject unexpected child");

        remove_empty_dir_durable_if_identity_unix(&root, &path, metadata.dev(), metadata.ino())
            .expect_err("empty-directory cleanup refuses recursive deletion");

        let quarantined = fs::read_dir(&container)
            .expect("read quarantine parent")
            .filter_map(Result::ok)
            .find(|entry| entry.path().join("object/unexpected.txt").is_file())
            .expect("unexpected child remains quarantined for review");
        assert_eq!(
            fs::read(quarantined.path().join("object/unexpected.txt")).unwrap(),
            b"do not delete"
        );
        fs::remove_dir_all(quarantined.path()).expect("remove test quarantine");
        let _ = fs::remove_dir_all(container);
    }

    #[cfg(unix)]
    #[test]
    fn identity_quarantine_recovers_crash_after_atomic_rename() {
        use rustix::fs::RenameFlags;
        use std::os::unix::fs::MetadataExt;

        let container = temp_root("identity-quarantine-crash-recovery");
        let root = container.join("trusted");
        fs::create_dir_all(&root).expect("create trusted root");
        let path = root.join("sensitive");
        fs::write(&path, b"crash-sensitive bytes").expect("write sensitive bytes");
        let metadata = fs::metadata(&path).expect("inspect sensitive file");
        let root_descriptor = open_unix_root(&root).unwrap();
        let (source_parent, source_name) =
            open_unix_parent_from_root(&root_descriptor, &root, &path).unwrap();
        let (quarantine_parent, quarantine, quarantine_name, _) = create_unix_identity_quarantine(
            &root_descriptor,
            UnixQuarantineKind::File,
            metadata.dev(),
            metadata.ino(),
        )
        .expect("prepare durable quarantine");
        rustix::fs::renameat_with(
            &source_parent,
            &source_name,
            &quarantine,
            "object",
            RenameFlags::NOREPLACE,
        )
        .expect("simulate atomic quarantine before crash");
        rustix::fs::fsync(&source_parent).unwrap();
        rustix::fs::fsync(&quarantine).unwrap();
        drop(quarantine);
        drop(quarantine_parent);
        assert!(container.join(&quarantine_name).exists());

        recover_unix_identity_quarantines(&root).expect("recover crashed quarantine");

        assert!(!container.join(quarantine_name).exists());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(container);
    }

    #[cfg(unix)]
    #[test]
    fn identity_quarantine_recovery_is_bounded_before_collection() {
        use std::os::unix::fs::MetadataExt;

        let container = temp_root("identity-quarantine-bounded-recovery");
        let root = container.join("trusted");
        fs::create_dir_all(&root).expect("create trusted root");
        let metadata = fs::metadata(&root).expect("inspect trusted root");
        let prefix = unix_quarantine_prefix((metadata.dev(), metadata.ino()));
        for index in 0..65 {
            fs::create_dir(container.join(format!("{prefix}bounded-{index}")))
                .expect("create synthetic quarantine");
        }

        let error = recover_unix_identity_quarantines(&root)
            .expect_err("recovery refuses an unbounded quarantine scan");

        assert!(error.to_string().contains("bounded entry limit"));
        let _ = fs::remove_dir_all(container);
    }

    #[test]
    fn write_sync_failure_stops_before_parent_sync_and_preserves_written_file() {
        let root = temp_root("write-sync-failure");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("record.json");
        let parent_syncs = RefCell::new(Vec::new());

        let error = write_new_file_durable_with_sync(
            &root,
            &path,
            b"record",
            |_| Err(io::Error::other("injected file sync failure")),
            |parent| {
                parent_syncs.borrow_mut().push(parent.to_path_buf());
                Ok(())
            },
        )
        .expect_err("file sync failure must propagate");

        assert_eq!(error.to_string(), "injected file sync failure");
        assert!(parent_syncs.borrow().is_empty());
        assert_eq!(fs::read(&path).expect("written file remains"), b"record");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copy_sync_failure_stops_before_parent_sync_and_preserves_copied_file() {
        let root = temp_root("copy-sync-failure");
        fs::create_dir_all(&root).expect("create root");
        let source = root.join("source");
        let destination = root.join("destination");
        fs::write(&source, "preserved edit").expect("write source");
        let parent_syncs = RefCell::new(Vec::new());

        let error = copy_new_file_durable_with_sync(
            &root,
            &source,
            &root,
            &destination,
            |_| Err(io::Error::other("injected copied-file sync failure")),
            |parent| {
                parent_syncs.borrow_mut().push(parent.to_path_buf());
                Ok(())
            },
        )
        .expect_err("copied file sync failure must propagate");

        assert_eq!(error.to_string(), "injected copied-file sync failure");
        assert!(parent_syncs.borrow().is_empty());
        assert_eq!(
            fs::read_to_string(&destination).expect("copied file remains"),
            "preserved edit"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn directory_sync_failure_stops_before_creating_deeper_children() {
        let root = temp_root("directory-sync-failure");
        fs::create_dir_all(&root).expect("create root");
        let first = root.join("first");
        let target = first.join("second");
        let synced = RefCell::new(Vec::new());

        let error = create_dir_all_durable_with_sync(&root, &target, |path| {
            synced.borrow_mut().push(path.to_path_buf());
            if path == first {
                Err(io::Error::other("injected created-directory sync failure"))
            } else {
                Ok(())
            }
        })
        .expect_err("created directory sync failure must propagate");

        assert_eq!(error.to_string(), "injected created-directory sync failure");
        assert_eq!(synced.into_inner(), vec![root.clone(), first.clone()]);
        assert!(first.is_dir());
        assert!(!target.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn durable_operations_reject_symlink_ancestors() {
        use std::os::unix::fs::symlink;

        let root = temp_root("symlink-ancestor");
        let owned = root.join("owned");
        let real = root.join("outside");
        let alias = owned.join("alias");
        fs::create_dir_all(&owned).expect("create owned root");
        fs::create_dir_all(&real).expect("create real directory");
        symlink(&real, &alias).expect("create alias");
        let path = alias.join("record");

        let error = write_new_file_durable(&owned, &path, b"record")
            .expect_err("symlink ancestor must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("symlink"));
        assert!(!real.join("record").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn retained_root_descriptor_cannot_be_redirected_after_open() {
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::symlink;

        let root = temp_root("retained-root-descriptor");
        let owned = root.join("owned");
        let moved = root.join("moved-owned");
        let outside = root.join("outside");
        fs::create_dir_all(&owned).expect("create owned root");
        fs::create_dir_all(&outside).expect("create outside root");
        let anchor = open_unix_root(&owned).expect("open retained root descriptor");

        fs::rename(&owned, &moved).expect("move owned root after open");
        symlink(&outside, &owned).expect("replace pathname with symlink");
        let descriptor = rustix::fs::openat(
            &anchor,
            "record",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .expect("descriptor-relative create remains anchored");
        let mut file = File::from(descriptor);
        file.write_all(b"anchored").expect("write anchored file");

        assert_eq!(fs::read(moved.join("record")).unwrap(), b"anchored");
        assert!(!outside.join("record").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn anchored_cloud_copy_rejects_symlink_inside_source_root() {
        use std::os::unix::fs::symlink;

        let root = temp_root("copy-symlink-ancestor");
        let source_root = root.join("source");
        let destination_root = root.join("destination");
        let outside = root.join("outside");
        fs::create_dir_all(&source_root).expect("create source root");
        fs::create_dir_all(&destination_root).expect("create destination root");
        fs::create_dir_all(&outside).expect("create outside root");
        fs::write(outside.join("edit.md"), "outside").expect("write outside file");
        symlink(&outside, source_root.join("alias")).expect("create source alias");

        let error = copy_new_file_durable_allow_cloud_placeholder(
            &source_root,
            &source_root.join("alias/edit.md"),
            &destination_root,
            &destination_root.join("edit.md"),
        )
        .expect_err("copy must reject an internal source alias");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!destination_root.join("edit.md").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn trusted_root_ignores_symlinked_system_prefix() {
        use std::os::unix::fs::symlink;

        let root = temp_root("trusted-system-prefix");
        let real_prefix = root.join("real-prefix");
        let alias_prefix = root.join("system-prefix");
        let real_owned = real_prefix.join("owned");
        fs::create_dir_all(&real_owned).expect("create owned root");
        symlink(&real_prefix, &alias_prefix).expect("create system prefix alias");
        let trusted_root = alias_prefix.join("owned");
        let path = trusted_root.join("record");

        validate_no_symlink_or_reparse_ancestors(&trusted_root, &path)
            .expect("aliases above the explicit trusted root are accepted");
        write_new_file_durable(&trusted_root, &path, b"record")
            .expect("write beneath trusted root");
        assert_eq!(
            fs::read_to_string(real_owned.join("record")).unwrap(),
            "record"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn macos_var_temp_prefix_is_outside_validation_boundary() {
        let trusted_root =
            std::env::temp_dir().join(format!("locality-macos-prefix-{}", std::process::id()));
        fs::create_dir_all(&trusted_root).expect("create trusted temp root");
        let path = trusted_root.join("record");

        validate_no_symlink_or_reparse_ancestors(&trusted_root, &path)
            .expect("the /var system alias is above the trusted root");
        let _ = fs::remove_dir_all(trusted_root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_rename_requests_write_through_metadata_ordering() {
        use std::cell::Cell;
        use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;

        let flags_seen = Cell::new(0);
        rename_noreplace_windows_with(
            Path::new(r"C:\source"),
            Path::new(r"C:\destination"),
            |_, _, flags| {
                flags_seen.set(flags);
                1
            },
            MOVEFILE_WRITE_THROUGH,
        )
        .expect("injected move succeeds");
        assert_eq!(flags_seen.get(), MOVEFILE_WRITE_THROUGH);
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_flush_failure_is_surfaced() {
        let root = temp_root("windows-directory-flush-failure");
        fs::create_dir_all(&root).expect("create root");

        let error = sync_windows_directory_with(&root, &root, |_| {
            Err(io::Error::other("injected directory flush failure"))
        })
        .expect_err("directory flush failure must propagate");

        assert_eq!(error.to_string(), "injected directory flush failure");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_flush_requests_write_access() {
        use windows_sys::Win32::Storage::FileSystem::{FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA};

        let access = windows_directory_sync_access();
        assert_ne!(access & FILE_WRITE_DATA, 0);
        assert_ne!(access & FILE_WRITE_ATTRIBUTES, 0);
    }

    #[cfg(any(
        target_os = "linux",
        target_vendor = "apple",
        target_os = "redox",
        windows
    ))]
    #[test]
    fn rename_destination_parent_sync_failure_preserves_completed_no_replace_rename() {
        let root = temp_root("rename-destination-sync-failure");
        let source_parent = root.join("source");
        let destination_parent = root.join("destination");
        fs::create_dir_all(&source_parent).expect("create source parent");
        fs::create_dir_all(&destination_parent).expect("create destination parent");
        let source = source_parent.join("record");
        let destination = destination_parent.join("record");
        fs::write(&source, "record").expect("write source");

        let error =
            rename_noreplace_durable_with_sync(&root, &source, &root, &destination, |parent| {
                assert_eq!(parent, destination_parent);
                Err(io::Error::other("injected destination sync failure"))
            })
            .expect_err("destination parent sync failure must propagate");

        assert_eq!(error.to_string(), "injected destination sync failure");
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(&destination).expect("renamed destination remains"),
            "record"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(any(
        target_os = "linux",
        target_vendor = "apple",
        target_os = "redox",
        windows
    ))]
    #[test]
    fn rename_source_parent_sync_failure_occurs_after_destination_parent_sync() {
        let root = temp_root("rename-source-sync-failure");
        let source_parent = root.join("source");
        let destination_parent = root.join("destination");
        fs::create_dir_all(&source_parent).expect("create source parent");
        fs::create_dir_all(&destination_parent).expect("create destination parent");
        let source = source_parent.join("record");
        let destination = destination_parent.join("record");
        fs::write(&source, "record").expect("write source");
        let synced = RefCell::new(Vec::new());

        let error =
            rename_noreplace_durable_with_sync(&root, &source, &root, &destination, |parent| {
                synced.borrow_mut().push(parent.to_path_buf());
                if parent == source_parent {
                    Err(io::Error::other("injected source sync failure"))
                } else {
                    Ok(())
                }
            })
            .expect_err("source parent sync failure must propagate");

        assert_eq!(error.to_string(), "injected source sync failure");
        assert_eq!(synced.into_inner(), vec![destination_parent, source_parent]);
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(&destination).expect("renamed destination remains"),
            "record"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_parent_sync_failure_preserves_completed_removal() {
        let root = temp_root("removal-sync-failure");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("record");
        fs::write(&path, "record").expect("write record");

        let error = remove_path_durable_with_sync(&root, &path, |parent| {
            assert_eq!(parent, root);
            Err(io::Error::other("injected removal sync failure"))
        })
        .expect_err("removal parent sync failure must propagate");

        assert_eq!(error.to_string(), "injected removal sync failure");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn volume_identity_matches_for_a_directory_and_its_child() {
        let root = temp_root("same-volume");
        let child = root.join("child");
        fs::create_dir_all(&child).expect("create child");

        assert!(same_volume(&root, &child).expect("query volume identity"));
        let _ = fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .canonicalize()
            .expect("canonical temp directory")
            .join(format!(
                "loc-durable-fs-{label}-{}-{timestamp}-{sequence}",
                std::process::id()
            ))
    }
}
