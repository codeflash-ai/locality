//! Capability-relative filesystem operations for generation delivery.
//!
//! Unix targets open every directory component with `O_NOFOLLOW` and retain
//! the parent handle across inspection and mutation. Other targets use
//! `cap-std`'s handle-relative, beneath-root operations and reject observed
//! reparse/symlink components before descending.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

#[cfg(unix)]
use rustix::fd::OwnedFd;

pub(crate) struct SecureMount {
    #[cfg(unix)]
    root: OwnedFd,
    #[cfg(not(unix))]
    root: cap_std::fs::Dir,
}

pub(crate) struct SecureTarget {
    #[cfg(unix)]
    parent: OwnedFd,
    #[cfg(not(unix))]
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
        #[cfg(not(unix))]
        {
            let metadata = std::fs::symlink_metadata(root)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "generation mount root is not a no-follow directory",
                ));
            }
            let root = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
            Ok(Self { root })
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
        #[cfg(not(unix))]
        {
            let mut current = self.root.try_clone()?;
            for component in parent.components() {
                let Component::Normal(component) = component else {
                    return Err(invalid_relative_path());
                };
                match current.symlink_metadata(component) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "generation path traverses a symlink",
                        ));
                    }
                    Ok(metadata) if !metadata.is_dir() => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            "generation path ancestor is not a directory",
                        ));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound && create_parents => {
                        current.create_dir(component)?;
                        sync_cap_dir(&current)?;
                    }
                    Err(error) => return Err(error),
                }
                current = current.open_dir(component)?;
            }
            Ok(SecureTarget {
                parent: current,
                name,
            })
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
        #[cfg(not(unix))]
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
        #[cfg(not(unix))]
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
        #[cfg(not(unix))]
        {
            self.parent.hard_link(name, &self.parent, &self.name)?;
            self.parent.remove_file(name)?;
            sync_cap_dir(&self.parent)
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
        #[cfg(not(unix))]
        {
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            self.parent
                .open_with(name, &options)
                .map(cap_std::fs::File::into_std)
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
        #[cfg(not(unix))]
        {
            self.parent.hard_link(temporary, &self.parent, &self.name)?;
            self.parent.remove_file(temporary)?;
            sync_cap_dir(&self.parent)
        }
    }

    pub(crate) fn remove_named(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::unlinkat(&self.parent, name, rustix::fs::AtFlags::empty())?;
            rustix::fs::fsync(&self.parent)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            self.parent.remove_file(name)?;
            sync_cap_dir(&self.parent)
        }
    }
}

#[cfg(not(unix))]
fn sync_cap_dir(directory: &cap_std::fs::Dir) -> io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
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
