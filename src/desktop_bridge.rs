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

/// Published upstream identity, not an attestation of an installed executable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OfficialSource {
    pub repository_url: String,
    pub website_url: String,
    pub package_name: Option<String>,
    /// Whether this tool is published by the corresponding model vendor.
    /// Wonderland remains available manually but is not a vendor harness.
    pub model_vendor_official: bool,
}

/// Describes inspected upstream interfaces separately from implemented adapters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationCapabilities {
    pub manual_terminal: bool,
    pub structured_runner: bool,
    pub protocols: Vec<String>,
    pub sessions: bool,
    pub cancellation: bool,
    pub permissions: bool,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppMetadata {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub source: OfficialSource,
    pub capabilities: IntegrationCapabilities,
    /// Relative to the project containing the reference repositories.
    pub checkout_paths: Vec<PathBuf>,
    pub notes: String,
}

/// Catalog entries are candidates, not proof of local installation, authentication,
/// model identity, or protocol compatibility. Implemented native controls are
/// reported separately from interfaces merely inspected in upstream code.
pub fn official_apps() -> Vec<AppMetadata> {
    [
        (
            "codex",
            "Codex",
            "OpenAI",
            "openai/codex",
            "https://developers.openai.com/codex",
            Some("@openai/codex"),
            true,
            "app-server;exec-json",
            "anytool/ChatGPT/codex;codex",
            "通过官方 Codex 执行；模型与账号能力以实际安装版本为准",
        ),
        (
            "claude",
            "Claude Code",
            "Anthropic",
            "anthropics/claude-code",
            "https://code.claude.com",
            Some("@anthropic-ai/claude-code"),
            true,
            "stream-json;agent-sdk",
            "",
            "使用官方发行版；本地 claude-code-best 分支不属于官方候选",
        ),
        (
            "kimi-cli",
            "Kimi CLI · Python",
            "Moonshot AI",
            "MoonshotAI/kimi-cli",
            "https://moonshotai.github.io/kimi-cli",
            Some("kimi-cli"),
            true,
            "wire;acp",
            "anytool/kimi/kimi-cli;kimi-cli",
            "优先 Wire；与 Node 版 kimi 命令区分身份",
        ),
        (
            "kimi-code",
            "Kimi Code · Node",
            "Moonshot AI",
            "MoonshotAI/kimi-code",
            "https://github.com/MoonshotAI/kimi-code",
            Some("@moonshot-ai/kimi-code"),
            true,
            "acp;harness-sdk",
            "anytool/kimi/kimi-code;kimi-code",
            "受管 ACP 适配（kimi acp，固定 2.0.2）；与 Python kimi 命令同名，只认 npm dist/main.mjs 入口；真实握手与推理待账号验证",
        ),
        (
            "minimax",
            "MiniMax Code",
            "MiniMax",
            "MiniMax-AI/minimax-code",
            "https://github.com/MiniMax-AI/minimax-code",
            None,
            true,
            "acp",
            "anytool/minimax/minimax-code;minimax-code",
            "手动终端候选；MiniMax CLI 产品接口与 mcode 编程工具分别识别",
        ),
        (
            "mimo",
            "MiMo Code",
            "Xiaomi MiMo",
            "XiaomiMiMo/MiMo-Code",
            "https://mimo.xiaomi.com/coder",
            Some("@mimo-ai/cli"),
            true,
            "acp;http-sse",
            "anytool/mimo/MiMo-Code;MiMo-Code",
            "受管 ACP 适配（mimo acp，固定 0.1.15）；ACP 上报身份为上游 fork 的 OpenCode 名称；真实握手与推理待账号验证",
        ),
        (
            "deepseek",
            "DeepSeek Harness",
            "DeepSeek",
            "deepseek-ai/deepseek-harness",
            "https://github.com/deepseek-ai/deepseek-harness",
            Some("@deepseek-ai/dsh"),
            true,
            "acp;headless-json;sdk-jsonrpc",
            "anytool/deepseek/deepseek-harness;deepseek-harness",
            "受管任务使用已验证版本的 ACP；终端保留官方交互方式",
        ),
        (
            "zcode",
            "ZCode",
            "Z.ai",
            "zai-org/ZCode",
            "https://github.com/zai-org/ZCode",
            None,
            false,
            "zcode-v4",
            "anytool/zai/ZCode;ZCode",
            "手动终端候选；Protocol V4 需独立适配及版本验证",
        ),
        (
            "wonderland",
            "Wonderland CLI",
            "Wonderland",
            "Alice-Marx/Rust_for_AI-agent",
            "https://github.com/Alice-Marx/Rust_for_AI-agent",
            Some("rust-ai-wonderland-cli"),
            false,
            "",
            "",
            "本项目 CLI；不代替模型厂商的官方执行工具",
        ),
    ]
    .into_iter()
    .map(
        |(
            id,
            name,
            provider,
            repository,
            website,
            package,
            native,
            protocols,
            checkouts,
            notes,
        )| AppMetadata {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            source: OfficialSource {
                repository_url: format!("https://github.com/{repository}.git"),
                website_url: website.into(),
                package_name: package.map(str::to_owned),
                model_vendor_official: id != "wonderland",
            },
            capabilities: IntegrationCapabilities {
                manual_terminal: true,
                structured_runner: crate::native_executor::supports_native(id),
                protocols: protocols
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect(),
                sessions: native,
                cancellation: native,
                permissions: native,
                notes: if native {
                    "已提供结构化适配器，运行前仍须验证版本与身份"
                } else {
                    "接口能力仅为上游检查结果；当前仅接入手动终端"
                }
                .into(),
            },
            checkout_paths: checkouts
                .split(';')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect(),
            notes: notes.into(),
        },
    )
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceEvidence {
    pub checkout: PathBuf,
    pub git_dir: PathBuf,
    /// Normalized public repository URL only. Credentials are never returned.
    pub configured_origin: Option<String>,
    pub expected_origin: String,
    pub origin_matches: bool,
    pub head_commit: Option<String>,
    pub limitations: String,
}

/// Read local Git metadata, including submodule/worktree gitdir indirection.
/// This does not execute Git, contact upstream, or certify the working tree,
/// build artifacts, package signatures, or the identity of a running model.
pub fn verify_checkout_source(checkout: &Path, source: &OfficialSource) -> Result<SourceEvidence> {
    let checkout = checkout.canonicalize().context("源码目录不存在")?;
    let marker = checkout.join(".git");
    let git_dir = if marker.is_dir() {
        marker.canonicalize()?
    } else {
        let marker_text = read_git_metadata(&marker)?;
        let directory = marker_text
            .trim()
            .strip_prefix("gitdir:")
            .context("无法识别 .git 文件")?
            .trim();
        anyhow::ensure!(
            !directory.is_empty() && !directory.contains(['\r', '\n', '\0']),
            "无效 gitdir 路径"
        );
        checkout
            .join(directory)
            .canonicalize()
            .context("gitdir 目录不存在")?
    };
    let common_file = git_dir.join("commondir");
    let common_dir = if common_file.is_file() {
        let directory = read_git_metadata(&common_file)?;
        let directory = directory.trim();
        anyhow::ensure!(
            !directory.is_empty() && !directory.contains(['\r', '\n', '\0']),
            "无效 commondir 路径"
        );
        git_dir
            .join(directory)
            .canonicalize()
            .context("commondir 目录不存在")?
    } else {
        git_dir.clone()
    };
    let config = read_git_metadata(&common_dir.join("config"))?;
    let mut in_origin = false;
    let mut origins = Vec::new();
    for line in config.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            in_origin = line.eq_ignore_ascii_case("[remote \"origin\"]")
                || line.eq_ignore_ascii_case("[remote.origin]");
            continue;
        }
        if in_origin {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("url") {
                    origins.push(value.trim().trim_matches('"'));
                }
            }
        }
    }
    anyhow::ensure!(origins.len() <= 1, "origin 配置包含多个 URL，无法确定来源");
    let configured_origin = origins.first().and_then(|url| public_github_origin(url));
    let expected_origin = public_github_origin(&source.repository_url)
        .context("官方来源需要明确的 GitHub 仓库 URL")?;
    let origin_matches = configured_origin.as_ref() == Some(&expected_origin);
    let head_commit = read_checkout_head(&git_dir, &common_dir)?;
    Ok(SourceEvidence {
        checkout,
        git_dir,
        configured_origin,
        expected_origin,
        origin_matches,
        head_commit,
        limitations: "仅比较本地 Git 配置和 HEAD；未验证远端、签名、工作区修改、构建产物或运行模型。未展开 include、URL rewrite、用户级或系统级 Git 配置。匹配不等于官方二进制认证。".into(),
    })
}

fn read_git_metadata(path: &Path) -> Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .with_context(|| format!("无法读取 Git 元数据：{}", path.display()))?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    anyhow::ensure!(text.len() <= 1024 * 1024, "Git 元数据过大");
    Ok(text)
}

fn public_github_origin(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    // Reject credential-bearing URLs rather than placing their contents in UI
    // diagnostics. SSH's fixed git username contains no account credential.
    let path = lower
        .strip_prefix("https://github.com/")
        .or_else(|| lower.strip_prefix("git@github.com:"))
        .or_else(|| lower.strip_prefix("ssh://git@github.com/"))?;
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_end_matches('/'));
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    {
        return None;
    }
    Some(format!("https://github.com/{path}.git"))
}

fn read_checkout_head(git_dir: &Path, common_dir: &Path) -> Result<Option<String>> {
    let head = read_git_metadata(&git_dir.join("HEAD"))?;
    let head = head.trim();
    if is_git_object_id(head) {
        return Ok(Some(head.to_ascii_lowercase()));
    }
    let Some(reference) = head.strip_prefix("ref: ") else {
        return Ok(None);
    };
    anyhow::ensure!(
        reference.starts_with("refs/")
            && reference.split('/').all(|p| !p.is_empty()
                && p != "."
                && p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))),
        "无法识别 Git HEAD 引用"
    );
    for directory in [git_dir, common_dir] {
        let loose = directory.join(reference);
        if loose.is_file() {
            let value = read_git_metadata(&loose)?;
            return Ok(is_git_object_id(value.trim()).then(|| value.trim().to_ascii_lowercase()));
        }
        let packed = directory.join("packed-refs");
        if packed.is_file() {
            for line in read_git_metadata(&packed)?.lines() {
                if let Some((id, name)) = line.split_once(' ') {
                    if name == reference && is_git_object_id(id) {
                        return Ok(Some(id.to_ascii_lowercase()));
                    }
                }
            }
        }
    }
    Ok(None)
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|c| c.is_ascii_hexdigit())
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
            "minimax",
            "MiniMax Code",
            "mcode",
            "请按 MiniMax-AI/minimax-code 官方说明安装 mcode 或构建 dist/cli.js",
        ),
        ("mimo", "MiMo Code", "mimo", "npm install -g @mimo-ai/cli"),
        (
            "deepseek",
            "DeepSeek Harness",
            "dsh",
            "安装 @deepseek-ai/dsh，或指定已构建 checkout 的 Node 入口",
        ),
        (
            "zcode",
            "ZCode",
            "zcode",
            "请按 zai-org/ZCode 官方说明安装或构建 ZCode CLI",
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
    for (id, name, entry) in [
        ("minimax", "MiniMax Code", "minimax-code/dist/cli.js"),
        (
            "zcode",
            "ZCode",
            "ZCode/apps/zcode-cli/packages/cli/dist/zcode.cjs",
        ),
    ] {
        let entry = root.join(entry);
        profiles.push(CliProfile {
            id: format!("local-{id}"),
            name: format!("本地 {name} · Node 构建"),
            executable: "node".into(),
            args: vec![entry.to_string_lossy().into_owned()],
            required_paths: vec![entry],
            install_hint: format!("请按 {name} 官方源码说明构建入口；桌面不会安装依赖或执行构建"),
            ..CliProfile::default()
        });
    }
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let abi = if cfg!(target_env = "musl") {
        "-musl"
    } else {
        ""
    };
    let mimo = root.join(format!(
        "MiMo-Code/packages/opencode/dist/mimocode-{os}-{arch}{abi}/bin/mimo{executable_suffix}"
    ));
    profiles.push(CliProfile {
        id: "local-mimo".into(),
        name: "本地 MiMo Code · 已构建程序".into(),
        executable: mimo.to_string_lossy().into_owned(),
        required_paths: vec![mimo],
        install_hint: "请先按 MiMo Code 官方说明构建当前平台程序；baseline 构建请修改为实际路径"
            .into(),
        ..CliProfile::default()
    });
    // Preserve legacy ids and paths for saved settings. Add distinct presets for
    // the submodule layout; do not silently redirect a configured executable.
    for (id, legacy, upstream) in [
        ("codex", "codex", "anytool/ChatGPT/codex"),
        ("claude", "claude-code", "anytool/Claude/claude-code-best"),
        ("kimi-cli", "kimi-cli", "anytool/kimi/kimi-cli"),
        ("kimi-code", "kimi-code", "anytool/kimi/kimi-code"),
        ("minimax", "minimax-code", "anytool/minimax/minimax-code"),
        ("mimo", "MiMo-Code", "anytool/mimo/MiMo-Code"),
        (
            "deepseek",
            "deepseek-harness",
            "anytool/deepseek/deepseek-harness",
        ),
        ("zcode", "ZCode", "anytool/zai/ZCode"),
    ] {
        let Some(mut profile) = profiles
            .iter()
            .find(|p| p.id == format!("local-{id}"))
            .cloned()
        else {
            continue;
        };
        let from = root.join(legacy);
        let to = root.join(upstream);
        let relocate = |path: &Path| {
            path.strip_prefix(&from)
                .map(|rest| to.join(rest))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        profile.id = format!("anytool-{id}");
        profile.name = format!("{} · anytool", profile.name);
        profile.executable = relocate(Path::new(&profile.executable))
            .to_string_lossy()
            .into_owned();
        profile.args = profile
            .args
            .iter()
            .map(|arg| relocate(Path::new(arg)).to_string_lossy().into_owned())
            .collect();
        profile.required_paths = profile
            .required_paths
            .iter()
            .map(|path| relocate(path))
            .collect();
        profiles.push(profile);
    }
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
    let args = if matches!(
        profile.id.as_str(),
        "deepseek" | "local-deepseek" | "anytool-deepseek"
    ) && !profile
        .args
        .iter()
        .any(|arg| arg == "--profile" || arg.starts_with("--profile="))
    {
        vec!["--profile".into(), "agent".into()]
    } else {
        Vec::new()
    };
    prepare_native_cli(profile, workspace, &args)
}

/// Compose the entire argv before Windows shim encoding. `profile.args` retains
/// interpreter entrypoints and user configuration; `extra_args` follows it.
/// Unlike prepare_cli, this does not inject an interactive DeepSeek profile.
/// Callers must not append protocol arguments to the returned LaunchSpec.
pub fn prepare_native_cli(
    profile: &CliProfile,
    workspace: &Path,
    extra_args: &[String],
) -> Result<LaunchSpec> {
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
    let mut args = profile.args.clone();
    args.extend_from_slice(extra_args);
    normalize_launch(executable, args, cwd, profile.name.clone())
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
    let (executable, args) = {
        // Windows PowerShell 5 otherwise inherits the machine's OEM code page
        // and can replace CJK output with '?' on non-Chinese Windows installs.
        let mut args = encoded_powershell_args("");
        args.retain(|arg| arg != "-NoProfile");
        args.insert(1, "-NoExit".into());
        (powershell()?, args)
    };
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
    let script = format!("[Console]::InputEncoding = New-Object System.Text.UTF8Encoding; [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding; $OutputEncoding = [Console]::OutputEncoding; {script}");
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
    fn catalog_separates_native_adapters_from_manual_candidates() {
        let apps = official_apps();
        assert_eq!(apps.len(), 9);
        for profile in default_cli_profiles() {
            assert!(apps.iter().any(|app| app.id == profile.id));
        }
        let native: Vec<_> = apps
            .iter()
            .filter(|app| app.capabilities.structured_runner)
            .map(|app| app.id.as_str())
            .collect();
        assert_eq!(
            native,
            [
                "codex",
                "claude",
                "kimi-cli",
                "kimi-code",
                "minimax",
                "mimo",
                "deepseek"
            ]
        );
        assert!(
            !apps
                .iter()
                .find(|app| app.id == "wonderland")
                .unwrap()
                .source
                .model_vendor_official
        );
        assert!(apps
            .iter()
            .find(|app| app.id == "claude")
            .unwrap()
            .checkout_paths
            .is_empty());
    }

    #[test]
    fn checkout_presets_preserve_legacy_and_submodule_paths() {
        let directory = tempfile::tempdir().unwrap();
        let profiles = checkout_cli_profiles(directory.path());
        let kimi = profiles
            .iter()
            .find(|p| p.id == "anytool-kimi-code")
            .unwrap();
        assert_eq!(
            kimi.required_paths,
            [directory
                .path()
                .join("anytool/kimi/kimi-code/apps/kimi-code/dist/main.mjs")]
        );
        assert!(profiles.iter().any(|p| p.id == "local-kimi-code"));
        assert!(profiles
            .iter()
            .find(|p| p.id == "anytool-claude")
            .unwrap()
            .name
            .contains("分支"));
    }

    #[test]
    fn source_evidence_reads_submodule_gitfile_and_rejects_fork_origin() {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        let git_dir = directory.path().join("metadata");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(checkout.join(".git"), "gitdir: ../metadata\n").unwrap();
        let revision = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(git_dir.join("HEAD"), format!("{revision}\n")).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n url = git@github.com:XiaomiMiMo/MiMo-Code.git\n",
        )
        .unwrap();
        let source = official_apps()
            .into_iter()
            .find(|app| app.id == "mimo")
            .unwrap()
            .source;
        let evidence = verify_checkout_source(&checkout, &source).unwrap();
        assert!(evidence.origin_matches);
        assert_eq!(evidence.head_commit.as_deref(), Some(revision));
        assert!(evidence.limitations.contains("未验证远端"));
        std::fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n url = https://github.com/example/MiMo-Code.git\n",
        )
        .unwrap();
        assert!(
            !verify_checkout_source(&checkout, &source)
                .unwrap()
                .origin_matches
        );
    }

    #[test]
    fn source_evidence_handles_worktree_common_config_and_packed_refs() {
        let directory = tempfile::tempdir().unwrap();
        let git_dir = directory.path().join(".git");
        let common = directory.path().join("common");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&common).unwrap();
        std::fs::write(git_dir.join("commondir"), "../common\n").unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            common.join("config"),
            "[remote \"origin\"]\n url = https://github.com/openai/codex.git\n",
        )
        .unwrap();
        let revision = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(
            common.join("packed-refs"),
            format!("# pack-refs\n{revision} refs/heads/main\n"),
        )
        .unwrap();
        let source = official_apps()
            .into_iter()
            .find(|app| app.id == "codex")
            .unwrap()
            .source;
        let evidence = verify_checkout_source(directory.path(), &source).unwrap();
        assert!(evidence.origin_matches);
        assert_eq!(evidence.head_commit.as_deref(), Some(revision));
    }

    #[test]
    fn origin_normalization_rejects_credentials_and_deceptive_hosts() {
        for url in [
            "https://token@github.com/openai/codex.git",
            "https://github.com.example/openai/codex.git",
            "https://github.com/openai/codex.git?token=secret",
            "https://github.com/openai/../codex.git",
        ] {
            assert!(public_github_origin(url).is_none());
        }
        assert_eq!(
            public_github_origin("ssh://git@github.com/OpenAI/codex.git"),
            Some("https://github.com/openai/codex.git".into())
        );
    }

    #[test]
    fn native_argv_keeps_extra_arguments_and_omits_manual_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let profile = CliProfile {
            id: "deepseek".into(),
            executable: env::current_exe().unwrap().to_string_lossy().into_owned(),
            args: vec!["entry.js".into()],
            ..CliProfile::default()
        };
        assert_eq!(
            prepare_cli(&profile, directory.path()).unwrap().args,
            ["entry.js", "--profile", "agent"]
        );
        let spec = prepare_native_cli(
            &profile,
            directory.path(),
            &["--profile".into(), "acp".into()],
        )
        .unwrap();
        assert_eq!(spec.args, ["entry.js", "--profile", "acp"]);
        assert!(prepare_native_cli(&profile, directory.path(), &["bad\0argument".into()]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_argv_is_composed_before_powershell_encoding() {
        let directory = tempfile::tempdir().unwrap();
        let shim = directory.path().join("fixture.ps1");
        std::fs::write(&shim, "# unused fixture").unwrap();
        let profile = CliProfile {
            executable: shim.to_string_lossy().into_owned(),
            args: vec!["prefix".into()],
            ..CliProfile::default()
        };
        let spec = prepare_native_cli(
            &profile,
            directory.path(),
            &["acp".into(), "a' b $()".into()],
        )
        .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(spec.args.last().unwrap())
            .unwrap();
        let words: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let command = String::from_utf16(&words).unwrap();
        assert!(command.contains("'prefix' 'acp' 'a'' b $()'"));
        assert_eq!(spec.args[spec.args.len() - 2], "-EncodedCommand");
    }

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
