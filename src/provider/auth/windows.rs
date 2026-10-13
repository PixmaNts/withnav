//! Credential files carry a protected DACL before any secret is written.
use crate::error::ProviderError;
use std::{ffi::c_void, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    System::Threading::*,
};

fn fail() -> ProviderError {
    super::io_api(std::io::Error::last_os_error())
}
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn sid_string(sid: PSID) -> Result<String, ProviderError> {
    unsafe {
        let mut text = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return Err(fail());
        }
        let _guard = Local(text.cast());
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}
fn current_sid() -> Result<String, ProviderError> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(fail());
        }
        let mut size = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        );
        let error = (ok == 0).then(std::io::Error::last_os_error);
        CloseHandle(token);
        if let Some(error) = error {
            return Err(super::io_api(error));
        }
        sid_string((*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid)
    }
}
pub(super) fn protect(path: &Path) -> Result<(), ProviderError> {
    let sddl = format!("D:P(A;;FA;;;{})(A;;FA;;;SY)(A;;FA;;;BA)", current_sid()?);
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut sd = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut sd,
            ptr::null_mut(),
        ) == 0
        {
            return Err(fail());
        }
        let _guard = Local(sd);
        let mut acl = ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        if GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) == 0 {
            return Err(fail());
        }
        let error = SetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        );
        if error != 0 {
            return Err(super::io_api(std::io::Error::from_raw_os_error(
                error as i32,
            )));
        }
    }
    Ok(())
}
pub(super) fn check_perms_or_fail(path: &Path) -> Result<(), ProviderError> {
    let allowed = [current_sid()?, "S-1-5-18".into(), "S-1-5-32-544".into()];
    let insecure = || {
        ProviderError::Auth(format!("auth.json at {} has insecure permissions; restrict its DACL to the current user, SYSTEM and Administrators", path.display()))
    };
    unsafe {
        let mut sd = ptr::null_mut();
        let mut acl = ptr::null_mut();
        let error = GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut sd,
        );
        if error != 0 {
            return Err(super::io_api(std::io::Error::from_raw_os_error(
                error as i32,
            )));
        }
        let _guard = Local(sd);
        if acl.is_null() {
            return Err(insecure());
        }
        for index in 0..(*acl).AceCount {
            let mut ace = ptr::null_mut();
            if GetAce(acl, u32::from(index), &mut ace) == 0 {
                return Err(fail());
            }
            let header = &*ace.cast::<ACE_HEADER>();
            if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
                continue;
            }
            if header.AceType == 1 {
                continue;
            }
            if header.AceType != 0 {
                return Err(insecure());
            }
            let entry = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            let sid = ptr::addr_of!(entry.SidStart).cast_mut().cast();
            if !allowed.contains(&sid_string(sid)?) {
                return Err(insecure());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everyone_access_is_refused() {
        let root = tempfile::tempdir().unwrap();
        super::super::set_key(root.path(), "mock", "secret").unwrap();
        let path = root.path().join("auth.json");
        check_perms_or_fail(&path).unwrap();
        let text: Vec<u16> = "D:P(A;;FA;;;WD)".encode_utf16().chain(Some(0)).collect();
        unsafe {
            let mut sd = ptr::null_mut();
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut sd,
                    ptr::null_mut()
                ),
                0
            );
            let _guard = Local(sd);
            let mut acl = ptr::null_mut();
            let mut present = 0;
            let mut defaulted = 0;
            assert_ne!(
                GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted),
                0
            );
            assert_eq!(
                SetNamedSecurityInfoW(
                    wide(&path).as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    acl,
                    ptr::null()
                ),
                0
            );
        }
        assert!(super::super::load_store(root.path())
            .unwrap_err()
            .to_string()
            .contains("insecure permissions"));
    }
}
