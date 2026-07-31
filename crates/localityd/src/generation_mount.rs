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
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).write(true).create(true);
            let lock = root
                .open_with(GENERATION_MOUNT_LOCK_FILE, &options)?
                .into_std();
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
            match self.parent.symlink_metadata(name) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    Err(not_regular_file())
                }
                Ok(_) => self
                    .parent
                    .open(name)
                    .map(cap_std::fs::File::into_std)
                    .map(Some),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
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
            if self.open_named(name)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "generation preimage already exists",
                ));
            }
            self.parent.rename(&self.name, &self.parent, name)?;
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
            self.parent.hard_link(name, &self.parent, &self.name)?;
            self.parent.remove_file(name)?;
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
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            self.parent
                .open_with(name, &options)
                .map(cap_std::fs::File::into_std)
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
            self.parent.hard_link(temporary, &self.parent, &self.name)?;
            self.parent.remove_file(temporary)?;
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
    directory.try_clone()?.into_std_file().sync_all()
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
