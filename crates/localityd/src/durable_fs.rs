//! Strict filesystem durability primitives shared by recovery paths.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(crate) fn create_dir_all_durable(path: &Path) -> io::Result<()> {
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
                if metadata.file_type().is_symlink() {
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

pub(crate) fn write_new_file_durable(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_new_file_durable_with_sync(path, contents, |file| file.sync_all(), sync_directory)
}

pub(crate) fn write_new_file_durable_with_sync(
    path: &Path,
    contents: &[u8],
    sync_file: impl FnOnce(&File) -> io::Result<()>,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
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

pub(crate) fn rename_noreplace_durable(source: &Path, destination: &Path) -> io::Result<()> {
    rename_noreplace_durable_with_sync(source, destination, sync_directory)
}

pub(crate) fn rename_noreplace_durable_with_sync(
    source: &Path,
    destination: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
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

pub(crate) fn remove_dir_all_durable(path: &Path) -> io::Result<()> {
    remove_dir_all_durable_with_sync(path, sync_directory)
}

pub(crate) fn remove_dir_all_durable_with_sync(
    path: &Path,
    mut sync_parent: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
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

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
    // File::sync_all flushes file handles and rename requests write-through,
    // but Windows exposes no portable parent-directory fsync through std.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(all(
    unix,
    any(target_os = "linux", target_vendor = "apple", target_os = "redox")
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
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

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
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
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
        std::env::temp_dir().join(format!(
            "loc-durable-fs-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ))
    }
}
