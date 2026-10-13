//! Windows process trees are assigned while suspended, before any child code runs.
use std::io;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{JobObjects::*, Threading::*},
};

pub(crate) struct Job(HANDLE);
// The handle is exclusively owned; Windows permits use from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub(crate) fn spawn(
        command: &mut tokio::process::Command,
        limits: Option<(u64, u64)>,
    ) -> io::Result<(tokio::process::Child, Self)> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some((memory, seconds)) = limits {
            info.BasicLimitInformation.LimitFlags |=
                JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_PROCESS_TIME;
            info.ProcessMemoryLimit = memory as usize;
            info.BasicLimitInformation.PerProcessUserTimeLimit = (seconds * 10_000_000) as i64;
        }
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        command.creation_flags(CREATE_SUSPENDED).kill_on_drop(true);
        let mut child = command.spawn()?;
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("child process handle unavailable"))?
            as HANDLE;
        if unsafe { AssignProcessToJobObject(handle, process) } == 0 {
            let error = io::Error::last_os_error();
            let _ = child.start_kill();
            return Err(error);
        }
        // Resuming the process avoids needing an inherited primary-thread handle.
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtResumeProcess(process: HANDLE) -> i32;
        }
        if unsafe { NtResumeProcess(process) } < 0 {
            let _ = child.start_kill();
            return Err(io::Error::other("cannot resume assigned child process"));
        }
        Ok((child, job))
    }

    pub(crate) fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.terminate();
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn dropping_job_kills_launcher_and_ping_grandchild() {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args([
            "/c",
            "start /b ping -n 30 127.0.0.1 >nul & ping -n 30 127.0.0.1 >nul",
        ]);
        let (mut child, job) = Job::spawn(&mut command, None).unwrap();
        // Query the owned job rather than unrelated ping processes on this machine.
        #[repr(C)]
        struct ProcessList {
            assigned: u32,
            count: u32,
            pids: [usize; 16],
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let pids = loop {
            let mut list = ProcessList {
                assigned: 0,
                count: 0,
                pids: [0; 16],
            };
            assert_ne!(
                unsafe {
                    QueryInformationJobObject(
                        job.0,
                        JobObjectBasicProcessIdList,
                        (&mut list as *mut ProcessList).cast(),
                        std::mem::size_of_val(&list) as u32,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            if list.count >= 2 {
                break list.pids[..list.count as usize].to_vec();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "cmd did not spawn ping"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let handles: Vec<_> = pids
            .iter()
            .map(|pid| unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, *pid as u32) })
            .collect();
        assert!(handles.iter().all(|handle| !handle.is_null()));
        drop(job);
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        for handle in handles {
            assert_eq!(unsafe { WaitForSingleObject(handle, 5000) }, 0);
            unsafe {
                CloseHandle(handle);
            }
        }
    }
}
