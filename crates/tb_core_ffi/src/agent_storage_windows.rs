//! Windows-only secure storage primitives shared by account and history stores.
//!
//! Path-taking helpers intentionally permit ancestor reparse points. Their caller
//! must anchor every path beneath the trusted per-user `dirs::data_dir` parent;
//! this module validates only the final component. Path identity guarantees are
//! point-in-time: callers must keep using the returned handle and revalidate
//! before any later pathname operation.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{self, null, null_mut};
use std::slice;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER,
    ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
};
#[cfg(test)]
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
#[cfg(test)]
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::{
    AclSizeInformation, AddAccessAllowedAceEx, CopySid, CreateWellKnownSid, GetAce,
    GetAclInformation, GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetSecurityDescriptorLength, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
    IsValidAcl, IsValidSecurityDescriptor, IsValidSid, SetSecurityDescriptorControl,
    SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TokenUser, WinLocalSystemSid,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, NO_INHERITANCE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FileAttributeTagInfo, FileIdInfo, FlushFileBuffers,
    GetFileInformationByHandleEx, GetFileType, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DEVICE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TYPE_DISK, OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const ACCESS_ALLOWED_ACE_TYPE_VALUE: u8 = 0;
const SID_HEADER_LEN: usize = 8;
const ACE_SID_OFFSET: usize = size_of::<ACE_HEADER>() + size_of::<u32>();
const SECURITY_DESCRIPTOR_REVISION_VALUE: u32 = 1;
const STORAGE_SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const VALIDATION_SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const STORAGE_SECURITY_ACCESS: u32 = READ_CONTROL;
const SECURE_STORAGE_FALLBACK_SUFFIX: &str = ".secure";

#[derive(Clone, Copy, Eq, PartialEq)]
struct StorageIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[derive(Clone, Copy)]
enum StorageObjectKind {
    Directory,
    RegularFile,
}

impl StorageObjectKind {
    fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }

    fn open_flags(self) -> u32 {
        let type_flag = if self.is_directory() {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        type_flag | FILE_FLAG_OPEN_REPARSE_POINT
    }
}

/// Resolve the sticky exact-secure root for new account and v3 history artifacts.
/// An existing fallback wins even if the preferred root later disappears or becomes
/// secure. A colliding fallback must itself pass the exact directory contract;
/// otherwise resolution fails closed before consulting or creating the preferred root.
///
/// The caller must place `preferred` beneath the trusted per-user `dirs::data_dir`
/// parent. The fallback is a sibling formed by appending `.secure` to its filename.
pub(crate) fn resolve_secure_storage_directory(preferred: &Path) -> io::Result<PathBuf> {
    let preferred_name = preferred.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "secure storage root has no filename",
        )
    })?;
    let mut fallback_name = preferred_name.to_os_string();
    fallback_name.push(SECURE_STORAGE_FALLBACK_SUFFIX);
    let fallback = preferred.with_file_name(fallback_name);

    match open_existing_secure_storage_directory(&fallback) {
        Ok(_) => return Ok(fallback),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Only a confirmed contract rejection activates fallback. Operational
    // failures stay unavailable rather than permanently splitting an exact root.
    match ensure_secure_storage_directory(preferred) {
        Ok(directory) => drop(directory),
        Err(error) if is_security_verification_failure(&error) => {
            drop(ensure_secure_storage_directory(&fallback)?);
            return Ok(fallback);
        }
        Err(error) => return Err(error),
    }

    // Recheck after preferred creation/verification so a concurrently installed
    // fallback is never ignored. Exact fallback remains the sticky authority.
    match open_existing_secure_storage_directory(&fallback) {
        Ok(_) => Ok(fallback),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(preferred.to_path_buf()),
        Err(error) => Err(error),
    }
}

fn open_existing_secure_storage_directory(path: &Path) -> io::Result<File> {
    open_secure_storage_object(
        path,
        GENERIC_WRITE | FILE_READ_ATTRIBUTES | STORAGE_SECURITY_ACCESS,
        OPEN_EXISTING,
        StorageObjectKind::Directory,
    )
}

/// Create only the final directory component securely when absent, then open and
/// verify that exact directory without following a final-component reparse point.
///
/// The caller must place `path` beneath the trusted per-user `dirs::data_dir`
/// parent. Ancestor reparse points are allowed and are not ACL-validated here.
#[allow(dead_code)] // Stage 2A2 primitive; production callers are wired later.
pub(crate) fn ensure_secure_storage_directory(path: &Path) -> io::Result<File> {
    ensure_secure_storage_directory_with(path, |_| Ok(()))
}

fn ensure_secure_storage_directory_with(
    path: &Path,
    before_verify: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<File> {
    let wide = wide_path(path)?;
    let mut security = CreationSecurity::new()?;
    let attributes = security.attributes();
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        let status = unsafe { GetLastError() };
        if status != ERROR_ALREADY_EXISTS {
            return Err(win32_status_error(status));
        }
    }

    open_secure_storage_object_with(
        path,
        GENERIC_WRITE | FILE_READ_ATTRIBUTES | STORAGE_SECURITY_ACCESS,
        OPEN_EXISTING,
        StorageObjectKind::Directory,
        before_verify,
    )
}

/// Create a new secure regular file opened for reading and writing.
/// The caller must anchor `path` beneath the trusted per-user data parent.
#[allow(dead_code)] // Stage 2A2 primitive; production callers are wired later.
pub(crate) fn create_new_secure_file(path: &Path) -> io::Result<File> {
    open_secure_storage_object(
        path,
        GENERIC_READ | GENERIC_WRITE | STORAGE_SECURITY_ACCESS,
        CREATE_NEW,
        StorageObjectKind::RegularFile,
    )
}

/// Open an existing regular file only when it already satisfies the exact
/// owner/DACL contract. This helper never tightens an existing object in place:
/// changing its DACL cannot revoke access already granted to retained handles.
/// The caller must anchor `path` beneath the trusted per-user data parent.
#[allow(dead_code)] // Stage 2A2 primitive; production callers are wired later.
pub(crate) fn open_existing_secure_file(path: &Path, writable: bool) -> io::Result<File> {
    let data_access = if writable {
        GENERIC_READ | GENERIC_WRITE
    } else {
        GENERIC_READ
    };
    open_secure_storage_object(
        path,
        data_access | STORAGE_SECURITY_ACCESS,
        OPEN_EXISTING,
        StorageObjectKind::RegularFile,
    )
}

/// Create a secure regular file when absent, or open it only when the existing
/// object already satisfies the exact owner/DACL contract. Never repairs in place.
/// The caller must anchor `path` beneath the trusted per-user data parent.
#[allow(dead_code)] // Stage 2A2 primitive; production callers are wired later.
pub(crate) fn open_or_create_secure_file(path: &Path) -> io::Result<File> {
    open_secure_storage_object(
        path,
        GENERIC_READ | GENERIC_WRITE | STORAGE_SECURITY_ACCESS,
        OPEN_ALWAYS,
        StorageObjectKind::RegularFile,
    )
}

/// Open or atomically create an exact secure lock file, acquire an exclusive
/// `fs2` lock, then revalidate its owner, DACL, type, and pathname identity.
///
/// The returned `File` keeps both the exclusive lock and a Windows handle that
/// does not share delete access. Callers may explicitly use
/// `fs2::FileExt::unlock` or drop it. The caller must anchor `path` beneath the
/// trusted per-user data parent.
#[allow(dead_code)] // Stage 2A3a primitive; production callers are wired later.
pub(crate) fn open_secure_lock_file(path: &Path) -> io::Result<File> {
    let file = open_windows_path_with_share(
        path,
        GENERIC_READ | GENERIC_WRITE | STORAGE_SECURITY_ACCESS,
        OPEN_ALWAYS,
        StorageObjectKind::RegularFile.open_flags(),
        VALIDATION_SHARE_MODE,
    )?;
    fs2::FileExt::lock_exclusive(&file)?;

    let verification = (|| {
        let handle = file.as_raw_handle() as HANDLE;
        let identity = storage_identity(handle, StorageObjectKind::RegularFile)?;
        verify_storage_handle(handle)?;
        verify_path_identity(path, StorageObjectKind::RegularFile, identity)
    })();
    if let Err(error) = verification {
        let _ = fs2::FileExt::unlock(&file);
        return Err(error);
    }
    Ok(file)
}

/// Atomically replace `destination_path` with `staged_path` after exact secure
/// validation. Both paths must be distinct direct children of `directory_path`,
/// and `directory` must name that exact secure directory.
///
/// The caller must hold the exclusive lock returned by `open_secure_lock_file`
/// for the whole operation. The staged file is flushed before commit. On a
/// replace error this helper leaves staged-file cleanup to the caller. Post-commit
/// verification or directory flush errors are reported honestly; the completed
/// replace cannot be rolled back here.
#[allow(dead_code)] // Stage 2A3a primitive; production callers are wired later.
pub(crate) fn replace_secure_file(
    directory: &File,
    directory_path: &Path,
    staged_path: &Path,
    destination_path: &Path,
) -> io::Result<()> {
    replace_secure_file_with(
        directory,
        directory_path,
        staged_path,
        destination_path,
        |staged, destination| tokscale_core::fs_atomic::replace_file(staged, destination),
    )
}

fn replace_secure_file_with(
    directory: &File,
    directory_path: &Path,
    staged_path: &Path,
    destination_path: &Path,
    replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    validate_replace_paths(directory_path, staged_path, destination_path)?;

    let directory_handle = directory.as_raw_handle() as HANDLE;
    let directory_identity = storage_identity(directory_handle, StorageObjectKind::Directory)?;
    verify_storage_handle(directory_handle)?;
    verify_path_identity(
        directory_path,
        StorageObjectKind::Directory,
        directory_identity,
    )?;

    let staged_identity = {
        let staged = open_existing_secure_file(staged_path, true)?;
        staged.sync_all()?;
        let identity = storage_identity(
            staged.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )?;
        if identity.volume_serial_number != directory_identity.volume_serial_number {
            return Err(security_verification_failed());
        }
        identity
    };

    let destination_identity = match open_existing_secure_file(destination_path, false) {
        Ok(destination) => {
            let identity = storage_identity(
                destination.as_raw_handle() as HANDLE,
                StorageObjectKind::RegularFile,
            )?;
            if identity.volume_serial_number != directory_identity.volume_serial_number {
                return Err(security_verification_failed());
            }
            Some(identity)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if destination_identity == Some(staged_identity) {
        return Err(security_verification_failed());
    }

    // Every staged/destination validation handle has left scope before the
    // pathname-based vendor helper runs, so it cannot block MoveFileExW.
    replace(staged_path, destination_path)?;

    let installed_identity = {
        let installed = open_existing_secure_file(destination_path, false)?;
        storage_identity(
            installed.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )?
    };
    if installed_identity != staged_identity {
        return Err(security_verification_failed());
    }
    match std::fs::symlink_metadata(staged_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err(security_verification_failed()),
        Err(error) => return Err(error),
    }

    verify_path_identity(
        directory_path,
        StorageObjectKind::Directory,
        directory_identity,
    )?;
    flush_secure_storage_directory(directory)
}

/// Move one exact secure regular file to one explicit quarantine candidate by
/// creating a hard link and then unlinking the source pathname.
///
/// `source_path` and `candidate_path` must be distinct direct children of
/// `directory_path`. This helper tries exactly one candidate and never chooses a
/// collision suffix. The caller must hold the exclusive lock returned by
/// `open_secure_lock_file` for this entire call and must anchor every path beneath
/// the trusted per-user data parent. These pathname operations coordinate trusted
/// callers; they do not defend against a malicious process running as the same SID.
#[allow(dead_code)] // Stage 2A3b primitive; production callers are wired later.
pub(crate) fn quarantine_secure_file_candidate(
    directory: &File,
    directory_path: &Path,
    source_path: &Path,
    candidate_path: &Path,
) -> io::Result<()> {
    quarantine_secure_file_candidate_with(
        directory,
        directory_path,
        source_path,
        candidate_path,
        |source, candidate| std::fs::hard_link(source, candidate),
        |path| std::fs::remove_file(path),
        |directory| flush_secure_storage_directory(directory),
    )
}

fn quarantine_secure_file_candidate_with(
    directory: &File,
    directory_path: &Path,
    source_path: &Path,
    candidate_path: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    mut unlink: impl FnMut(&Path) -> io::Result<()>,
    flush: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    validate_replace_paths(directory_path, source_path, candidate_path)?;

    let directory_handle = directory.as_raw_handle() as HANDLE;
    let directory_identity = storage_identity(directory_handle, StorageObjectKind::Directory)?;
    verify_storage_handle(directory_handle)?;
    verify_path_identity(
        directory_path,
        StorageObjectKind::Directory,
        directory_identity,
    )?;

    // Keep this handle alive through the complete hard-link/unlink transaction.
    // It shares delete access so the pathname unlink can proceed, while retaining
    // the original file identity for every comparison and rollback decision.
    let source = open_existing_secure_file(source_path, false)?;
    let source_identity = storage_identity(
        source.as_raw_handle() as HANDLE,
        StorageObjectKind::RegularFile,
    )?;
    if source_identity.volume_serial_number != directory_identity.volume_serial_number {
        return Err(security_verification_failed());
    }

    // CreateHardLinkW, reached through std, provides create-new collision
    // semantics. Never pre-delete or overwrite an existing candidate.
    link(source_path, candidate_path)?;

    if let Err(error) = verify_secure_file_path(&source, source_path) {
        rollback_quarantine_candidate(
            &source,
            source_path,
            candidate_path,
            source_identity,
            &mut unlink,
        )?;
        return Err(error);
    }
    let candidate = match open_secure_file_with_identity(candidate_path, source_identity) {
        Ok(candidate) => candidate,
        Err(error) => {
            rollback_quarantine_candidate(
                &source,
                source_path,
                candidate_path,
                source_identity,
                &mut unlink,
            )?;
            return Err(error);
        }
    };
    drop(candidate);

    if let Err(error) = unlink(source_path) {
        rollback_quarantine_candidate(
            &source,
            source_path,
            candidate_path,
            source_identity,
            &mut unlink,
        )?;
        return Err(error);
    }

    // The source unlink has committed. Verification or flush failures from here
    // are reported honestly; removing the surviving candidate would lose data.
    verify_path_absent(source_path)?;
    drop(open_secure_file_with_identity(
        candidate_path,
        source_identity,
    )?);
    verify_path_identity(
        directory_path,
        StorageObjectKind::Directory,
        directory_identity,
    )?;
    flush(directory)
}

fn open_secure_file_with_identity(path: &Path, expected: StorageIdentity) -> io::Result<File> {
    let file = open_existing_secure_file(path, false)?;
    let actual = storage_identity(
        file.as_raw_handle() as HANDLE,
        StorageObjectKind::RegularFile,
    )?;
    if actual == expected {
        Ok(file)
    } else {
        Err(security_verification_failed())
    }
}

fn rollback_quarantine_candidate(
    source: &File,
    source_path: &Path,
    candidate_path: &Path,
    expected: StorageIdentity,
    unlink: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    // Never delete by pathname until the candidate is securely opened and its
    // full volume + 128-bit file identity matches the retained source handle.
    let candidate = open_secure_file_with_identity(candidate_path, expected)?;
    verify_secure_file_path(source, source_path)?;
    drop(candidate);

    unlink(candidate_path)?;
    verify_secure_file_path(source, source_path)?;
    verify_path_absent(candidate_path)
}

fn verify_path_absent(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(security_verification_failed()),
        Err(error) => Err(error),
    }
}

fn validate_replace_paths(
    directory_path: &Path,
    staged_path: &Path,
    destination_path: &Path,
) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows storage replace path",
        )
    };
    if staged_path == destination_path
        || staged_path.file_name().is_none()
        || destination_path.file_name().is_none()
        || staged_path.parent() != Some(directory_path)
        || destination_path.parent() != Some(directory_path)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Verify that a currently open secure regular file names the same non-reparse
/// object at `path` during this call. This function never mutates either object.
/// The guarantee is point-in-time only: keep using `file`, and call this again
/// immediately before any later operation that must act on the pathname.
#[allow(dead_code)] // Stage 2A2 primitive; production callers are wired later.
pub(crate) fn verify_secure_file_path(file: &File, path: &Path) -> io::Result<()> {
    let handle = file.as_raw_handle() as HANDLE;
    let identity = storage_identity(handle, StorageObjectKind::RegularFile)?;
    verify_storage_handle(handle)?;
    verify_path_identity(path, StorageObjectKind::RegularFile, identity)
}

/// Durably flush a verified secure storage directory handle.
#[allow(dead_code)] // Stage 2A2 primitive; production callers are wired later.
pub(crate) fn flush_secure_storage_directory(directory: &File) -> io::Result<()> {
    flush_storage_directory_handle(directory.as_raw_handle() as HANDLE)
}

fn open_secure_storage_object(
    path: &Path,
    access: u32,
    disposition: u32,
    kind: StorageObjectKind,
) -> io::Result<File> {
    open_secure_storage_object_with(path, access, disposition, kind, |_| Ok(()))
}

fn open_secure_storage_object_with(
    path: &Path,
    access: u32,
    disposition: u32,
    kind: StorageObjectKind,
    before_verify: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<File> {
    let file = open_windows_path(path, access, disposition, kind.open_flags())?;
    let identity = storage_identity(file.as_raw_handle() as HANDLE, kind)?;
    before_verify(&file)?;

    // Existing objects are accepted only when they already satisfy the exact
    // contract. Never repair a permissive DACL in place: an access check that
    // granted another process a handle remains effective after the DACL changes.
    verify_storage_handle(file.as_raw_handle() as HANDLE)?;
    verify_path_identity(path, kind, identity)?;
    Ok(file)
}

fn verify_path_identity(
    path: &Path,
    kind: StorageObjectKind,
    expected: StorageIdentity,
) -> io::Result<()> {
    verify_path_identity_with(path, kind, expected, |_| Ok(()))
}

fn verify_path_identity_with(
    path: &Path,
    kind: StorageObjectKind,
    expected: StorageIdentity,
    while_validation_open: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    // FILE_READ_DATA (FILE_LIST_DIRECTORY for directories) makes this a counted
    // read open, so omitting FILE_SHARE_DELETE blocks competing DELETE-access
    // opens for the validation lifetime. The primary data handle still shares
    // delete for later, separately revalidated replacement.
    let validation = open_windows_path_with_share(
        path,
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL,
        OPEN_EXISTING,
        kind.open_flags(),
        VALIDATION_SHARE_MODE,
    )?;
    let actual = storage_identity(validation.as_raw_handle() as HANDLE, kind)?;
    verify_storage_handle(validation.as_raw_handle() as HANDLE)?;
    while_validation_open(&validation)?;
    if actual == expected {
        Ok(())
    } else {
        Err(security_verification_failed())
    }
}

fn open_windows_path(path: &Path, access: u32, disposition: u32, flags: u32) -> io::Result<File> {
    open_windows_path_with_share(path, access, disposition, flags, STORAGE_SHARE_MODE)
}

fn open_windows_path_with_share(
    path: &Path,
    access: u32,
    disposition: u32,
    flags: u32,
    share_mode: u32,
) -> io::Result<File> {
    let wide = wide_path(path)?;
    let handle = match disposition {
        OPEN_EXISTING => unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                share_mode,
                null(),
                disposition,
                flags,
                null_mut(),
            )
        },
        CREATE_NEW | OPEN_ALWAYS => {
            let mut security = CreationSecurity::new()?;
            let attributes = security.attributes();
            unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    access,
                    share_mode,
                    &attributes,
                    disposition,
                    flags,
                    null_mut(),
                )
            }
        }
        _ => return Err(security_operation_failed()),
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_win32_error());
    }
    if handle.is_null() {
        return Err(security_operation_failed());
    }

    Ok(unsafe { File::from_raw_handle(handle as _) })
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows storage path",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn storage_identity(handle: HANDLE, kind: StorageObjectKind) -> io::Result<StorageIdentity> {
    validate_handle(handle)?;
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return Err(security_verification_failed());
    }

    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_win32_error());
    }
    let is_directory = attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if attributes.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE) != 0
        || is_directory != kind.is_directory()
    {
        return Err(security_verification_failed());
    }

    let mut identity = FILE_ID_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut identity as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_win32_error());
    }

    Ok(StorageIdentity {
        volume_serial_number: identity.VolumeSerialNumber,
        file_id: identity.FileId.Identifier,
    })
}

fn flush_storage_directory_handle(handle: HANDLE) -> io::Result<()> {
    storage_identity(handle, StorageObjectKind::Directory)?;
    verify_storage_handle(handle)?;
    if unsafe { FlushFileBuffers(handle) } == 0 {
        Err(last_win32_error())
    } else {
        Ok(())
    }
}

/// Fill a newly allocated buffer with the Windows system-preferred CNG RNG.
/// No fallback is permitted: an NTSTATUS failure returns no bytes.
#[allow(dead_code)] // Stage 2A1 primitive; production callers are wired later.
pub(crate) fn cng_random_bytes(len: usize) -> io::Result<Vec<u8>> {
    random_bytes_with(len, |buffer, buffer_len| unsafe {
        BCryptGenRandom(
            null_mut(),
            buffer,
            buffer_len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    })
}

fn random_bytes_with(
    len: usize,
    fill: impl FnOnce(*mut u8, u32) -> NTSTATUS,
) -> io::Result<Vec<u8>> {
    let len_u32 = u32::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "requested random buffer is too large",
        )
    })?;
    let mut bytes = vec![0u8; len];
    if bytes.is_empty() {
        return Ok(bytes);
    }

    // BCrypt's BCRYPT_SUCCESS macro is the standard NT_SUCCESS(status >= 0)
    // predicate. On failure, clear the local buffer before returning only Err.
    if fill(bytes.as_mut_ptr(), len_u32) < 0 {
        bytes.fill(0);
        return Err(io::Error::other("secure random generation failed"));
    }
    Ok(bytes)
}

/// Verify without modifying a file or directory handle's owner or DACL.
#[allow(dead_code)] // Stage 2A1 primitive; production callers are wired later.
pub(crate) fn verify_storage_handle(handle: HANDLE) -> io::Result<()> {
    validate_handle(handle)?;
    let current_user = current_process_user_sid()?;
    let local_system = well_known_sid(WinLocalSystemSid)?;
    verify_storage_handle_with(handle, &current_user, &local_system)
}

fn verify_storage_handle_with(
    handle: HANDLE,
    current_user: &Sid,
    local_system: &Sid,
) -> io::Result<()> {
    let snapshot = read_security_snapshot(handle)?;
    inspect_owner(&snapshot.owner, current_user.as_bytes())?;
    inspect_acl(
        &snapshot.acl,
        current_user.as_bytes(),
        local_system.as_bytes(),
    )
}

fn inspect_owner(owner: &[u8], current_user: &[u8]) -> io::Result<()> {
    if !owner.is_empty() && owner == current_user {
        Ok(())
    } else {
        Err(security_verification_failed())
    }
}

fn validate_handle(handle: HANDLE) -> io::Result<()> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows storage handle",
        ))
    } else {
        Ok(())
    }
}

fn expected_principals<'a>(current_user: &'a Sid, local_system: &'a Sid) -> Vec<&'a Sid> {
    if current_user.as_bytes() == local_system.as_bytes() {
        vec![current_user]
    } else {
        vec![current_user, local_system]
    }
}

struct AlignedBuffer {
    words: Vec<usize>,
    byte_len: usize,
}

impl AlignedBuffer {
    fn zeroed(byte_len: usize) -> io::Result<Self> {
        if byte_len == 0 {
            return Err(security_operation_failed());
        }
        let word_size = size_of::<usize>();
        let word_count = byte_len
            .checked_add(word_size - 1)
            .ok_or_else(security_operation_failed)?
            / word_size;
        Ok(Self {
            words: vec![0usize; word_count],
            byte_len,
        })
    }

    fn as_ptr(&self) -> *const u8 {
        self.words.as_ptr().cast()
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.words.as_mut_ptr().cast()
    }

    fn len(&self) -> usize {
        self.byte_len
    }
}

struct Sid {
    buffer: AlignedBuffer,
    byte_len: usize,
}

impl Sid {
    fn zeroed(byte_len: usize) -> io::Result<Self> {
        Ok(Self {
            buffer: AlignedBuffer::zeroed(byte_len)?,
            byte_len,
        })
    }

    fn as_psid(&self) -> PSID {
        self.buffer.as_ptr() as PSID
    }

    fn as_mut_psid(&mut self) -> PSID {
        self.buffer.as_mut_ptr().cast()
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.buffer.as_ptr(), self.byte_len) }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(self.0);
            }
        }
    }
}

fn current_process_user_sid() -> io::Result<Sid> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_win32_error());
    }
    if token.is_null() || token == INVALID_HANDLE_VALUE {
        return Err(security_operation_failed());
    }
    let token = OwnedHandle(token);

    let mut required = 0u32;
    let first = unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required) };
    let first_error = unsafe { GetLastError() };
    if first != 0
        || first_error != ERROR_INSUFFICIENT_BUFFER
        || required < size_of::<TOKEN_USER>() as u32
    {
        return Err(security_operation_failed());
    }

    let mut token_buffer = AlignedBuffer::zeroed(required as usize)?;
    let mut returned = 0u32;
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            token_buffer.as_mut_ptr().cast(),
            required,
            &mut returned,
        )
    } == 0
    {
        return Err(last_win32_error());
    }
    if returned < size_of::<TOKEN_USER>() as u32 || returned > required {
        return Err(security_operation_failed());
    }

    let token_user = unsafe { ptr::read_unaligned(token_buffer.as_ptr().cast::<TOKEN_USER>()) };
    copy_sid_from_bounded(
        token_user.User.Sid,
        token_buffer.as_ptr(),
        returned as usize,
    )
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Sid> {
    let mut required = 0u32;
    let first = unsafe { CreateWellKnownSid(kind, null_mut(), null_mut(), &mut required) };
    let first_error = unsafe { GetLastError() };
    if first != 0 || first_error != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(security_operation_failed());
    }

    let mut sid = Sid::zeroed(required as usize)?;
    let mut returned = required;
    if unsafe { CreateWellKnownSid(kind, null_mut(), sid.as_mut_psid(), &mut returned) } == 0 {
        return Err(last_win32_error());
    }
    if returned == 0 || returned > required {
        return Err(security_operation_failed());
    }

    let validated = validate_sid_within(sid.as_psid(), sid.buffer.as_ptr(), returned as usize)?;
    if validated != returned as usize {
        return Err(security_operation_failed());
    }
    sid.byte_len = validated;
    Ok(sid)
}

fn copy_sid_from_bounded(source: PSID, base: *const u8, available: usize) -> io::Result<Sid> {
    let byte_len = validate_sid_within(source, base, available)?;
    let mut copy = Sid::zeroed(byte_len)?;
    if unsafe { CopySid(byte_len as u32, copy.as_mut_psid(), source) } == 0 {
        return Err(last_win32_error());
    }
    if validate_sid_within(copy.as_psid(), copy.buffer.as_ptr(), copy.buffer.len())? != byte_len {
        return Err(security_operation_failed());
    }
    Ok(copy)
}

fn validate_sid_within(source: PSID, base: *const u8, available: usize) -> io::Result<usize> {
    if source.is_null() || base.is_null() {
        return Err(security_operation_failed());
    }

    let base_address = base as usize;
    let end_address = base_address
        .checked_add(available)
        .ok_or_else(security_operation_failed)?;
    let sid_address = source as usize;
    let header_end = sid_address
        .checked_add(SID_HEADER_LEN)
        .ok_or_else(security_operation_failed)?;
    if sid_address < base_address || header_end > end_address {
        return Err(security_operation_failed());
    }

    let sub_authority_count = unsafe { *(source.cast::<u8>().add(1)) } as usize;
    let byte_len = SID_HEADER_LEN
        .checked_add(
            sub_authority_count
                .checked_mul(size_of::<u32>())
                .ok_or_else(security_operation_failed)?,
        )
        .ok_or_else(security_operation_failed)?;
    let sid_end = sid_address
        .checked_add(byte_len)
        .ok_or_else(security_operation_failed)?;
    if sid_end > end_address || unsafe { IsValidSid(source) } == 0 {
        return Err(security_operation_failed());
    }
    if unsafe { GetLengthSid(source) } as usize != byte_len {
        return Err(security_operation_failed());
    }
    Ok(byte_len)
}

struct AclBuffer(AlignedBuffer);

impl AclBuffer {
    fn as_ptr(&self) -> *const ACL {
        self.0.as_ptr().cast()
    }
}

fn build_full_control_acl(principals: &[&Sid]) -> io::Result<AclBuffer> {
    if principals.is_empty() {
        return Err(security_operation_failed());
    }

    let ace_prefix_len = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();
    let mut acl_len = size_of::<ACL>();
    for principal in principals {
        acl_len = acl_len
            .checked_add(
                ace_prefix_len
                    .checked_add(principal.byte_len)
                    .ok_or_else(security_operation_failed)?,
            )
            .ok_or_else(security_operation_failed)?;
    }
    if acl_len > u16::MAX as usize {
        return Err(security_operation_failed());
    }

    let mut buffer = AlignedBuffer::zeroed(acl_len)?;
    let acl = buffer.as_mut_ptr().cast::<ACL>();
    if unsafe { InitializeAcl(acl, acl_len as u32, ACL_REVISION) } == 0 {
        return Err(last_win32_error());
    }
    for principal in principals {
        if unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                NO_INHERITANCE,
                FILE_ALL_ACCESS,
                principal.as_psid(),
            )
        } == 0
        {
            return Err(last_win32_error());
        }
    }
    if unsafe { IsValidAcl(acl) } == 0 {
        return Err(security_operation_failed());
    }
    Ok(AclBuffer(buffer))
}

struct CreationSecurity {
    // These allocations back pointers stored in the absolute descriptor.
    _owner: Sid,
    _acl: AclBuffer,
    descriptor: SECURITY_DESCRIPTOR,
}

impl CreationSecurity {
    fn new() -> io::Result<Self> {
        let owner = current_process_user_sid()?;
        let local_system = well_known_sid(WinLocalSystemSid)?;
        let principals = expected_principals(&owner, &local_system);
        let acl = build_full_control_acl(&principals)?;
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_pointer: PSECURITY_DESCRIPTOR =
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();

        if unsafe {
            InitializeSecurityDescriptor(descriptor_pointer, SECURITY_DESCRIPTOR_REVISION_VALUE)
        } == 0
        {
            return Err(last_win32_error());
        }
        if unsafe { SetSecurityDescriptorOwner(descriptor_pointer, owner.as_psid(), 0) } == 0 {
            return Err(last_win32_error());
        }
        if unsafe { SetSecurityDescriptorDacl(descriptor_pointer, 1, acl.as_ptr(), 0) } == 0 {
            return Err(last_win32_error());
        }
        if unsafe {
            SetSecurityDescriptorControl(descriptor_pointer, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        } == 0
        {
            return Err(last_win32_error());
        }
        if unsafe { IsValidSecurityDescriptor(descriptor_pointer) } == 0 {
            return Err(security_operation_failed());
        }

        Ok(Self {
            _owner: owner,
            _acl: acl,
            descriptor,
        })
    }

    fn attributes(&mut self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut self.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        }
    }
}

#[cfg(test)]
fn set_protected_handle_dacl(handle: HANDLE, acl: *const ACL) -> io::Result<()> {
    let status = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(win32_status_error(status))
    }
}

#[derive(Clone, Eq, PartialEq)]
struct AclSnapshot {
    dacl_present: bool,
    dacl_null: bool,
    protected: bool,
    aces: Vec<AceSnapshot>,
}

#[derive(Clone, Eq, PartialEq)]
struct AceSnapshot {
    ace_type: u8,
    flags: u8,
    mask: u32,
    sid: Vec<u8>,
}

struct SecuritySnapshot {
    owner: Vec<u8>,
    acl: AclSnapshot,
}

fn read_acl_snapshot(handle: HANDLE) -> io::Result<AclSnapshot> {
    Ok(read_security_snapshot(handle)?.acl)
}

fn read_security_snapshot(handle: HANDLE) -> io::Result<SecuritySnapshot> {
    validate_handle(handle)?;

    let mut owner = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    let descriptor_allocation = LocalAllocation(descriptor);
    if status != ERROR_SUCCESS {
        return Err(win32_status_error(status));
    }
    if descriptor_allocation.0.is_null() {
        return Err(security_operation_failed());
    }

    let descriptor_len = unsafe { GetSecurityDescriptorLength(descriptor_allocation.0) } as usize;
    if descriptor_len == 0 {
        return Err(security_operation_failed());
    }
    let owner_len =
        validate_sid_within(owner, descriptor_allocation.0.cast::<u8>(), descriptor_len)?;
    let owner = unsafe { slice::from_raw_parts(owner.cast::<u8>(), owner_len) }.to_vec();

    let mut control = 0u16;
    let mut revision = 0u32;
    if unsafe { GetSecurityDescriptorControl(descriptor_allocation.0, &mut control, &mut revision) }
        == 0
    {
        return Err(last_win32_error());
    }

    let mut dacl_present = 0;
    let mut dacl = null_mut();
    let mut dacl_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor_allocation.0,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(last_win32_error());
    }

    let mut snapshot = AclSnapshot {
        dacl_present: dacl_present != 0,
        dacl_null: dacl.is_null(),
        protected: control & SE_DACL_PROTECTED != 0,
        aces: Vec::new(),
    };
    if !snapshot.dacl_present || snapshot.dacl_null {
        return Ok(SecuritySnapshot {
            owner,
            acl: snapshot,
        });
    }
    let descriptor_start = descriptor_allocation.0 as usize;
    let descriptor_end = descriptor_start
        .checked_add(descriptor_len)
        .ok_or_else(security_verification_failed)?;
    let acl_start = dacl as usize;
    let acl_header_end = acl_start
        .checked_add(size_of::<ACL>())
        .ok_or_else(security_verification_failed)?;
    if acl_start < descriptor_start || acl_header_end > descriptor_end {
        return Err(security_verification_failed());
    }

    let acl_header = unsafe { ptr::read_unaligned(dacl) };
    let acl_size = acl_header.AclSize as usize;
    let allocated_acl_end = acl_start
        .checked_add(acl_size)
        .ok_or_else(security_verification_failed)?;
    if acl_size < size_of::<ACL>() || allocated_acl_end > descriptor_end {
        return Err(security_verification_failed());
    }
    if unsafe { IsValidAcl(dacl) } == 0 {
        return Err(security_verification_failed());
    }

    let mut info = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(last_win32_error());
    }

    let bytes_in_use = info.AclBytesInUse as usize;
    if bytes_in_use < size_of::<ACL>() || bytes_in_use > acl_size {
        return Err(security_verification_failed());
    }
    let acl_end = acl_start
        .checked_add(bytes_in_use)
        .ok_or_else(security_verification_failed)?;
    let first_ace = acl_header_end;

    snapshot.aces.reserve(info.AceCount as usize);
    for index in 0..info.AceCount {
        let mut ace = null_mut::<c_void>();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(last_win32_error());
        }
        if ace.is_null() {
            return Err(security_verification_failed());
        }

        let ace_start = ace as usize;
        let header_end = ace_start
            .checked_add(size_of::<ACE_HEADER>())
            .ok_or_else(security_verification_failed)?;
        if ace_start < first_ace || header_end > acl_end {
            return Err(security_verification_failed());
        }
        let header = unsafe { ptr::read_unaligned(ace.cast::<ACE_HEADER>()) };
        let ace_size = header.AceSize as usize;
        let ace_end = ace_start
            .checked_add(ace_size)
            .ok_or_else(security_verification_failed)?;
        if ace_size < size_of::<ACE_HEADER>() || ace_end > acl_end {
            return Err(security_verification_failed());
        }

        if header.AceType != ACCESS_ALLOWED_ACE_TYPE_VALUE {
            snapshot.aces.push(AceSnapshot {
                ace_type: header.AceType,
                flags: header.AceFlags,
                mask: 0,
                sid: Vec::new(),
            });
            continue;
        }
        if ace_size < ACE_SID_OFFSET + SID_HEADER_LEN {
            return Err(security_verification_failed());
        }

        let ace_bytes = ace.cast::<u8>();
        let mask =
            unsafe { ptr::read_unaligned(ace_bytes.add(size_of::<ACE_HEADER>()).cast::<u32>()) };
        let sid = unsafe { ace_bytes.add(ACE_SID_OFFSET) } as PSID;
        let sid_len = validate_sid_within(sid, ace_bytes, ace_size)?;
        if ACE_SID_OFFSET + sid_len != ace_size {
            return Err(security_verification_failed());
        }
        let sid = unsafe { slice::from_raw_parts(sid.cast::<u8>(), sid_len) }.to_vec();
        snapshot.aces.push(AceSnapshot {
            ace_type: header.AceType,
            flags: header.AceFlags,
            mask,
            sid,
        });
    }

    Ok(SecuritySnapshot {
        owner,
        acl: snapshot,
    })
}

fn inspect_acl(snapshot: &AclSnapshot, current_user: &[u8], local_system: &[u8]) -> io::Result<()> {
    if current_user.is_empty()
        || local_system.is_empty()
        || !snapshot.dacl_present
        || snapshot.dacl_null
        || !snapshot.protected
    {
        return Err(security_verification_failed());
    }

    let same_principal = current_user == local_system;
    let expected_count = if same_principal { 1 } else { 2 };
    if snapshot.aces.len() != expected_count {
        return Err(security_verification_failed());
    }

    let mut current_seen = false;
    let mut system_seen = same_principal;
    for ace in &snapshot.aces {
        if ace.ace_type != ACCESS_ALLOWED_ACE_TYPE_VALUE
            || ace.flags != NO_INHERITANCE as u8
            || ace.mask != FILE_ALL_ACCESS
        {
            return Err(security_verification_failed());
        }

        if ace.sid == current_user {
            if current_seen {
                return Err(security_verification_failed());
            }
            current_seen = true;
        } else if !same_principal && ace.sid == local_system {
            if system_seen {
                return Err(security_verification_failed());
            }
            system_seen = true;
        } else {
            // This rejects every broad or foreign SID, including Users,
            // Authenticated Users, and Everyone, for both allow and deny ACEs.
            return Err(security_verification_failed());
        }
    }

    if current_seen && system_seen {
        Ok(())
    } else {
        Err(security_verification_failed())
    }
}

fn last_win32_error() -> io::Error {
    win32_status_error(unsafe { GetLastError() })
}

fn win32_status_error(status: u32) -> io::Error {
    io::Error::from_raw_os_error(status as i32)
}

fn security_operation_failed() -> io::Error {
    io::Error::other("Windows security operation failed")
}

fn security_verification_failed() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Windows storage security verification failed",
    )
}

fn is_security_verification_failure(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied && error.raw_os_error().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::mem;
    use std::os::windows::fs::{symlink_file, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{WinAuthenticatedUserSid, WinWorldSid, INHERITED_ACE};
    use windows_sys::Win32::Storage::FileSystem::{DELETE, READ_CONTROL, WRITE_DAC};

    const ACCESS_DENIED_ACE_TYPE_VALUE: u8 = 1;
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempArtifact {
        file: Option<File>,
        path: PathBuf,
    }

    impl TempArtifact {
        fn create() -> io::Result<Self> {
            for _ in 0..32 {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "tokenbar-storage-{}-{timestamp}-{sequence}.tmp",
                    std::process::id()
                ));
                let result = open_windows_path(
                    &path,
                    GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC,
                    CREATE_NEW,
                    FILE_ATTRIBUTE_NORMAL,
                );
                match result {
                    Ok(file) => {
                        return Ok(Self {
                            file: Some(file),
                            path,
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "unable to create temporary storage artifact",
            ))
        }

        fn handle(&self) -> HANDLE {
            self.file
                .as_ref()
                .expect("temporary file is open")
                .as_raw_handle() as HANDLE
        }

        fn cleanup(mut self) -> io::Result<()> {
            self.file.take();
            let path = mem::take(&mut self.path);
            fs::remove_file(path)
        }
    }

    impl Drop for TempArtifact {
        fn drop(&mut self) {
            self.file.take();
            if !self.path.as_os_str().is_empty() {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn create() -> io::Result<Self> {
            for _ in 0..32 {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "tokenbar-storage-root-{}-{timestamp}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self { path }),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "unable to create temporary storage root",
            ))
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }

        fn cleanup(mut self) -> io::Result<()> {
            let path = mem::take(&mut self.path);
            fs::remove_dir_all(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            if !self.path.as_os_str().is_empty() {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }

    fn create_test_file(path: &Path, contents: &[u8]) -> io::Result<File> {
        let mut file = open_windows_path(
            path,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
        )?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(file)
    }

    fn create_permissive_test_file(path: &Path, contents: &[u8]) -> io::Result<File> {
        let file = create_test_file(path, contents)?;
        let everyone = well_known_sid(WinWorldSid)?;
        let acl = build_full_control_acl(&[&everyone])?;
        set_protected_handle_dacl(file.as_raw_handle() as HANDLE, acl.as_ptr())?;
        Ok(file)
    }

    fn create_permissive_test_directory(path: &Path) -> io::Result<File> {
        drop(ensure_secure_storage_directory(path)?);
        let directory = open_windows_path(
            path,
            GENERIC_WRITE | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC,
            OPEN_EXISTING,
            StorageObjectKind::Directory.open_flags(),
        )?;
        let everyone = well_known_sid(WinWorldSid)?;
        let acl = build_full_control_acl(&[&everyone])?;
        set_protected_handle_dacl(directory.as_raw_handle() as HANDLE, acl.as_ptr())?;
        Ok(directory)
    }

    fn create_junction(link: &Path, target: &Path) -> io::Result<()> {
        let output = Command::new("cmd")
            .arg("/D")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other("unable to create test junction"))
        }
    }

    fn read_open_file(file: &mut File) -> io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn regular_file_identity(file: &File) -> io::Result<StorageIdentity> {
        storage_identity(
            file.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )
    }

    fn create_secure_test_file(
        path: &Path,
        contents: &[u8],
    ) -> io::Result<(File, StorageIdentity)> {
        let mut file = create_new_secure_file(path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        let identity = regular_file_identity(&file)?;
        Ok((file, identity))
    }

    fn read_secure_test_file(path: &Path) -> io::Result<(Vec<u8>, StorageIdentity)> {
        let mut file = open_existing_secure_file(path, false)?;
        let identity = regular_file_identity(&file)?;
        let bytes = read_open_file(&mut file)?;
        Ok((bytes, identity))
    }

    fn sid_string(sid: &Sid) -> io::Result<String> {
        let mut wide = null_mut();
        if unsafe { ConvertSidToStringSidW(sid.as_psid(), &mut wide) } == 0 {
            return Err(last_win32_error());
        }
        let allocation = LocalAllocation(wide.cast());
        if allocation.0.is_null() {
            return Err(security_operation_failed());
        }

        let mut len = 0usize;
        while len < 256 && unsafe { *wide.add(len) } != 0 {
            len += 1;
        }
        if len == 256 {
            return Err(security_operation_failed());
        }
        String::from_utf16(unsafe { slice::from_raw_parts(wide, len) })
            .map_err(|_| security_operation_failed())
    }

    fn assert_generic_error(error: &io::Error, sensitive_values: &[&str]) {
        let message = error.to_string().to_lowercase();
        for value in sensitive_values {
            if !value.is_empty() {
                assert!(
                    !message.contains(&value.to_lowercase()),
                    "security error does not disclose sensitive context"
                );
            }
        }
    }

    #[test]
    fn cng_random_returns_requested_distinct_bytes() {
        let first = cng_random_bytes(32).expect("first CNG request succeeds");
        let second = cng_random_bytes(32).expect("second CNG request succeeds");
        assert!(first.len() == 32, "first CNG result has requested length");
        assert!(second.len() == 32, "second CNG result has requested length");
        assert!(first != second, "independent CNG requests differ");
    }

    #[test]
    fn cng_failure_returns_no_partially_filled_bytes() {
        let result = random_bytes_with(32, |buffer, buffer_len| {
            unsafe {
                ptr::write_bytes(buffer, 0xA5, buffer_len as usize);
            }
            -1
        });
        assert!(result.is_err(), "failed CNG request returns only an error");
    }

    #[test]
    fn creation_dacl_round_trips_exact_principals_and_permissions() {
        let artifact = TempArtifact::create().expect("create secure temporary file");
        verify_storage_handle(artifact.handle()).expect("creation security verifies");

        let current_user = current_process_user_sid().expect("read current process SID");
        let local_system = well_known_sid(WinLocalSystemSid).expect("create LocalSystem SID");
        let snapshot = read_acl_snapshot(artifact.handle()).expect("read temporary file DACL");
        inspect_acl(&snapshot, current_user.as_bytes(), local_system.as_bytes())
            .expect("DACL satisfies exact contract");
        let expected_count = if current_user.as_bytes() == local_system.as_bytes() {
            1
        } else {
            2
        };
        assert!(snapshot.dacl_present, "DACL is present");
        assert!(!snapshot.dacl_null, "DACL is non-null");
        assert!(snapshot.protected, "DACL is protected");
        assert!(
            snapshot.aces.len() == expected_count,
            "DACL has exactly the expected principals"
        );
        assert!(
            snapshot
                .aces
                .iter()
                .all(|ace| ace.flags & INHERITED_ACE as u8 == 0),
            "DACL has no inherited ACE"
        );

        artifact.cleanup().expect("remove temporary file");
    }

    #[test]
    fn broad_aces_fail_closed_without_in_place_repair() {
        let artifact = TempArtifact::create().expect("create temporary file");

        for kind in [WinWorldSid, WinAuthenticatedUserSid] {
            let broad_sid = well_known_sid(kind).expect("create broad well-known SID");
            let broad_acl =
                build_full_control_acl(&[&broad_sid]).expect("build permissive temporary ACL");
            set_protected_handle_dacl(artifact.handle(), broad_acl.as_ptr())
                .expect("apply permissive temporary ACL");
            let before = read_acl_snapshot(artifact.handle()).expect("snapshot broad DACL");

            assert!(
                verify_storage_handle(artifact.handle()).is_err(),
                "broad ACE is rejected"
            );
            assert!(
                read_acl_snapshot(artifact.handle()).expect("read rejected DACL") == before,
                "verification never repairs the broad DACL in place"
            );
        }

        artifact.cleanup().expect("remove temporary file");
    }

    #[test]
    fn owner_mismatch_fails_closed() {
        let current_user = current_process_user_sid().expect("read current process SID");
        let local_system = well_known_sid(WinLocalSystemSid).expect("create LocalSystem SID");
        let everyone = well_known_sid(WinWorldSid).expect("create Everyone SID");
        let foreign_owner = if current_user.as_bytes() != local_system.as_bytes() {
            &local_system
        } else {
            &everyone
        };

        inspect_owner(foreign_owner.as_bytes(), current_user.as_bytes())
            .expect_err("foreign owner is rejected");
        inspect_owner(current_user.as_bytes(), current_user.as_bytes())
            .expect("current process owner is accepted");
    }

    #[test]
    fn pure_acl_inspection_rejects_missing_null_extra_deny_and_incomplete_entries() {
        let current_user = current_process_user_sid().expect("read current process SID");
        let local_system = well_known_sid(WinLocalSystemSid).expect("create LocalSystem SID");
        let everyone = well_known_sid(WinWorldSid).expect("create Everyone SID");
        let mut aces = vec![AceSnapshot {
            ace_type: ACCESS_ALLOWED_ACE_TYPE_VALUE,
            flags: NO_INHERITANCE as u8,
            mask: FILE_ALL_ACCESS,
            sid: current_user.as_bytes().to_vec(),
        }];
        if current_user.as_bytes() != local_system.as_bytes() {
            aces.push(AceSnapshot {
                ace_type: ACCESS_ALLOWED_ACE_TYPE_VALUE,
                flags: NO_INHERITANCE as u8,
                mask: FILE_ALL_ACCESS,
                sid: local_system.as_bytes().to_vec(),
            });
        }
        let valid = AclSnapshot {
            dacl_present: true,
            dacl_null: false,
            protected: true,
            aces,
        };
        assert!(
            inspect_acl(&valid, current_user.as_bytes(), local_system.as_bytes()).is_ok(),
            "baseline ACL is accepted"
        );

        let mut missing = valid.clone();
        missing.dacl_present = false;
        assert!(
            inspect_acl(&missing, current_user.as_bytes(), local_system.as_bytes()).is_err(),
            "missing DACL is rejected"
        );

        let mut null_dacl = valid.clone();
        null_dacl.dacl_null = true;
        assert!(
            inspect_acl(&null_dacl, current_user.as_bytes(), local_system.as_bytes()).is_err(),
            "null DACL is rejected"
        );

        let mut unprotected = valid.clone();
        unprotected.protected = false;
        assert!(
            inspect_acl(
                &unprotected,
                current_user.as_bytes(),
                local_system.as_bytes()
            )
            .is_err(),
            "unprotected DACL is rejected"
        );

        let mut inherited = valid.clone();
        inherited.aces[0].flags = INHERITED_ACE as u8;
        assert!(
            inspect_acl(&inherited, current_user.as_bytes(), local_system.as_bytes()).is_err(),
            "inherited ACE is rejected"
        );

        let mut extra = valid.clone();
        extra.aces.push(AceSnapshot {
            ace_type: ACCESS_ALLOWED_ACE_TYPE_VALUE,
            flags: NO_INHERITANCE as u8,
            mask: FILE_ALL_ACCESS,
            sid: everyone.as_bytes().to_vec(),
        });
        assert!(
            inspect_acl(&extra, current_user.as_bytes(), local_system.as_bytes()).is_err(),
            "extra broad allow ACE is rejected"
        );

        let mut denied = valid.clone();
        denied.aces.push(AceSnapshot {
            ace_type: ACCESS_DENIED_ACE_TYPE_VALUE,
            flags: NO_INHERITANCE as u8,
            mask: FILE_ALL_ACCESS,
            sid: everyone.as_bytes().to_vec(),
        });
        assert!(
            inspect_acl(&denied, current_user.as_bytes(), local_system.as_bytes()).is_err(),
            "extra broad deny ACE is rejected"
        );

        let mut incomplete = valid;
        incomplete.aces[0].mask = FILE_ALL_ACCESS & !1;
        assert!(
            inspect_acl(
                &incomplete,
                current_user.as_bytes(),
                local_system.as_bytes()
            )
            .is_err(),
            "incomplete access mask is rejected"
        );
    }

    #[test]
    fn secure_root_resolver_preserves_legacy_preferred_and_uses_exact_fallback() {
        let root = TempRoot::create().expect("create temporary root");
        let preferred = root.join("com.nyanako.tokenbar");
        let fallback = root.join("com.nyanako.tokenbar.secure");
        fs::create_dir(&preferred).expect("create inherited legacy directory");
        let preferred_handle = open_windows_path(
            &preferred,
            FILE_READ_ATTRIBUTES | READ_CONTROL,
            OPEN_EXISTING,
            StorageObjectKind::Directory.open_flags(),
        )
        .expect("open inherited legacy directory");
        assert!(
            verify_storage_handle(preferred_handle.as_raw_handle() as HANDLE).is_err(),
            "legacy directory does not satisfy the exact DACL contract"
        );
        let preferred_identity = storage_identity(
            preferred_handle.as_raw_handle() as HANDLE,
            StorageObjectKind::Directory,
        )
        .expect("snapshot legacy directory identity");
        let preferred_acl = read_acl_snapshot(preferred_handle.as_raw_handle() as HANDLE)
            .expect("snapshot legacy directory DACL");

        let v1_path = preferred.join("codex-weekly-history.json");
        let v1_bytes = b"legacy-v1-sentinel";
        fs::write(&v1_path, v1_bytes).expect("write legacy v1 sentinel");
        let v1_handle = open_windows_path(
            &v1_path,
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
        )
        .expect("open legacy v1 sentinel");
        let v1_identity = regular_file_identity(&v1_handle).expect("snapshot v1 identity");
        let v1_mtime = fs::metadata(&v1_path)
            .expect("read v1 metadata")
            .modified()
            .expect("read v1 mtime");

        assert_eq!(
            resolve_secure_storage_directory(&preferred).expect("resolve secure sibling"),
            fallback
        );
        let fallback_handle = ensure_secure_storage_directory(&fallback)
            .expect("fallback remains an exact secure directory");
        verify_storage_handle(fallback_handle.as_raw_handle() as HANDLE)
            .expect("fallback DACL is exact");

        let preferred_after = open_windows_path(
            &preferred,
            FILE_READ_ATTRIBUTES | READ_CONTROL,
            OPEN_EXISTING,
            StorageObjectKind::Directory.open_flags(),
        )
        .expect("reopen legacy directory");
        assert!(
            storage_identity(
                preferred_after.as_raw_handle() as HANDLE,
                StorageObjectKind::Directory
            )
            .expect("reread legacy directory identity")
                == preferred_identity
        );
        assert!(
            read_acl_snapshot(preferred_after.as_raw_handle() as HANDLE)
                .expect("reread legacy directory DACL")
                == preferred_acl,
            "resolver never secures the legacy root in place"
        );
        let v1_after = open_windows_path(
            &v1_path,
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
        )
        .expect("reopen legacy v1 sentinel");
        assert!(regular_file_identity(&v1_after).expect("reread v1 identity") == v1_identity);
        assert_eq!(fs::read(&v1_path).expect("reread v1 bytes"), v1_bytes);
        assert_eq!(
            fs::metadata(&v1_path)
                .expect("reread v1 metadata")
                .modified()
                .expect("reread v1 mtime"),
            v1_mtime
        );
        assert_eq!(
            fs::read_dir(&preferred)
                .expect("list legacy directory")
                .map(|entry| entry.expect("read legacy entry").file_name())
                .collect::<Vec<_>>(),
            [std::ffi::OsString::from("codex-weekly-history.json")]
        );

        drop(v1_after);
        drop(v1_handle);
        drop(preferred_after);
        drop(preferred_handle);
        drop(fallback_handle);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_root_resolver_keeps_existing_fallback_sticky() {
        let root = TempRoot::create().expect("create temporary root");
        let preferred = root.join("com.nyanako.tokenbar");
        let fallback = root.join("com.nyanako.tokenbar.secure");
        drop(
            ensure_secure_storage_directory(&fallback)
                .expect("create exact secure fallback before preferred"),
        );

        assert_eq!(
            resolve_secure_storage_directory(&preferred)
                .expect("existing fallback wins while preferred is absent"),
            fallback
        );
        drop(
            ensure_secure_storage_directory(&preferred)
                .expect("create exact preferred after fallback"),
        );
        assert_eq!(
            resolve_secure_storage_directory(&preferred)
                .expect("existing fallback remains sticky after preferred is exact"),
            fallback
        );

        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_root_resolver_uses_exact_preferred_when_fallback_is_absent() {
        let root = TempRoot::create().expect("create temporary root");
        let preferred = root.join("com.nyanako.tokenbar");
        let fallback = root.join("com.nyanako.tokenbar.secure");
        drop(ensure_secure_storage_directory(&preferred).expect("create exact secure preferred"));

        assert_eq!(
            resolve_secure_storage_directory(&preferred).expect("resolve exact preferred"),
            preferred
        );
        assert!(
            !fallback.exists(),
            "resolver does not create an unused fallback"
        );

        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_root_resolver_fails_closed_on_fallback_collisions() {
        {
            let root = TempRoot::create().expect("create temporary root");
            let preferred = root.join("com.nyanako.tokenbar");
            let fallback = root.join("com.nyanako.tokenbar.secure");
            let fallback_file =
                create_test_file(&fallback, b"fallback-file").expect("create file collision");
            let fallback_identity =
                regular_file_identity(&fallback_file).expect("snapshot file collision identity");

            resolve_secure_storage_directory(&preferred)
                .expect_err("regular-file fallback collision fails closed");
            assert!(
                !preferred.exists(),
                "preferred receives no secure artifacts"
            );
            assert_eq!(
                fs::read(&fallback).expect("read file collision"),
                b"fallback-file"
            );
            assert!(
                regular_file_identity(&fallback_file).expect("reread file collision identity")
                    == fallback_identity
            );

            drop(fallback_file);
            root.cleanup().expect("remove temporary root");
        }

        {
            let root = TempRoot::create().expect("create temporary root");
            let preferred = root.join("com.nyanako.tokenbar");
            let fallback = root.join("com.nyanako.tokenbar.secure");
            let target = root.join("fallback-target");
            let target_file =
                create_test_file(&target, b"fallback-target").expect("create reparse target");
            let target_identity =
                regular_file_identity(&target_file).expect("snapshot reparse target identity");
            symlink_file(&target, &fallback).expect("create fallback reparse collision");

            resolve_secure_storage_directory(&preferred)
                .expect_err("fallback reparse collision fails closed");
            assert!(
                !preferred.exists(),
                "preferred receives no secure artifacts"
            );
            assert!(fs::symlink_metadata(&fallback)
                .expect("fallback reparse remains")
                .file_type()
                .is_symlink());
            assert_eq!(
                fs::read(&target).expect("read reparse target"),
                b"fallback-target"
            );
            assert!(
                regular_file_identity(&target_file).expect("reread reparse target identity")
                    == target_identity
            );

            fs::remove_file(&fallback).expect("remove fallback reparse collision");
            drop(target_file);
            root.cleanup().expect("remove temporary root");
        }

        {
            let root = TempRoot::create().expect("create temporary root");
            let preferred = root.join("com.nyanako.tokenbar");
            let fallback = root.join("com.nyanako.tokenbar.secure");
            let fallback_directory = create_permissive_test_directory(&fallback)
                .expect("create permissive fallback collision");
            let fallback_acl = read_acl_snapshot(fallback_directory.as_raw_handle() as HANDLE)
                .expect("snapshot permissive fallback DACL");

            resolve_secure_storage_directory(&preferred)
                .expect_err("permissive fallback collision fails closed");
            assert!(
                !preferred.exists(),
                "preferred receives no secure artifacts"
            );
            assert!(
                read_acl_snapshot(fallback_directory.as_raw_handle() as HANDLE)
                    .expect("reread permissive fallback DACL")
                    == fallback_acl,
                "collision rejection never repairs fallback in place"
            );

            drop(fallback_directory);
            root.cleanup().expect("remove temporary root");
        }
    }

    #[test]
    fn new_objects_are_secure_on_their_first_open_handle() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory = ensure_secure_storage_directory_with(&storage_path, |created| {
            verify_storage_handle(created.as_raw_handle() as HANDLE)
        })
        .expect("directory is secure at creation time");

        let create_new_path = storage_path.join("create-new.json");
        let create_new = open_secure_storage_object_with(
            &create_new_path,
            GENERIC_READ | GENERIC_WRITE | STORAGE_SECURITY_ACCESS,
            CREATE_NEW,
            StorageObjectKind::RegularFile,
            |created| verify_storage_handle(created.as_raw_handle() as HANDLE),
        )
        .expect("CREATE_NEW file is secure on its first handle");

        let open_always_path = storage_path.join("open-always.json");
        let open_always = open_secure_storage_object_with(
            &open_always_path,
            GENERIC_READ | GENERIC_WRITE | STORAGE_SECURITY_ACCESS,
            OPEN_ALWAYS,
            StorageObjectKind::RegularFile,
            |created| verify_storage_handle(created.as_raw_handle() as HANDLE),
        )
        .expect("OPEN_ALWAYS-created file is secure on its first handle");

        drop(open_always);
        drop(create_new);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_directory_and_files_round_trip_type_dacl_identity_and_bytes() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        storage_identity(
            directory.as_raw_handle() as HANDLE,
            StorageObjectKind::Directory,
        )
        .expect("directory type and identity verify");
        verify_storage_handle(directory.as_raw_handle() as HANDLE)
            .expect("directory DACL verifies");
        flush_secure_storage_directory(&directory).expect("directory flush succeeds");

        let file_path = storage_path.join("history.json");
        let mut file = create_new_secure_file(&file_path).expect("create secure file");
        file.write_all(b"secure history")
            .expect("write secure file");
        file.sync_all().expect("sync secure file");
        let original_identity = storage_identity(
            file.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )
        .expect("file type and identity verify");
        verify_storage_handle(file.as_raw_handle() as HANDLE).expect("file DACL verifies");
        verify_secure_file_path(&file, &file_path).expect("file path identity verifies");
        drop(file);

        let mut reopened =
            open_existing_secure_file(&file_path, false).expect("reopen secure file read-only");
        let reopened_identity = storage_identity(
            reopened.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )
        .expect("reopened file identity verifies");
        assert!(
            original_identity == reopened_identity,
            "reopen preserves stable file identity"
        );
        assert!(
            read_open_file(&mut reopened).expect("read reopened secure file") == b"secure history",
            "reopened bytes match"
        );
        let mut writable_reopened =
            open_existing_secure_file(&file_path, true).expect("reopen secure file read/write");
        writable_reopened
            .seek(SeekFrom::End(0))
            .expect("seek writable secure file");
        writable_reopened
            .write_all(b"!")
            .expect("write reopened secure file");
        writable_reopened
            .sync_all()
            .expect("sync reopened secure file");
        verify_secure_file_path(&writable_reopened, &file_path)
            .expect("writable file path identity verifies");
        drop(writable_reopened);
        assert!(
            read_open_file(&mut reopened).expect("reread secure file") == b"secure history!",
            "read/write reopen persists bytes"
        );

        let lock_path = storage_path.join("history.lock");
        let lock = open_or_create_secure_file(&lock_path).expect("create secure lock file");
        let lock_identity = storage_identity(
            lock.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )
        .expect("lock identity verifies");
        drop(lock);
        let reopened_lock =
            open_or_create_secure_file(&lock_path).expect("open existing secure lock file");
        assert!(
            storage_identity(
                reopened_lock.as_raw_handle() as HANDLE,
                StorageObjectKind::RegularFile,
            )
            .expect("reopened lock identity verifies")
                == lock_identity,
            "open-or-create preserves identity"
        );

        drop(reopened_lock);
        drop(reopened);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_lock_preserves_one_identity_and_blocks_delete_until_handles_drop() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let lock_path = storage_path.join("history.lock");
        let renamed_path = storage_path.join("history.lock.renamed");

        let first = open_secure_lock_file(&lock_path).expect("acquire secure lock");
        let first_identity = regular_file_identity(&first).expect("read first lock identity");
        verify_storage_handle(first.as_raw_handle() as HANDLE).expect("lock DACL is exact");
        verify_secure_file_path(&first, &lock_path).expect("first lock path identity verifies");

        let second = open_windows_path_with_share(
            &lock_path,
            GENERIC_READ | GENERIC_WRITE | STORAGE_SECURITY_ACCESS,
            OPEN_EXISTING,
            StorageObjectKind::RegularFile.open_flags(),
            VALIDATION_SHARE_MODE,
        )
        .expect("open second lock handle");
        let second_identity = regular_file_identity(&second).expect("read second lock identity");
        verify_storage_handle(second.as_raw_handle() as HANDLE)
            .expect("second handle DACL is exact");
        assert!(
            first_identity == second_identity,
            "both lock handles name the same file identity"
        );
        fs2::FileExt::try_lock_exclusive(&second)
            .expect_err("second exclusive lock attempt fails while first holds it");

        fs::rename(&lock_path, &renamed_path)
            .expect_err("live no-share-delete handles block rename");
        fs::remove_file(&lock_path).expect_err("live no-share-delete handles block delete");
        assert!(lock_path.exists(), "lock pathname remains installed");

        fs2::FileExt::unlock(&first).expect("release first exclusive lock");
        drop(second);
        drop(first);
        fs::rename(&lock_path, &renamed_path).expect("rename succeeds after lock handles drop");
        let renamed = open_existing_secure_file(&renamed_path, false)
            .expect("renamed lock remains an exact secure file");
        assert!(
            regular_file_identity(&renamed).expect("read renamed lock identity") == first_identity,
            "rename preserves the lock identity"
        );

        drop(renamed);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_atomic_replace_existing_destination_preserves_staged_identity() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let staged_path = storage_path.join("history.tmp");
        let destination_path = storage_path.join("history.json");

        let (staged, staged_identity) =
            create_secure_test_file(&staged_path, b"new history").expect("create staged file");
        let (destination, old_identity) =
            create_secure_test_file(&destination_path, b"last-good history")
                .expect("create destination file");
        assert!(
            staged_identity != old_identity,
            "staged and destination identities start distinct"
        );
        drop(destination);
        drop(staged);

        replace_secure_file(&directory, &storage_path, &staged_path, &destination_path)
            .expect("replace existing destination and flush directory");

        let (bytes, installed_identity) =
            read_secure_test_file(&destination_path).expect("open exact installed destination");
        assert!(
            bytes == b"new history",
            "installed bytes come from staged file"
        );
        assert!(
            installed_identity == staged_identity,
            "installed destination keeps staged file identity"
        );
        assert!(
            std::fs::symlink_metadata(&staged_path)
                .expect_err("staged pathname disappears")
                .kind()
                == io::ErrorKind::NotFound,
            "staged pathname is absent after replace"
        );

        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_atomic_replace_absent_destination_preserves_staged_identity() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let staged_path = storage_path.join("account.tmp");
        let destination_path = storage_path.join("account.json");
        let (staged, staged_identity) =
            create_secure_test_file(&staged_path, b"new account").expect("create staged file");
        drop(staged);
        assert!(!destination_path.exists(), "destination starts absent");

        replace_secure_file(&directory, &storage_path, &staged_path, &destination_path)
            .expect("install absent destination and flush directory");

        let (bytes, installed_identity) =
            read_secure_test_file(&destination_path).expect("open exact installed destination");
        assert!(
            bytes == b"new account",
            "installed bytes come from staged file"
        );
        assert!(
            installed_identity == staged_identity,
            "new destination keeps staged file identity"
        );
        assert!(!staged_path.exists(), "staged pathname disappears");

        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn injected_replace_failure_preserves_destination_and_staged_files() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let staged_path = storage_path.join("history.tmp");
        let destination_path = storage_path.join("history.json");
        let (staged, staged_identity) =
            create_secure_test_file(&staged_path, b"staged bytes").expect("create staged file");
        let (destination, destination_identity) =
            create_secure_test_file(&destination_path, b"last-good bytes")
                .expect("create destination file");
        drop(destination);
        drop(staged);

        let mut replace_called = false;
        replace_secure_file_with(
            &directory,
            &storage_path,
            &staged_path,
            &destination_path,
            |_, _| {
                replace_called = true;
                Err(io::Error::other("injected replace failure"))
            },
        )
        .expect_err("injected replace failure is returned");
        assert!(replace_called, "replace seam was reached");

        let (destination_bytes, destination_after) =
            read_secure_test_file(&destination_path).expect("reopen last-good destination");
        assert!(
            destination_bytes == b"last-good bytes" && destination_after == destination_identity,
            "failed replace preserves destination bytes and identity"
        );
        let (staged_bytes, staged_after) =
            read_secure_test_file(&staged_path).expect("reopen caller-owned staged file");
        assert!(
            staged_bytes == b"staged bytes" && staged_after == staged_identity,
            "failed replace leaves staged bytes and identity for caller cleanup"
        );

        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_replace_rejects_permissive_staged_and_destination_without_mutation() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");

        let staged_for_destination = storage_path.join("destination-check.tmp");
        let permissive_destination_path = storage_path.join("permissive-destination.json");
        let (staged, staged_identity) =
            create_secure_test_file(&staged_for_destination, b"new bytes")
                .expect("create secure staged file");
        let permissive_destination =
            create_permissive_test_file(&permissive_destination_path, b"permissive last-good")
                .expect("create permissive destination");
        let permissive_destination_identity =
            regular_file_identity(&permissive_destination).expect("read permissive identity");
        let permissive_destination_acl =
            read_acl_snapshot(permissive_destination.as_raw_handle() as HANDLE)
                .expect("snapshot permissive destination DACL");
        drop(staged);

        replace_secure_file(
            &directory,
            &storage_path,
            &staged_for_destination,
            &permissive_destination_path,
        )
        .expect_err("permissive destination is rejected before replace");
        assert!(
            fs::read(&permissive_destination_path).expect("read permissive destination")
                == b"permissive last-good"
                && regular_file_identity(&permissive_destination)
                    .expect("reread permissive destination identity")
                    == permissive_destination_identity
                && read_acl_snapshot(permissive_destination.as_raw_handle() as HANDLE)
                    .expect("reread permissive destination DACL")
                    == permissive_destination_acl,
            "destination rejection preserves bytes, identity, and DACL"
        );
        let (staged_bytes, staged_after) = read_secure_test_file(&staged_for_destination)
            .expect("reopen rejected secure staged file");
        assert!(
            staged_bytes == b"new bytes" && staged_after == staged_identity,
            "destination rejection preserves staged file"
        );

        let permissive_staged_path = storage_path.join("permissive-staged.tmp");
        let destination_for_staged = storage_path.join("staged-check.json");
        let permissive_staged =
            create_permissive_test_file(&permissive_staged_path, b"permissive staged")
                .expect("create permissive staged file");
        let permissive_staged_identity =
            regular_file_identity(&permissive_staged).expect("read permissive staged identity");
        let permissive_staged_acl = read_acl_snapshot(permissive_staged.as_raw_handle() as HANDLE)
            .expect("snapshot permissive staged DACL");
        let (destination, destination_identity) =
            create_secure_test_file(&destination_for_staged, b"secure last-good")
                .expect("create secure destination");
        drop(destination);

        replace_secure_file(
            &directory,
            &storage_path,
            &permissive_staged_path,
            &destination_for_staged,
        )
        .expect_err("permissive staged file is rejected before replace");
        assert!(
            fs::read(&permissive_staged_path).expect("read permissive staged file")
                == b"permissive staged"
                && regular_file_identity(&permissive_staged)
                    .expect("reread permissive staged identity")
                    == permissive_staged_identity
                && read_acl_snapshot(permissive_staged.as_raw_handle() as HANDLE)
                    .expect("reread permissive staged DACL")
                    == permissive_staged_acl,
            "staged rejection preserves permissive bytes, identity, and DACL"
        );
        let (destination_bytes, destination_after) =
            read_secure_test_file(&destination_for_staged).expect("reopen secure last-good");
        assert!(
            destination_bytes == b"secure last-good" && destination_after == destination_identity,
            "staged rejection preserves destination bytes and identity"
        );

        drop(permissive_staged);
        drop(permissive_destination);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_replace_rejects_final_reparse_points_without_touching_targets() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");

        let destination_target_path = storage_path.join("destination-target.json");
        let destination_link_path = storage_path.join("destination-link.json");
        let staged_for_link = storage_path.join("destination-link.tmp");
        let (target, target_identity) =
            create_secure_test_file(&destination_target_path, b"destination target")
                .expect("create destination link target");
        let (staged, staged_identity) =
            create_secure_test_file(&staged_for_link, b"staged for destination link")
                .expect("create staged file");
        drop(target);
        drop(staged);
        symlink_file(&destination_target_path, &destination_link_path)
            .expect("create final destination symlink");

        replace_secure_file(
            &directory,
            &storage_path,
            &staged_for_link,
            &destination_link_path,
        )
        .expect_err("final destination reparse point is rejected");
        assert!(
            fs::symlink_metadata(&destination_link_path)
                .expect("destination link remains")
                .file_type()
                .is_symlink(),
            "destination reparse point is not replaced"
        );
        let (target_bytes, target_after) = read_secure_test_file(&destination_target_path)
            .expect("reopen destination link target");
        assert!(
            target_bytes == b"destination target" && target_after == target_identity,
            "destination link target remains unchanged"
        );
        let (staged_bytes, staged_after) =
            read_secure_test_file(&staged_for_link).expect("reopen staged file");
        assert!(
            staged_bytes == b"staged for destination link" && staged_after == staged_identity,
            "rejected destination link leaves staged file unchanged"
        );

        let staged_target_path = storage_path.join("staged-target.json");
        let staged_link_path = storage_path.join("staged-link.tmp");
        let destination_for_link = storage_path.join("staged-link-destination.json");
        let (staged_target, staged_target_identity) =
            create_secure_test_file(&staged_target_path, b"staged target")
                .expect("create staged link target");
        let (destination, destination_identity) =
            create_secure_test_file(&destination_for_link, b"last-good for staged link")
                .expect("create destination for staged link");
        drop(staged_target);
        drop(destination);
        symlink_file(&staged_target_path, &staged_link_path).expect("create final staged symlink");

        replace_secure_file(
            &directory,
            &storage_path,
            &staged_link_path,
            &destination_for_link,
        )
        .expect_err("final staged reparse point is rejected");
        assert!(
            fs::symlink_metadata(&staged_link_path)
                .expect("staged link remains")
                .file_type()
                .is_symlink(),
            "staged reparse point remains untouched"
        );
        let (staged_target_bytes, staged_target_after) =
            read_secure_test_file(&staged_target_path).expect("reopen staged link target");
        assert!(
            staged_target_bytes == b"staged target"
                && staged_target_after == staged_target_identity,
            "staged link target remains unchanged"
        );
        let (destination_bytes, destination_after) =
            read_secure_test_file(&destination_for_link).expect("reopen last-good destination");
        assert!(
            destination_bytes == b"last-good for staged link"
                && destination_after == destination_identity,
            "rejected staged link preserves last-good destination"
        );

        fs::remove_file(&destination_link_path).expect("remove destination symlink");
        fs::remove_file(&staged_link_path).expect("remove staged symlink");
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_replace_rejects_wrong_types_without_touching_last_good() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");

        let staged_directory_path = storage_path.join("staged-directory");
        let staged_directory = ensure_secure_storage_directory(&staged_directory_path)
            .expect("create wrong-type staged directory");
        let staged_marker = staged_directory_path.join("marker.bin");
        fs::write(&staged_marker, b"staged directory marker").expect("write staged marker");
        let destination_path = storage_path.join("wrong-staged.json");
        let (destination, destination_identity) =
            create_secure_test_file(&destination_path, b"last-good wrong staged")
                .expect("create last-good destination");
        drop(destination);

        replace_secure_file(
            &directory,
            &storage_path,
            &staged_directory_path,
            &destination_path,
        )
        .expect_err("directory staged object is rejected");
        assert!(
            fs::read(&staged_marker).expect("read staged directory marker")
                == b"staged directory marker",
            "wrong-type staged directory remains unchanged"
        );
        let (destination_bytes, destination_after) =
            read_secure_test_file(&destination_path).expect("reopen last-good destination");
        assert!(
            destination_bytes == b"last-good wrong staged"
                && destination_after == destination_identity,
            "wrong-type staged rejection preserves last-good destination"
        );

        let destination_directory_path = storage_path.join("destination-directory");
        let destination_directory = ensure_secure_storage_directory(&destination_directory_path)
            .expect("create wrong-type destination directory");
        let destination_marker = destination_directory_path.join("marker.bin");
        fs::write(&destination_marker, b"destination directory marker")
            .expect("write destination marker");
        let staged_path = storage_path.join("wrong-destination.tmp");
        let (staged, staged_identity) =
            create_secure_test_file(&staged_path, b"staged wrong destination")
                .expect("create staged file");
        drop(staged);

        replace_secure_file(
            &directory,
            &storage_path,
            &staged_path,
            &destination_directory_path,
        )
        .expect_err("directory destination object is rejected");
        assert!(
            fs::read(&destination_marker).expect("read destination directory marker")
                == b"destination directory marker",
            "wrong-type destination directory remains unchanged"
        );
        let (staged_bytes, staged_after) =
            read_secure_test_file(&staged_path).expect("reopen rejected staged file");
        assert!(
            staged_bytes == b"staged wrong destination" && staged_after == staged_identity,
            "wrong-type destination rejection preserves staged file"
        );

        drop(destination_directory);
        drop(staged_directory);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_replace_rejects_invalid_cross_directory_and_same_identity_paths() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let other_storage_path = root.join("other-storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let other_directory = ensure_secure_storage_directory(&other_storage_path)
            .expect("create second secure directory");

        let cross_staged_path = other_storage_path.join("cross-staged.tmp");
        let destination_path = storage_path.join("cross-staged.json");
        let (cross_staged, cross_staged_identity) =
            create_secure_test_file(&cross_staged_path, b"cross staged")
                .expect("create cross-directory staged file");
        let (destination, destination_identity) =
            create_secure_test_file(&destination_path, b"cross staged last-good")
                .expect("create destination");
        drop(cross_staged);
        drop(destination);
        replace_secure_file(
            &directory,
            &storage_path,
            &cross_staged_path,
            &destination_path,
        )
        .expect_err("cross-directory staged path is rejected");
        assert!(
            read_secure_test_file(&cross_staged_path).expect("reopen cross-directory staged file")
                == (b"cross staged".to_vec(), cross_staged_identity),
            "cross-directory staged file remains unchanged"
        );
        assert!(
            read_secure_test_file(&destination_path).expect("reopen cross-staged destination")
                == (b"cross staged last-good".to_vec(), destination_identity),
            "cross-directory staged rejection preserves destination"
        );

        let staged_path = storage_path.join("cross-destination.tmp");
        let cross_destination_path = other_storage_path.join("cross-destination.json");
        let (staged, staged_identity) = create_secure_test_file(&staged_path, b"local staged")
            .expect("create local staged file");
        let (cross_destination, cross_destination_identity) =
            create_secure_test_file(&cross_destination_path, b"remote last-good")
                .expect("create cross-directory destination");
        drop(staged);
        drop(cross_destination);
        replace_secure_file(
            &directory,
            &storage_path,
            &staged_path,
            &cross_destination_path,
        )
        .expect_err("cross-directory destination path is rejected");
        assert!(
            read_secure_test_file(&staged_path).expect("reopen local staged file")
                == (b"local staged".to_vec(), staged_identity),
            "cross-directory destination rejection preserves staged file"
        );
        assert!(
            read_secure_test_file(&cross_destination_path)
                .expect("reopen cross-directory destination")
                == (b"remote last-good".to_vec(), cross_destination_identity),
            "cross-directory destination remains unchanged"
        );

        assert!(
            validate_replace_paths(&storage_path, Path::new(""), &destination_path).is_err(),
            "missing staged filename is rejected"
        );
        replace_secure_file(&directory, &storage_path, &staged_path, &staged_path)
            .expect_err("identical staged and destination paths are rejected");
        assert!(
            read_secure_test_file(&staged_path).expect("reopen identical-path file")
                == (b"local staged".to_vec(), staged_identity),
            "identical path rejection leaves file unchanged"
        );

        let hard_link_path = storage_path.join("same-identity.json");
        fs::hard_link(&staged_path, &hard_link_path).expect("create same-identity hard link");
        replace_secure_file(&directory, &storage_path, &staged_path, &hard_link_path)
            .expect_err("distinct paths with the same file identity are rejected");
        assert!(
            read_secure_test_file(&staged_path)
                .expect("reopen staged hard-link identity")
                .1
                == staged_identity
                && read_secure_test_file(&hard_link_path)
                    .expect("reopen destination hard-link identity")
                    .1
                    == staged_identity,
            "same-identity rejection preserves both hard-link pathnames"
        );

        drop(other_directory);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn injected_post_replace_identity_mismatch_is_detected() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let staged_path = storage_path.join("history.tmp");
        let destination_path = storage_path.join("history.json");
        let alternate_path = storage_path.join("alternate.tmp");
        let displaced_path = storage_path.join("displaced-staged.json");
        let (staged, staged_identity) =
            create_secure_test_file(&staged_path, b"expected staged bytes")
                .expect("create expected staged file");
        let (destination, _) = create_secure_test_file(&destination_path, b"old destination")
            .expect("create old destination");
        let (alternate, alternate_identity) =
            create_secure_test_file(&alternate_path, b"unexpected secure bytes")
                .expect("create alternate secure file");
        drop(alternate);
        drop(destination);
        drop(staged);

        replace_secure_file_with(
            &directory,
            &storage_path,
            &staged_path,
            &destination_path,
            |staged, destination| {
                tokscale_core::fs_atomic::replace_file(staged, &displaced_path)?;
                tokscale_core::fs_atomic::replace_file(&alternate_path, destination)
            },
        )
        .expect_err("post-replace identity mismatch is detected");

        let (installed_bytes, installed_identity) =
            read_secure_test_file(&destination_path).expect("reopen injected destination");
        assert!(
            installed_bytes == b"unexpected secure bytes"
                && installed_identity == alternate_identity
                && installed_identity != staged_identity,
            "injected alternate file is present and visibly has the wrong identity"
        );
        let (displaced_bytes, displaced_identity) =
            read_secure_test_file(&displaced_path).expect("reopen displaced expected staged file");
        assert!(
            displaced_bytes == b"expected staged bytes" && displaced_identity == staged_identity,
            "expected staged identity is distinct and retained by the injected seam"
        );
        assert!(!staged_path.exists(), "original staged pathname is absent");

        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_quarantine_candidate_preserves_bytes_dacl_identity_and_flushes_directory() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let source_path = storage_path.join("history.json");
        let candidate_path = storage_path.join("history.corrupt-1.json");
        let (source, source_identity) =
            create_secure_test_file(&source_path, b"corrupt history bytes")
                .expect("create quarantine source");
        let source_acl = read_acl_snapshot(source.as_raw_handle() as HANDLE)
            .expect("snapshot quarantine source DACL");
        drop(source);

        quarantine_secure_file_candidate(&directory, &storage_path, &source_path, &candidate_path)
            .expect("quarantine one candidate and flush directory");

        assert!(
            fs::symlink_metadata(&source_path)
                .expect_err("source pathname disappears")
                .kind()
                == io::ErrorKind::NotFound,
            "source pathname is absent after quarantine"
        );
        let mut candidate =
            open_existing_secure_file(&candidate_path, false).expect("open quarantine candidate");
        assert!(
            regular_file_identity(&candidate).expect("read quarantine identity") == source_identity,
            "candidate keeps the source FILE_ID_INFO identity"
        );
        assert!(
            read_open_file(&mut candidate).expect("read quarantine bytes")
                == b"corrupt history bytes",
            "candidate keeps source bytes"
        );
        assert!(
            read_acl_snapshot(candidate.as_raw_handle() as HANDLE)
                .expect("read quarantine candidate DACL")
                == source_acl,
            "candidate keeps the source DACL"
        );

        drop(candidate);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_quarantine_existing_candidate_collision_preserves_both_files() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let source_path = storage_path.join("account.json");
        let candidate_path = storage_path.join("account.corrupt-1.json");
        let (source, source_identity) = create_secure_test_file(&source_path, b"source evidence")
            .expect("create quarantine source");
        let source_acl =
            read_acl_snapshot(source.as_raw_handle() as HANDLE).expect("snapshot source DACL");
        let (candidate, candidate_identity) =
            create_secure_test_file(&candidate_path, b"existing evidence")
                .expect("create colliding candidate");
        let candidate_acl = read_acl_snapshot(candidate.as_raw_handle() as HANDLE)
            .expect("snapshot candidate DACL");
        assert!(
            source_identity != candidate_identity,
            "source and existing candidate identities start distinct"
        );
        drop(candidate);
        drop(source);

        let error = quarantine_secure_file_candidate(
            &directory,
            &storage_path,
            &source_path,
            &candidate_path,
        )
        .expect_err("existing candidate collision is returned");
        assert!(
            error.kind() == io::ErrorKind::AlreadyExists,
            "native hard-link collision keeps create-new semantics"
        );

        assert!(
            read_secure_test_file(&source_path).expect("reopen source after collision")
                == (b"source evidence".to_vec(), source_identity),
            "collision preserves source bytes and identity"
        );
        let source_after =
            open_existing_secure_file(&source_path, false).expect("open source after collision");
        assert!(
            read_acl_snapshot(source_after.as_raw_handle() as HANDLE)
                .expect("read source DACL after collision")
                == source_acl,
            "collision preserves source DACL"
        );
        assert!(
            read_secure_test_file(&candidate_path).expect("reopen candidate after collision")
                == (b"existing evidence".to_vec(), candidate_identity),
            "collision preserves candidate bytes and identity"
        );
        let candidate_after = open_existing_secure_file(&candidate_path, false)
            .expect("open candidate after collision");
        assert!(
            read_acl_snapshot(candidate_after.as_raw_handle() as HANDLE)
                .expect("read candidate DACL after collision")
                == candidate_acl,
            "collision preserves candidate DACL"
        );

        drop(candidate_after);
        drop(source_after);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_quarantine_reparse_and_directory_collisions_touch_no_targets() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");

        let symlink_source_path = storage_path.join("symlink-source.json");
        let symlink_candidate_path = storage_path.join("symlink-candidate.json");
        let symlink_target_path = storage_path.join("symlink-target.json");
        let (symlink_source, symlink_source_identity) =
            create_secure_test_file(&symlink_source_path, b"symlink source evidence")
                .expect("create source for symlink collision");
        let symlink_source_acl = read_acl_snapshot(symlink_source.as_raw_handle() as HANDLE)
            .expect("snapshot symlink-collision source DACL");
        let (symlink_target, symlink_target_identity) =
            create_secure_test_file(&symlink_target_path, b"symlink target bytes")
                .expect("create symlink collision target");
        let symlink_target_acl = read_acl_snapshot(symlink_target.as_raw_handle() as HANDLE)
            .expect("snapshot symlink target DACL");
        drop(symlink_target);
        drop(symlink_source);
        symlink_file(&symlink_target_path, &symlink_candidate_path)
            .expect("create colliding final symlink");

        quarantine_secure_file_candidate(
            &directory,
            &storage_path,
            &symlink_source_path,
            &symlink_candidate_path,
        )
        .expect_err("existing symlink candidate is never overwritten");
        assert!(
            fs::symlink_metadata(&symlink_candidate_path)
                .expect("symlink candidate remains")
                .file_type()
                .is_symlink(),
            "candidate symlink remains installed"
        );
        assert!(
            read_secure_test_file(&symlink_source_path)
                .expect("reopen source after symlink collision")
                == (b"symlink source evidence".to_vec(), symlink_source_identity),
            "symlink collision preserves source bytes and identity"
        );
        let symlink_source_after = open_existing_secure_file(&symlink_source_path, false)
            .expect("open source after symlink collision");
        assert!(
            read_acl_snapshot(symlink_source_after.as_raw_handle() as HANDLE)
                .expect("read source DACL after symlink collision")
                == symlink_source_acl,
            "symlink collision preserves source DACL"
        );
        assert!(
            read_secure_test_file(&symlink_target_path).expect("reopen symlink collision target")
                == (b"symlink target bytes".to_vec(), symlink_target_identity),
            "symlink collision does not touch target bytes or identity"
        );
        let symlink_target_after = open_existing_secure_file(&symlink_target_path, false)
            .expect("open symlink target after collision");
        assert!(
            read_acl_snapshot(symlink_target_after.as_raw_handle() as HANDLE)
                .expect("read symlink target DACL after collision")
                == symlink_target_acl,
            "symlink collision does not touch target DACL"
        );

        let directory_source_path = storage_path.join("directory-source.json");
        let directory_candidate_path = storage_path.join("directory-candidate");
        let directory_marker_path = directory_candidate_path.join("marker.bin");
        let (directory_source, directory_source_identity) =
            create_secure_test_file(&directory_source_path, b"directory source evidence")
                .expect("create source for directory collision");
        let candidate_directory = ensure_secure_storage_directory(&directory_candidate_path)
            .expect("create colliding candidate directory");
        let candidate_directory_identity = storage_identity(
            candidate_directory.as_raw_handle() as HANDLE,
            StorageObjectKind::Directory,
        )
        .expect("read candidate directory identity");
        let candidate_directory_acl =
            read_acl_snapshot(candidate_directory.as_raw_handle() as HANDLE)
                .expect("snapshot candidate directory DACL");
        fs::write(&directory_marker_path, b"directory target marker")
            .expect("write candidate directory marker");
        drop(directory_source);

        quarantine_secure_file_candidate(
            &directory,
            &storage_path,
            &directory_source_path,
            &directory_candidate_path,
        )
        .expect_err("existing directory candidate is never overwritten");
        assert!(
            read_secure_test_file(&directory_source_path)
                .expect("reopen source after directory collision")
                == (
                    b"directory source evidence".to_vec(),
                    directory_source_identity
                ),
            "directory collision preserves source bytes and identity"
        );
        verify_path_identity(
            &directory_candidate_path,
            StorageObjectKind::Directory,
            candidate_directory_identity,
        )
        .expect("candidate directory path and identity remain installed");
        assert!(
            read_acl_snapshot(candidate_directory.as_raw_handle() as HANDLE)
                .expect("read candidate directory DACL after collision")
                == candidate_directory_acl,
            "directory collision preserves candidate DACL"
        );
        assert!(
            fs::read(&directory_marker_path).expect("read candidate directory marker")
                == b"directory target marker",
            "directory collision preserves target contents"
        );

        fs::remove_file(&symlink_candidate_path).expect("remove candidate symlink");
        drop(candidate_directory);
        drop(symlink_target_after);
        drop(symlink_source_after);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn injected_quarantine_unlink_failure_rolls_back_link_and_preserves_source() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let source_path = storage_path.join("history.json");
        let candidate_path = storage_path.join("history.corrupt-2.json");
        let (source, source_identity) =
            create_secure_test_file(&source_path, b"retained source bytes")
                .expect("create quarantine source");
        let source_acl =
            read_acl_snapshot(source.as_raw_handle() as HANDLE).expect("snapshot source DACL");
        drop(source);
        let mut flush_called = false;

        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &source_path,
            &candidate_path,
            |source, candidate| fs::hard_link(source, candidate),
            |path| {
                if path == source_path.as_path() {
                    Err(io::Error::other("injected source unlink failure"))
                } else {
                    fs::remove_file(path)
                }
            },
            |_| {
                flush_called = true;
                Ok(())
            },
        )
        .expect_err("injected source unlink failure is returned");
        assert!(!flush_called, "failed transaction never flushes as success");
        assert!(
            fs::symlink_metadata(&candidate_path)
                .expect_err("rollback removes candidate link")
                .kind()
                == io::ErrorKind::NotFound,
            "rollback removes only the new candidate link"
        );
        assert!(
            read_secure_test_file(&source_path).expect("reopen source after rollback")
                == (b"retained source bytes".to_vec(), source_identity),
            "rollback preserves source bytes and identity"
        );
        let source_after =
            open_existing_secure_file(&source_path, false).expect("open source after rollback");
        assert!(
            read_acl_snapshot(source_after.as_raw_handle() as HANDLE)
                .expect("read source DACL after rollback")
                == source_acl,
            "rollback preserves source DACL"
        );

        drop(source_after);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn injected_quarantine_candidate_identity_mismatch_preserves_unrelated_candidate() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let source_path = storage_path.join("account.json");
        let candidate_path = storage_path.join("account.corrupt-2.json");
        let (source, source_identity) =
            create_secure_test_file(&source_path, b"authenticated source evidence")
                .expect("create quarantine source");
        let source_acl =
            read_acl_snapshot(source.as_raw_handle() as HANDLE).expect("snapshot source DACL");
        drop(source);
        let mut replacement_identity = None;

        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &source_path,
            &candidate_path,
            |source, candidate| fs::hard_link(source, candidate),
            |path| {
                if path == source_path.as_path() {
                    fs::remove_file(&candidate_path)?;
                    let (replacement, identity) =
                        create_secure_test_file(&candidate_path, b"unrelated candidate bytes")?;
                    replacement_identity = Some(identity);
                    drop(replacement);
                    Err(io::Error::other("injected source unlink failure"))
                } else {
                    panic!("identity-mismatched rollback candidate must not be unlinked")
                }
            },
            |_| panic!("identity-mismatched transaction must not flush"),
        )
        .expect_err("rollback detects candidate identity mismatch");

        assert!(
            read_secure_test_file(&source_path).expect("reopen source after mismatch")
                == (b"authenticated source evidence".to_vec(), source_identity),
            "identity mismatch leaves source bytes and identity intact"
        );
        let source_after =
            open_existing_secure_file(&source_path, false).expect("open source after mismatch");
        assert!(
            read_acl_snapshot(source_after.as_raw_handle() as HANDLE)
                .expect("read source DACL after mismatch")
                == source_acl,
            "identity mismatch leaves source DACL intact"
        );
        assert!(
            read_secure_test_file(&candidate_path).expect("unrelated candidate remains installed")
                == (
                    b"unrelated candidate bytes".to_vec(),
                    replacement_identity.expect("replacement identity was recorded")
                ),
            "rollback never deletes an unrelated replacement candidate"
        );

        drop(source_after);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_quarantine_rejects_unsafe_source_objects_before_link() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");

        let permissive_source_path = storage_path.join("permissive.json");
        let permissive_candidate_path = storage_path.join("permissive.corrupt.json");
        let permissive =
            create_permissive_test_file(&permissive_source_path, b"permissive evidence")
                .expect("create permissive source");
        let permissive_identity =
            regular_file_identity(&permissive).expect("read permissive source identity");
        let permissive_acl = read_acl_snapshot(permissive.as_raw_handle() as HANDLE)
            .expect("snapshot permissive source DACL");
        let permissive_path_text = permissive_source_path.to_string_lossy().into_owned();
        let error = quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &permissive_source_path,
            &permissive_candidate_path,
            |_, _| panic!("permissive source must be rejected before link"),
            |_| panic!("permissive source must be rejected before unlink"),
            |_| panic!("permissive source must be rejected before flush"),
        )
        .expect_err("permissive source is rejected");
        assert_generic_error(&error, &[&permissive_path_text]);
        assert!(
            fs::read(&permissive_source_path).expect("read rejected permissive source")
                == b"permissive evidence"
                && regular_file_identity(&permissive).expect("reread permissive source identity")
                    == permissive_identity
                && read_acl_snapshot(permissive.as_raw_handle() as HANDLE)
                    .expect("reread permissive source DACL")
                    == permissive_acl,
            "permissive rejection preserves source bytes, identity, and DACL"
        );
        assert!(
            !permissive_candidate_path.exists(),
            "permissive rejection creates no candidate"
        );

        let symlink_target_path = storage_path.join("source-target.json");
        let symlink_source_path = storage_path.join("source-link.json");
        let symlink_candidate_path = storage_path.join("source-link.corrupt.json");
        let (symlink_target, symlink_target_identity) =
            create_secure_test_file(&symlink_target_path, b"source symlink target")
                .expect("create source symlink target");
        let symlink_target_acl = read_acl_snapshot(symlink_target.as_raw_handle() as HANDLE)
            .expect("snapshot source symlink target DACL");
        drop(symlink_target);
        symlink_file(&symlink_target_path, &symlink_source_path)
            .expect("create final source symlink");
        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &symlink_source_path,
            &symlink_candidate_path,
            |_, _| panic!("source symlink must be rejected before link"),
            |_| panic!("source symlink must be rejected before unlink"),
            |_| panic!("source symlink must be rejected before flush"),
        )
        .expect_err("final source symlink is rejected");
        assert!(
            fs::symlink_metadata(&symlink_source_path)
                .expect("source symlink remains")
                .file_type()
                .is_symlink(),
            "source symlink remains untouched"
        );
        assert!(
            read_secure_test_file(&symlink_target_path).expect("reopen source symlink target")
                == (b"source symlink target".to_vec(), symlink_target_identity),
            "source symlink rejection preserves target bytes and identity"
        );
        let symlink_target_after = open_existing_secure_file(&symlink_target_path, false)
            .expect("open source symlink target after rejection");
        assert!(
            read_acl_snapshot(symlink_target_after.as_raw_handle() as HANDLE)
                .expect("read source symlink target DACL")
                == symlink_target_acl,
            "source symlink rejection preserves target DACL"
        );
        assert!(
            !symlink_candidate_path.exists(),
            "source symlink rejection creates no candidate"
        );

        let directory_source_path = storage_path.join("source-directory");
        let directory_candidate_path = storage_path.join("source-directory.corrupt");
        let directory_marker_path = directory_source_path.join("marker.bin");
        let source_directory = ensure_secure_storage_directory(&directory_source_path)
            .expect("create wrong-type source directory");
        let source_directory_identity = storage_identity(
            source_directory.as_raw_handle() as HANDLE,
            StorageObjectKind::Directory,
        )
        .expect("read source directory identity");
        let source_directory_acl = read_acl_snapshot(source_directory.as_raw_handle() as HANDLE)
            .expect("snapshot source directory DACL");
        fs::write(&directory_marker_path, b"source directory marker")
            .expect("write source directory marker");
        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &directory_source_path,
            &directory_candidate_path,
            |_, _| panic!("directory source must be rejected before link"),
            |_| panic!("directory source must be rejected before unlink"),
            |_| panic!("directory source must be rejected before flush"),
        )
        .expect_err("directory source is rejected");
        verify_path_identity(
            &directory_source_path,
            StorageObjectKind::Directory,
            source_directory_identity,
        )
        .expect("source directory path and identity remain installed");
        assert!(
            read_acl_snapshot(source_directory.as_raw_handle() as HANDLE)
                .expect("read source directory DACL after rejection")
                == source_directory_acl,
            "directory source rejection preserves DACL"
        );
        assert!(
            fs::read(&directory_marker_path).expect("read source directory marker")
                == b"source directory marker",
            "directory source rejection preserves contents"
        );
        assert!(
            !directory_candidate_path.exists(),
            "directory source rejection creates no candidate"
        );

        fs::remove_file(&symlink_source_path).expect("remove source symlink");
        drop(source_directory);
        drop(symlink_target_after);
        drop(permissive);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn secure_quarantine_rejects_invalid_paths_and_directory_identity_before_link() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let other_storage_path = root.join("other-storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let other_directory = ensure_secure_storage_directory(&other_storage_path)
            .expect("create second secure directory");
        let local_source_path = storage_path.join("local.json");
        let cross_source_path = other_storage_path.join("cross.json");
        let (local_source, local_identity) =
            create_secure_test_file(&local_source_path, b"local evidence")
                .expect("create local source");
        let (cross_source, cross_identity) =
            create_secure_test_file(&cross_source_path, b"cross evidence")
                .expect("create cross-directory source");
        drop(cross_source);
        drop(local_source);

        let local_candidate_path = storage_path.join("cross.corrupt.json");
        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &cross_source_path,
            &local_candidate_path,
            |_, _| panic!("cross-directory source must be rejected before link"),
            |_| panic!("cross-directory source must be rejected before unlink"),
            |_| panic!("cross-directory source must be rejected before flush"),
        )
        .expect_err("cross-directory source path is rejected");
        assert!(
            read_secure_test_file(&cross_source_path).expect("reopen cross-directory source")
                == (b"cross evidence".to_vec(), cross_identity),
            "cross-directory source remains unchanged"
        );
        assert!(
            !local_candidate_path.exists(),
            "cross-directory source creates no candidate"
        );

        let cross_candidate_path = other_storage_path.join("local.corrupt.json");
        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &local_source_path,
            &cross_candidate_path,
            |_, _| panic!("cross-directory candidate must be rejected before link"),
            |_| panic!("cross-directory candidate must be rejected before unlink"),
            |_| panic!("cross-directory candidate must be rejected before flush"),
        )
        .expect_err("cross-directory candidate path is rejected");
        assert!(
            read_secure_test_file(&local_source_path).expect("reopen local source")
                == (b"local evidence".to_vec(), local_identity),
            "cross-directory candidate rejection preserves source"
        );
        assert!(
            !cross_candidate_path.exists(),
            "cross-directory candidate is not created"
        );

        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &local_source_path,
            &local_source_path,
            |_, _| panic!("same path must be rejected before link"),
            |_| panic!("same path must be rejected before unlink"),
            |_| panic!("same path must be rejected before flush"),
        )
        .expect_err("same source and candidate path is rejected");
        assert!(
            read_secure_test_file(&local_source_path).expect("reopen same-path source")
                == (b"local evidence".to_vec(), local_identity),
            "same-path rejection preserves source"
        );

        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            Path::new(""),
            &local_candidate_path,
            |_, _| panic!("missing filename must be rejected before link"),
            |_| panic!("missing filename must be rejected before unlink"),
            |_| panic!("missing filename must be rejected before flush"),
        )
        .expect_err("missing source filename is rejected");
        assert!(
            !local_candidate_path.exists(),
            "missing filename creates no candidate"
        );

        let mismatched_candidate_path = other_storage_path.join("mismatch.corrupt.json");
        quarantine_secure_file_candidate_with(
            &directory,
            &other_storage_path,
            &cross_source_path,
            &mismatched_candidate_path,
            |_, _| panic!("directory identity mismatch must be rejected before link"),
            |_| panic!("directory identity mismatch must be rejected before unlink"),
            |_| panic!("directory identity mismatch must be rejected before flush"),
        )
        .expect_err("directory handle and pathname identity mismatch is rejected");
        assert!(
            read_secure_test_file(&cross_source_path)
                .expect("reopen source after directory mismatch")
                == (b"cross evidence".to_vec(), cross_identity),
            "directory mismatch preserves source"
        );
        assert!(
            !mismatched_candidate_path.exists(),
            "directory mismatch creates no candidate"
        );

        drop(other_directory);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn post_unlink_flush_failure_returns_error_without_false_rollback() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let source_path = storage_path.join("history.json");
        let candidate_path = storage_path.join("history.corrupt-3.json");
        let (source, source_identity) =
            create_secure_test_file(&source_path, b"committed quarantine bytes")
                .expect("create quarantine source");
        let source_acl =
            read_acl_snapshot(source.as_raw_handle() as HANDLE).expect("snapshot source DACL");
        drop(source);

        quarantine_secure_file_candidate_with(
            &directory,
            &storage_path,
            &source_path,
            &candidate_path,
            |source, candidate| fs::hard_link(source, candidate),
            |path| fs::remove_file(path),
            |_| Err(io::Error::other("injected directory flush failure")),
        )
        .expect_err("post-unlink flush failure is returned honestly");

        assert!(
            fs::symlink_metadata(&source_path)
                .expect_err("committed source pathname remains absent")
                .kind()
                == io::ErrorKind::NotFound,
            "post-commit failure does not recreate the source pathname"
        );
        let mut candidate = open_existing_secure_file(&candidate_path, false)
            .expect("open sole retained quarantine candidate");
        assert!(
            regular_file_identity(&candidate).expect("read retained candidate identity")
                == source_identity,
            "post-commit failure retains the original identity at the candidate"
        );
        assert!(
            read_open_file(&mut candidate).expect("read retained candidate bytes")
                == b"committed quarantine bytes",
            "post-commit failure retains source bytes at the candidate"
        );
        assert!(
            read_acl_snapshot(candidate.as_raw_handle() as HANDLE)
                .expect("read retained candidate DACL")
                == source_acl,
            "post-commit failure retains source DACL at the candidate"
        );

        drop(candidate);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn permissive_existing_file_is_rejected_without_mutation() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let file_path = storage_path.join("legacy.json");
        let mut retained = create_permissive_test_file(&file_path, b"legacy bytes")
            .expect("create permissive file with a retained writer");
        let before = read_acl_snapshot(retained.as_raw_handle() as HANDLE)
            .expect("snapshot permissive file DACL");

        open_existing_secure_file(&file_path, false)
            .expect_err("a permissive existing file is never repaired in place");
        assert!(
            read_acl_snapshot(retained.as_raw_handle() as HANDLE).expect("read rejected file DACL")
                == before,
            "rejection leaves the permissive DACL unchanged"
        );

        retained
            .seek(SeekFrom::End(0))
            .expect("seek retained writer");
        retained
            .write_all(b"!")
            .expect("retained access survives any later DACL change");
        retained.sync_all().expect("sync retained writer");
        assert!(
            fs::read(&file_path).expect("read retained-writer bytes") == b"legacy bytes!",
            "test demonstrates why in-place DACL repair cannot establish a boundary"
        );

        drop(retained);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn permissive_existing_directory_is_rejected_without_mutation() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("legacy-storage");
        let retained =
            create_permissive_test_directory(&storage_path).expect("create permissive directory");
        let before = read_acl_snapshot(retained.as_raw_handle() as HANDLE)
            .expect("snapshot permissive directory DACL");

        ensure_secure_storage_directory(&storage_path)
            .expect_err("a permissive existing directory is never repaired in place");
        assert!(
            read_acl_snapshot(retained.as_raw_handle() as HANDLE)
                .expect("read rejected directory DACL")
                == before,
            "rejection leaves the permissive directory DACL unchanged"
        );

        drop(retained);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn final_file_symlink_and_directory_junction_fail_without_touching_targets() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");

        let file_target_path = storage_path.join("file-target.json");
        let file_target = create_permissive_test_file(&file_target_path, b"target file bytes")
            .expect("create file target");
        let file_target_acl = read_acl_snapshot(file_target.as_raw_handle() as HANDLE)
            .expect("snapshot file target DACL");
        let file_link = storage_path.join("file-link.json");
        symlink_file(&file_target_path, &file_link)
            .expect("create unprivileged final file symlink");

        assert!(
            open_existing_secure_file(&file_link, false).is_err(),
            "final file symlink is rejected"
        );
        assert!(
            fs::read(&file_target_path).expect("read file target") == b"target file bytes",
            "file target bytes are unchanged"
        );
        assert!(
            read_acl_snapshot(file_target.as_raw_handle() as HANDLE)
                .expect("read file target DACL after rejection")
                == file_target_acl,
            "file target DACL is unchanged"
        );
        fs::remove_file(&file_link).expect("remove file symlink");

        let directory_target_path = storage_path.join("directory-target");
        let directory_target = create_permissive_test_directory(&directory_target_path)
            .expect("create directory target");
        let marker_path = directory_target_path.join("marker.bin");
        fs::write(&marker_path, b"target directory bytes").expect("write target marker");
        let directory_target_acl = read_acl_snapshot(directory_target.as_raw_handle() as HANDLE)
            .expect("snapshot directory target DACL");
        let junction_path = storage_path.join("directory-junction");
        create_junction(&junction_path, &directory_target_path)
            .expect("create unprivileged directory junction");

        assert!(
            ensure_secure_storage_directory(&junction_path).is_err(),
            "final directory junction is rejected"
        );
        assert!(
            fs::read(&marker_path).expect("read target marker") == b"target directory bytes",
            "directory target bytes are unchanged"
        );
        assert!(
            read_acl_snapshot(directory_target.as_raw_handle() as HANDLE)
                .expect("read directory target DACL after rejection")
                == directory_target_acl,
            "directory target DACL is unchanged"
        );
        fs::remove_dir(&junction_path).expect("remove directory junction");

        drop(directory_target);
        drop(file_target);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn ancestor_junction_resolves_to_real_final_objects_with_matching_identity() {
        let root = TempRoot::create().expect("create temporary root");
        let real_parent = root.join("real-parent");
        fs::create_dir(&real_parent).expect("create real parent");
        let junction_parent = root.join("junction-parent");
        create_junction(&junction_parent, &real_parent).expect("create ancestor junction");

        let storage_through_junction = junction_parent.join("storage");
        let storage_physical = real_parent.join("storage");
        let directory = ensure_secure_storage_directory(&storage_through_junction)
            .expect("create real final directory through ancestor junction");
        let physical_directory = open_windows_path(
            &storage_physical,
            FILE_READ_ATTRIBUTES | READ_CONTROL,
            OPEN_EXISTING,
            StorageObjectKind::Directory.open_flags(),
        )
        .expect("open physical directory target");
        assert!(
            storage_identity(
                directory.as_raw_handle() as HANDLE,
                StorageObjectKind::Directory,
            )
            .expect("read junction-path directory identity")
                == storage_identity(
                    physical_directory.as_raw_handle() as HANDLE,
                    StorageObjectKind::Directory,
                )
                .expect("read physical directory identity"),
            "junction and physical directory paths name the same final object"
        );

        let file_through_junction = storage_through_junction.join("history.json");
        let file_physical = storage_physical.join("history.json");
        let mut file = create_new_secure_file(&file_through_junction)
            .expect("create real final file through ancestor junction");
        file.write_all(b"junction bytes").expect("write file");
        file.sync_all().expect("sync file");
        let mut physical_file = open_windows_path(
            &file_physical,
            GENERIC_READ | FILE_READ_ATTRIBUTES | READ_CONTROL,
            OPEN_EXISTING,
            StorageObjectKind::RegularFile.open_flags(),
        )
        .expect("open physical file target");
        assert!(
            storage_identity(
                file.as_raw_handle() as HANDLE,
                StorageObjectKind::RegularFile
            )
            .expect("read junction-path file identity")
                == storage_identity(
                    physical_file.as_raw_handle() as HANDLE,
                    StorageObjectKind::RegularFile,
                )
                .expect("read physical file identity"),
            "junction and physical file paths name the same final object"
        );
        assert!(
            read_open_file(&mut physical_file).expect("read physical file") == b"junction bytes",
            "physical target contains bytes written through junction"
        );

        drop(physical_file);
        drop(file);
        drop(physical_directory);
        drop(directory);
        fs::remove_dir(&junction_parent).expect("remove ancestor junction");
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn validation_handle_blocks_replacement_until_identity_check_finishes() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let live_path = storage_path.join("history.json");
        let replacement_path = storage_path.join("replacement.json");
        let detached_path = storage_path.join("detached.json");

        let original = create_new_secure_file(&live_path).expect("create original file");
        let original_identity = storage_identity(
            original.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )
        .expect("read original identity");
        let replacement =
            create_new_secure_file(&replacement_path).expect("create replacement file");
        drop(replacement);

        verify_path_identity_with(
            &live_path,
            StorageObjectKind::RegularFile,
            original_identity,
            |_validation| {
                OpenOptions::new()
                    .access_mode(DELETE)
                    .share_mode(STORAGE_SHARE_MODE)
                    .open(&live_path)
                    .expect_err("validation handle blocks a competing DELETE-access open");
                let blocked = Command::new("cmd")
                    .arg("/D")
                    .arg("/C")
                    .arg("move")
                    .arg("/Y")
                    .arg(&live_path)
                    .arg(&detached_path)
                    .output()?;
                assert!(
                    !blocked.status.success(),
                    "another process cannot detach the path during validation"
                );
                assert!(
                    live_path.exists(),
                    "live path remains installed during validation"
                );
                assert!(
                    !detached_path.exists(),
                    "detached path is absent while validation handle is alive"
                );
                Ok(())
            },
        )
        .expect("point-in-time identity validation succeeds");

        let detached = Command::new("cmd")
            .arg("/D")
            .arg("/C")
            .arg("move")
            .arg("/Y")
            .arg(&live_path)
            .arg(&detached_path)
            .output()
            .expect("run detach rename after validation");
        assert!(
            detached.status.success(),
            "detach rename succeeds after validation handle closes"
        );
        fs::rename(&replacement_path, &live_path)
            .expect("replacement install succeeds after validation handle closes");

        drop(original);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn path_replacement_is_detected_without_rewriting_either_file() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        let live_path = storage_path.join("history.json");
        let replacement_path = storage_path.join("replacement.json");
        let detached_path = storage_path.join("detached.json");

        let mut original = create_new_secure_file(&live_path).expect("create original file");
        original
            .write_all(b"old open handle bytes")
            .expect("write original file");
        original.sync_all().expect("sync original file");
        let original_identity = storage_identity(
            original.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )
        .expect("read original identity");

        let mut replacement =
            create_new_secure_file(&replacement_path).expect("create replacement file");
        replacement
            .write_all(b"new path bytes")
            .expect("write replacement file");
        replacement.sync_all().expect("sync replacement file");
        let replacement_identity = storage_identity(
            replacement.as_raw_handle() as HANDLE,
            StorageObjectKind::RegularFile,
        )
        .expect("read replacement identity");
        assert!(
            original_identity != replacement_identity,
            "test files have distinct identities"
        );
        drop(replacement);

        fs::rename(&live_path, &detached_path).expect("detach original path");
        fs::rename(&replacement_path, &live_path).expect("install replacement path");
        assert!(
            verify_secure_file_path(&original, &live_path).is_err(),
            "identity revalidation detects replacement"
        );
        assert!(
            read_open_file(&mut original).expect("read old open handle")
                == b"old open handle bytes",
            "old open handle bytes are unchanged"
        );
        assert!(
            fs::read(&live_path).expect("read replacement path") == b"new path bytes",
            "replacement path bytes are unchanged"
        );

        drop(original);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn file_and_directory_helpers_reject_the_wrong_object_type() {
        let root = TempRoot::create().expect("create temporary root");
        let directory_path = root.join("permissive-directory");
        let directory =
            create_permissive_test_directory(&directory_path).expect("create permissive directory");
        let directory_acl = read_acl_snapshot(directory.as_raw_handle() as HANDLE)
            .expect("snapshot permissive directory DACL");
        assert!(
            open_existing_secure_file(&directory_path, false).is_err(),
            "file helper rejects a directory"
        );
        assert!(
            read_acl_snapshot(directory.as_raw_handle() as HANDLE)
                .expect("read rejected directory DACL")
                == directory_acl,
            "wrong-type rejection leaves the original permissive directory DACL unchanged"
        );

        let regular_path = root.join("permissive-regular-file");
        let regular = create_permissive_test_file(&regular_path, b"regular bytes")
            .expect("create permissive regular file");
        let regular_acl = read_acl_snapshot(regular.as_raw_handle() as HANDLE)
            .expect("snapshot permissive regular file DACL");
        assert!(
            ensure_secure_storage_directory(&regular_path).is_err(),
            "directory helper rejects a regular file"
        );
        assert!(
            read_acl_snapshot(regular.as_raw_handle() as HANDLE)
                .expect("read rejected regular file DACL")
                == regular_acl,
            "wrong-type rejection leaves the original permissive file DACL unchanged"
        );

        drop(regular);
        drop(directory);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn security_errors_do_not_disclose_path_sid_or_username() {
        let root = TempRoot::create().expect("create temporary root");
        let sensitive_path = root.join("private-storage-object");
        let file = create_permissive_test_file(&sensitive_path, b"private")
            .expect("create sensitive test file");
        let path_text = sensitive_path.to_string_lossy().into_owned();
        let current_user = current_process_user_sid().expect("read current process SID");
        let sid_text = sid_string(&current_user).expect("format current process SID");
        let username = std::env::var("USERNAME").expect("read current Windows username");

        let path_error = ensure_secure_storage_directory(&sensitive_path)
            .expect_err("wrong-type path is rejected generically");
        assert_generic_error(&path_error, &[&path_text, &sid_text, &username]);

        let local_system = well_known_sid(WinLocalSystemSid).expect("create LocalSystem SID");
        let everyone = well_known_sid(WinWorldSid).expect("create Everyone SID");
        let foreign_owner = if current_user.as_bytes() != local_system.as_bytes() {
            &local_system
        } else {
            &everyone
        };
        let owner_error = inspect_owner(foreign_owner.as_bytes(), current_user.as_bytes())
            .expect_err("foreign owner is rejected generically");
        assert_generic_error(&owner_error, &[&path_text, &sid_text, &username]);

        drop(file);
        root.cleanup().expect("remove temporary root");
    }

    #[test]
    fn directory_flush_and_missing_inputs_fail_closed() {
        let root = TempRoot::create().expect("create temporary root");
        let storage_path = root.join("storage");
        let directory =
            ensure_secure_storage_directory(&storage_path).expect("create secure directory");
        flush_secure_storage_directory(&directory).expect("valid directory flush succeeds");
        assert!(
            flush_storage_directory_handle(INVALID_HANDLE_VALUE).is_err(),
            "invalid handle is rejected"
        );

        let missing_file = storage_path.join("missing.json");
        assert!(
            open_existing_secure_file(&missing_file, false).is_err(),
            "nonexistent file is rejected"
        );
        let missing_parent = root.join("missing-parent");
        assert!(
            ensure_secure_storage_directory(&missing_parent.join("storage")).is_err(),
            "missing ancestor is not created"
        );
        assert!(!missing_parent.exists(), "missing ancestor remains absent");

        drop(directory);
        root.cleanup().expect("remove temporary root");
    }
}
