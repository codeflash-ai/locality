//! Strict filesystem durability primitives shared by recovery paths.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub fn create_dir_all_durable(path: &Path) -> io::Result<()> {
    create_dir_all_durable_with_sync(path, sync_directory)
}

pub(crate) fn create_dir_all_durable_with_sync(
    path: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
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

pub fn write_new_file_durable(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_new_file_durable_with_sync(path, contents, |file| file.sync_all(), sync_directory)
}

pub fn copy_new_file_durable(source: &Path, destination: &Path) -> io::Result<u64> {
    copy_new_file_durable_with_sync(source, destination, |file| file.sync_all(), sync_directory)
}

pub(crate) fn copy_new_file_durable_with_sync(
    source: &Path,
    destination: &Path,
    sync_file: impl FnOnce(&File) -> io::Result<()>,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<u64> {
    validate_from_parent(source)?;
    validate_from_parent(destination)?;
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

pub(crate) fn write_new_file_durable_with_sync(
    path: &Path,
    contents: &[u8],
    sync_file: impl FnOnce(&File) -> io::Result<()>,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_from_parent(path)?;
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

pub fn rename_noreplace_durable(source: &Path, destination: &Path) -> io::Result<()> {
    validate_from_parent(source)?;
    validate_from_parent(destination)?;
    #[cfg(all(
        unix,
        any(target_os = "linux", target_vendor = "apple", target_os = "redox")
    ))]
    {
        let source_root = source.parent().expect("validated source parent");
        let destination_root = destination.parent().expect("validated destination parent");
        return rename_noreplace_durable_unix(source_root, destination_root, source, destination);
    }
    #[cfg(not(all(
        unix,
        any(target_os = "linux", target_vendor = "apple", target_os = "redox")
    )))]
    {
        rename_noreplace_durable_with_sync(source, destination, sync_directory)
    }
}

pub fn rename_noreplace_durable_anchored(
    trusted_root: &Path,
    source: &Path,
    destination: &Path,
) -> io::Result<()> {
    #[cfg(windows)]
    validate_no_symlink_or_reparse_ancestors_allow_final(trusted_root, source)?;
    #[cfg(not(windows))]
    validate_no_symlink_or_reparse_ancestors(trusted_root, source)?;
    validate_no_symlink_or_reparse_ancestors(trusted_root, destination)?;
    #[cfg(all(
        unix,
        any(target_os = "linux", target_vendor = "apple", target_os = "redox")
    ))]
    {
        return rename_noreplace_durable_unix(trusted_root, trusted_root, source, destination);
    }
    #[cfg(windows)]
    {
        return rename_noreplace_durable_windows_anchored(trusted_root, source, destination);
    }
    #[cfg(not(any(
        all(
            unix,
            any(target_os = "linux", target_vendor = "apple", target_os = "redox")
        ),
        windows
    )))]
    {
        rename_noreplace_durable_with_sync(source, destination, sync_directory)
    }
}

#[cfg(windows)]
fn rename_noreplace_durable_windows_anchored(
    trusted_root: &Path,
    source: &Path,
    destination: &Path,
) -> io::Result<()> {
    let (source_parents, source_name) = open_windows_parent_anchored(trusted_root, source)?;
    let (destination_parents, destination_name) =
        open_windows_parent_anchored(trusted_root, destination)?;
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
fn open_windows_parent_anchored(
    trusted_root: &Path,
    path: &Path,
) -> io::Result<(
    Vec<crate::windows_workspace_fs::WindowsDirectory>,
    std::ffi::OsString,
)> {
    validate_no_symlink_or_reparse_ancestors_allow_final(trusted_root, path)?;
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
    let mut parents =
        vec![crate::windows_workspace_fs::WindowsDirectory::open_absolute_anchor(trusted_root)?];
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path `{}` contains unsafe components", path.display()),
            ));
        };
        let child = parents
            .last()
            .expect("anchored parent")
            .open_directory_anchor(component)?;
        parents.push(child);
    }
    Ok((parents, name.to_os_string()))
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

    let (source_parent, source_name) = open_unix_parent_without_symlinks(source_root, source)?;
    let (destination_parent, destination_name) =
        open_unix_parent_without_symlinks(destination_root, destination)?;
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

#[cfg(all(
    unix,
    any(target_os = "linux", target_vendor = "apple", target_os = "redox")
))]
fn open_unix_parent_without_symlinks(
    trusted_root: &Path,
    path: &Path,
) -> io::Result<(std::os::fd::OwnedFd, std::ffi::OsString)> {
    use rustix::fs::{Mode, OFlags};

    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
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
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory =
        rustix::fs::open(trusted_root, flags, Mode::empty()).map_err(io::Error::from)?;
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
        directory = rustix::fs::openat(&directory, component, flags, Mode::empty())
            .map_err(io::Error::from)?;
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
    source: &Path,
    destination: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_from_parent(source)?;
    validate_from_parent(destination)?;
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

pub fn remove_dir_all_durable(path: &Path) -> io::Result<()> {
    remove_dir_all_durable_with_sync(path, sync_directory)
}

pub fn remove_dir_all_durable_anchored(trusted_root: &Path, path: &Path) -> io::Result<()> {
    validate_no_symlink_or_reparse_ancestors(trusted_root, path)?;
    #[cfg(windows)]
    {
        let (parents, name) = open_windows_parent_anchored(trusted_root, path)?;
        let parent = parents.last().expect("anchored cleanup parent");
        let directory = parent.open_directory_for_anchored_cleanup(&name)?;
        let device = directory.identity()?.device;
        directory.remove_contents(path, device)?;
        directory.mark_delete()?;
        parent.sync()?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        remove_dir_all_durable(path)
    }
}

pub(crate) fn remove_dir_all_durable_with_sync(
    path: &Path,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_from_parent(path)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("directory `{}` has no parent", path.display()),
        )
    })?;
    fs::remove_dir_all(path)?;
    sync_parent(parent)
}

pub(crate) fn remove_path_durable(path: &Path) -> io::Result<()> {
    remove_path_durable_with_sync(path, sync_directory)
}

pub(crate) fn remove_path_durable_with_sync(
    path: &Path,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_from_parent(path)?;
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
    path.strip_prefix(trusted_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "path `{}` is outside trusted root `{}`",
                path.display(),
                trusted_root.display()
            ),
        )
    })?;
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

fn validate_from_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` has no parent", path.display()),
        )
    })?;
    validate_no_symlink_or_reparse_ancestors(parent, path)
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
pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
pub fn sync_directory(path: &Path) -> io::Result<()> {
    sync_windows_directory_with(path, |directory| match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
            ) =>
        {
            // FlushFileBuffers is not supported for directory handles on all
            // Windows filesystems. File bodies are flushed separately and
            // renames request MOVEFILE_WRITE_THROUGH.
            Ok(())
        }
        Err(error) => Err(error),
    })
}

#[cfg(windows)]
fn sync_windows_directory_with(
    path: &Path,
    flush: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    validate_no_symlink_or_reparse_ancestors(path, path)?;
    let directory = OpenOptions::new()
        .access_mode(windows_directory_sync_access())
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
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
pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
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

    #[test]
    fn write_sync_failure_stops_before_parent_sync_and_preserves_written_file() {
        let root = temp_root("write-sync-failure");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("record.json");
        let parent_syncs = RefCell::new(Vec::new());

        let error = write_new_file_durable_with_sync(
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
            &source,
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

        let error = create_dir_all_durable_with_sync(&target, |path| {
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

        let error = validate_no_symlink_or_reparse_ancestors(&owned, &path)
            .expect_err("symlink ancestor must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("symlink"));
        assert!(!real.join("record").exists());
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
        write_new_file_durable(&path, b"record").expect("write beneath trusted root");
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

        let error = sync_windows_directory_with(&root, |_| {
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

        let error = rename_noreplace_durable_with_sync(&source, &destination, |parent| {
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

        let error = rename_noreplace_durable_with_sync(&source, &destination, |parent| {
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

        let error = remove_path_durable_with_sync(&path, |parent| {
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
