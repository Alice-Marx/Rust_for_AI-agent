//! Real PTY jobs exercise session cleanup, rather than only the shell's PGID.
use super::TerminalProcessTree;
use crate::desktop_terminal::{TerminalCommand, TerminalSession};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn running(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // A killed grandchild may await init's reaper briefly. Zombies cannot
        // continue work and are already terminated for this regression.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        return stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().next())
            .is_some_and(|state| state != "Z");
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let bytes = std::mem::size_of_val(&info) as i32;
        let count = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                bytes,
            )
        };
        return count == bytes && info.pbi_status != 5; // Darwin SZOMB.
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    true
}

#[test]
fn desktop_terminal_refuses_the_callers_session() {
    assert!(TerminalProcessTree::attach(std::process::id()).is_err());
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn desktop_terminal_closing_stops_separate_background_job_groups() {
    verify_job_cleanup(false);
    verify_job_cleanup(true);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_job_cleanup(shell_exits_first: bool) {
    struct UnrelatedProcess(std::process::Child);
    impl Drop for UnrelatedProcess {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let mut unrelated = UnrelatedProcess(
        std::process::Command::new("/bin/sleep")
            .arg("120")
            .spawn()
            .unwrap(),
    );
    let workspace = tempfile::tempdir().unwrap();
    let pid_file = workspace.path().join("jobs.txt");
    let mut terminal = TerminalSession::spawn(
        TerminalCommand {
            program: PathBuf::from("/bin/bash"),
            args: vec![
                "--noprofile".into(),
                "--norc".into(),
                "+H".into(),
                "-i".into(),
            ],
            cwd: workspace.path().into(),
            env: BTreeMap::new(),
            title: "session cleanup regression".into(),
        },
        24,
        110,
    )
    .unwrap();
    let leader = terminal.process_id().unwrap() as libc::pid_t;
    let started = Instant::now();
    while terminal.screen_text().trim().is_empty() && started.elapsed() < Duration::from_secs(10) {
        terminal.poll();
        thread::sleep(Duration::from_millis(10));
    }
    // Interactive bash creates a distinct PGID for each background job. Both
    // ignore SIGHUP, so closing the PTY or just killing the shell is insufficient.
    terminal.send_input(b"(trap '' HUP; : > job1.ready; exec /bin/sleep 120) & job_one=$!; (trap '' HUP; : > job2.ready; exec /bin/sleep 120) & job_two=$!; printf '%s %s\\n' \"$job_one\" \"$job_two\" > jobs.txt\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let jobs: Vec<libc::pid_t> = loop {
        terminal.poll();
        if let Ok(value) = std::fs::read_to_string(&pid_file) {
            let pids: Vec<_> = value
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if pids.len() == 2
                && workspace.path().join("job1.ready").exists()
                && workspace.path().join("job2.ready").exists()
            {
                break pids;
            }
        }
        assert!(
            Instant::now() < deadline,
            "PTY did not create jobs: {}",
            terminal.screen_text()
        );
        thread::sleep(Duration::from_millis(10));
    };
    for pid in &jobs {
        assert_eq!(unsafe { libc::getsid(*pid) }, leader);
        assert_ne!(
            unsafe { libc::getpgid(*pid) },
            leader,
            "fixture must use shell job control"
        );
    }
    let stop = Instant::now();
    if shell_exits_first {
        terminal.send_input(b"exit\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(
            terminal.status(),
            crate::desktop_terminal::TerminalStatus::Running
        ) && Instant::now() < deadline
        {
            terminal.poll();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !matches!(
                terminal.status(),
                crate::desktop_terminal::TerminalStatus::Running
            ),
            "shell did not exit"
        );
    } else {
        terminal.stop();
    }
    assert!(
        stop.elapsed() < Duration::from_secs(2),
        "session cleanup blocked the UI"
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    while jobs.iter().any(|pid| running(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        jobs.iter().all(|pid| !running(*pid)),
        "background terminal jobs survived: {jobs:?}"
    );
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "cleanup killed a process in an unrelated session"
    );
    // The test runner remains in its unrelated OS session throughout cleanup.
    assert_ne!(unsafe { libc::getsid(0) }, leader);
}
