//! AppContainer process creation. No network capability, a unique SID per execution,
//! only the disposable run directory granted access. Job assignment precedes ResumeThread.
use crate::sandbox::{SandboxPolicy, SandboxResult};
use anyhow::{Context, Result};
use std::{
    ffi::OsStr,
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Isolation::*, *},
    System::{JobObjects::*, Threading::*},
};

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}
fn check(ok: i32) -> Result<()> {
    anyhow::ensure!(
        ok != 0,
        "Windows sandbox: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Profile {
    name: Vec<u16>,
    sid: PSID,
}
impl Drop for Profile {
    fn drop(&mut self) {
        unsafe {
            FreeSid(self.sid);
            DeleteAppContainerProfile(self.name.as_ptr());
        }
    }
}
struct Attributes(Vec<usize>);
impl Attributes {
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}

fn grant(directory: &Path, sid: PSID) -> Result<()> {
    let path = wide(directory);
    unsafe {
        let mut descriptor = null_mut();
        let mut old_acl = null_mut();
        let result = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_acl,
            null_mut(),
            &mut descriptor,
        );
        anyhow::ensure!(result == 0, "read sandbox ACL: {result}");
        let mut entry: EXPLICIT_ACCESS_W = zeroed();
        entry.grfAccessPermissions = 0x1f01ff; // FILE_ALL_ACCESS, scoped to disposable directory only.
        entry.grfAccessMode = GRANT_ACCESS;
        entry.grfInheritance = SUB_CONTAINERS_AND_OBJECTS_INHERIT;
        entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        entry.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
        entry.Trustee.ptstrName = sid.cast();
        let mut acl = null_mut();
        let result = SetEntriesInAclW(1, &entry, old_acl, &mut acl);
        if result != 0 {
            LocalFree(descriptor);
            anyhow::bail!("build sandbox ACL: {result}");
        }
        let result = SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        );
        LocalFree(acl.cast());
        LocalFree(descriptor);
        anyhow::ensure!(result == 0, "grant sandbox directory: {result}");
    }
    Ok(())
}

fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if matches!(
            entry.file_name().to_str(),
            Some(
                "site-packages"
                    | "__pycache__"
                    | "test"
                    | "tests"
                    | "idlelib"
                    | "tkinter"
                    | "ensurepip"
            )
        ) {
            continue;
        }
        if entry.file_type()?.is_symlink() {
            continue;
        }
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dest.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn runtime(language: &str, directory: &Path) -> Result<PathBuf> {
    let exe = match language {
        "python" => "python.exe",
        "node" | "javascript" => "node.exe",
        _ => anyhow::bail!(
            "AppContainer supports python and node; unsupported interpreter is refused"
        ),
    };
    let path = std::env::var_os("PATH").context("PATH missing")?;
    let source = std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
        .context("sandbox interpreter not installed")?;
    let source_dir = source.parent().unwrap();
    let target = directory.join("runtime");
    std::fs::create_dir_all(&target)?;
    std::fs::copy(&source, target.join(exe))?;
    if language == "python" {
        for entry in std::fs::read_dir(source_dir)? {
            let entry = entry?;
            let extension = entry
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if entry.file_type()?.is_file() && matches!(extension.as_str(), "dll" | "zip" | "_pth")
            {
                std::fs::copy(entry.path(), target.join(entry.file_name()))?;
            }
        }
        for name in ["Lib", "DLLs"] {
            if source_dir.join(name).is_dir() {
                copy_tree(&source_dir.join(name), &target.join(name))?;
            }
        }
    }
    Ok(target.join(exe))
}

fn quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

pub fn execute(
    language: &str,
    script: &Path,
    directory: &Path,
    policy: &SandboxPolicy,
    timeout_ms: u64,
) -> Result<SandboxResult> {
    anyhow::ensure!(
        !policy.allow_network,
        "Windows AppContainer uses a no-network policy"
    );
    let program = runtime(language, directory)?;
    // SAFETY: every OS pointer refers to live, correctly sized local storage; each
    // owned process/thread/job/profile/allocation is released once by its owner.
    unsafe {
        let name = wide(format!("Wonderland.{}", uuid::Uuid::new_v4().simple()));
        let mut sid = null_mut();
        let hr = CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            name.as_ptr(),
            null(),
            0,
            &mut sid,
        );
        anyhow::ensure!(hr >= 0, "AppContainer creation failed: {hr:#x}");
        let profile = Profile { name, sid };
        grant(directory, profile.sid)?;
        let stdout = std::fs::File::create(directory.join("stdout.txt"))?;
        let stderr = std::fs::File::create(directory.join("stderr.txt"))?;
        let stdin_path = directory.join("stdin.txt");
        std::fs::write(&stdin_path, b"")?;
        let stdin = std::fs::File::open(stdin_path)?;
        let mut handles = [
            stdin.as_raw_handle(),
            stdout.as_raw_handle(),
            stderr.as_raw_handle(),
        ];
        for handle in handles {
            check(SetHandleInformation(
                handle,
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            ))
            .context("inherit stdio")?;
        }
        let mut size = 0;
        InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size);
        let mut attributes = Attributes(vec![0; size.div_ceil(size_of::<usize>())]);
        check(InitializeProcThreadAttributeList(
            attributes.ptr(),
            2,
            0,
            &mut size,
        ))
        .context("initialize process attributes")?;
        let capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: profile.sid,
            Capabilities: null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        check(UpdateProcThreadAttribute(
            attributes.ptr(),
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            (&capabilities as *const SECURITY_CAPABILITIES).cast(),
            size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        ))
        .context("set AppContainer security capabilities")?;
        check(UpdateProcThreadAttribute(
            attributes.ptr(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            size_of_val(&handles),
            null_mut(),
            null(),
        ))
        .context("set inherited handles")?;
        let mut startup: STARTUPINFOEXW = zeroed();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
        startup.lpAttributeList = attributes.ptr();
        let command = format!(
            "{} {} {}",
            quote(&program),
            if language == "python" { "-I -S -B" } else { "" },
            quote(script)
        );
        let mut command = wide(command);
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let mut environment_values = std::collections::BTreeMap::new();
        for key in [
            "SystemRoot",
            "SystemDrive",
            "WINDIR",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "ProgramData",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ComSpec",
        ] {
            if let Ok(value) = std::env::var(key) {
                environment_values.insert(key.to_string(), value);
            }
        }
        environment_values.insert("SystemRoot".into(), system);
        environment_values.insert("TEMP".into(), directory.display().to_string());
        environment_values.insert("TMP".into(), directory.display().to_string());
        let environment = wide(
            environment_values
                .iter()
                .map(|(k, v)| format!("{k}={v}\0"))
                .collect::<String>(),
        );
        let mut info: PROCESS_INFORMATION = zeroed();
        check(CreateProcessW(
            wide(&program).as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_SUSPENDED
                | CREATE_NO_WINDOW,
            environment.as_ptr().cast(),
            wide(directory).as_ptr(),
            &startup.StartupInfo,
            &mut info,
        ))
        .context("create AppContainer process")?;
        let process = Handle(info.hProcess);
        let thread = Handle(info.hThread);
        let job = Handle(CreateJobObjectW(null(), null()));
        let setup = (|| -> Result<()> {
            anyhow::ensure!(!job.0.is_null(), "sandbox job creation failed");
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.BasicLimitInformation.ActiveProcessLimit = policy.max_processes.unwrap_or(8);
            limits.JobMemoryLimit =
                (policy.memory_limit_mb.unwrap_or(512) as usize).saturating_mul(1024 * 1024);
            check(SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
            ))?;
            check(AssignProcessToJobObject(job.0, process.0))?;
            anyhow::ensure!(ResumeThread(thread.0) != u32::MAX, "sandbox resume failed");
            Ok(())
        })();
        if let Err(e) = setup {
            TerminateProcess(process.0, 1);
            return Err(e);
        }
        let started = std::time::Instant::now();
        let mut timed_out = false;
        let mut truncated = false;
        loop {
            if WaitForSingleObject(process.0, 20) == WAIT_OBJECT_0 {
                break;
            }
            timed_out = started.elapsed().as_millis() >= u128::from(timeout_ms);
            truncated = stdout.metadata()?.len() > policy.max_output_bytes as u64
                || stderr.metadata()?.len() > policy.max_output_bytes as u64;
            if timed_out || truncated {
                TerminateJobObject(job.0, 1);
                WaitForSingleObject(process.0, 5000);
                break;
            }
        }
        let mut exit = 0;
        check(GetExitCodeProcess(process.0, &mut exit))?;
        drop(job); // Kill any remaining descendants before reading and deleting files.
        drop(stdout);
        drop(stderr);
        drop(stdin);
        let read = |name: &str| -> Result<String> {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(directory.join(name))?
                .take(policy.max_output_bytes as u64)
                .read_to_end(&mut bytes)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        };
        let mut limits = vec![
            "appcontainer:filesystem-restricted".into(),
            "appcontainer:no-network".into(),
            "kill-on-close".into(),
            "env-cleared".into(),
            format!("timeout={timeout_ms}ms"),
            format!("cwd-isolated={}", directory.display()),
            format!(
                "job-object:memory={}MB",
                policy.memory_limit_mb.unwrap_or(512)
            ),
            format!(
                "job-object:max-processes={}",
                policy.max_processes.unwrap_or(8)
            ),
        ];
        if truncated {
            limits.push(format!("output-truncated={}B", policy.max_output_bytes));
        }
        let mut error = read("stderr.txt")?;
        if timed_out {
            error.push_str(&format!("\nsandbox timed out after {timeout_ms} ms"));
        }
        Ok(SandboxResult {
            stdout: read("stdout.txt")?,
            stderr: error,
            exit_code: if timed_out { None } else { Some(exit as i32) },
            timed_out,
            limits,
            isolation: "windows-appcontainer".into(),
        })
    }
}
