use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CALL_NOT_IMPLEMENTED, ERROR_INVALID_FUNCTION,
    ERROR_INVALID_HANDLE, ERROR_LOCK_VIOLATION, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    GENERIC_ALL, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetExplicitEntriesFromAclW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorDacl, GetTokenInformation,
    InitializeSecurityDescriptor, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers,
    GetFileInformationByHandle, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING, UnlockFileEx,
};
use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
const LOCK_OFFSET: u64 = 1 << 63;
const LOCK_LENGTH: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    volume_serial: u32,
    file_index: u64,
}

pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

pub(crate) fn lock_exclusive(file: &File) -> io::Result<()> {
    lock(file, LOCKFILE_EXCLUSIVE_LOCK)
}

pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<()> {
    lock(file, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)?;
    Ok(())
}

pub(crate) fn unlock(file: &File) -> io::Result<()> {
    let mut overlapped = lock_overlapped();
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as HANDLE,
            0,
            LOCK_LENGTH,
            0,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn is_lock_busy(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == ERROR_LOCK_VIOLATION as i32 || code == ERROR_SHARING_VIOLATION as i32
    )
}

fn lock(file: &File, flags: u32) -> io::Result<()> {
    let mut overlapped = lock_overlapped();
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            flags,
            0,
            LOCK_LENGTH,
            0,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn lock_overlapped() -> OVERLAPPED {
    OVERLAPPED {
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: LOCK_OFFSET as u32,
                OffsetHigh: (LOCK_OFFSET >> 32) as u32,
            },
        },
        ..OVERLAPPED::default()
    }
}

pub(crate) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide_path(from);
    let to = wide_path(to);
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn flush_directory(path: &Path) -> io::Result<()> {
    let path = wide_path(path);
    let handle = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return if unsupported_directory_flush(&error) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let result = unsafe { FlushFileBuffers(handle) };
    let flush_error = (result == 0).then(io::Error::last_os_error);
    let close_result = unsafe { CloseHandle(handle) };
    if let Some(error) = flush_error {
        if unsupported_directory_flush(&error) {
            return if close_result == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            };
        }
        return Err(error);
    }
    if close_result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unsupported_directory_flush(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_CALL_NOT_IMPLEMENTED as i32
                || code == ERROR_INVALID_FUNCTION as i32
                || code == ERROR_INVALID_HANDLE as i32
                || code == ERROR_NOT_SUPPORTED as i32
    )
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("private Mu path is not a directory: {}", path.display());
            }
            validate_private_dacl(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir(path),
        Err(error) => {
            Err(error).with_context(|| format!("checking private Mu directory {}", path.display()))
        }
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    let user_sid = current_user_sid()?;
    let trustee = TRUSTEE_W {
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ptstrName: user_sid.as_ptr() as *mut u16,
        ..TRUSTEE_W::default()
    };
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: SET_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: trustee,
    };
    let mut acl: *mut ACL = std::ptr::null_mut();
    let status = unsafe { SetEntriesInAclW(1, &access, std::ptr::null(), &mut acl) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("building private DACL for {}", path.display()));
    }

    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let initialized = unsafe {
        InitializeSecurityDescriptor(
            &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut _,
            SECURITY_DESCRIPTOR_REVISION,
        )
    };
    if initialized == 0 {
        unsafe { LocalFree(acl as _) };
        return Err(io::Error::last_os_error()).context("initializing private security descriptor");
    }
    let dacl_set = unsafe {
        SetSecurityDescriptorDacl(
            &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut _,
            1,
            acl,
            0,
        )
    };
    if dacl_set == 0 {
        unsafe { LocalFree(acl as _) };
        return Err(io::Error::last_os_error()).context("setting private directory DACL");
    }
    let protected = unsafe {
        SetSecurityDescriptorControl(
            &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut _,
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        )
    };
    if protected == 0 {
        unsafe { LocalFree(acl as _) };
        return Err(io::Error::last_os_error()).context("protecting private directory DACL");
    }
    let owner_set = unsafe {
        SetSecurityDescriptorOwner(
            &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut _,
            user_sid.as_ptr() as *mut _,
            0,
        )
    };
    if owner_set == 0 {
        let error = io::Error::last_os_error();
        unsafe { LocalFree(acl as _) };
        return Err(error).context("setting private directory owner");
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut _,
        bInheritHandle: 0,
    };
    let wide = wide_path(path);
    let created = unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) };
    let create_error = (created == 0).then(|| unsafe { GetLastError() });
    unsafe { LocalFree(acl as _) };
    if created != 0 {
        return Ok(());
    }
    if create_error == Some(ERROR_ALREADY_EXISTS) {
        return match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                validate_private_dacl(path)
            }
            Ok(_) => bail!("private Mu path is not a directory: {}", path.display()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "checking private Mu directory after concurrent creation {}",
                    path.display()
                )
            }),
        };
    }
    Err(io::Error::from_raw_os_error(
        create_error.expect("failed CreateDirectoryW has an error code") as i32,
    ))
    .with_context(|| format!("creating private Mu directory {}", path.display()))
}

fn validate_private_dacl(path: &Path) -> Result<()> {
    let user_sid = current_user_sid()?;
    let wide = wide_path(path);
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("reading private directory DACL {}", path.display()));
    }
    let result = (|| -> Result<()> {
        if owner.is_null() {
            bail!("private Mu directory has no owner: {}", path.display());
        }
        let mut present = 0;
        let mut descriptor_dacl = std::ptr::null_mut();
        let mut defaulted = 0;
        let valid = unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &mut present,
                &mut descriptor_dacl,
                &mut defaulted,
            )
        };
        if valid == 0 {
            return Err(io::Error::last_os_error()).context("reading private directory DACL");
        }
        if present == 0 || descriptor_dacl.is_null() {
            bail!("private Mu directory has a null DACL: {}", path.display());
        }
        if unsafe { EqualSid(owner, user_sid.as_ptr() as *mut _) } == 0 {
            bail!(
                "private Mu directory is owned by another user: {}",
                path.display()
            );
        }
        let mut count = 0;
        let mut entries = std::ptr::null_mut();
        let status =
            unsafe { GetExplicitEntriesFromAclW(descriptor_dacl, &mut count, &mut entries) };
        if status != 0 {
            unsafe { LocalFree(entries as _) };
            return Err(io::Error::from_raw_os_error(status as i32))
                .context("reading private directory access entries");
        }
        if count != 0 && entries.is_null() {
            bail!(
                "private Mu directory has no readable access entries: {}",
                path.display()
            );
        }
        let entries_slice = if entries.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(entries, count as usize) }
        };
        let only_user = entries_slice.iter().all(|entry| {
            let trustee = &entry.Trustee;
            trustee.TrusteeForm == TRUSTEE_IS_SID
                && !trustee.ptstrName.is_null()
                && unsafe { EqualSid(trustee.ptstrName as *mut _, user_sid.as_ptr() as *mut _) }
                    != 0
        });
        let has_user_entry = !entries_slice.is_empty();
        unsafe { LocalFree(entries as _) };
        if !only_user {
            bail!(
                "private Mu directory has access for another principal: {}",
                path.display()
            );
        }
        if !has_user_entry {
            bail!(
                "private Mu directory has no user access entry: {}",
                path.display()
            );
        }
        Ok(())
    })();
    unsafe { LocalFree(descriptor as _) };
    result
}

fn current_user_sid() -> Result<Vec<u8>> {
    let mut token = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error()).context("opening current user token");
    }
    let result = (|| -> Result<Vec<u8>> {
        let mut required = 0;
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(io::Error::last_os_error()).context("querying current user token size");
        }
        let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0usize; words];
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr() as *mut _,
                (storage.len() * std::mem::size_of::<usize>()) as u32,
                &mut required,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error()).context("reading current user token");
        }
        let token_user = unsafe { &*(storage.as_ptr() as *const TOKEN_USER) };
        let sid = token_user.User.Sid;
        if sid.is_null() {
            bail!("current user token has no SID");
        }
        let length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid) } as usize;
        let mut copy = vec![0u8; length];
        if unsafe {
            windows_sys::Win32::Security::CopySid(length as u32, copy.as_mut_ptr() as *mut _, sid)
        } == 0
        {
            return Err(io::Error::last_os_error()).context("copying current user SID");
        }
        Ok(copy)
    })();
    unsafe { CloseHandle(token) };
    result
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
