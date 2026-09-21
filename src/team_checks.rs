//! Explicit user-supplied verification commands, with bounded output and tree cleanup.
use crate::{
    process_tree::ProcessTree,
    team_store::{CheckResult, CheckSpec},
};
use anyhow::{Context, Result};
use std::{path::Path, process::Stdio, time::Instant};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::watch,
    time::{Duration, Instant as Deadline},
};

async fn drain(mut reader: impl AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let take = n.min(24_000usize.saturating_sub(kept.len()));
                kept.extend_from_slice(&buf[..take]);
                truncated |= take < n;
            }
        }
    }
    let mut text = String::from_utf8_lossy(&kept).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    text
}

pub async fn run_checks(
    checks: &[CheckSpec],
    cwd: &Path,
    cancel: watch::Receiver<bool>,
    deadline: Deadline,
) -> Result<Vec<CheckResult>> {
    let mut results = Vec::new();
    for (index, spec) in checks.iter().enumerate() {
        if *cancel.borrow() || Deadline::now() >= deadline {
            results.push(CheckResult {
                check_index: index,
                success: false,
                exit_code: None,
                timed_out: Deadline::now() >= deadline,
                output: "Not started: collaboration cancelled or deadline exceeded".into(),
                duration_ms: 0,
            });
            continue;
        }
        results.push(
            match run_check(index, spec, cwd, cancel.clone(), deadline).await {
                Ok(result) => result,
                Err(error) => CheckResult {
                    check_index: index,
                    success: false,
                    exit_code: None,
                    timed_out: false,
                    output: format!("Verification failed: {error}")
                        .chars()
                        .take(4096)
                        .collect(),
                    duration_ms: 0,
                },
            },
        );
    }
    Ok(results)
}

async fn run_check(
    index: usize,
    spec: &CheckSpec,
    cwd: &Path,
    mut cancel: watch::Receiver<bool>,
    deadline: Deadline,
) -> Result<CheckResult> {
    crate::team_store::validate_check(spec)?;
    let start = Instant::now();
    let mut command = tokio::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(CheckResult {
                check_index: index,
                success: false,
                exit_code: None,
                timed_out: false,
                output: format!("Could not start verification executable: {error}"),
                duration_ms: start.elapsed().as_millis() as u64,
            })
        }
    };
    let tree = match ProcessTree::attach(
        child
            .id()
            .context("verification process identity unavailable")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let mut stdout = tokio::spawn(drain(
        child
            .stdout
            .take()
            .context("verification stdout unavailable")?,
    ));
    let mut stderr = tokio::spawn(drain(
        child
            .stderr
            .take()
            .context("verification stderr unavailable")?,
    ));
    let end = deadline.min(Deadline::now() + Duration::from_secs(spec.timeout_secs));
    let mut timed_out = false;
    let status = tokio::select! {
        status = child.wait() => Some(status?),
        _ = tokio::time::sleep_until(end) => { timed_out = true; None },
        _ = cancel.changed() => None,
    };
    // Close containment before awaiting readers: descendants may retain pipe handles.
    drop(tree);
    if status.is_none() {
        let _ = child.kill().await;
    }
    let out = tokio::time::timeout(Duration::from_secs(3), async {
        ((&mut stdout).await, (&mut stderr).await)
    })
    .await;
    let (mut output, captured) = match out {
        Ok((Ok(out), Ok(err))) => (format!("{out}\n{err}"), true),
        _ => {
            stdout.abort();
            stderr.abort();
            ("Verification output drain did not complete".into(), false)
        }
    };
    if output.len() > 32_000 {
        let mut end = 32_000;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push_str("\n[output truncated]");
    }
    Ok(CheckResult {
        check_index: index,
        success: captured && status.is_some_and(|s| s.success()) && !*cancel.borrow(),
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        output,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn missing_program_is_recorded_as_failure_not_success() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(false);
        let checks = [CheckSpec {
            program: "wonderland-nonexistent-test-program-8765".into(),
            args: vec![],
            timeout_secs: 5,
        }];
        let result = run_checks(
            &checks,
            dir.path(),
            rx,
            Deadline::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(!result[0].success);
        assert_eq!(result[0].exit_code, None);
    }
    #[tokio::test]
    async fn cancelled_run_does_not_launch_check() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(true);
        let checks = [CheckSpec {
            program: "git".into(),
            args: vec!["--version".into()],
            timeout_secs: 5,
        }];
        let results = run_checks(
            &checks,
            dir.path(),
            rx,
            Deadline::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(!results[0].success);
        assert_eq!(results[0].exit_code, None);
        assert_eq!(results[0].duration_ms, 0);
    }
    #[tokio::test]
    async fn captures_real_exit_codes_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(false);
        let checks = [
            CheckSpec {
                program: "git".into(),
                args: vec!["--version".into()],
                timeout_secs: 5,
            },
            CheckSpec {
                program: "git".into(),
                args: vec!["--not-a-valid-git-option".into()],
                timeout_secs: 5,
            },
        ];
        let results = run_checks(
            &checks,
            dir.path(),
            rx,
            Deadline::now() + Duration::from_secs(15),
        )
        .await
        .unwrap();
        assert!(results[0].success);
        assert!(results[0].output.contains("git version"));
        assert!(!results[1].success);
        assert!(results[1].exit_code.is_some());
    }
    #[tokio::test]
    async fn deadline_keeps_completed_checks_and_marks_unstarted_checks() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(false);
        let checks = vec![
            CheckSpec {
                program: "git".into(),
                args: vec!["--version".into()],
                timeout_secs: 5
            };
            2
        ];
        let results = run_checks(
            &checks,
            dir.path(),
            rx,
            Deadline::now() - Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|r| r.timed_out && !r.success && r.duration_ms == 0));
    }
}
