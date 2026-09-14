use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

pub const MAX_ACTIVE_JOBS: usize = 64;
pub const CREATION_FLAGS: u32 = CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP;

const KILL_GRACE: Duration = Duration::from_millis(500);
static ACTIVE_JOB_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn active_job_count() -> usize {
    ACTIVE_JOB_COUNT.load(Ordering::SeqCst)
}

pub struct Job {
    handle: HANDLE,
}

impl Job {
    pub fn new() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("creating Windows Job Object");
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(error).context("configuring Windows Job Object");
        }

        ACTIVE_JOB_COUNT.fetch_add(1, Ordering::SeqCst);
        Ok(Self { handle })
    }

    pub fn assign_and_resume(&self, child: &Child) -> Result<()> {
        let process = child.as_raw_handle() as HANDLE;
        if unsafe { AssignProcessToJobObject(self.handle, process) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("assigning bash to Windows Job Object");
        }
        resume_process_thread(child.id())
    }

    pub fn terminate(&self, child: &mut Child) {
        if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
            let _ = child.kill();
        }
        if !wait_for_exit(child, KILL_GRACE) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
        ACTIVE_JOB_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

fn resume_process_thread(process_id: u32) -> Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("enumerating bash threads");
    }

    let result = (|| -> Result<()> {
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut available = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while available {
            if entry.th32OwnerProcessID == process_id {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(std::io::Error::last_os_error())
                        .context("opening suspended bash thread");
                }
                let resumed = unsafe { ResumeThread(thread) };
                unsafe {
                    CloseHandle(thread);
                }
                if resumed == u32::MAX {
                    return Err(std::io::Error::last_os_error())
                        .context("resuming suspended bash thread");
                }
                return Ok(());
            }
            available = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        bail!("unable to find the suspended bash thread")
    })();

    unsafe {
        CloseHandle(snapshot);
    }
    result
}

fn wait_for_exit(child: &mut Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}
