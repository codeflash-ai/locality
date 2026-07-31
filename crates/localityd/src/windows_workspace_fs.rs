use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::mem::{offset_of, size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_RENAME_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
    FileAttributeTagInfo, FileBasicInfo, FileDispositionInfoEx, FileIdInfo, FileRenameInfo,
    FlushFileBuffers, GetFileInformationByHandleEx, LOCKFILE_EXCLUSIVE_LOCK, LockFileEx,
    SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{IO_STATUS_BLOCK, OVERLAPPED};

use crate::replica_materializer::{WorkspaceGenerationFileBinding, WorkspaceGenerationIdentity};

const DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY
    | FILE_TRAVERSE
    | FILE_READ_ATTRIBUTES
    | FILE_WRITE_ATTRIBUTES
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | DELETE
    | SYNCHRONIZE;
const READ_DIRECTORY_ACCESS: u32 =
    FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const MUTABLE_FILE_ACCESS: u32 = FILE_READ_DATA
    | FILE_WRITE_DATA
    | FILE_READ_ATTRIBUTES
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | SYNCHRONIZE;
const READ_FILE_ACCESS: u32 = FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const LOCK_FILE_ACCESS: u32 = FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const LOCK_SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

pub(crate) struct WindowsDirectory {
    handle: OwnedHandle,
}

impl WindowsDirectory {
    pub(crate) fn open_absolute(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(SHARING)
            .custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
                    | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .open(path)?;
        let handle: OwnedHandle = file.into();
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_absolute_read_only(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(SHARING)
            .custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
                    | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .open(path)?;
        let handle: OwnedHandle = file.into();
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn create_directory(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            DIRECTORY_ACCESS,
            FILE_CREATE,
            FILE_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_directory(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_directory_read_only(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            READ_DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_or_create_directory(&self, name: &OsStr) -> io::Result<Self> {
        match self.open_directory(name) {
            Ok(directory) => Ok(directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match nt_open_relative(
                    &self.handle,
                    name,
                    DIRECTORY_ACCESS,
                    FILE_OPEN_IF,
                    FILE_DIRECTORY_FILE,
                    SHARING,
                ) {
                    Ok(handle) => {
                        reject_reparse(&handle)?;
                        Ok(Self { handle })
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn create_file(&self, name: &OsStr) -> io::Result<File> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            MUTABLE_FILE_ACCESS,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(File::from(handle))
    }

    pub(crate) fn open_or_create_lock_file(&self, name: &OsStr) -> io::Result<File> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            LOCK_FILE_ACCESS,
            FILE_OPEN_IF,
            FILE_NON_DIRECTORY_FILE,
            LOCK_SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(File::from(handle))
    }

    fn open_file_handle(&self, name: &OsStr) -> io::Result<OwnedHandle> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            MUTABLE_FILE_ACCESS,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(handle)
    }

    fn open_file_read_handle(&self, name: &OsStr) -> io::Result<OwnedHandle> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            READ_FILE_ACCESS,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(handle)
    }

    pub(crate) fn file_binding(
        &self,
        name: &OsStr,
        max_content_bytes: usize,
    ) -> io::Result<WorkspaceGenerationFileBinding> {
        let handle = self.open_file_read_handle(name)?;
        let identity = handle_identity(&handle)?;
        let mut content = Vec::new();
        File::from(handle)
            .take(max_content_bytes.saturating_add(1) as u64)
            .read_to_end(&mut content)?;
        if content.len() > max_content_bytes {
            return Err(io::Error::other(
                "workspace ownership marker exceeds its content bound",
            ));
        }
        Ok(WorkspaceGenerationFileBinding { identity, content })
    }

    pub(crate) fn preflight_contents(
        &self,
        named_path: &Path,
        expected_device: u64,
    ) -> io::Result<()> {
        for entry in std::fs::read_dir(named_path)? {
            let entry = entry?;
            let name = entry.file_name();
            match self.open_directory(&name) {
                Ok(child) => {
                    let identity = child.identity()?;
                    if identity.device != expected_device {
                        return Err(io::Error::other(
                            "workspace cleanup refuses to cross a volume boundary",
                        ));
                    }
                    child.preflight_contents(&entry.path(), expected_device)?;
                }
                Err(directory_error) => match self.open_file_read_handle(&name) {
                    Ok(file) => {
                        if handle_identity(&file)?.device != expected_device {
                            return Err(io::Error::other(
                                "workspace cleanup refuses to cross a volume boundary",
                            ));
                        }
                    }
                    Err(file_error)
                        if directory_error.kind() == io::ErrorKind::NotFound
                            && file_error.kind() == io::ErrorKind::NotFound => {}
                    Err(file_error) => return Err(file_error),
                },
            }
        }
        Ok(())
    }

    pub(crate) fn remove_contents(
        &self,
        named_path: &Path,
        expected_device: u64,
    ) -> io::Result<()> {
        self.clear_read_only()?;
        for entry in std::fs::read_dir(named_path)? {
            let entry = entry?;
            let name = entry.file_name();
            match self.open_directory(&name) {
                Ok(child) => {
                    if child.identity()?.device != expected_device {
                        return Err(io::Error::other(
                            "workspace cleanup refuses to cross a volume boundary",
                        ));
                    }
                    child.remove_contents(&entry.path(), expected_device)?;
                    child.mark_delete()?;
                }
                Err(directory_error) => match self.open_file_handle(&name) {
                    Ok(file) => {
                        if handle_identity(&file)?.device != expected_device {
                            return Err(io::Error::other(
                                "workspace cleanup refuses to cross a volume boundary",
                            ));
                        }
                        mark_handle_delete(&file)?;
                    }
                    Err(file_error)
                        if directory_error.kind() == io::ErrorKind::NotFound
                            && file_error.kind() == io::ErrorKind::NotFound => {}
                    Err(file_error) => return Err(file_error),
                },
            }
        }
        self.sync()
    }

    pub(crate) fn mark_delete(&self) -> io::Result<()> {
        mark_handle_delete(&self.handle)
    }

    pub(crate) fn identity(&self) -> io::Result<WorkspaceGenerationIdentity> {
        handle_identity(&self.handle)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        // SAFETY: the handle remains owned for the duration of the call.
        if unsafe { FlushFileBuffers(raw_handle(&self.handle)) } == 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
            ) {
                // Windows does not guarantee that directory handles support
                // FlushFileBuffers. File bodies are flushed separately.
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn set_read_only(&self) -> io::Result<()> {
        set_handle_read_only(&self.handle, true)
    }

    pub(crate) fn clear_read_only(&self) -> io::Result<()> {
        set_handle_read_only(&self.handle, false)
    }

    pub(crate) fn rename_no_replace(
        &self,
        destination_parent: &WindowsDirectory,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        let name = wide_name(destination_name)?;
        let byte_len = name
            .len()
            .checked_mul(2)
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| io::Error::other("workspace destination name is too long"))?;
        let fixed = offset_of!(FILE_RENAME_INFO, FileName);
        let total = fixed
            .checked_add(name.len().saturating_mul(2))
            .ok_or_else(|| io::Error::other("workspace rename buffer overflow"))?;
        let words = total.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; words];
        let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: `storage` is suitably aligned and large enough for the fixed
        // header plus the exact UTF-16 name payload.
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = raw_handle(&destination_parent.handle);
            (*info).FileNameLength = byte_len;
            ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
            if SetFileInformationByHandle(
                raw_handle(&self.handle),
                FileRenameInfo,
                info.cast(),
                total as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

pub(crate) fn lock_file_exclusive(file: &File) -> io::Result<()> {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the file handle and overlapped structure are valid for this
    // synchronous lock acquisition. Closing the retained file releases it.
    if unsafe {
        LockFileEx(
            file.as_raw_handle().cast(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            1,
            0,
            &mut overlapped,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn read_regular_file_no_follow(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let named = std::fs::symlink_metadata(path)?;
    if !named.is_file() || named.file_type().is_symlink() {
        return Err(io::Error::other(
            "workspace publication state is not an ordinary file or is a reparse point",
        ));
    }
    if named.len() > max_bytes as u64 {
        return Err(io::Error::other(
            "workspace publication state exceeds its fixed size limit",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .share_mode(SHARING)
        .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    let handle: OwnedHandle = file.into();
    reject_reparse(&handle)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "workspace publication state is not an ordinary file",
        ));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(io::Error::other(
            "workspace publication state exceeds its fixed size limit",
        ));
    }
    let mut bytes = Vec::new();
    File::from(handle)
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::other(
            "workspace publication state exceeds its fixed size limit",
        ));
    }
    Ok(bytes)
}

fn set_handle_read_only(handle: &OwnedHandle, read_only: bool) -> io::Result<()> {
    let mut basic = FILE_BASIC_INFO::default();
    query_handle(
        handle,
        FileBasicInfo,
        (&mut basic as *mut FILE_BASIC_INFO).cast(),
        size_of::<FILE_BASIC_INFO>(),
    )?;
    if read_only {
        basic.FileAttributes |= FILE_ATTRIBUTE_READONLY;
    } else {
        basic.FileAttributes &= !FILE_ATTRIBUTE_READONLY;
    }
    // SAFETY: pointers and length describe the live `FILE_BASIC_INFO`.
    if unsafe {
        SetFileInformationByHandle(
            raw_handle(handle),
            FileBasicInfo,
            (&basic as *const FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn set_file_read_only(file: &File) -> io::Result<()> {
    let handle: HANDLE = file.as_raw_handle().cast();
    let mut basic = FILE_BASIC_INFO::default();
    query_raw_handle(
        handle,
        FileBasicInfo,
        (&mut basic as *mut FILE_BASIC_INFO).cast(),
        size_of::<FILE_BASIC_INFO>(),
    )?;
    basic.FileAttributes |= FILE_ATTRIBUTE_READONLY;
    // SAFETY: pointers and length describe the live `FILE_BASIC_INFO`.
    if unsafe {
        SetFileInformationByHandle(
            handle,
            FileBasicInfo,
            (&basic as *const FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mark_handle_delete(handle: &OwnedHandle) -> io::Result<()> {
    set_handle_read_only(handle, false)?;
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: pointers and length describe the live disposition structure.
    if unsafe {
        SetFileInformationByHandle(
            raw_handle(handle),
            FileDispositionInfoEx,
            (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn nt_open_relative(
    parent: &OwnedHandle,
    name: &OsStr,
    access: u32,
    disposition: u32,
    kind: u32,
    sharing: u32,
) -> io::Result<OwnedHandle> {
    let mut name = wide_name(name)?;
    let length = name
        .len()
        .checked_mul(2)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| io::Error::other("workspace path component is too long"))?;
    let unicode = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: raw_handle(parent),
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    // SAFETY: all pointers reference live stack values for the synchronous
    // call, and a successful call returns one owned handle.
    unsafe {
        let mut handle: HANDLE = ptr::null_mut();
        let mut status_block: IO_STATUS_BLOCK = zeroed();
        let status = NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            sharing,
            disposition,
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        );
        if status < 0 {
            let kind = if status == STATUS_OBJECT_NAME_NOT_FOUND {
                io::ErrorKind::NotFound
            } else if status == STATUS_OBJECT_NAME_COLLISION {
                io::ErrorKind::AlreadyExists
            } else {
                io::ErrorKind::Other
            };
            return Err(io::Error::new(
                kind,
                io::Error::from_raw_os_error(RtlNtStatusToDosError(status) as i32),
            ));
        }
        Ok(OwnedHandle::from_raw_handle(handle.cast()))
    }
}

fn wide_name(name: &OsStr) -> io::Result<Vec<u16>> {
    let name = name.encode_wide().collect::<Vec<_>>();
    if name.is_empty()
        || name
            .iter()
            .any(|unit| *unit == 0 || *unit == b'\\' as u16 || *unit == b'/' as u16)
    {
        return Err(io::Error::other("invalid workspace path component"));
    }
    Ok(name)
}

fn reject_reparse(handle: &OwnedHandle) -> io::Result<()> {
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    query_handle(
        handle,
        FileAttributeTagInfo,
        (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
        size_of::<FILE_ATTRIBUTE_TAG_INFO>(),
    )?;
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "workspace traversal encountered a reparse point",
        ));
    }
    Ok(())
}

fn handle_identity(handle: &OwnedHandle) -> io::Result<WorkspaceGenerationIdentity> {
    let mut info = FILE_ID_INFO::default();
    query_handle(
        handle,
        FileIdInfo,
        (&mut info as *mut FILE_ID_INFO).cast(),
        size_of::<FILE_ID_INFO>(),
    )?;
    let bytes = info.FileId.Identifier;
    Ok(WorkspaceGenerationIdentity {
        device: info.VolumeSerialNumber,
        inode: u64::from_le_bytes(bytes[..8].try_into().expect("fixed file ID")),
        inode_high: u64::from_le_bytes(bytes[8..].try_into().expect("fixed file ID")),
    })
}

fn query_handle(
    handle: &OwnedHandle,
    class: i32,
    output: *mut core::ffi::c_void,
    length: usize,
) -> io::Result<()> {
    query_raw_handle(raw_handle(handle), class, output, length)
}

fn query_raw_handle(
    handle: HANDLE,
    class: i32,
    output: *mut core::ffi::c_void,
    length: usize,
) -> io::Result<()> {
    // SAFETY: callers provide a valid output object and matching byte length.
    if unsafe { GetFileInformationByHandleEx(handle, class, output, length as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn publication_lock_access_allows_contenders_but_denies_delete_sharing() {
        assert_eq!(LOCK_FILE_ACCESS & DELETE, 0);
        assert_eq!(LOCK_SHARING & FILE_SHARE_DELETE, 0);
        assert_ne!(LOCK_FILE_ACCESS & (FILE_READ_DATA | FILE_WRITE_DATA), 0);
        assert_eq!(
            LOCK_SHARING & (FILE_SHARE_READ | FILE_SHARE_WRITE),
            LOCK_SHARING
        );
    }

    #[test]
    fn publication_lock_contender_opens_same_object_then_waits_in_lock_file_ex() {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).expect("random test directory suffix");
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!("locality-windows-lock-{suffix}"));
        std::fs::create_dir(&path).expect("create lock test directory");
        let parent = WindowsDirectory::open_absolute(&path).expect("open lock test directory");
        let first = parent
            .open_or_create_lock_file(OsStr::new("publication.lock"))
            .expect("create first lock handle");
        lock_file_exclusive(&first).expect("acquire first lock");

        let (opened_tx, opened_rx) = mpsc::channel();
        let (locked_tx, locked_rx) = mpsc::channel();
        let contender_path = path.clone();
        let contender = std::thread::spawn(move || {
            let parent =
                WindowsDirectory::open_absolute(&contender_path).expect("open contender parent");
            let file = parent
                .open_or_create_lock_file(OsStr::new("publication.lock"))
                .expect("contender opens the existing lock object");
            opened_tx.send(()).expect("report contender open");
            lock_file_exclusive(&file).expect("contender acquires after release");
            locked_tx.send(()).expect("report contender lock");
        });

        opened_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("contender must open despite no delete sharing");
        assert!(
            locked_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "contender must wait in LockFileEx"
        );
        drop(first);
        locked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("contender acquires after first handle closes");
        contender.join().expect("contender thread");
        drop(parent);
        std::fs::remove_dir_all(path).expect("remove lock test directory");
    }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_names_reject_separators_and_nul() {
        assert!(wide_name(OsStr::new("child")).is_ok());
        assert!(wide_name(OsStr::new("child\\escape")).is_err());
        assert!(wide_name(OsStr::new("child/escape")).is_err());
        assert!(wide_name(OsStr::new("child\0escape")).is_err());
    }
}
