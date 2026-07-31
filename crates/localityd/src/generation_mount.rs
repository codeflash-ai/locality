//! Capability-relative filesystem operations for generation delivery.
//!
//! Unix targets open every directory component with `O_NOFOLLOW` and retain
//! the parent handle across inspection and mutation. Windows opens the root
//! itself with no-follow semantics, validates the resulting handle, and then
//! uses `cap-std`'s handle-relative beneath-root operations.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

#[cfg(unix)]
use rustix::fd::OwnedFd;

pub(crate) const GENERATION_MOUNT_LOCK_FILE: &str = ".locality-generation.lock";
pub(crate) const GENERATION_STATE_LOCK_FILE: &str = ".generation-delivery.lock";

pub(crate) struct GenerationStateLock {
    _file: File,
}

pub(crate) struct SecureMount {
    #[cfg(unix)]
    root: OwnedFd,
    #[cfg(windows)]
    root: cap_std::fs::Dir,
    #[cfg(windows)]
    _lock: File,
}

pub(crate) struct SecureTarget {
    #[cfg(unix)]
    parent: OwnedFd,
    #[cfg(windows)]
    parent: cap_std::fs::Dir,
    name: OsString,
}

impl SecureMount {
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, open};

            let root = open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            rustix::fs::flock(&root, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
            Ok(Self { root })
        }
        #[cfg(windows)]
        {
            let root = open_windows_root(root)?;
            let lock = open_windows_regular_file(
                &root,
                OsStr::new(GENERATION_MOUNT_LOCK_FILE),
                true,
                true,
                false,
                false,
            )?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "mount lock disappeared"))?;
            lock_windows_file(&lock)?;
            Ok(Self { root, _lock: lock })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = root;
            Err(unsupported_platform())
        }
    }

    pub(crate) fn target(&self, relative: &Path, create_parents: bool) -> io::Result<SecureTarget> {
        let parent = relative.parent().ok_or_else(invalid_relative_path)?;
        let name = relative
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(invalid_relative_path)?
            .to_os_string();

        #[cfg(unix)]
        {
            let mut current = rustix::io::dup(&self.root)?;
            for component in parent.components() {
                let Component::Normal(component) = component else {
                    return Err(invalid_relative_path());
                };
                current = open_child_directory(current, component, create_parents)?;
            }
            Ok(SecureTarget {
                parent: current,
                name,
            })
        }
        #[cfg(windows)]
        {
            let mut current = self.root.try_clone()?;
            for component in parent.components() {
                let Component::Normal(component) = component else {
                    return Err(invalid_relative_path());
                };
                current = match open_windows_child_directory(&current, component) {
                    Ok(child) => child,
                    Err(error) if error.kind() == io::ErrorKind::NotFound && create_parents => {
                        current.create_dir(component)?;
                        sync_cap_dir(&current)?;
                        open_windows_child_directory(&current, component)?
                    }
                    Err(error) => return Err(error),
                };
            }
            Ok(SecureTarget {
                parent: current,
                name,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (relative, create_parents, name);
            Err(unsupported_platform())
        }
    }
}

impl GenerationStateLock {
    pub(crate) fn acquire(root: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, open, openat};

            let directory = open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let file = openat(
                &directory,
                GENERATION_STATE_LOCK_FILE,
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?;
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
            Ok(Self {
                _file: File::from(file),
            })
        }
        #[cfg(windows)]
        {
            let directory = open_windows_root(root)?;
            let file = open_windows_regular_file(
                &directory,
                OsStr::new(GENERATION_STATE_LOCK_FILE),
                true,
                true,
                false,
                false,
            )?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "state lock disappeared"))?;
            lock_windows_file(&file)?;
            Ok(Self { _file: file })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = root;
            Err(unsupported_platform())
        }
    }
}

#[cfg(unix)]
fn open_child_directory(parent: OwnedFd, name: &OsStr, create: bool) -> io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, fsync, mkdirat, openat};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(&parent, name, flags, Mode::empty()) {
        Ok(child) => Ok(child),
        Err(error) if error == rustix::io::Errno::NOENT && create => {
            mkdirat(&parent, name, Mode::from_raw_mode(0o755))?;
            fsync(&parent)?;
            openat(&parent, name, flags, Mode::empty()).map_err(io::Error::from)
        }
        Err(error) => Err(error.into()),
    }
}

impl SecureTarget {
    pub(crate) fn open_current(&self) -> io::Result<Option<File>> {
        self.open_named(&self.name)
    }

    pub(crate) fn open_named(&self, name: &OsStr) -> io::Result<Option<File>> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, openat};

            match openat(
                &self.parent,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => {
                    let file = File::from(file);
                    if !file.metadata()?.is_file() {
                        return Err(not_regular_file());
                    }
                    Ok(Some(file))
                }
                Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
        #[cfg(windows)]
        {
            open_windows_regular_file(&self.parent, name, false, false, false, true)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(unsupported_platform())
        }
    }

    pub(crate) fn move_current_to(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::renameat_with(
                &self.parent,
                &self.name,
                &self.parent,
                name,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            rustix::fs::fsync(&self.parent)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            rename_windows_noreplace(&self.parent, &self.name, name)?;
            sync_cap_dir(&self.parent)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(unsupported_platform())
        }
    }

    pub(crate) fn restore_named(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::renameat_with(
                &self.parent,
                name,
                &self.parent,
                &self.name,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            rustix::fs::fsync(&self.parent)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            rename_windows_noreplace(&self.parent, name, &self.name)?;
            sync_cap_dir(&self.parent)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(unsupported_platform())
        }
    }

    pub(crate) fn create_named(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, openat};

            openat(
                &self.parent,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map(File::from)
            .map_err(io::Error::from)
        }
        #[cfg(windows)]
        {
            open_windows_regular_file(&self.parent, name, true, true, true, true)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "created file disappeared"))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(unsupported_platform())
        }
    }

    pub(crate) fn publish_named(&self, temporary: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::linkat(
                &self.parent,
                temporary,
                &self.parent,
                &self.name,
                rustix::fs::AtFlags::empty(),
            )?;
            rustix::fs::unlinkat(&self.parent, temporary, rustix::fs::AtFlags::empty())?;
            rustix::fs::fsync(&self.parent)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            rename_windows_noreplace(&self.parent, temporary, &self.name)?;
            sync_cap_dir(&self.parent)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = temporary;
            Err(unsupported_platform())
        }
    }

    pub(crate) fn remove_named(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::unlinkat(&self.parent, name, rustix::fs::AtFlags::empty())?;
            rustix::fs::fsync(&self.parent)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            self.parent.remove_file(name)?;
            sync_cap_dir(&self.parent)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(unsupported_platform())
        }
    }

    pub(crate) fn remove_current(&self) -> io::Result<()> {
        self.remove_named(&self.name)
    }
}

#[cfg(windows)]
fn sync_cap_dir(directory: &cap_std::fs::Dir) -> io::Result<()> {
    match directory.try_clone()?.into_std_file().sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
            ) =>
        {
            // Windows does not guarantee FlushFileBuffers support for
            // directory handles. File bodies are flushed before publication.
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn open_windows_root(path: &Path) -> io::Result<cap_std::fs::Dir> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, OPEN_EXISTING,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedHandle::from_raw_handle(handle as _) };
    let root = File::from(owned);
    validate_windows_directory_handle(&root)?;
    Ok(cap_std::fs::Dir::from_std_file(root))
}

#[cfg(windows)]
fn open_windows_child_directory(
    parent: &cap_std::fs::Dir,
    name: &OsStr,
) -> io::Result<cap_std::fs::Dir> {
    use cap_std::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let child = parent.open_with(name, &options)?.into_std();
    validate_windows_directory_handle(&child)?;
    Ok(cap_std::fs::Dir::from_std_file(child))
}

#[cfg(windows)]
fn open_windows_regular_file(
    parent: &cap_std::fs::Dir,
    name: &OsStr,
    write: bool,
    create: bool,
    exclusive: bool,
    share_delete: bool,
) -> io::Result<Option<File>> {
    use cap_std::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = cap_std::fs::OpenOptions::new();
    let share_mode =
        FILE_SHARE_READ | FILE_SHARE_WRITE | if share_delete { FILE_SHARE_DELETE } else { 0 };
    options
        .read(true)
        .write(write)
        .share_mode(share_mode)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    if create {
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true);
        }
    }
    match parent.open_with(name, &options) {
        Ok(file) => {
            let file = file.into_std();
            validate_windows_regular_handle(&file)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Renames through an already-open parent and asks NTFS/ReFS to reject an
/// existing destination in the same kernel operation. A path preflight plus
/// `rename` is not sufficient because another process can create the evidence
/// leaf between those calls.
#[cfg(windows)]
fn rename_windows_noreplace(
    parent: &cap_std::fs::Dir,
    source_name: &OsStr,
    destination_name: &OsStr,
) -> io::Result<()> {
    use cap_std::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::STATUS_OBJECT_NAME_EXISTS;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let mut options = cap_std::fs::OpenOptions::new();
    options
        .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    validate_windows_leaf_name(source_name)?;
    let source = parent.open_with(source_name, &options)?.into_std();
    validate_windows_regular_handle(&source)?;
    let destination = validate_windows_leaf_name(destination_name)?;
    let byte_length = rename_information_buffer_size(destination.len())?;
    let byte_length_u32 = u32::try_from(byte_length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer is too large"))?;
    let name_byte_length = destination
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename leaf is too long"))?;
    let mut buffer = vec![0_usize; byte_length.div_ceil(std::mem::size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let root = parent.try_clone()?.into_std_file();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = root.as_raw_handle() as _;
        (*info).FileNameLength = name_byte_length;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            destination.len(),
        );
    }
    let mut status_block = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
    let status = unsafe {
        NtSetInformationFile(
            source.as_raw_handle() as _,
            &mut status_block,
            info.cast(),
            byte_length_u32,
            FileRenameInformation,
        )
    };
    if status < 0 || status == STATUS_OBJECT_NAME_EXISTS {
        return Err(rename_status_error(status));
    }
    Ok(())
}

#[cfg(windows)]
fn rename_status_error(status: windows_sys::Win32::Foundation::NTSTATUS) -> io::Error {
    use windows_sys::Win32::Foundation::{
        RtlNtStatusToDosError, STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_COLLISION,
        STATUS_OBJECT_NAME_EXISTS, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    };

    let source = io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32);
    let kind = match status {
        STATUS_OBJECT_NAME_COLLISION | STATUS_OBJECT_NAME_EXISTS => io::ErrorKind::AlreadyExists,
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_NO_SUCH_FILE => {
            io::ErrorKind::NotFound
        }
        _ => source.kind(),
    };
    io::Error::new(kind, source)
}

#[cfg(windows)]
fn validate_windows_leaf_name(name: &OsStr) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let name = name.encode_wide().collect::<Vec<_>>();
    if name.is_empty()
        || name
            .iter()
            .any(|unit| *unit == 0 || *unit == b'\\' as u16 || *unit == b'/' as u16)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "generation rename name is not a leaf",
        ));
    }
    Ok(name)
}

#[cfg(windows)]
fn rename_information_buffer_size(name_units: usize) -> io::Result<usize> {
    use windows_sys::Wdk::Storage::FileSystem::FILE_RENAME_INFORMATION;

    let trailing_units = name_units
        .checked_sub(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename leaf is empty"))?;
    let trailing_bytes = trailing_units
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer overflow"))?;
    // FILE_RENAME_INFORMATION already includes one UTF-16 code unit plus tail
    // padding. Using offset_of(FileName) omits that padding and Windows rejects
    // the otherwise valid request with ERROR_INVALID_PARAMETER.
    std::mem::size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(trailing_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer overflow"))
}

#[cfg(windows)]
fn validate_windows_regular_handle(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FileAttributeTagInfo, GetFileInformationByHandleEx,
    };

    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileAttributeTagInfo,
            (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if attributes.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err(not_regular_file());
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_directory_handle(directory: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FileAttributeTagInfo, GetFileInformationByHandleEx,
    };

    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle() as _,
            FileAttributeTagInfo,
            (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "generation mount root handle is not a no-follow directory",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn lock_windows_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped = OVERLAPPED::default();
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn unsupported_platform() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "generation apply requires no-follow handle-relative filesystem support",
    )
}

fn invalid_relative_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "generation path is not a relative file path",
    )
}

fn not_regular_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "generation path is not a regular file",
    )
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Seek, SeekFrom};
    use std::mem::size_of;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "locality-generation-mount-{label}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn native_displacement_never_replaces_an_existing_leaf() {
        let root = temp_root("noreplace-existing");
        fs::write(root.join("source"), b"source").unwrap();
        fs::write(root.join("evidence"), b"evidence").unwrap();
        let directory = open_windows_root(&root).unwrap();
        let error =
            rename_windows_noreplace(&directory, OsStr::new("source"), OsStr::new("evidence"))
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(root.join("source")).unwrap(), b"source");
        assert_eq!(fs::read(root.join("evidence")).unwrap(), b"evidence");
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_rename_buffer_includes_struct_tail_and_maps_statuses() {
        use windows_sys::Wdk::Storage::FileSystem::FILE_RENAME_INFORMATION;
        use windows_sys::Win32::Foundation::{
            STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
            STATUS_OBJECT_PATH_NOT_FOUND,
        };

        assert_eq!(
            rename_information_buffer_size(1).unwrap(),
            size_of::<FILE_RENAME_INFORMATION>()
        );
        assert_eq!(
            rename_information_buffer_size(8).unwrap(),
            size_of::<FILE_RENAME_INFORMATION>() + 7 * size_of::<u16>()
        );
        assert!(rename_information_buffer_size(0).is_err());
        assert!(
            rename_information_buffer_size(8).unwrap()
                > std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName) + 8 * size_of::<u16>()
        );
        assert_eq!(
            rename_status_error(STATUS_OBJECT_NAME_COLLISION).kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            rename_status_error(STATUS_OBJECT_NAME_NOT_FOUND).kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            rename_status_error(STATUS_OBJECT_PATH_NOT_FOUND).kind(),
            io::ErrorKind::NotFound
        );
        for invalid in ["", "../escape", "child/name", "child\\name"] {
            assert_eq!(
                validate_windows_leaf_name(OsStr::new(invalid))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn native_displacement_moves_to_the_anchored_leaf() {
        let root = temp_root("rename-success");
        fs::write(root.join("source"), b"preserved bytes").unwrap();
        let directory = open_windows_root(&root).unwrap();

        rename_windows_noreplace(&directory, OsStr::new("source"), OsStr::new("evidence")).unwrap();

        assert!(!root.join("source").exists());
        assert_eq!(fs::read(root.join("evidence")).unwrap(), b"preserved bytes");
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_ordinary_handle_does_not_block_displacement() {
        let root = temp_root("retained-handle");
        fs::write(root.join("source"), b"retained bytes").unwrap();
        let directory = open_windows_root(&root).unwrap();
        let mut retained =
            open_windows_regular_file(&directory, OsStr::new("source"), false, false, false, true)
                .unwrap()
                .unwrap();

        rename_windows_noreplace(&directory, OsStr::new("source"), OsStr::new("evidence")).unwrap();

        let mut bytes = Vec::new();
        retained.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"retained bytes");
        assert!(!root.join("source").exists());
        assert_eq!(fs::read(root.join("evidence")).unwrap(), b"retained bytes");
        drop(retained);
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_handle_cannot_be_displaced() {
        let root = temp_root("lock-nonreplaceable");
        fs::write(root.join("lock"), b"lock bytes").unwrap();
        let directory = open_windows_root(&root).unwrap();
        let mut lock =
            open_windows_regular_file(&directory, OsStr::new("lock"), true, false, false, false)
                .unwrap()
                .unwrap();
        lock_windows_file(&lock).unwrap();

        let error =
            rename_windows_noreplace(&directory, OsStr::new("lock"), OsStr::new("displaced"))
                .unwrap_err();
        assert_eq!(
            error.raw_os_error().map(|code| code as u32),
            Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION)
        );

        // An exclusive LockFileEx byte-range lock rejects overlapping reads
        // through a second handle with ERROR_LOCK_VIOLATION. Verify the bytes
        // through the handle that owns the lock instead.
        lock.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        lock.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"lock bytes");
        assert!(root.join("lock").exists());
        assert!(!root.join("displaced").exists());
        drop(lock);
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_displacement_race_has_exactly_one_winner() {
        let root = temp_root("noreplace-race");
        fs::write(root.join("source-a"), b"a").unwrap();
        fs::write(root.join("source-b"), b"b").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for source in ["source-a", "source-b"] {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                let directory = open_windows_root(&root).unwrap();
                barrier.wait();
                rename_windows_noreplace(&directory, OsStr::new(source), OsStr::new("evidence"))
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|error| { error.kind() == io::ErrorKind::AlreadyExists }))
                .count(),
            1
        );
        drop(results);
        let evidence = fs::read(root.join("evidence")).unwrap();
        assert!(evidence == b"a" || evidence == b"b");
        assert_eq!(
            [root.join("source-a"), root.join("source-b")]
                .into_iter()
                .filter(|path| path.exists())
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }
}
