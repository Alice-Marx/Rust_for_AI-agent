//! Desktop-owned launch adapters. Upstream CLIs keep their own authentication,
//! tools, configuration, and release lifecycle; no upstream source is modified.
use anyhow::{bail, Context, Result};
#[cfg(any(windows, test))]
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CliProfile {
    pub id: String,
    pub name: String,
    /// Executable name on PATH or an absolute path. For source checkouts, use
    /// the interpreter here (node/python/uv) and put its entry point in args.
    pub executable: String,
    pub args: Vec<String>,
    /// Arguments appended after args during the bounded version probe.
    pub version_args: Vec<String>,
    pub enabled: bool,
    pub install_hint: String,
    /// Required local build artifacts/dependency environments for checkout presets.
    pub required_paths: Vec<PathBuf>,
}

impl Default for CliProfile {
    fn default() -> Self {
        Self {
            id: "custom".into(),
            name: "自定义 CLI".into(),
            executable: String::new(),
            args: Vec::new(),
            version_args: vec!["--version".into()],
            enabled: true,
            install_hint: "填写已安装的程序路径；Node/Python 项目可填写解释器及入口文件参数".into(),
            required_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliStatus {
    pub id: String,
    pub name: String,
    pub executable: Option<PathBuf>,
    pub version: Option<String>,
    pub error: Option<String>,
}

/// A concrete argv-based launch contract, shared by external terminals and PTYs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub label: String,
}

pub fn default_cli_profiles() -> Vec<CliProfile> {
    [
        ("codex", "Codex", "codex", "npm install -g @openai/codex"),
        (
            "claude",
            "Claude Code",
            "claude",
            "使用 Anthropic 官方安装程序安装 Claude Code",
        ),
        (
            "kimi-cli",
            "Kimi CLI · Python",
            "kimi-cli",
            "uv tool install kimi-cli",
        ),
        (
            "kimi-code",
            "Kimi Code · Node",
            "kimi",
            "npm install -g @moonshot-ai/kimi-code；注意与 Python kimi 命令同名",
        ),
        (
            "deepseek",
            "DeepSeek Harness",
            "dsh",
            "安装 @deepseek-ai/dsh，或指定已构建 checkout 的 Node 入口",
        ),
        (
            "wonderland",
            "Wonderland CLI",
            "wonderland-cli",
            "npm install -g rust-ai-wonderland-cli",
        ),
    ]
    .into_iter()
    .map(|(id, name, executable, hint)| CliProfile {
        id: id.into(),
        name: name.into(),
        executable: executable.into(),
        install_hint: hint.into(),
        ..CliProfile::default()
    })
    .collect()
}

/// Opt-in presets for existing source checkouts. These execute local build
/// artifacts in the user's workspace; they never build, install, or update them.
/// `root` is the directory containing codex/, claude-code/, kimi-cli/, etc.
pub fn checkout_cli_profiles(root: &Path) -> Vec<CliProfile> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        env::current_dir().unwrap_or_default().join(root)
    };
    let executable_suffix = if cfg!(windows) { ".exe" } else { "" };
    let codex = root.join(format!(
        "codex/codex-rs/target/release/codex{executable_suffix}"
    ));
    let mut profiles = vec![CliProfile {
        id: "local-codex".into(),
        name: "本地 Codex · Rust 构建".into(),
        executable: codex.to_string_lossy().into_owned(),
        required_paths: vec![codex],
        install_hint: "请先按照 Codex 仓库构建说明生成 release 可执行文件；也可修改为实际构建路径"
            .into(),
        ..CliProfile::default()
    }];
    for (id, name, entry, hint) in [
        (
            "local-claude",
            "本地 Claude Code Best · 分支",
            "claude-code/dist/cli-node.js",
            "此 checkout 是 claude-code-best 分支；请按其说明使用 Bun 构建 dist/cli-node.js",
        ),
        (
            "local-kimi-code",
            "本地 Kimi Code · Node 构建",
            "kimi-code/apps/kimi-code/dist/main.mjs",
            "请按 Kimi Code 仓库说明构建 apps/kimi-code/dist/main.mjs",
        ),
        (
            "local-deepseek",
            "本地 DeepSeek Harness · Node 构建",
            "deepseek-harness/apps/cli/lib/bin.js",
            "请按 DeepSeek Harness 仓库说明构建 apps/cli/lib/bin.js",
        ),
    ] {
        let entry = root.join(entry);
        profiles.push(CliProfile {
            id: id.into(),
            name: name.into(),
            executable: "node".into(),
            args: vec![entry.to_string_lossy().into_owned()],
            required_paths: vec![entry],
            install_hint: hint.into(),
            ..CliProfile::default()
        });
    }
    let kimi = root.join("kimi-cli");
    let venv = kimi.join(".venv");
    let local_kimi = venv.join(if cfg!(windows) {
        "Scripts/kimi-cli.exe"
    } else {
        "bin/kimi-cli"
    });
    let (executable, args, required_paths) = if local_kimi.is_file() {
        (
            local_kimi.to_string_lossy().into_owned(),
            Vec::new(),
            vec![local_kimi],
        )
    } else {
        (
            "uv".into(),
            vec![
                "--offline".into(),
                "run".into(),
                "--project".into(),
                kimi.to_string_lossy().into_owned(),
                "--no-sync".into(),
                "--frozen".into(),
                "kimi".into(),
            ],
            vec![venv, kimi.join("pyproject.toml")],
        )
    };
    profiles.push(CliProfile {
        id: "local-kimi-cli".into(), name: "本地 Kimi CLI · Python 环境".into(),
        executable, args, required_paths,
        install_hint: "请先按 Kimi CLI 仓库说明准备 .venv；桌面仅使用现有环境，uv 以 offline/no-sync/frozen 启动".into(),
        ..CliProfile::default()
    });
    profiles
}

fn validate_profile(profile: &CliProfile) -> Result<()> {
    anyhow::ensure!(profile.enabled, "此 CLI 已禁用");
    for path in &profile.required_paths {
        anyhow::ensure!(
            path.exists(),
            "缺少本地构建或环境：{}。{}",
            path.display(),
            profile.install_hint
        );
    }
    // An interpreter on PATH is insufficient when its explicitly selected entry
    // point does not exist. This also applies to user-created custom profiles.
    for argument in &profile.args {
        let path = Path::new(argument);
        if path.is_absolute()
            && path.extension().is_some_and(|extension| {
                matches!(
                    extension.to_string_lossy().to_ascii_lowercase().as_str(),
                    "js" | "mjs" | "cjs" | "py" | "ts" | "ps1"
                )
            })
        {
            anyhow::ensure!(path.is_file(), "CLI 入口文件不存在：{}", path.display());
        }
    }
    Ok(())
}

/// Does filesystem discovery and a bounded child-process probe. Call from a
/// worker thread, never from an egui update callback.
pub fn detect_cli(profile: &CliProfile) -> CliStatus {
    let mut status = CliStatus {
        id: profile.id.clone(),
        name: profile.name.clone(),
        executable: None,
        version: None,
        error: None,
    };
    let result = (|| -> Result<String> {
        validate_profile(profile)?;
        let executable = find_executable(&profile.executable)
            .with_context(|| format!("未找到 {}。{}", profile.executable, profile.install_hint))?;
        status.executable = Some(executable.clone());
        if profile.version_args.is_empty() {
            return Ok("已找到程序（未执行版本检查）".into());
        }
        let mut args = profile.args.clone();
        args.extend(profile.version_args.iter().cloned());
        let spec = normalize_launch(executable, args, env::temp_dir(), profile.name.clone())?;
        let version = probe_version(&spec, Duration::from_secs(5))?;
        validate_version_identity(profile, &version)?;
        Ok(version)
    })();
    match result {
        Ok(version) => status.version = Some(version),
        Err(error) => status.error = Some(error.to_string()),
    }
    status
}

pub fn prepare_cli(profile: &CliProfile, workspace: &Path) -> Result<LaunchSpec> {
    validate_profile(profile)?;
    let cwd = validated_workspace(workspace)?;
    let executable = find_executable(&profile.executable)
        .with_context(|| format!("未找到 {}。{}", profile.executable, profile.install_hint))?;
    // Both Kimi implementations install `kimi`. The detection banner alone is
    // insufficient: PATH may have changed, and a launch is allowed before the
    // background scan completes. Recheck this ambiguous builtin at launch.
    if profile.id == "kimi-code" {
        let mut version_args = profile.args.clone();
        version_args.push("--version".into());
        let version_spec = normalize_launch(
            executable.clone(),
            version_args,
            env::temp_dir(),
            profile.name.clone(),
        )?;
        validate_version_identity(
            profile,
            &probe_version(&version_spec, Duration::from_secs(5))?,
        )?;
    }
    normalize_launch(executable, profile.args.clone(), cwd, profile.name.clone())
}

fn validate_version_identity(profile: &CliProfile, version: &str) -> Result<()> {
    anyhow::ensure!(
        !(profile.id == "kimi-code" && version.trim_start().starts_with("kimi, version ")),
        "当前 kimi 命令属于 Python Kimi CLI；请为 Kimi Code · Node 指定独立安装路径"
    );
    Ok(())
}

pub fn prepare_shell(workspace: &Path) -> Result<LaunchSpec> {
    let cwd = validated_workspace(workspace)?;
    #[cfg(windows)]
    let (executable, args) = (powershell()?, vec!["-NoLogo".into()]);
    #[cfg(unix)]
    let (executable, args) = (
        env::var("SHELL")
            .ok()
            .and_then(|s| find_executable(&s))
            .or_else(|| find_executable("bash"))
            .or_else(|| find_executable("sh"))
            .context("未找到可用的 shell")?,
        vec!["-l".into()],
    );
    Ok(LaunchSpec {
        executable,
        args,
        cwd,
        env: BTreeMap::new(),
        label: "终端".into(),
    })
}

fn validated_workspace(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(!path.as_os_str().is_empty(), "请先选择工作目录");
    anyhow::ensure!(path.is_dir(), "工作目录不存在：{}", path.display());
    // Keep the ordinary Windows path, rather than adding a verbatim \\?\ prefix
    // which some upstream CLIs and shells do not understand.
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    })
}

/// Discover only existing local executables. Never invokes npm/npx/uv installers.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\0') {
        return None;
    }
    let path = Path::new(name);
    if path.is_absolute() || path.components().count() > 1 {
        return resolve_in_dirs(name, &[], &executable_extensions());
    }
    let mut dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|value| {
            env::split_paths(&value)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let home = PathBuf::from(home);
        dirs.extend([
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            home.join(".bun/bin"),
        ]);
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = env::var_os("APPDATA") {
            dirs.push(PathBuf::from(appdata).join("npm"));
        }
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            let local = PathBuf::from(local);
            dirs.push(local.join("Microsoft/WindowsApps"));
            let codex_bin = local.join("OpenAI/Codex/bin");
            if let Ok(entries) = std::fs::read_dir(codex_bin) {
                let mut versions: Vec<_> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                versions.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
                dirs.extend(versions.into_iter().rev());
            }
        }
    }
    resolve_in_dirs(name, &dirs, &executable_extensions())
}

fn executable_extensions() -> Vec<String> {
    if cfg!(windows) {
        let value = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut extensions: Vec<_> = value
            .split(';')
            .filter(|e| e.starts_with('.') && !e.contains(['/', '\\']))
            .map(str::to_owned)
            .collect();
        // npm supplies a PowerShell shim that avoids cmd.exe argument expansion.
        extensions.push(".ps1".into());
        extensions
    } else {
        vec![String::new()]
    }
}

fn resolve_in_dirs(name: &str, dirs: &[PathBuf], extensions: &[String]) -> Option<PathBuf> {
    let path = Path::new(name);
    let explicit = path.is_absolute() || path.components().count() > 1;
    let bases = if explicit {
        vec![path.to_path_buf()]
    } else {
        dirs.iter().map(|p| p.join(path)).collect()
    };
    for base in bases {
        let mut candidates = Vec::new();
        if base.extension().is_some() || !cfg!(windows) {
            candidates.push(base.clone());
        }
        if base.extension().is_none() {
            candidates.extend(extensions.iter().map(|ext| {
                let mut value: OsString = base.as_os_str().to_owned();
                value.push(ext);
                PathBuf::from(value)
            }));
        }
        for candidate in candidates {
            if executable_file(&candidate) {
                return if candidate.is_absolute() {
                    Some(candidate)
                } else {
                    env::current_dir().ok().map(|cwd| cwd.join(candidate))
                };
            }
        }
    }
    None
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
}

#[allow(unused_mut)] // Only Windows shims replace the executable and argv.
fn normalize_launch(
    mut executable: PathBuf,
    mut args: Vec<String>,
    cwd: PathBuf,
    label: String,
) -> Result<LaunchSpec> {
    anyhow::ensure!(
        !args.iter().any(|arg| arg.contains('\0')),
        "CLI 参数不能包含空字符"
    );
    #[cfg(windows)]
    {
        let mut extension = executable
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "cmd" | "bat") && executable.with_extension("ps1").is_file()
        {
            executable.set_extension("ps1");
            extension = "ps1".into();
        }
        if extension == "ps1" {
            let script = format!(
                "& {} {}; if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }}",
                powershell_quote(&executable.to_string_lossy()),
                args.iter()
                    .map(|arg| powershell_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            args = encoded_powershell_args(&script);
            executable = powershell()?;
        } else if matches!(extension.as_str(), "cmd" | "bat") {
            // cmd expands % even inside double quotes; reject ambiguous input
            // rather than turn a custom argument into shell code. The normal
            // npm case uses its adjacent PowerShell shim above.
            let line = batch_command_line(&executable, &args)?;
            let script = format!("$p = New-Object System.Diagnostics.ProcessStartInfo; $p.FileName = $env:ComSpec; $p.Arguments = {}; $p.UseShellExecute = $false; $c = [System.Diagnostics.Process]::Start($p); $c.WaitForExit(); exit $c.ExitCode", powershell_quote(&format!("/d /v:off /s /c {line}")));
            args = encoded_powershell_args(&script);
            executable = powershell()?;
        }
    }
    Ok(LaunchSpec {
        executable,
        args,
        cwd,
        env: BTreeMap::new(),
        label,
    })
}

#[cfg(windows)]
fn powershell() -> Result<PathBuf> {
    find_executable("pwsh")
        .or_else(|| find_executable("powershell"))
        .or_else(|| {
            env::var_os("SystemRoot")
                .map(|root| {
                    PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe")
                })
                .filter(|p| p.is_file())
        })
        .context("未找到 PowerShell")
}

pub fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(any(windows, test))]
fn encoded_powershell_args(script: &str) -> Vec<String> {
    let script = format!("[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding; $OutputEncoding = [Console]::OutputEncoding; {script}");
    let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    vec![
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-ExecutionPolicy".into(),
        "Bypass".into(),
        "-EncodedCommand".into(),
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ]
}

#[cfg(any(windows, test))]
fn batch_command_line(executable: &Path, args: &[String]) -> Result<String> {
    let values: Vec<_> = std::iter::once(executable.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .collect();
    anyhow::ensure!(
        !values
            .iter()
            .any(|value| value.contains(['%', '"', '\r', '\n', '\0'])),
        "批处理 CLI 的路径或参数包含不支持的引号、百分号或换行；请改用 .ps1、.exe 或解释器入口"
    );
    Ok(format!(
        "\"{}\"",
        values
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

/// Windows native argv quoting (CommandLineToArgvW/CRT), not cmd shell syntax.
#[cfg(any(windows, test))]
fn windows_argv_quote(value: &str) -> String {
    let mut output = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
        } else {
            output.extend(std::iter::repeat_n(
                '\\',
                if character == '"' {
                    slashes * 2 + 1
                } else {
                    slashes
                },
            ));
            output.push(character);
            slashes = 0;
        }
    }
    output.extend(std::iter::repeat_n('\\', slashes * 2));
    output.push('"');
    output
}

/// Explicit user action: open a new visible OS terminal, independent of the app.
pub fn launch_external(spec: &LaunchSpec) -> Result<()> {
    validated_workspace(&spec.cwd)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut script = format!(
            "Set-Location -LiteralPath {}; ",
            powershell_quote(&spec.cwd.to_string_lossy())
        );
        for (key, value) in &spec.env {
            script.push_str(&format!(
                "[Environment]::SetEnvironmentVariable({}, {}, 'Process'); ",
                powershell_quote(key),
                powershell_quote(value)
            ));
        }
        script.push_str(&format!(
            "$p = New-Object System.Diagnostics.ProcessStartInfo; $p.FileName = {}; $p.Arguments = {}; $p.UseShellExecute = $false; $c = [System.Diagnostics.Process]::Start($p); $c.WaitForExit(); Write-Host ''; Write-Host '进程已结束。可以继续使用此终端。'",
            powershell_quote(&spec.executable.to_string_lossy()),
            powershell_quote(&spec.args.iter().map(|arg| windows_argv_quote(arg)).collect::<Vec<_>>().join(" "))
        ));
        let mut arguments = encoded_powershell_args(&script);
        arguments.insert(1, "-NoExit".into());
        Command::new(powershell()?)
            .args(arguments)
            .current_dir(&spec.cwd)
            .creation_flags(0x00000010) // CREATE_NEW_CONSOLE: this action opens a visible terminal.
            .spawn()
            .context("打开终端失败")?;
    }
    #[cfg(target_os = "macos")]
    {
        let line = posix_launch_line(spec);
        let apple_string = serde_json::to_string(&line)?;
        Command::new("osascript")
            .args([
                "-e",
                &format!("tell application \"Terminal\" to do script {apple_string}"),
                "-e",
                "tell application \"Terminal\" to activate",
            ])
            .spawn()
            .context("打开 Terminal 失败")?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let line = posix_launch_line(spec);
        let terminals = [
            ("x-terminal-emulator", "-e"),
            ("gnome-terminal", "--"),
            ("konsole", "-e"),
            ("xfce4-terminal", "-x"),
            ("xterm", "-e"),
        ];
        let (terminal, separator) = terminals
            .into_iter()
            .find_map(|(name, sep)| find_executable(name).map(|exe| (exe, sep)))
            .context("未找到终端程序；可使用应用内终端")?;
        Command::new(terminal)
            .args([separator, "sh", "-lc", &line])
            .current_dir(&spec.cwd)
            .spawn()
            .context("打开终端失败")?;
    }
    Ok(())
}

#[cfg(unix)]
fn posix_launch_line(spec: &LaunchSpec) -> String {
    let mut values = vec!["env".into()];
    values.extend(
        spec.env
            .iter()
            .map(|(key, value)| posix_quote(&format!("{key}={value}"))),
    );
    values.push(posix_quote(&spec.executable.to_string_lossy()));
    values.extend(spec.args.iter().map(|arg| posix_quote(arg)));
    format!(
        "cd {} && {}; printf '\\n进程已结束。\\n'; exec \"${{SHELL:-/bin/sh}}\" -l",
        posix_quote(&spec.cwd.to_string_lossy()),
        values.join(" ")
    )
}

pub fn open_workspace(workspace: &Path) -> Result<()> {
    let path = validated_workspace(workspace)?;
    #[cfg(windows)]
    let executable = "explorer.exe";
    #[cfg(target_os = "macos")]
    let executable = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let executable = "xdg-open";
    Command::new(executable)
        .arg(path)
        .spawn()
        .context("打开工作目录失败")?;
    Ok(())
}

fn probe_version(spec: &LaunchSpec, timeout: Duration) -> Result<String> {
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("无法启动 CLI 版本检查")?;
    let process_tree = match crate::process_tree::ProcessTree::attach(child.id()) {
        Ok(tree) => Some(tree),
        // A very short version command may exit before its job is attached.
        Err(_) if child.try_wait()?.is_some() => None,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.context("无法限制 CLI 版本检查进程的生命周期"));
        }
    };
    let (sender, receiver) = mpsc::channel();
    for mut stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0u8; 2048];
            while let Ok(count) = stream.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let keep = count.min(8192usize.saturating_sub(output.len()));
                output.extend_from_slice(&buffer[..keep]);
            }
            let _ = sender.send(output);
        });
    }
    drop(sender);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            drop(process_tree);
            let _ = child.wait();
            bail!("CLI 版本检查超时（{} 秒）；仍可尝试启动", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(25));
    };
    drop(process_tree);
    let mut output = String::new();
    for _ in 0..2 {
        if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(250)) {
            output.push_str(&String::from_utf8_lossy(&bytes));
            output.push('\n');
        }
    }
    let output: String = output
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(1024)
        .collect();
    anyhow::ensure!(
        status.success(),
        "CLI 版本检查失败（{status}）：{}",
        output.trim()
    );
    Ok(if output.trim().is_empty() {
        "已安装".into()
    } else {
        output.trim().to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_literals_preserve_unicode_quotes_and_metacharacters() {
        let value = "项目 O'Brien; $(echo bad) & 50%";
        assert_eq!(
            powershell_quote(value),
            "'项目 O''Brien; $(echo bad) & 50%'"
        );
        assert_eq!(
            posix_quote(value),
            "'项目 O'\"'\"'Brien; $(echo bad) & 50%'"
        );
        assert_eq!(powershell_quote(""), "''");
    }

    #[test]
    fn windows_argv_preserves_empty_quotes_and_trailing_backslashes() {
        assert_eq!(windows_argv_quote(""), "\"\"");
        assert_eq!(windows_argv_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(
            windows_argv_quote("C:\\项目 space\\"),
            "\"C:\\项目 space\\\\\""
        );
        assert_eq!(windows_argv_quote("$()&;%"), "\"$()&;%\"");
    }

    #[test]
    fn batch_quote_guards_expansion_but_handles_spaces_and_unicode() {
        assert_eq!(
            batch_command_line(
                Path::new("C:\\工具 目录\\cli.cmd"),
                &["a&b".into(), "".into()]
            )
            .unwrap(),
            "\"\"C:\\工具 目录\\cli.cmd\" \"a&b\" \"\"\""
        );
        for value in ["%PATH%", "\" & calc", "x\r\ny"] {
            assert!(batch_command_line(Path::new("cli.cmd"), &[value.into()]).is_err());
        }
    }

    #[test]
    fn discovery_respects_directory_order_extensions_and_explicit_paths() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let extension = if cfg!(windows) { ".exe" } else { "" };
        let expected = first.join(format!("fixture{extension}"));
        std::fs::write(&expected, "fixture").unwrap();
        std::fs::write(second.join(format!("fixture{extension}")), "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&expected, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            resolve_in_dirs("fixture", &[first, second], &[extension.into()]),
            Some(expected.clone())
        );
        assert_eq!(
            resolve_in_dirs(&expected.to_string_lossy(), &[], &[extension.into()]),
            Some(expected)
        );
        assert!(
            resolve_in_dirs("missing", &[directory.path().into()], &[extension.into()]).is_none()
        );
    }

    #[test]
    fn invalid_workspace_is_rejected_before_spawn() {
        assert!(prepare_shell(Path::new("")).is_err());
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-a-directory");
        std::fs::write(&file, "data").unwrap();
        assert!(prepare_shell(&file).is_err());
        assert!(prepare_cli(&CliProfile::default(), &directory.path().join("missing")).is_err());
    }

    #[test]
    fn checkout_and_custom_profiles_require_real_entrypoints() {
        let directory = tempfile::tempdir().unwrap();
        let profiles = checkout_cli_profiles(directory.path());
        for profile in profiles {
            assert!(validate_profile(&profile)
                .unwrap_err()
                .to_string()
                .contains("缺少本地构建或环境"));
        }
        let missing_entry = directory.path().join("missing.mjs");
        let mut profile = CliProfile {
            executable: "node".into(),
            args: vec![missing_entry.to_string_lossy().into_owned()],
            ..CliProfile::default()
        };
        assert!(validate_profile(&profile).is_err());
        std::fs::write(&missing_entry, "console.log('local');").unwrap();
        assert!(validate_profile(&profile).is_ok());
        profile.enabled = false;
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn relative_path_discovery_becomes_absolute_before_changing_workspace() {
        let cwd = env::current_dir().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("cli-path-test-")
            .tempdir_in(&cwd)
            .unwrap();
        let file = directory.path().join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        std::fs::write(&file, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let relative = file.strip_prefix(&cwd).unwrap();
        let resolved =
            resolve_in_dirs(&relative.to_string_lossy(), &[], &executable_extensions()).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, file);
    }

    #[cfg(windows)]
    #[test]
    fn kimi_node_launch_rejects_python_command_even_before_detection() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("kimi.ps1");
        std::fs::write(&script, "Write-Output 'kimi, version 1.0.0'").unwrap();
        let profile = CliProfile {
            id: "kimi-code".into(),
            executable: script.to_string_lossy().into_owned(),
            ..CliProfile::default()
        };
        let error = prepare_cli(&profile, directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Python Kimi CLI"));
    }

    #[test]
    fn encoded_powershell_is_utf16_and_keeps_literal_input() {
        let script = "Write-Output '中文 $HOME'";
        let arguments = encoded_powershell_args(script);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(arguments.last().unwrap())
            .unwrap();
        let words: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert!(String::from_utf16(&words).unwrap().ends_with(script));
    }

    #[cfg(windows)]
    #[test]
    fn npm_powershell_shim_preserves_argument_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let batch = directory.path().join("工具 & cli.cmd");
        std::fs::write(&batch, "@echo wrong-shim").unwrap();
        std::fs::write(
            batch.with_extension("ps1"),
            "$args | ConvertTo-Json -Compress",
        )
        .unwrap();
        let args = vec![
            "a b".into(),
            "O'Brien".into(),
            "$(bad)&x;100%".into(),
            "中文".into(),
        ];
        let spec =
            normalize_launch(batch, args.clone(), directory.path().into(), "test".into()).unwrap();
        let output = probe_version(&spec, Duration::from_secs(5)).unwrap();
        assert_eq!(serde_json::from_str::<Vec<String>>(&output).unwrap(), args);
    }

    #[cfg(windows)]
    #[test]
    fn batch_only_launcher_preserves_quoted_metacharacters() {
        let directory = tempfile::tempdir().unwrap();
        let batch = directory.path().join("batch with spaces.cmd");
        // The fixture retains quotes before expansion: cmd otherwise treats an
        // ampersand introduced by the batch file itself as command syntax.
        std::fs::write(&batch, "@echo off\r\necho %1\r\n").unwrap();
        let spec = normalize_launch(
            batch,
            vec!["a&b".into()],
            directory.path().into(),
            "test".into(),
        )
        .unwrap();
        let output = probe_version(&spec, Duration::from_secs(5)).unwrap();
        assert_eq!(output.trim(), "\"a&b\"");
    }

    #[cfg(windows)]
    #[test]
    fn version_probe_times_out_hung_program() {
        let spec = LaunchSpec {
            executable: powershell().unwrap(),
            args: encoded_powershell_args("Start-Sleep -Seconds 10"),
            cwd: env::temp_dir(),
            env: BTreeMap::new(),
            label: "test".into(),
        };
        let started = Instant::now();
        assert!(probe_version(&spec, Duration::from_millis(100))
            .unwrap_err()
            .to_string()
            .contains("超时"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    #[ignore = "requires a user-installed CLI; set WONDERLAND_TEST_CLI_ID"]
    fn installed_cli_version_probe() {
        let selected = env::var("WONDERLAND_TEST_CLI_ID").expect("set WONDERLAND_TEST_CLI_ID");
        let profile = default_cli_profiles()
            .into_iter()
            .find(|profile| profile.id == selected)
            .expect("builtin profile id");
        let status = detect_cli(&profile);
        assert!(
            status.executable.is_some(),
            "{}",
            status.error.as_deref().unwrap_or("missing executable")
        );
        assert!(status.error.is_none(), "{:?}", status.error);
        assert!(status.version.is_some());
        println!("{}: {}", status.name, status.version.unwrap());
    }
}
