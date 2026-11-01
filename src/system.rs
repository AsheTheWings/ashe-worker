//! The real input sources of a worker process: its environment, the user's
//! application data folders, and its two input files.

use crate::inputs::{FileLayer, KnownFolder};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub fn process_environment() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

pub fn read_overlay(path: &Path) -> FileLayer {
    match std::fs::read_to_string(path) {
        Ok(content) => FileLayer::Present(content),
        Err(error) if error.kind() == ErrorKind::NotFound => FileLayer::Absent,
        Err(error) => FileLayer::Unreadable(error.kind().to_string()),
    }
}

pub fn read_protected(path: &Path) -> FileLayer {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return FileLayer::Absent,
        Err(error) => return FileLayer::Unreadable(error.kind().to_string()),
        Ok(metadata) if !metadata.is_file() => {
            return FileLayer::Unreadable("not a regular file".to_string());
        }
        Ok(_) => {}
    }
    match owner_only(path) {
        Ok(true) => {}
        Ok(false) => return FileLayer::Unprotected,
        Err(reason) => return FileLayer::Unreadable(reason),
    }
    match std::fs::read_to_string(path) {
        Ok(content) => FileLayer::Present(content),
        Err(error) => FileLayer::Unreadable(error.kind().to_string()),
    }
}

#[cfg(unix)]
fn owner_only(path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).map_err(|error| error.kind().to_string())?;
    Ok(metadata.mode() & 0o077 == 0)
}

#[cfg(unix)]
pub fn known_folder(folder: KnownFolder) -> Option<PathBuf> {
    let variable = match folder {
        KnownFolder::RoamingAppData => "XDG_CONFIG_HOME",
        KnownFolder::LocalAppData => "XDG_DATA_HOME",
    };
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// A file is owner-only when every access-allowed entry of its effective DACL
/// names the current user, the local system, or the administrators group.
#[cfg(windows)]
fn owner_only(path: &Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetTokenInformation, INHERIT_ONLY_ACE, IsWellKnownSid, PSECURITY_DESCRIPTOR, PSID,
        TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PCWSTR;

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|_| "the process token is unavailable".to_string())?;
        let mut length = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut length);
        let mut buffer = vec![0u8; length as usize];
        let user = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        );
        let _ = CloseHandle(token);
        user.map_err(|_| "the current user is unavailable".to_string())?;
        let user_sid = (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid;

        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let status = GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        );
        if status.is_err() {
            return Err("its access control list is unreadable".to_string());
        }
        let result = if dacl.is_null() {
            Ok(false)
        } else {
            let mut owner_only = true;
            for index in 0..u32::from((*dacl).AceCount) {
                let mut ace = std::ptr::null_mut();
                if GetAce(dacl, index, &mut ace).is_err() {
                    owner_only = false;
                    break;
                }
                let header = &*(ace as *const ACE_HEADER);
                if header.AceFlags & INHERIT_ONLY_ACE.0 as u8 != 0 {
                    continue;
                }
                if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                    continue;
                }
                let allowed = ace as *const ACCESS_ALLOWED_ACE;
                let sid = PSID(std::ptr::addr_of!((*allowed).SidStart) as *mut _);
                let trusted = EqualSid(sid, user_sid).is_ok()
                    || IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
                    || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool();
                if !trusted {
                    owner_only = false;
                    break;
                }
            }
            Ok(owner_only)
        };
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result
    }
}

#[cfg(windows)]
pub fn known_folder(folder: KnownFolder) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    let id = match folder {
        KnownFolder::RoamingAppData => FOLDERID_RoamingAppData,
        KnownFolder::LocalAppData => FOLDERID_LocalAppData,
    };
    unsafe {
        let path = SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT, None).ok()?;
        let value = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        value.map(PathBuf::from)
    }
}
