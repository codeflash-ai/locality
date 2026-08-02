use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FileRenameInformation, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_NO_SUCH_FILE,
    STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_EXISTS, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FileAttributeTagInfo,
    FileBasicInfo, FileDispositionInfoEx, FileIdInfo, FlushFileBuffers,
    GetFileInformationByHandleEx, LOCKFILE_EXCLUSIVE_LOCK, LockFileEx, SYNCHRONIZE,
    SetFileInformationByHandle,
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
const SYNC_DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY
    | FILE_TRAVERSE
    | FILE_READ_ATTRIBUTES
    | FILE_WRITE_DATA
    | FILE_WRITE_ATTRIBUTES
    | SYNCHRONIZE;
const CLEANUP_DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY
    | FILE_TRAVERSE
    | FILE_READ_ATTRIBUTES
    | FILE_WRITE_DATA
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | SYNCHRONIZE;
const ATTRIBUTE_DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY
    | FILE_TRAVERSE
    | FILE_READ_ATTRIBUTES
    | FILE_WRITE_ATTRIBUTES
    | SYNCHRONIZE;
const MUTABLE_FILE_ACCESS: u32 = FILE_READ_DATA
    | FILE_WRITE_DATA
    | FILE_READ_ATTRIBUTES
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | SYNCHRONIZE;
const CLEANUP_FILE_ACCESS: u32 =
    FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | DELETE | SYNCHRONIZE;
const READ_FILE_ACCESS: u32 = FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const LOCK_FILE_ACCESS: u32 = FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const ANCHOR_SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const LOCK_SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

pub(crate) struct WindowsDirectory {
    handle: OwnedHandle,
}

impl WindowsDirectory {
    pub(crate) fn open_absolute_anchor(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .access_mode(READ_DIRECTORY_ACCESS)
            .share_mode(ANCHOR_SHARING)
            .custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
                    | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .open(path)?;
        let handle: OwnedHandle = file.into();
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_directory_anchor(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            READ_DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_absolute_sync_anchor(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .access_mode(SYNC_DIRECTORY_ACCESS)
            .share_mode(ANCHOR_SHARING)
            .custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
                    | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .open(path)?;
        let handle: OwnedHandle = file.into();
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_directory_sync_anchor(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            SYNC_DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

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

    pub(crate) fn open_directory_for_cleanup(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            CLEANUP_DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_directory_for_anchored_cleanup(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            CLEANUP_DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(Self { handle })
    }

    pub(crate) fn open_directory_for_attributes(&self, name: &OsStr) -> io::Result<Self> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            ATTRIBUTE_DIRECTORY_ACCESS,
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

    pub(crate) fn create_file_anchored(&self, name: &OsStr) -> io::Result<File> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            MUTABLE_FILE_ACCESS,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(File::from(handle))
    }

    pub(crate) fn open_file_for_durable_copy(&self, name: &OsStr) -> io::Result<File> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            READ_FILE_ACCESS,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(File::from(handle))
    }

    pub(crate) fn open_file_for_durable_copy_allow_cloud_placeholder(
        &self,
        name: &OsStr,
    ) -> io::Result<File> {
        let named = nt_open_relative(
            &self.handle,
            name,
            READ_FILE_ACCESS,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
        query_handle(
            &named,
            FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>(),
        )?;
        if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return Ok(File::from(named));
        }
        if !is_cloud_files_placeholder(tag.FileAttributes, tag.ReparseTag) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable copy source reparse point is not a Cloud Files placeholder",
            ));
        }
        let hydrated = nt_open_relative_follow_final(
            &self.handle,
            name,
            READ_FILE_ACCESS,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        drop(named);
        Ok(File::from(hydrated))
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

    fn open_file_cleanup_handle(&self, name: &OsStr) -> io::Result<OwnedHandle> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            CLEANUP_FILE_ACCESS,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            SHARING,
        )?;
        reject_reparse(&handle)?;
        Ok(handle)
    }

    fn open_any_cleanup_handle_allow_reparse(&self, name: &OsStr) -> io::Result<OwnedHandle> {
        nt_open_relative(
            &self.handle,
            name,
            FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | DELETE | SYNCHRONIZE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT,
            SHARING,
        )
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
            match self.open_directory_read_only(&name) {
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
            match self.open_directory_for_cleanup(&name) {
                Ok(child) => {
                    if child.identity()?.device != expected_device {
                        return Err(io::Error::other(
                            "workspace cleanup refuses to cross a volume boundary",
                        ));
                    }
                    child.remove_contents(&entry.path(), expected_device)?;
                    child.mark_delete()?;
                }
                Err(directory_error) => match self.open_any_cleanup_handle_allow_reparse(&name) {
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

    pub(crate) fn remove_file_for_anchored_cleanup(&self, name: &OsStr) -> io::Result<()> {
        let handle = nt_open_relative(
            &self.handle,
            name,
            CLEANUP_FILE_ACCESS,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            ANCHOR_SHARING,
        )?;
        reject_reparse(&handle)?;
        mark_handle_delete(&handle)
    }

    pub(crate) fn identity(&self) -> io::Result<WorkspaceGenerationIdentity> {
        handle_identity(&self.handle)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        // SAFETY: the handle remains owned for the duration of the call.
        if unsafe { FlushFileBuffers(raw_handle(&self.handle)) } == 0 {
            return Err(io::Error::last_os_error());
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
        rename_handle_no_replace(&self.handle, destination_parent, destination_name)
    }

    pub(crate) fn rename_child_no_replace_allow_final_reparse(
        &self,
        source_name: &OsStr,
        destination_parent: &WindowsDirectory,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        let source = nt_open_relative(
            &self.handle,
            source_name,
            FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT,
            SHARING,
        )?;
        rename_handle_no_replace(&source, destination_parent, destination_name)
    }
}

fn rename_handle_no_replace(
    source: &OwnedHandle,
    destination_parent: &WindowsDirectory,
    destination_name: &OsStr,
) -> io::Result<()> {
    let name = wide_name(destination_name)?;
    let byte_len = name
        .len()
        .checked_mul(2)
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| io::Error::other("workspace destination name is too long"))?;
    let total = rename_information_buffer_size(name.len())?;
    let total_u32 = u32::try_from(total)
        .map_err(|_| io::Error::other("workspace rename buffer is too large"))?;
    let words = total.div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; words];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `storage` is suitably aligned and large enough for the fixed
    // header plus the exact UTF-16 name payload. Both source and parent
    // handles remain owned for the synchronous native rename call.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = raw_handle(&destination_parent.handle);
        (*info).FileNameLength = byte_len;
        ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let mut status_block: IO_STATUS_BLOCK = zeroed();
        let status = NtSetInformationFile(
            raw_handle(source),
            &mut status_block,
            info.cast(),
            total_u32,
            FileRenameInformation,
        );
        if status < 0 || status == STATUS_OBJECT_NAME_EXISTS {
            return Err(rename_status_error(status));
        }
    }
    Ok(())
}

fn rename_information_buffer_size(name_units: usize) -> io::Result<usize> {
    let trailing_units = name_units
        .checked_sub(1)
        .ok_or_else(|| io::Error::other("workspace destination name is empty"))?;
    let trailing_bytes = trailing_units
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::other("workspace rename buffer overflow"))?;
    // `FILE_RENAME_INFORMATION` already contains one UTF-16 code unit plus tail
    // padding. Starting at `FileName` omits that padding and Windows rejects
    // the otherwise valid rename buffer with ERROR_INVALID_PARAMETER.
    size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(trailing_bytes)
        .ok_or_else(|| io::Error::other("workspace rename buffer overflow"))
}

fn rename_status_error(status: NTSTATUS) -> io::Error {
    let kind = match status {
        STATUS_OBJECT_NAME_COLLISION | STATUS_OBJECT_NAME_EXISTS => io::ErrorKind::AlreadyExists,
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_NO_SUCH_FILE => {
            io::ErrorKind::NotFound
        }
        _ => io::ErrorKind::Other,
    };
    io::Error::new(
        kind,
        io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32),
    )
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
        .custom_flags(PUBLICATION_STATE_OPEN_FLAGS)
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

const PUBLICATION_STATE_OPEN_FLAGS: u32 =
    windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

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
    nt_open_relative_impl(parent, name, access, disposition, kind, sharing, false)
}

fn nt_open_relative_follow_final(
    parent: &OwnedHandle,
    name: &OsStr,
    access: u32,
    disposition: u32,
    kind: u32,
    sharing: u32,
) -> io::Result<OwnedHandle> {
    nt_open_relative_impl(parent, name, access, disposition, kind, sharing, true)
}

fn nt_open_relative_impl(
    parent: &OwnedHandle,
    name: &OsStr,
    access: u32,
    disposition: u32,
    kind: u32,
    sharing: u32,
    follow_final: bool,
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
            kind | FILE_SYNCHRONOUS_IO_NONALERT
                | if follow_final {
                    0
                } else {
                    FILE_OPEN_REPARSE_POINT
                },
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

fn is_cloud_files_placeholder(attributes: u32, reparse_tag: u32) -> bool {
    use windows_sys::Win32::Storage::CloudFilters::{
        CF_PLACEHOLDER_STATE_PLACEHOLDER, CfGetPlaceholderStateFromAttributeTag,
    };

    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && unsafe { CfGetPlaceholderStateFromAttributeTag(attributes, reparse_tag) }
            & CF_PLACEHOLDER_STATE_PLACEHOLDER
            != 0
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
    use std::io::Write;
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
    fn ancestor_anchor_handles_deny_reparse_substitution() {
        assert_eq!(ANCHOR_SHARING & FILE_SHARE_DELETE, 0);
        assert_eq!(
            ANCHOR_SHARING & (FILE_SHARE_READ | FILE_SHARE_WRITE),
            ANCHOR_SHARING
        );
        assert_ne!(SYNC_DIRECTORY_ACCESS & FILE_WRITE_DATA, 0);
        assert_ne!(SYNC_DIRECTORY_ACCESS & FILE_WRITE_ATTRIBUTES, 0);
        assert_eq!(
            READ_DIRECTORY_ACCESS & (FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES),
            0
        );
    }

    #[test]
    fn cloud_copy_allows_cloud_placeholder_tags_but_not_junctions() {
        const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001a;
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xa000_0003;

        assert!(is_cloud_files_placeholder(
            FILE_ATTRIBUTE_REPARSE_POINT,
            IO_REPARSE_TAG_CLOUD
        ));
        assert!(!is_cloud_files_placeholder(
            FILE_ATTRIBUTE_REPARSE_POINT,
            IO_REPARSE_TAG_MOUNT_POINT
        ));
        assert!(!is_cloud_files_placeholder(0, IO_REPARSE_TAG_CLOUD));
    }

    #[test]
    fn publication_state_open_contract_does_not_follow_reparse_points() {
        assert_ne!(
            PUBLICATION_STATE_OPEN_FLAGS
                & windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            0
        );
    }

    #[test]
    fn rename_buffer_includes_the_struct_tail_and_alignment() {
        assert_eq!(
            rename_information_buffer_size(1).expect("one-unit rename buffer"),
            size_of::<FILE_RENAME_INFORMATION>()
        );
        assert_eq!(
            rename_information_buffer_size(8).expect("eight-unit rename buffer"),
            size_of::<FILE_RENAME_INFORMATION>() + 7 * size_of::<u16>()
        );
        assert!(rename_information_buffer_size(0).is_err());
        assert!(
            rename_information_buffer_size(8).expect("aligned rename buffer")
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
    }

    #[test]
    fn anchored_directory_rename_uses_a_complete_file_rename_info_buffer() {
        let path = temporary_test_directory("rename");
        std::fs::create_dir(&path).expect("create rename test directory");
        let parent = WindowsDirectory::open_absolute(&path).expect("open rename parent");
        let source = parent
            .create_directory(OsStr::new("source"))
            .expect("create rename source");
        let expected = source.identity().expect("source identity");

        source
            .rename_no_replace(&parent, OsStr::new("destination"))
            .expect("rename relative to retained parent handle");
        assert_eq!(
            parent
                .open_directory_read_only(OsStr::new("destination"))
                .expect("open renamed destination")
                .identity()
                .expect("destination identity"),
            expected
        );

        let collision_source = parent
            .create_directory(OsStr::new("collision-source"))
            .expect("create collision source");
        let collision_source_identity = collision_source.identity().expect("collision source id");
        let collision_destination = parent
            .create_directory(OsStr::new("collision-destination"))
            .expect("create collision destination");
        let collision_destination_identity = collision_destination
            .identity()
            .expect("collision destination id");
        let error = collision_source
            .rename_no_replace(&parent, OsStr::new("collision-destination"))
            .expect_err("native relative rename must not replace an existing destination");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            parent
                .open_directory_read_only(OsStr::new("collision-source"))
                .expect("collision source remains")
                .identity()
                .expect("remaining collision source id"),
            collision_source_identity
        );
        assert_eq!(
            parent
                .open_directory_read_only(OsStr::new("collision-destination"))
                .expect("collision destination remains")
                .identity()
                .expect("remaining collision destination id"),
            collision_destination_identity
        );

        drop(collision_destination);
        drop(collision_source);
        drop(source);
        drop(parent);
        std::fs::remove_dir_all(path).expect("remove rename test directory");
    }

    #[test]
    fn read_only_tree_uses_validation_handles_before_owned_cleanup_handles() {
        assert_eq!(READ_DIRECTORY_ACCESS & (FILE_WRITE_ATTRIBUTES | DELETE), 0);
        assert_eq!(
            CLEANUP_DIRECTORY_ACCESS & (FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY),
            0
        );
        assert_eq!(CLEANUP_FILE_ACCESS & FILE_WRITE_DATA, 0);

        let path = temporary_test_directory("read-only-cleanup");
        std::fs::create_dir(&path).expect("create cleanup test directory");
        let tree_path = path.join("tree");
        let parent = WindowsDirectory::open_absolute(&path).expect("open cleanup parent");
        let root = parent
            .create_directory(OsStr::new("tree"))
            .expect("create cleanup root");
        let child = root
            .create_directory(OsStr::new("child"))
            .expect("create cleanup child");
        let mut file = child
            .create_file(OsStr::new("sealed.txt"))
            .expect("create cleanup file");
        file.write_all(b"sealed\n").expect("write cleanup file");
        file.sync_all().expect("sync cleanup file");
        set_file_read_only(&file).expect("seal cleanup file");
        drop(file);
        child.set_read_only().expect("seal cleanup child");
        root.set_read_only().expect("seal cleanup root");
        let expected = root.identity().expect("cleanup root identity");
        drop(child);

        let validation_parent =
            WindowsDirectory::open_absolute_read_only(&path).expect("open validation parent");
        let validation_root = validation_parent
            .open_directory_read_only(OsStr::new("tree"))
            .expect("open read-only validation root");
        assert_eq!(
            validation_root.identity().expect("validation identity"),
            expected
        );
        validation_root
            .preflight_contents(&tree_path, expected.device)
            .expect("preflight sealed tree with read-only handles");
        drop(validation_root);
        drop(validation_parent);

        root.remove_contents(&tree_path, expected.device)
            .expect("remove sealed tree through cleanup-specific handles");
        root.mark_delete().expect("remove empty cleanup root");
        drop(root);
        drop(parent);
        assert!(!tree_path.exists());
        std::fs::remove_dir_all(path).expect("remove cleanup test directory");
    }

    fn temporary_test_directory(label: &str) -> std::path::PathBuf {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).expect("random test directory suffix");
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir().join(format!("locality-windows-{label}-{suffix}"))
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
