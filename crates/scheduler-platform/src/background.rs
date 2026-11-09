use std::{
    ffi::OsStr,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::Path,
    process::ExitStatus,
    ptr::null,
};

use anyhow::{Result, bail};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
        WaitForSingleObject,
    },
};

pub struct BackgroundProcess(OwnedHandle);

impl BackgroundProcess {
    pub fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let handle = self.0.as_raw_handle();
        // The process handle is valid while self owns it.
        unsafe {
            match WaitForSingleObject(handle, 0) {
                WAIT_TIMEOUT => Ok(None),
                WAIT_OBJECT_0 => {
                    let mut code = 0;
                    if GetExitCodeProcess(handle, &mut code) == 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(Some(ExitStatus::from_raw(code)))
                    }
                }
                _ => Err(io::Error::last_os_error()),
            }
        }
    }
}

/// Launch a hidden process without inheriting any handles from the foreground CLI.
pub fn spawn_background(program: &Path, args: &[&OsStr]) -> Result<BackgroundProcess> {
    let mut line = Vec::new();
    append_argument(&mut line, program.as_os_str())?;
    for arg in args {
        line.push(u16::from(b' '));
        append_argument(&mut line, arg)?;
    }
    line.push(0);
    let application: Vec<_> = program.as_os_str().encode_wide().chain([0]).collect();
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // Neither STARTF_USESTDHANDLES nor handle inheritance is enabled. This keeps the
    // daemon from holding open pipes captured by an invoking PowerShell session.
    // Both UTF-16 strings are terminated, and the command line is writable.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW,
            null(),
            null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error().into());
    }
    // On success, Windows transfers ownership of both handles to this process.
    unsafe {
        drop(OwnedHandle::from_raw_handle(process.hThread));
        Ok(BackgroundProcess(OwnedHandle::from_raw_handle(
            process.hProcess,
        )))
    }
}

fn append_argument(line: &mut Vec<u16>, argument: &OsStr) -> Result<()> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    line.push(QUOTE);
    let mut backslashes = 0;
    for unit in argument.encode_wide() {
        match unit {
            0 => bail!("A background process argument contains a null character"),
            BACKSLASH => backslashes += 1,
            QUOTE => {
                line.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(BACKSLASH, backslashes));
    line.push(QUOTE);
    Ok(())
}
