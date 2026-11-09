use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt, path::Path, ptr::null};

use anyhow::{Result, bail};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::Threading::{CREATE_NEW_CONSOLE, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW},
};

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;

pub(crate) fn spawn(program: &Path, args: &[&OsStr], directory: &Path) -> Result<()> {
    let mut command_line = Vec::new();
    append_argument(&mut command_line, program.as_os_str())?;
    for arg in args {
        command_line.push(u16::from(b' '));
        append_argument(&mut command_line, arg)?;
    }
    command_line.push(0);

    let program = terminated_path(program)?;
    let directory = terminated_path(directory)?;
    let mut title: Vec<_> = "Runspur Claude Code".encode_utf16().chain([0]).collect();
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpTitle: title.as_mut_ptr(),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // Keep STARTF_USESTDHANDLES clear so Windows supplies the new console's streams.
    // The UTF-16 inputs are terminated and the command line is writable for the duration of the call.
    let created = unsafe {
        CreateProcessW(
            program.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NEW_CONSOLE,
            null(),
            directory.as_ptr(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error().into());
    }
    // A successful call transfers these two handles to the caller.
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

fn terminated_path(path: &Path) -> Result<Vec<u16>> {
    let mut wide: Vec<_> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        bail!("The console path contains a null character");
    }
    wide.push(0);
    Ok(wide)
}

fn append_argument(line: &mut Vec<u16>, argument: &OsStr) -> Result<()> {
    line.push(QUOTE);
    let mut backslashes = 0;
    for unit in argument.encode_wide() {
        match unit {
            0 => bail!("A console argument contains a null character"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_windows_paths_and_preserves_backslashes() {
        let mut line = Vec::new();
        append_argument(&mut line, OsStr::new(r"D:\项目 示例\")).unwrap();
        assert_eq!(String::from_utf16(&line).unwrap(), r#""D:\项目 示例\\""#);

        line.clear();
        append_argument(&mut line, OsStr::new(r#"a\"b"#)).unwrap();
        assert_eq!(String::from_utf16(&line).unwrap(), r#""a\\\"b""#);

        line.clear();
        append_argument(&mut line, OsStr::new("")).unwrap();
        assert_eq!(String::from_utf16(&line).unwrap(), "\"\"");
    }
}
