//! Lifecycle containment for host commands. This is not a security sandbox.
#[cfg(windows)]
pub struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for ProcessTree {}
#[cfg(windows)]
impl ProcessTree {
    pub fn attach(id: u32) -> anyhow::Result<Self> {
        use windows_sys::Win32::{
            Foundation::*,
            System::{JobObjects::*, Threading::*},
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            anyhow::ensure!(
                !job.is_null(),
                "create command job: {}",
                std::io::Error::last_os_error()
            );
            let guard = Self(job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            anyhow::ensure!(
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32
                ) != 0,
                "set command job limits"
            );
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false as i32, id);
            anyhow::ensure!(!process.is_null(), "open command process");
            let result = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            anyhow::ensure!(
                result != 0,
                "assign command job: {}",
                std::io::Error::last_os_error()
            );
            Ok(guard)
        }
    }
}
#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Owns the native terminal's entire OS session, including shell jobs that use
/// separate process groups. Host Agent commands continue using `ProcessTree`.
#[cfg(windows)]
pub struct TerminalProcessTree {
    _job: ProcessTree,
}

#[cfg(windows)]
impl TerminalProcessTree {
    pub fn attach(id: u32) -> anyhow::Result<Self> {
        ProcessTree::attach(id).map(|job| Self { _job: job })
    }
}

#[cfg(unix)]
pub struct TerminalProcessTree(Option<terminal_session::Session>);

#[cfg(unix)]
impl TerminalProcessTree {
    pub fn attach(id: u32) -> anyhow::Result<Self> {
        terminal_session::Session::attach(id).map(|session| Self(Some(session)))
    }
}

#[cfg(unix)]
impl Drop for TerminalProcessTree {
    fn drop(&mut self) {
        let Some(session) = self.0.take() else {
            return;
        };
        // Process enumeration and job teardown must never hold up an egui frame.
        if let Err(error) = std::thread::Builder::new()
            .name("wonderland-terminal-session-close".into())
            .spawn(move || session.terminate())
        {
            // The caller still kills/reaps its direct PTY child. Do not hide a
            // failed session cleanup or turn it into an unbounded UI stall.
            tracing::error!(%error, "could not start terminal session cleanup; background jobs may remain");
        }
    }
}

#[cfg(unix)]
mod terminal_session {
    use anyhow::{Context, Result};
    use std::{
        collections::BTreeSet,
        time::{Duration, Instant},
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Identity {
        pid: libc::pid_t,
        session: libc::pid_t,
        uid: libc::uid_t,
        born: u128,
    }

    pub(super) struct Session {
        leader: Identity,
    }

    impl Session {
        pub(super) fn attach(id: u32) -> Result<Self> {
            let pid = libc::pid_t::try_from(id).context("invalid terminal process id")?;
            anyhow::ensure!(pid > 1, "invalid terminal session leader");
            let leader = inspect(pid).context("read terminal process identity")?;
            // portable-pty calls setsid before exec. Refuse an ordinary process
            // in the app/user shell's session: it is never ours to terminate.
            anyhow::ensure!(
                leader.session == pid,
                "terminal process is not an independent session leader"
            );
            anyhow::ensure!(
                unsafe { libc::getsid(0) } != pid,
                "cannot own the desktop's session"
            );
            anyhow::ensure!(
                leader.uid == unsafe { libc::geteuid() },
                "terminal belongs to another user"
            );
            #[cfg(target_os = "linux")]
            {
                let handle = ProcessHandle::open(leader).context("terminal sessions require Linux pidfd support (kernel 5.3+) and access to /proc")?;
                anyhow::ensure!(
                    handle.signal(0),
                    "cannot signal terminal through Linux pidfd"
                );
            }
            Ok(Self { leader })
        }

        fn owns(&self, process: Identity) -> bool {
            process.session == self.leader.pid
                && process.uid == self.leader.uid
                && process.born >= self.leader.born
        }

        pub(super) fn terminate(&self) {
            // If the old session vanished and its numerical leader PID was
            // recycled, never touch the new process or its session.
            if inspect(self.leader.pid).is_some_and(|current| current != self.leader) {
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut seen = BTreeSet::new();
            let mut handles = Vec::new();
            // Stop the shell first, then freeze its existing descendants. A
            // second scan catches children forked during the first scan. Jobs
            // remain members of this session even after their parent exits.
            if let Some(handle) = ProcessHandle::open(self.leader) {
                if !handle.signal(libc::SIGSTOP) {
                    tracing::warn!(
                        pid = self.leader.pid,
                        "could not stop terminal session leader during cleanup"
                    );
                }
                seen.insert((self.leader.pid, self.leader.born));
                handles.push(handle);
            }
            for _ in 0..16 {
                let mut added = false;
                let pids = match process_ids() {
                    Ok(pids) => pids,
                    Err(error) => {
                        tracing::error!(session = self.leader.pid, %error, "could not enumerate terminal session jobs during cleanup");
                        break;
                    }
                };
                for pid in pids {
                    let Some(identity) = inspect(pid) else {
                        continue;
                    };
                    if !self.owns(identity) || !seen.insert((identity.pid, identity.born)) {
                        continue;
                    }
                    if let Some(handle) = ProcessHandle::open(identity) {
                        if !handle.signal(libc::SIGSTOP) {
                            tracing::warn!(
                                pid = identity.pid,
                                "could not stop terminal session member during cleanup"
                            );
                        }
                        handles.push(handle);
                        added = true;
                    } else if inspect(identity.pid) == Some(identity) {
                        tracing::error!(
                            pid = identity.pid,
                            "could not pin terminal session member for cleanup"
                        );
                    }
                }
                if !added {
                    break;
                }
                if Instant::now() >= deadline {
                    tracing::warn!(
                        session = self.leader.pid,
                        "terminal session cleanup reached its rescan limit"
                    );
                    break;
                }
            }
            // Each handle is tied to a validated process, never a bare PGID.
            // Kill the shell last so it cannot launch replacements afterwards.
            for handle in handles.iter().rev() {
                if !handle.signal(libc::SIGKILL) {
                    tracing::warn!(
                        session = self.leader.pid,
                        "a terminal session member could not be terminated or already exited"
                    );
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct ProcessHandle {
        fd: std::os::fd::OwnedFd,
    }

    #[cfg(target_os = "linux")]
    impl ProcessHandle {
        fn open(identity: Identity) -> Option<Self> {
            use std::os::fd::FromRawFd;
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0u32) };
            if raw < 0 {
                return None;
            }
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
            // Open the stable kernel handle before checking identity again.
            // Any subsequent PID recycling cannot retarget pidfd_send_signal.
            if inspect(identity.pid)? != identity {
                return None;
            }
            Some(Self { fd })
        }

        fn signal(&self, signal: libc::c_int) -> bool {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                ) == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    struct ProcessHandle {
        identity: Identity,
    }

    #[cfg(not(target_os = "linux"))]
    impl ProcessHandle {
        fn open(identity: Identity) -> Option<Self> {
            (inspect(identity.pid)? == identity).then_some(Self { identity })
        }

        fn signal(&self, signal: libc::c_int) -> bool {
            // Darwin has no pidfd signal API. Recheck session, uid and BSD start
            // time immediately before every signal, including after SIGSTOP.
            // Unlike Linux this cannot eliminate a concurrent external kill /
            // PID reuse between the final check and kill(2). Never signal a
            // stored PID or a process group without this identity check.
            if inspect(self.identity.pid) == Some(self.identity) {
                unsafe {
                    libc::kill(self.identity.pid, signal) == 0
                        || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                }
            } else {
                true // The original process already exited; never signal its replacement.
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn inspect(pid: libc::pid_t) -> Option<Identity> {
        use std::os::unix::fs::MetadataExt;
        let base = std::path::PathBuf::from(format!("/proc/{pid}"));
        let uid = std::fs::metadata(&base).ok()?.uid();
        let stat = std::fs::read_to_string(base.join("stat")).ok()?;
        // comm is parenthesized and can itself contain spaces or parentheses.
        let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
        Some(Identity {
            pid,
            session: fields.get(3)?.parse().ok()?,
            uid,
            born: fields.get(19)?.parse().ok()?,
        })
    }

    #[cfg(target_os = "linux")]
    fn process_ids() -> Result<Vec<libc::pid_t>> {
        Ok(std::fs::read_dir("/proc")?
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect())
    }

    #[cfg(target_os = "macos")]
    fn inspect(pid: libc::pid_t) -> Option<Identity> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as libc::c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let session = unsafe { libc::getsid(pid) };
        if session < 0 {
            return None;
        }
        Some(Identity {
            pid,
            session,
            uid: info.pbi_uid,
            born: (u128::from(info.pbi_start_tvsec) << 64) | u128::from(info.pbi_start_tvusec),
        })
    }

    #[cfg(target_os = "macos")]
    fn process_ids() -> Result<Vec<libc::pid_t>> {
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        anyhow::ensure!(
            count >= 0,
            "proc_listallpids: {}",
            std::io::Error::last_os_error()
        );
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut pids = vec![0; count as usize + 256];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()).min(i32::MAX as usize) as i32;
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        anyhow::ensure!(
            count >= 0,
            "proc_listallpids: {}",
            std::io::Error::last_os_error()
        );
        pids.truncate((count.max(0) as usize).min(pids.len()));
        pids.retain(|pid| *pid > 1);
        Ok(pids)
    }

    // Other Unix targets fail closed at attach rather than owning the entire
    // calling shell session or guessing how that OS exposes process identities.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn inspect(_pid: libc::pid_t) -> Option<Identity> {
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn process_ids() -> Result<Vec<libc::pid_t>> {
        Ok(Vec::new())
    }
}

#[cfg(all(test, unix))]
#[path = "process_tree_terminal_tests.rs"]
mod terminal_tests;

#[cfg(unix)]
pub struct ProcessTree(u32);
#[cfg(unix)]
impl ProcessTree {
    pub fn attach(id: u32) -> anyhow::Result<Self> {
        Ok(Self(id))
    }
}
#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
