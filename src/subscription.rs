//! CLIProxyAPI sidecar 托管与订阅账号接入。
//!
//! 目标：让主程序在**只登录订阅账号**（Codex / Claude / Kimi / Antigravity /
//! xAI / Devin / Meta 的 OAuth）的情况下也能直接使用，不再依赖外部脚本。
//! 本模块负责：
//! - 定位（必要时下载并做 SHA-256 校验）CLIProxyAPI 可执行文件；
//! - 生成仅监听本机的最小配置与随机访问密钥（与安装包启动脚本共用同一份设置）；
//! - 启动 / 健康检查 / 停止 sidecar 进程；
//! - 通过管理 API 完成各提供商 OAuth 登录、账号列举与删除。
//!
//! OAuth 凭据由 CLIProxyAPI 自己持有（auth-dir），主程序不读取也不保存 token。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// 默认监听端口，与安装包启动脚本保持一致。
pub const DEFAULT_PORT: u16 = 18317;
/// 内置默认版本，与 packaging/windows/build-windows-package.ps1 同步。
pub const DEFAULT_VERSION: &str = "7.3.7";
const RELEASE_BASE: &str = "https://github.com/router-for-me/CLIProxyAPI/releases/download";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);

pub fn configured_port(directory: &Path) -> Result<u16> {
    let requested = std::env::var("CLIPROXYAPI_PORT")
        .ok()
        .map(|s| s.parse::<u16>())
        .transpose()?;
    resolve_port(directory, requested)
}

fn resolve_port(directory: &Path, requested: Option<u16>) -> Result<u16> {
    let existing = if directory.join("config.yaml").is_file() {
        let config: serde_yaml::Value =
            serde_yaml::from_slice(&std::fs::read(directory.join("config.yaml"))?)?;
        let port = config["port"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|p| *p > 0)
            .context("config.yaml requires a valid nonzero port")?;
        Some(port)
    } else {
        None
    };
    anyhow::ensure!(requested != Some(0), "CLIPROXYAPI_PORT must be nonzero");
    if let (Some(requested), Some(existing)) = (requested, existing) {
        anyhow::ensure!(requested==existing,"CLIPROXYAPI_PORT conflicts with the port in existing config.yaml; update the configuration or use its port {existing}");
    }
    Ok(requested.or(existing).unwrap_or(DEFAULT_PORT))
}

/// 本机访问密钥，与 PowerShell 启动器共用同一份文件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LauncherSettings {
    pub schema_version: u32,
    pub api_key: String,
    pub management_key: String,
}

/// 已经就绪的 sidecar 端点信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionEndpoint {
    pub base_url: String,
    pub management_url: String,
    pub api_key: String,
    pub management_key: String,
    pub port: u16,
}

impl SubscriptionEndpoint {
    pub fn from_parts(port: u16, api_key: String, management_key: String) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            management_url: format!("http://127.0.0.1:{port}/v0/management"),
            api_key,
            management_key,
            port,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubscriptionConfig {
    pub binary: PathBuf,
    pub data_dir: PathBuf,
    pub port: u16,
    pub settings: LauncherSettings,
}

/// 托管 CLIProxyAPI 进程。
pub struct SubscriptionManager {
    config: SubscriptionConfig,
    child: Mutex<Option<Child>>,
}

impl SubscriptionManager {
    pub fn new(config: SubscriptionConfig) -> Self {
        Self {
            config,
            child: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &SubscriptionConfig {
        &self.config
    }

    pub fn endpoint(&self) -> SubscriptionEndpoint {
        SubscriptionEndpoint::from_parts(
            self.config.port,
            self.config.settings.api_key.clone(),
            self.config.settings.management_key.clone(),
        )
    }

    pub fn config_path(&self) -> PathBuf {
        self.config.data_dir.join("config.yaml")
    }

    pub fn settings_path(&self) -> PathBuf {
        self.config.data_dir.join("launcher-settings.json")
    }

    fn auth_dir(&self) -> PathBuf {
        self.config.data_dir.join("auth")
    }

    /// 确保 sidecar 正在运行：已就绪则直接复用，否则拉起新进程。
    pub async fn ensure_running(&self) -> Result<SubscriptionEndpoint> {
        let endpoint = self.endpoint();
        if self.is_healthy().await {
            tracing::info!(
                port = self.config.port,
                "reusing running CLIProxyAPI sidecar"
            );
            return Ok(endpoint);
        }
        if !self.config.binary.is_file() {
            bail!(
                "CLIProxyAPI sidecar 未找到：{}；请重新运行 Wonderland 安装包，或设置 CLIPROXYAPI_BIN 指向 cli-proxy-api 可执行文件",
                self.config.binary.display()
            );
        }

        std::fs::create_dir_all(&self.config.data_dir)?;
        std::fs::create_dir_all(self.auth_dir())?;
        // 已存在配置时不覆盖：用户可能自己加过提供商配置。
        if !self.config_path().is_file() {
            let yaml = render_config(
                self.config.port,
                &self.auth_dir(),
                &self.config.settings.api_key,
                &self.config.settings.management_key,
            );
            std::fs::write(self.config_path(), yaml)
                .with_context(|| format!("写入 {} 失败", self.config_path().display()))?;
            tracing::info!(path = %self.config_path().display(), "created CLIProxyAPI config");
        }

        let out_log = std::fs::File::create(self.config.data_dir.join("cliproxyapi.out.log"))?;
        let err_log = std::fs::File::create(self.config.data_dir.join("cliproxyapi.err.log"))?;
        let mut command = Command::new(&self.config.binary);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let child = command
            .arg("-config")
            .arg(self.config_path())
            .current_dir(&self.config.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_log))
            .stderr(Stdio::from(err_log))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("启动 {} 失败", self.config.binary.display()))?;
        *self.child.lock().await = Some(child);

        if !self.wait_until_healthy().await {
            self.stop().await;
            bail!(
                "CLIProxyAPI 启动后 {} 秒内未就绪，请查看 {}",
                HEALTH_TIMEOUT.as_secs(),
                self.config.data_dir.join("cliproxyapi.err.log").display()
            );
        }
        Ok(endpoint)
    }

    /// 使用带访问密钥的模型端点验证受管实例。
    pub async fn is_healthy(&self) -> bool {
        let url = format!("http://127.0.0.1:{}/v1/models", self.config.port);
        matches!(
            tokio::time::timeout(Duration::from_secs(2), reqwest::Client::new().get(&url).bearer_auth(&self.config.settings.api_key).send()).await,
            Ok(Ok(response)) if response.status().is_success()
        )
    }

    async fn wait_until_healthy(&self) -> bool {
        let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if self.is_healthy().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        false
    }

    /// 停止由本进程启动的 sidecar（外部启动的进程不受影响）。
    pub async fn stop(&self) {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// 重启 sidecar。
    pub async fn restart(&self) -> Result<SubscriptionEndpoint> {
        self.stop().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.ensure_running().await
    }
}

/// 渲染 CLIProxyAPI 配置。纯函数，便于单测。
pub fn render_config(port: u16, auth_dir: &Path, api_key: &str, management_key: &str) -> String {
    let auth_dir = auth_dir.display().to_string().replace('\\', "/");
    let mut out = String::new();
    let _ = writeln!(out, "host: \"127.0.0.1\"");
    let _ = writeln!(out, "port: {port}");
    let _ = writeln!(out, "auth-dir: \"{auth_dir}\"");
    let _ = writeln!(out, "api-keys:");
    let _ = writeln!(out, "  - \"{api_key}\"");
    let _ = writeln!(out, "remote-management:");
    let _ = writeln!(out, "  allow-remote: false");
    let _ = writeln!(out, "  secret-key: \"{management_key}\"");
    let _ = writeln!(out, "  disable-control-panel: true");
    let _ = writeln!(out, "  disable-auto-update-panel: true");
    out
}

/// bcrypt 对输入长度有硬上限（72 字节），管理密钥必须远低于它，
/// 否则 CLIProxyAPI 会拒绝启动配置。
pub const MAX_SECRET_LEN: usize = 64;

/// 生成本机访问密钥。
///
/// 无 rand 依赖：用 uuid v4 的 128 位随机性（32 个十六进制字符）加时间戳后缀，
/// 长度控制在 bcrypt 的 72 字节上限之内。
pub fn generate_secret() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let random = uuid::Uuid::new_v4().simple().to_string();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let secret = format!("{random}{:08x}", (nanos as u64) & 0xffff_ffff);
    secret[..secret.len().min(MAX_SECRET_LEN)].to_string()
}

/// 读取既有密钥，缺失或损坏时生成新的一份并落盘。
pub fn load_or_create_settings(path: &Path) -> Result<LauncherSettings> {
    if let Ok(raw) = std::fs::read_to_string(path) {
        if let Ok(settings) = serde_json::from_str::<LauncherSettings>(&raw) {
            if !settings.api_key.trim().is_empty() && !settings.management_key.trim().is_empty() {
                return Ok(settings);
            }
        } else {
            tracing::warn!(path = %path.display(), "launcher-settings.json 不可解析，将重新生成密钥");
        }
    }
    let settings = LauncherSettings {
        schema_version: 1,
        api_key: generate_secret(),
        management_key: generate_secret(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&settings)?)?;
    Ok(settings)
}

/// sidecar 数据目录：优先 CLIPROXYAPI_DATA_DIR，其次安装版的数据目录。
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLIPROXYAPI_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.trim().is_empty() {
            return PathBuf::from(local)
                .join("WonderlandData")
                .join("CLIProxyAPI");
        }
    }
    PathBuf::from(".agent-data").join("cliproxyapi")
}

/// 可执行文件候选路径：显式环境变量 → 主程序同级目录 → 数据目录 → PATH。
pub fn binary_candidates(exe_dir: &Path, data_dir: &Path) -> Vec<PathBuf> {
    let name = if cfg!(windows) {
        "cli-proxy-api.exe"
    } else {
        "cli-proxy-api"
    };
    let mut candidates = Vec::new();
    if let Ok(explicit) = std::env::var("CLIPROXYAPI_BIN") {
        if !explicit.trim().is_empty() {
            candidates.push(PathBuf::from(explicit));
        }
    }
    candidates.push(exe_dir.join(name));
    candidates.push(exe_dir.join("cliproxyapi").join(name));
    candidates.push(data_dir.join("bin").join(name));
    if let Ok(path) = std::env::var("PATH") {
        for entry in std::env::split_paths(&path) {
            candidates.push(entry.join(name));
        }
    }
    candidates
}

pub fn locate_binary(exe_dir: &Path, data_dir: &Path) -> Option<PathBuf> {
    binary_candidates(exe_dir, data_dir)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// 从 GitHub Release 下载并校验 sidecar；已存在则直接返回。
pub async fn download_binary(version: &str, target_dir: &Path) -> Result<PathBuf> {
    let name = if cfg!(windows) {
        "cli-proxy-api.exe"
    } else {
        "cli-proxy-api"
    };
    let target = target_dir.join(name);
    if target.is_file() {
        return Ok(target);
    }
    let asset = release_asset_name(version);
    let base = format!("{RELEASE_BASE}/v{version}");
    let client = reqwest::Client::new();
    let checksums = client
        .get(format!("{base}/checksums.txt"))
        .send()
        .await
        .context("下载 CLIProxyAPI checksums.txt 失败")?
        .error_for_status()?
        .text()
        .await
        .context("读取 checksums.txt 失败")?;
    let expected = checksums
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let file = parts.next()?.trim_start_matches('*');
            (file == asset).then(|| hash.to_ascii_lowercase())
        })
        .with_context(|| format!("checksums.txt 中没有 {asset} 的 SHA-256"))?;

    let bytes = client
        .get(format!("{base}/{asset}"))
        .send()
        .await
        .context("下载 CLIProxyAPI 发布包失败")?
        .error_for_status()?
        .bytes()
        .await
        .context("读取 CLIProxyAPI 发布包失败")?;

    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!("CLIProxyAPI 下载校验失败：期望 {expected}，实际 {actual}");
    }
    std::fs::create_dir_all(target_dir)?;

    if asset.ends_with(".tar.gz") {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_ref()));
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_file()
                && entry.path()?.file_name() == Some(std::ffi::OsStr::new(name))
            {
                let temp = target.with_extension("download");
                let mut file = std::fs::File::create(&temp)?;
                std::io::copy(&mut entry, &mut file)?;
                drop(file);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
                }
                std::fs::rename(temp, &target)?;
                return Ok(target);
            }
        }
        bail!("CLIProxyAPI archive missing executable");
    }

    let reader = std::io::Cursor::new(bytes.to_vec());
    let mut archive = zip::ZipArchive::new(reader).context("CLIProxyAPI 发布包不是合法 zip")?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let entry_name = entry.name().to_string();
        if !entry_name.ends_with(name) {
            continue;
        }
        let temp = target.with_extension("download");
        let mut file = std::fs::File::create(&temp)?;
        std::io::copy(&mut entry, &mut file)?;
        drop(file);
        std::fs::rename(temp, &target)?;
        tracing::info!(path = %target.display(), "downloaded CLIProxyAPI sidecar");
        return Ok(target);
    }
    bail!("CLIProxyAPI 发布包里没有 {name}")
}

fn release_asset_name(version: &str) -> String {
    let platform = if cfg!(windows) {
        "windows_amd64"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "darwin_arm64"
        } else {
            "darwin_amd64"
        }
    } else if cfg!(target_arch = "aarch64") {
        "linux_arm64"
    } else {
        "linux_amd64"
    };
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!("CLIProxyAPI_{version}_{platform}.{extension}")
}

/// SHA-256 十六进制摘要（避免引入额外依赖，自带一份最小实现）。
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for index in 0..16 {
            w[index] = u32::from_be_bytes([
                block[index * 4],
                block[index * 4 + 1],
                block[index * 4 + 2],
                block[index * 4 + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let mut work = h;
        for index in 0..64 {
            let s1 = work[4].rotate_right(6) ^ work[4].rotate_right(11) ^ work[4].rotate_right(25);
            let ch = (work[4] & work[5]) ^ ((!work[4]) & work[6]);
            let temp1 = work[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = work[0].rotate_right(2) ^ work[0].rotate_right(13) ^ work[0].rotate_right(22);
            let maj = (work[0] & work[1]) ^ (work[0] & work[2]) ^ (work[1] & work[2]);
            let temp2 = s0.wrapping_add(maj);
            work[7] = work[6];
            work[6] = work[5];
            work[5] = work[4];
            work[4] = work[3].wrapping_add(temp1);
            work[3] = work[2];
            work[2] = work[1];
            work[1] = work[0];
            work[0] = temp1.wrapping_add(temp2);
        }
        for index in 0..8 {
            h[index] = h[index].wrapping_add(work[index]);
        }
    }
    let mut digest = [0u8; 32];
    for (index, value) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    digest
}

/// 一个已登录的订阅账号。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthAccount {
    pub name: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStart {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStatus {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// 支持的订阅提供商别名 → 规范名。
pub fn normalize_provider(alias: &str) -> Option<&'static str> {
    match alias.trim().to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some("claude"),
        "codex" | "openai" | "chatgpt" => Some("codex"),
        "kimi" | "moonshot" => Some("kimi"),
        "antigravity" | "anti-gravity" | "gemini" => Some("antigravity"),
        "xai" | "grok" => Some("xai"),
        "devin" => Some("devin"),
        "meta" => Some("meta"),
        _ => None,
    }
}

pub fn auth_url_path(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("anthropic-auth-url"),
        "codex" => Some("codex-auth-url"),
        "antigravity" => Some("antigravity-auth-url"),
        "kimi" => Some("kimi-auth-url"),
        "xai" => Some("xai-auth-url"),
        "devin" => Some("devin-auth-url"),
        "meta" => Some("meta-auth-url"),
        _ => None,
    }
}

/// 订阅账号管理客户端（管理 API 薄封装）。
#[derive(Clone)]
pub struct SubscriptionAccounts {
    client: reqwest::Client,
    endpoint: SubscriptionEndpoint,
}

impl SubscriptionAccounts {
    pub fn new(endpoint: SubscriptionEndpoint) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
        }
    }

    pub fn endpoint(&self) -> &SubscriptionEndpoint {
        &self.endpoint
    }

    fn management(&self, path: &str) -> String {
        format!("{}/{}", self.endpoint.management_url, path)
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(
                "authorization",
                format!("Bearer {}", self.endpoint.management_key),
            )
            .header("x-management-key", &self.endpoint.management_key)
    }

    /// 发起 OAuth：返回需要用户在浏览器打开的授权链接与轮询用 state。
    pub async fn start_login(&self, provider: &str) -> Result<LoginStart> {
        let provider = normalize_provider(provider)
            .with_context(|| format!("不支持的订阅提供商：{provider}"))?;
        let path = auth_url_path(provider).context("没有对应的登录端点")?;
        let response = self
            .authorized(self.client.get(self.management(path)))
            .send()
            .await
            .context("请求 CLIProxyAPI 登录端点失败")?
            .error_for_status()
            .context("CLIProxyAPI 拒绝登录请求（检查管理密钥与 remote-management 配置）")?;
        let value: serde_json::Value = response.json().await.context("解析登录响应失败")?;
        Ok(LoginStart {
            status: value
                .get("status")
                .and_then(|status| status.as_str())
                .unwrap_or("wait")
                .to_string(),
            url: value
                .get("url")
                .or_else(|| value.get("auth_url"))
                .and_then(|url| url.as_str())
                .map(str::to_string),
            state: value
                .get("state")
                .and_then(|state| state.as_str())
                .map(str::to_string),
            error: value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string),
        })
    }

    pub async fn login_status(&self, state: &str) -> Result<LoginStatus> {
        let response = self
            .authorized(
                self.client
                    .get(self.management("get-auth-status"))
                    .query(&[("state", state)]),
            )
            .send()
            .await
            .context("查询登录状态失败")?
            .error_for_status()
            .context("CLIProxyAPI 拒绝状态查询")?;
        let value: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        Ok(LoginStatus {
            status: value
                .get("status")
                .and_then(|status| status.as_str())
                .unwrap_or("wait")
                .to_string(),
            error: value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string),
        })
    }

    pub async fn cancel_login(&self, state: &str) -> Result<bool> {
        let response = self
            .authorized(
                self.client
                    .delete(format!("{}/oauth-callback", self.endpoint.management_url))
                    .query(&[("state", state)]),
            )
            .send()
            .await
            .context("取消登录失败")?;
        Ok(response.status().is_success())
    }

    /// 列出已登录账号（auth-files）。
    pub async fn list_accounts(&self) -> Result<Vec<AuthAccount>> {
        let response = self
            .authorized(self.client.get(self.management("auth-files")))
            .send()
            .await
            .context("查询订阅账号失败")?
            .error_for_status()
            .context("CLIProxyAPI 拒绝账号查询")?;
        let value: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        let entries = value
            .get("files")
            .or_else(|| value.get("data"))
            .or_else(|| value.get("auth_files"))
            .and_then(|entries| entries.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(entries
            .iter()
            .filter_map(|entry| {
                let name = entry
                    .get("name")
                    .or_else(|| entry.get("id"))
                    .and_then(|name| name.as_str())?
                    .to_string();
                Some(AuthAccount {
                    name,
                    provider: entry
                        .get("provider")
                        .or_else(|| entry.get("type"))
                        .and_then(|provider| provider.as_str())
                        .map(str::to_string),
                    email: entry
                        .get("email")
                        .and_then(|email| email.as_str())
                        .map(str::to_string),
                    disabled: entry
                        .get("disabled")
                        .and_then(|disabled| disabled.as_bool())
                        .unwrap_or(false),
                })
            })
            .collect())
    }

    pub async fn delete_account(&self, name: &str) -> Result<()> {
        self.authorized(
            self.client
                .delete(self.management("auth-files"))
                .query(&[("name", name)]),
        )
        .send()
        .await
        .context("删除订阅账号失败")?
        .error_for_status()
        .context("CLIProxyAPI 拒绝删除账号")?;
        Ok(())
    }

    pub async fn refresh_accounts(&self) -> Result<()> {
        self.authorized(self.client.post(self.management("auth-files/refresh")))
            .send()
            .await
            .context("刷新订阅账号失败")?
            .error_for_status()
            .context("CLIProxyAPI 拒绝刷新账号")?;
        Ok(())
    }

    /// 订阅账号可用的模型 id 列表（统一 /v1/models）。
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .client
            .get(format!("{}/models", self.endpoint.base_url))
            .header("authorization", format!("Bearer {}", self.endpoint.api_key))
            .send()
            .await
            .context("查询订阅模型失败")?
            .error_for_status()
            .context("CLIProxyAPI 拒绝模型查询")?;
        let value: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        Ok(value
            .get("data")
            .and_then(|data| data.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// 按环境搭建托管实例；返回 None 表示未启用订阅模式。
pub fn from_env(exe_dir: &Path) -> Option<(SubscriptionManager, SubscriptionEndpoint)> {
    let provider = std::env::var("AGENT_PROVIDER")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let enabled = provider == "subscription"
        || provider == "cliproxyapi"
        || std::env::var("CLIPROXYAPI_ENABLED")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
    if !enabled {
        return None;
    }
    let data_dir = default_data_dir();
    let binary = locate_binary(exe_dir, &data_dir)?;
    let port = std::env::var("CLIPROXYAPI_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let settings = load_or_create_settings(&data_dir.join("launcher-settings.json")).ok()?;
    let manager = SubscriptionManager::new(SubscriptionConfig {
        binary,
        data_dir,
        port,
        settings,
    });
    let endpoint = manager.endpoint();
    Some((manager, endpoint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_configured_port_and_reject_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(resolve_port(directory.path(), None).unwrap(), DEFAULT_PORT);
        std::fs::write(directory.path().join("config.yaml"), "port: 18327\n").unwrap();
        assert_eq!(resolve_port(directory.path(), None).unwrap(), 18327);
        assert_eq!(resolve_port(directory.path(), Some(18327)).unwrap(), 18327);
        assert!(resolve_port(directory.path(), Some(18317)).is_err());
        assert!(resolve_port(directory.path(), Some(0)).is_err());
        std::fs::write(directory.path().join("config.yaml"), "port: 99999\n").unwrap();
        assert!(resolve_port(directory.path(), None).is_err());
    }

    #[test]
    fn renders_minimal_local_config() {
        let yaml = render_config(
            18317,
            Path::new("F:/data/CLIProxyAPI/auth"),
            "key-1",
            "mgmt-1",
        );
        assert!(yaml.contains("host: \"127.0.0.1\""));
        assert!(yaml.contains("port: 18317"));
        assert!(yaml.contains("api-keys:"));
        assert!(yaml.contains("  - \"key-1\""));
        assert!(yaml.contains("allow-remote: false"));
        assert!(yaml.contains("secret-key: \"mgmt-1\""));
        // Windows 路径必须转成正斜杠，YAML 里反斜杠会被当转义。
        assert!(yaml.contains("auth-dir: \"F:/data/CLIProxyAPI/auth\""));
        assert!(!yaml.contains('\\'));
    }

    #[test]
    fn converts_windows_separators_in_auth_dir() {
        let yaml = render_config(1, Path::new("C:\\tmp\\auth"), "a", "b");
        assert!(yaml.contains("auth-dir: \"C:/tmp/auth\""));
    }

    #[test]
    fn endpoint_urls_are_derived_from_port() {
        let endpoint = SubscriptionEndpoint::from_parts(18317, "a".to_string(), "b".to_string());
        assert_eq!(endpoint.base_url, "http://127.0.0.1:18317/v1");
        assert_eq!(
            endpoint.management_url,
            "http://127.0.0.1:18317/v0/management"
        );
    }

    #[test]
    fn provider_aliases_normalize() {
        assert_eq!(normalize_provider("Claude"), Some("claude"));
        assert_eq!(normalize_provider("anthropic"), Some("claude"));
        assert_eq!(normalize_provider("openai"), Some("codex"));
        assert_eq!(normalize_provider("kimi"), Some("kimi"));
        assert_eq!(normalize_provider("grok"), Some("xai"));
        assert_eq!(normalize_provider("unknown"), None);
        assert_eq!(auth_url_path("codex"), Some("codex-auth-url"));
        assert_eq!(auth_url_path("kimi"), Some("kimi-auth-url"));
        assert_eq!(auth_url_path("nope"), None);
    }

    #[test]
    fn generated_secrets_fit_bcrypt_limit() {
        for _ in 0..16 {
            let secret = generate_secret();
            // CLIProxyAPI 用 bcrypt 处理 management key，超过 72 字节会直接启动失败。
            assert!(secret.len() <= 72, "secret too long: {secret}");
            assert!(secret.len() >= 32, "secret too short: {secret}");
            assert!(secret.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn settings_are_created_then_reused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launcher-settings.json");
        let created = load_or_create_settings(&path).unwrap();
        assert!(!created.api_key.is_empty());
        assert_ne!(created.api_key, created.management_key);
        let reused = load_or_create_settings(&path).unwrap();
        assert_eq!(created, reused);
    }

    #[test]
    fn broken_settings_file_is_regenerated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launcher-settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let settings = load_or_create_settings(&path).unwrap();
        assert!(!settings.api_key.is_empty());
        // 空密钥同样触发重建。
        std::fs::write(
            &path,
            serde_json::json!({"schema_version": 1, "api_key": "", "management_key": ""})
                .to_string(),
        )
        .unwrap();
        let rebuilt = load_or_create_settings(&path).unwrap();
        assert!(!rebuilt.api_key.is_empty());
    }

    #[test]
    fn binary_candidates_cover_install_and_data_dirs() {
        let candidates = binary_candidates(Path::new("F:/app"), Path::new("F:/data"));
        let name = if cfg!(windows) {
            "cli-proxy-api.exe"
        } else {
            "cli-proxy-api"
        };
        assert!(candidates.contains(&Path::new("F:/app").join(name)));
        assert!(candidates.contains(&Path::new("F:/app").join("cliproxyapi").join(name)));
        assert!(candidates.contains(&Path::new("F:/data").join("bin").join(name)));
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 跨越 64 字节分块边界。
        assert_eq!(
            sha256_hex(&b"a".repeat(1000)),
            sha256_hex(&b"a".repeat(1000))
        );
    }

    #[test]
    fn release_asset_name_targets_current_platform() {
        let asset = release_asset_name("7.3.7");
        assert!(asset.starts_with("CLIProxyAPI_7.3.7_"));
        assert!(asset.ends_with(if cfg!(windows) { ".zip" } else { ".tar.gz" }));
        if cfg!(windows) {
            assert!(asset.contains("windows_amd64"));
        }
    }
}
