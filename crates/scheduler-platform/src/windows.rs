use std::{
    ffi::c_void,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
        DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
pub fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

pub fn current_sid() -> Result<String> {
    sid_for_process(unsafe { GetCurrentProcess() })
}

pub fn sid_for_process(process: windows_sys::Win32::Foundation::HANDLE) -> Result<String> {
    unsafe {
        let mut raw = ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut raw) == 0 {
            return Err(last_error().into());
        }
        let token = OwnedHandle::from_raw_handle(raw);
        let mut size = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut size,
        );
        if size == 0 {
            return Err(last_error().into());
        }
        let mut bytes = vec![0u8; size as usize];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            bytes.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(last_error().into());
        }
        let user = ptr::read_unaligned(bytes.as_ptr().cast::<TOKEN_USER>());
        let mut sid = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(last_error().into());
        }
        let mut len = 0;
        while *sid.add(len) != 0 {
            len += 1;
        }
        let result = String::from_utf16(std::slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());
        Ok(result?)
    }
}

pub struct SecurityDescriptor {
    raw: *mut c_void,
}
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.raw);
        }
    }
}
impl SecurityDescriptor {
    pub fn user_only(inherit: bool) -> Result<Self> {
        let sid = current_sid()?;
        let text = format!("D:P(A;{};GA;;;{sid})", if inherit { "OICI" } else { "" });
        let value = wide(std::ffi::OsStr::new(&text));
        let mut raw = ptr::null_mut();
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                value.as_ptr(),
                1,
                &mut raw,
                ptr::null_mut(),
            ) == 0
            {
                return Err(last_error().into());
            }
        }
        Ok(Self { raw })
    }
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.raw,
            bInheritHandle: 0,
        }
    }
}

pub fn secure_directory(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Security::SetFileSecurityW;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if std::fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("State directory cannot be a reparse point");
    }
    let descriptor = SecurityDescriptor::user_only(true)?;
    let path = wide(path.as_os_str());
    unsafe {
        if SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor.raw,
        ) == 0
        {
            return Err(last_error())
                .context("Cannot restrict state directory to the current user");
        }
    }
    Ok(())
}

pub fn protect(input: &[u8]) -> Result<Vec<u8>> {
    crypt(input, true)
}
pub fn unprotect(input: &[u8]) -> Result<Vec<u8>> {
    crypt(input, false)
}
fn crypt(input: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    if input.len() > u32::MAX as usize {
        bail!("Credential is too large");
    }
    unsafe {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut output: CRYPT_INTEGER_BLOB = std::mem::zeroed();
        let ok = if encrypt {
            CryptProtectData(
                &blob,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &blob,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::from_raw_os_error(GetLastError() as i32).into());
        }
        let bytes = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let copy = bytes.to_vec();
        if !encrypt {
            use zeroize::Zeroize;
            bytes.zeroize();
        }
        LocalFree(output.pbData.cast());
        Ok(copy)
    }
}
