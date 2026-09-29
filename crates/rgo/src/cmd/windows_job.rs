//! Windows process-tree guard for the supervised Cargo pilot.
//!
//! Cargo must join the job before its first instruction runs. Closing the
//! launcher's private job handle kills every remaining member if the launcher
//! dies, so its filesystem lifecycle lock cannot outlive its protection.

use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
#[cfg(debug_assertions)]
use std::time::Instant;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute, WaitForSingleObject,
};

struct Handle(HANDLE);

#[allow(unsafe_code)] // Closing an owned Win32 handle requires the platform API.
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: this wrapper exclusively owns each returned Win32 handle.
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct JobAttributeList {
    storage: Vec<u128>,
}

impl JobAttributeList {
    #[allow(unsafe_code)]
    fn new(job: &Handle) -> Result<Self> {
        let mut bytes = 0;
        // The first call is required to obtain the opaque list's size and
        // intentionally fails with ERROR_INSUFFICIENT_BUFFER.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(std::io::Error::last_os_error())
                .context("sizing the Cargo process attribute list");
        }
        let mut storage = vec![0u128; bytes.div_ceil(std::mem::size_of::<u128>())];
        let list = storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut bytes) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("initializing the Cargo process attribute list");
        }
        let attributes = Self { storage };
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                (&raw const job.0).cast::<c_void>(),
                std::mem::size_of::<HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("binding Cargo to its Job Object at process creation");
        }
        Ok(attributes)
    }

    fn as_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_ptr().cast_mut().cast()
    }
}

#[allow(unsafe_code)]
impl Drop for JobAttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

pub(super) struct JobGuard {
    // Keep the kill-on-close job handle private to this process. It must close
    // before the Cargo-session filesystem lock can be released on abnormal exit.
    job: Handle,
    process: Option<Handle>,
}

impl JobGuard {
    #[allow(unsafe_code)]
    pub(super) fn spawn(executable: &Path, args: &[OsString]) -> Result<Self> {
        let application: Vec<u16> = executable
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut command_line = quote_windows_arg(executable.as_os_str());
        for arg in args {
            command_line.push(b' ' as u16);
            command_line.extend(quote_windows_arg(arg));
        }
        command_line.push(0);

        // SAFETY: null security attributes create a non-inheritable job handle.
        let job = Handle(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
        if job.0.is_null() {
            return Err(std::io::Error::last_os_error()).context("creating Cargo Job Object");
        }
        // Passing standard streams to Cargo requires handle inheritance. Make
        // the job's non-inheritance explicit before creating that child.
        if unsafe { SetHandleInformation(job.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("protecting Cargo Job Object handle");
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast::<c_void>(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("setting kill-on-close for Cargo");
        }

        let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        if [stdin, stdout, stderr]
            .iter()
            .any(|handle| handle.is_null() || *handle == INVALID_HANDLE_VALUE)
        {
            bail!("Cargo launcher has no inheritable standard streams");
        }
        // Windows assigns this job during CreateProcessW. There is no child
        // outside the kill-on-close job if the guardian dies after creation.
        let attributes = JobAttributeList::new(&job)?;
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: stdin,
                hStdOutput: stdout,
                hStdError: stderr,
                ..Default::default()
            },
            lpAttributeList: attributes.as_ptr(),
        };
        let mut info = PROCESS_INFORMATION::default();
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut info,
            )
        };
        if created == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("creating suspended Cargo process {}", executable.display())
            });
        }
        let process = Handle(info.hProcess);
        let primary_thread = Handle(info.hThread);
        #[cfg(debug_assertions)]
        pause_after_assignment_for_test(info.dwProcessId)?;
        if unsafe { ResumeThread(primary_thread.0) } == u32::MAX {
            // Dropping the private job handle terminates its suspended child.
            return Err(std::io::Error::last_os_error()).context("resuming supervised Cargo");
        }
        // The attribute list retains a pointer to job.0 until deletion, so
        // release it before moving that handle into the returned guard.
        drop(attributes);
        Ok(Self {
            job,
            process: Some(process),
        })
    }

    #[allow(unsafe_code)]
    pub(super) fn wait_primary(&mut self) -> Result<u32> {
        let process = self
            .process
            .as_ref()
            .context("Cargo process was already reaped")?;
        if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(std::io::Error::last_os_error()).context("waiting for Cargo");
        }
        let mut code = 0;
        if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
            return Err(std::io::Error::last_os_error()).context("reading Cargo exit status");
        }
        // Job accounting can retain a terminated process while a process
        // handle remains open. Release ours before querying ActiveProcesses.
        drop(self.process.take());
        Ok(code)
    }

    #[allow(unsafe_code)]
    pub(super) fn wait_empty(&self) -> Result<()> {
        loop {
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            if unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicAccountingInformation,
                    (&raw mut accounting).cast::<c_void>(),
                    std::mem::size_of_val(&accounting) as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                // Dropping the job after this error kills remaining writers.
                return Err(std::io::Error::last_os_error())
                    .context("checking Cargo descendant liveness");
            }
            if accounting.ActiveProcesses == 0 {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

/// A debug-build-only fault point for the live Windows interruption fixture.
/// The child is still suspended and already belongs to the private job.
#[cfg(debug_assertions)]
fn pause_after_assignment_for_test(child_pid: u32) -> Result<()> {
    let Some(marker) = std::env::var_os("RGO_TEST_JOB_ASSIGNED_MARKER") else {
        return Ok(());
    };
    let marker = PathBuf::from(marker);
    let release = PathBuf::from(
        std::env::var_os("RGO_TEST_JOB_ASSIGNED_RELEASE")
            .context("RGO_TEST_JOB_ASSIGNED_RELEASE is required with the assignment marker")?,
    );
    let staging = marker.with_extension("tmp");
    std::fs::write(&staging, child_pid.to_string())?;
    std::fs::rename(&staging, &marker)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.is_file() {
        if Instant::now() >= deadline {
            bail!("timed out at the Windows Job Object assignment test point");
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

pub(super) fn quote_windows_arg(value: &OsStr) -> Vec<u16> {
    let mut quoted = vec![b'"' as u16];
    let mut backslashes = 0;
    for word in value.encode_wide() {
        if word == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if word == b'"' as u16 {
            quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            quoted.push(word);
            backslashes = 0;
            continue;
        }
        quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        backslashes = 0;
        quoted.push(word);
    }
    quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    quoted.push(b'"' as u16);
    quoted
}
