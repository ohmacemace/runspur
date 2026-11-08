use anyhow::{Result, bail};
use scheduler_core::ProcessIdentity;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

pub fn current_principal() -> Result<String> {
    #[cfg(windows)]
    {
        crate::windows::current_sid()
    }
    #[cfg(not(windows))]
    {
        bail!("User identity backend is not yet enabled on this release target")
    }
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub identity: ProcessIdentity,
    pub executable: PathBuf,
    pub parent_pid: Option<u32>,
}

#[cfg(windows)]
pub fn identity(pid: u32) -> Result<ProcessIdentity> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    unsafe {
        let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetProcessTimes(
            handle.as_raw_handle(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(ProcessIdentity {
            pid,
            started_at: (creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64,
        })
    }
}
#[cfg(not(windows))]
pub fn identity(_pid: u32) -> Result<ProcessIdentity> {
    bail!("Process identity backend is not enabled on this release target")
}

pub fn is_same_process(expected: &ProcessIdentity) -> bool {
    identity(expected.pid).is_ok_and(|actual| actual == *expected)
}

pub fn is_running(expected: &ProcessIdentity) -> bool {
    if !is_same_process(expected) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, expected.pid);
            if raw.is_null() {
                return false;
            }
            let process = OwnedHandle::from_raw_handle(raw);
            let mut code = 0;
            GetExitCodeProcess(process.as_raw_handle(), &mut code) != 0 && code == 259
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
pub fn process_principal(pid: u32) -> Result<String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    crate::windows::sid_for_process(process.as_raw_handle())
}
#[cfg(not(windows))]
pub fn process_principal(_pid: u32) -> Result<String> {
    bail!("Process principal inspection is not enabled")
}

#[cfg(windows)]
pub fn process_info(pid: u32) -> Result<ProcessInfo> {
    use std::{
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
    };
    let process_identity = identity(pid)?;
    unsafe {
        let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        let mut buffer = vec![0u16; 32768];
        let mut len = buffer.len() as u32;
        if QueryFullProcessImageNameW(handle.as_raw_handle(), 0, buffer.as_mut_ptr(), &mut len) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let executable = PathBuf::from(String::from_utf16(&buffer[..len as usize])?);
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = Process32FirstW(snapshot.as_raw_handle(), &mut entry) != 0;
        let mut parent_pid = None;
        while found {
            if entry.th32ProcessID == pid {
                parent_pid = (entry.th32ParentProcessID != 0).then_some(entry.th32ParentProcessID);
                break;
            }
            found = Process32NextW(snapshot.as_raw_handle(), &mut entry) != 0;
        }
        Ok(ProcessInfo {
            identity: process_identity,
            executable,
            parent_pid,
        })
    }
}
#[cfg(not(windows))]
pub fn process_info(_pid: u32) -> Result<ProcessInfo> {
    bail!("Process inspection is not enabled on this release target")
}

pub fn ancestors(pid: u32) -> Result<Vec<ProcessInfo>> {
    let mut result = Vec::new();
    let mut next = Some(pid);
    while let Some(pid) = next {
        if result.len() >= 24 || result.iter().any(|p: &ProcessInfo| p.identity.pid == pid) {
            break;
        }
        let process = process_info(pid)?;
        if result.last().is_some_and(|child: &ProcessInfo| {
            process.identity.started_at > child.identity.started_at
        }) {
            break;
        }
        next = process.parent_pid;
        result.push(process);
    }
    Ok(result)
}

#[cfg(windows)]
pub struct WorkerJob {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WorkerJob {
    pub fn new() -> Result<Self> {
        use std::{
            mem::size_of,
            os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
            ptr,
        };
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let raw = CreateJobObjectW(ptr::null(), ptr::null());
            if raw.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let handle = OwnedHandle::from_raw_handle(raw);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self { handle })
        }
    }
    pub fn attach(&self, pid: u32) -> Result<()> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::{
            JobObjects::AssignProcessToJobObject,
            Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                PROCESS_TERMINATE,
            },
        };
        unsafe {
            let raw = OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            );
            if raw.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let process = OwnedHandle::from_raw_handle(raw);
            if AssignProcessToJobObject(self.handle.as_raw_handle(), process.as_raw_handle()) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(())
    }
    pub fn contains(&self, pid: u32) -> Result<bool> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::{
            JobObjects::IsProcessInJob,
            Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        };
        unsafe {
            let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if raw.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let process = OwnedHandle::from_raw_handle(raw);
            let mut result = 0;
            if IsProcessInJob(
                process.as_raw_handle(),
                self.handle.as_raw_handle(),
                &mut result,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(result != 0)
        }
    }
    pub fn terminate(&self) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        if unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                self.handle.as_raw_handle(),
                1,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub fn is_empty(&self) -> Result<bool> {
        use std::{mem::size_of, os::windows::io::AsRawHandle, ptr};
        use windows_sys::Win32::System::JobObjects::*;
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(accounting.ActiveProcesses == 0)
    }
}

#[cfg(not(windows))]
pub struct WorkerJob;
#[cfg(not(windows))]
impl WorkerJob {
    pub fn new() -> Result<Self> {
        bail!("Worker supervision is not enabled on this release target")
    }
    pub fn attach(&self, _pid: u32) -> Result<()> {
        bail!("Worker supervision is not enabled")
    }
    pub fn contains(&self, _pid: u32) -> Result<bool> {
        bail!("Worker supervision is not enabled")
    }
    pub fn terminate(&self) -> Result<()> {
        bail!("Worker supervision is not enabled")
    }
    pub fn is_empty(&self) -> Result<bool> {
        bail!("Worker supervision is not enabled")
    }
}

#[derive(Clone, Default)]
pub struct WorkerRegistry {
    jobs: Arc<RwLock<HashMap<String, Arc<WorkerJob>>>>,
    orphans: Arc<RwLock<Vec<ProcessIdentity>>>,
}
impl WorkerRegistry {
    pub fn insert(&self, task_id: String, job: Arc<WorkerJob>) -> Result<()> {
        self.jobs
            .write()
            .map_err(|_| anyhow::anyhow!("Worker registry poisoned"))?
            .insert(task_id, job);
        Ok(())
    }
    pub fn contains(&self, pid: u32) -> Result<bool> {
        for job in self
            .jobs
            .read()
            .map_err(|_| anyhow::anyhow!("Worker registry poisoned"))?
            .values()
        {
            if job.contains(pid)? {
                return Ok(true);
            }
        }
        let orphans = self
            .orphans
            .read()
            .map_err(|_| anyhow::anyhow!("Worker registry poisoned"))?;
        if !orphans.is_empty() {
            let mut next = Some(pid);
            for _ in 0..24 {
                let Some(pid) = next else { break };
                let Ok(info) = process_info(pid) else { break };
                if orphans.contains(&info.identity) {
                    return Ok(true);
                }
                next = info.parent_pid;
            }
        }
        Ok(false)
    }
    pub fn block_orphan(&self, process: ProcessIdentity) -> Result<()> {
        self.orphans
            .write()
            .map_err(|_| anyhow::anyhow!("Worker registry poisoned"))?
            .push(process);
        Ok(())
    }
    pub fn retire(&self, task_id: &str) -> Result<bool> {
        let mut jobs = self
            .jobs
            .write()
            .map_err(|_| anyhow::anyhow!("Worker registry poisoned"))?;
        if let Some(job) = jobs.get(task_id) {
            job.terminate()?;
            if !job.is_empty()? {
                return Ok(false);
            }
        }
        jobs.remove(task_id);
        Ok(true)
    }
    pub fn reap(&self) -> Result<()> {
        self.jobs
            .write()
            .map_err(|_| anyhow::anyhow!("Worker registry poisoned"))?
            .retain(|_, job| !job.is_empty().unwrap_or(false));
        self.orphans
            .write()
            .map_err(|_| anyhow::anyhow!("Worker registry poisoned"))?
            .retain(is_running);
        Ok(())
    }
}

pub fn configure_suspended(command: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
        };
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

#[cfg(windows)]
pub fn resume_suspended(pid: u32) -> Result<()> {
    use std::{
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    unsafe {
        let raw = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if raw == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let snapshot = OwnedHandle::from_raw_handle(raw);
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut found = Thread32First(snapshot.as_raw_handle(), &mut entry) != 0;
        while found {
            if entry.th32OwnerProcessID == pid {
                let raw = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if raw.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                let thread = OwnedHandle::from_raw_handle(raw);
                if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                    return Err(std::io::Error::last_os_error().into());
                }
                return Ok(());
            }
            found = Thread32Next(snapshot.as_raw_handle(), &mut entry) != 0;
        }
    }
    bail!("Suspended worker thread was not found")
}
#[cfg(not(windows))]
pub fn resume_suspended(_pid: u32) -> Result<()> {
    bail!("Worker supervision is not enabled")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn worker_is_registered_before_any_execution() {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args(["/C", "exit", "0"]).kill_on_drop(true);
        configure_suspended(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let job = Arc::new(WorkerJob::new().unwrap());
        job.attach(pid).unwrap();
        let registry = WorkerRegistry::default();
        registry.insert("task".into(), job.clone()).unwrap();
        assert!(registry.contains(pid).unwrap());
        assert!(!job.contains(std::process::id()).unwrap());
        let first = identity(pid).unwrap();
        assert!(is_same_process(&first));
        resume_suspended(pid).unwrap();
        assert!(child.wait().await.unwrap().success());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !registry.retire("task").unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(job.is_empty().unwrap());
    }
}
