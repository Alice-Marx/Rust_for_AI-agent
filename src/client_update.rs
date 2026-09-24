//! Desktop client update discovery, verification, and hand-off.
//!
//! The desktop application owns this flow rather than the local task service:
//! a desktop may intentionally be connected to an older or external service.
//! The update source, artifact names, checksum manifest, and installer
//! arguments are all fixed here. Callers never supply a URL, command, or
//! destination path.

use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use url::Url;
use uuid::Uuid;

const REPOSITORY: &str = "Alice-Marx/Rust_for_AI-agent";
const RELEASE_API: &str =
    "https://api.github.com/repos/Alice-Marx/Rust_for_AI-agent/releases/latest";
const RELEASE_DOWNLOAD_ROOT: &str =
    "https://github.com/Alice-Marx/Rust_for_AI-agent/releases/download";
const CHECKSUM_FILE: &str = "SHA256SUMS.txt";
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_INSTALLER_BYTES: u64 = 1024 * 1024 * 1024;
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_version(value: &str) -> Result<Version> {
    let parts: Vec<_> = value.split('.').collect();
    ensure!(parts.len() == 3, "版本号必须是 major.minor.patch");
    let parse_part = |part: &str| -> Result<u64> {
        ensure!(
            !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()),
            "版本号只能包含十进制数字"
        );
        ensure!(part == "0" || !part.starts_with('0'), "版本号不能有前导零");
        Ok(part.parse()?)
    };
    Ok(Version {
        major: parse_part(parts[0])?,
        minor: parse_part(parts[1])?,
        patch: parse_part(parts[2])?,
    })
}

fn release_version(tag: &str) -> Result<(String, Version)> {
    let value = tag
        .strip_prefix('v')
        .context("GitHub Release tag 必须以 v 开头")?;
    Ok((value.to_owned(), parse_version(value)?))
}

fn setup_name(version: &str) -> String {
    format!("Wonderland-Setup-{version}-x64.exe")
}

fn download_url(tag: &str, name: &str) -> String {
    // Both tag and filename are validated/generated locally before this point.
    format!("{RELEASE_DOWNLOAD_ROOT}/{tag}/{name}")
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    size: u64,
}

#[derive(Clone, Debug)]
struct UpdateAsset {
    name: String,
    url: String,
    size: u64,
    sha256: String,
}

/// A checked, checksum-bound release. The URLs and checksum are private so a
/// UI cannot accidentally turn this into a general-purpose downloader.
#[derive(Clone, Debug)]
pub struct UpdateCheck {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub proxy_fallback_used: bool,
    asset: Option<UpdateAsset>,
}

/// A locally downloaded installer whose contents have already matched the
/// release manifest. It remains in the user's temporary directory until the
/// installer has been launched.
#[derive(Clone, Debug)]
pub struct PreparedUpdate {
    pub version: String,
    installer: PathBuf,
}

impl PreparedUpdate {
    pub fn installer_path(&self) -> &Path {
        &self.installer
    }
}

fn release_plan(release: GitHubRelease) -> Result<(String, Version, GitHubAsset, GitHubAsset)> {
    ensure!(!release.draft, "GitHub Release 仍是草稿，不能安装");
    ensure!(
        !release.prerelease,
        "GitHub Release 是预发布版本，不能自动安装"
    );
    let (version, parsed) = release_version(&release.tag_name)?;
    let installer_name = setup_name(&version);
    let select = |name: &str| -> Result<GitHubAsset> {
        let mut matches = release.assets.iter().filter(|asset| asset.name == name);
        let asset = matches
            .next()
            .cloned()
            .with_context(|| format!("GitHub Release 缺少 {name}"))?;
        ensure!(
            matches.next().is_none(),
            "GitHub Release 包含重复资产 {name}"
        );
        Ok(asset)
    };
    let installer = select(&installer_name)?;
    ensure!(
        installer.size > 0 && installer.size <= MAX_INSTALLER_BYTES,
        "安装器大小不在允许范围内"
    );
    let manifest = select(CHECKSUM_FILE)?;
    ensure!(
        manifest.size > 0 && manifest.size as usize <= MAX_MANIFEST_BYTES,
        "SHA256SUMS.txt 大小不在允许范围内"
    );
    Ok((version, parsed, installer, manifest))
}

fn checksum_from_manifest(contents: &str, filename: &str) -> Result<String> {
    let mut found = None;
    for line in contents.lines() {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        if fields.len() != 2 {
            continue;
        }
        let candidate = fields[1].strip_prefix('*').unwrap_or(fields[1]);
        if candidate != filename {
            continue;
        }
        let hash = fields[0];
        ensure!(
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "SHA256SUMS.txt 中 {filename} 的摘要格式无效"
        );
        ensure!(
            found.replace(hash.to_ascii_lowercase()).is_none(),
            "SHA256SUMS.txt 中 {filename} 的摘要重复"
        );
    }
    found.with_context(|| format!("SHA256SUMS.txt 中缺少 {filename} 的摘要"))
}

fn configured_proxy_values() -> Vec<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .filter_map(|name| env::var(name).ok())
    .filter(|value| !value.trim().is_empty())
    .collect()
}

fn proxy_value_is_loopback(value: &str) -> bool {
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let Ok(url) = Url::parse(&candidate) else {
        return false;
    };
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    )
}

/// Only bypass proxy configuration when every explicit proxy points back to
/// this computer. A corporate or user-selected remote proxy is never ignored.
fn can_bypass_loopback_proxy() -> bool {
    let values = configured_proxy_values();
    !values.is_empty() && values.iter().all(|value| proxy_value_is_loopback(value))
}

fn github_client(no_proxy: bool, timeout: Duration) -> Result<Client> {
    let mut builder = Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .user_agent(format!(
            "Wonderland/{}/desktop-updater",
            env!("CARGO_PKG_VERSION")
        ));
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder.build().context("无法创建 GitHub 更新客户端")
}

async fn request(url: &str, no_proxy: bool, timeout: Duration) -> Result<reqwest::Response> {
    github_client(no_proxy, timeout)?
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("无法连接 GitHub：{url}"))?
        .error_for_status()
        .with_context(|| format!("GitHub 返回失败状态：{url}"))
}

/// Returns whether the loopback-proxy fallback was used for this request.
async fn request_with_proxy_fallback(
    url: &str,
    timeout: Duration,
    prefer_no_proxy: bool,
) -> Result<(reqwest::Response, bool)> {
    if prefer_no_proxy {
        return Ok((request(url, true, timeout).await?, true));
    }
    match request(url, false, timeout).await {
        Ok(response) => Ok((response, false)),
        Err(_) if can_bypass_loopback_proxy() => {
            let response = request(url, true, timeout)
                .await
                .context("本机代理不可用，直连 GitHub 的重试也失败")?;
            Ok((response, true))
        }
        Err(error) => Err(error),
    }
}

async fn read_limited(url: &str, limit: usize, prefer_no_proxy: bool) -> Result<(Vec<u8>, bool)> {
    let (response, bypassed) =
        request_with_proxy_fallback(url, CHECK_TIMEOUT, prefer_no_proxy).await?;
    if let Some(length) = response.content_length() {
        ensure!(length <= limit as u64, "下载内容超过允许大小");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("读取 GitHub 响应失败")?;
        let total = bytes
            .len()
            .checked_add(chunk.len())
            .context("下载内容大小溢出")?;
        ensure!(total <= limit, "下载内容超过允许大小");
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, bypassed))
}

/// Check the stable GitHub Release and bind its installer to the checksum
/// manifest before exposing an available update to the desktop UI.
pub async fn check() -> Result<UpdateCheck> {
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let current = parse_version(&current_version).context("当前客户端版本无效")?;
    let (response, api_bypassed) =
        request_with_proxy_fallback(RELEASE_API, CHECK_TIMEOUT, false).await?;
    let release = response
        .json::<GitHubRelease>()
        .await
        .context("GitHub Release 响应格式无效")?;
    ensure!(!release.draft, "GitHub Release 仍是草稿，不能检查更新");
    ensure!(
        !release.prerelease,
        "GitHub Release 是预发布版本，不能自动安装"
    );
    let (latest_version, latest) = release_version(&release.tag_name)?;
    let release_url = format!(
        "https://github.com/{REPOSITORY}/releases/tag/{}",
        release.tag_name
    );
    if latest <= current {
        return Ok(UpdateCheck {
            current_version,
            latest_version,
            update_available: false,
            release_url,
            proxy_fallback_used: api_bypassed,
            asset: None,
        });
    }

    let (version, _, installer, _manifest) = release_plan(release)?;
    debug_assert_eq!(version, latest_version);
    let manifest_url = download_url(&format!("v{version}"), CHECKSUM_FILE);
    let (manifest_bytes, manifest_bypassed) =
        read_limited(&manifest_url, MAX_MANIFEST_BYTES, api_bypassed).await?;
    let manifest_text =
        std::str::from_utf8(&manifest_bytes).context("SHA256SUMS.txt 不是 UTF-8 文本")?;
    let installer_name = setup_name(&version);
    let sha256 = checksum_from_manifest(manifest_text, &installer_name)?;
    Ok(UpdateCheck {
        current_version,
        latest_version: version.clone(),
        update_available: true,
        release_url,
        proxy_fallback_used: api_bypassed || manifest_bypassed,
        asset: Some(UpdateAsset {
            name: installer_name.clone(),
            url: download_url(&format!("v{version}"), &installer_name),
            size: installer.size,
            sha256,
        }),
    })
}

fn configured_data_directory() -> Option<PathBuf> {
    let from_marker = || {
        let executable = env::current_exe().ok()?;
        let location_file = executable.parent()?.join("data-location.txt");
        let value = fs::read_to_string(location_file).ok()?;
        let path = PathBuf::from(value.trim().trim_start_matches('\u{feff}'));
        path.is_absolute().then_some(path)
    };
    from_marker().or_else(|| {
        env::var_os("AGENT_DATA_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    })
}

async fn create_update_cache_dir() -> Result<PathBuf> {
    let data_dir = configured_data_directory().context(
        "无法确定数据目录，已取消下载以避免把更新安装器写入系统临时目录；请从安装器重新选择数据目录",
    )?;
    let candidate = data_dir.join("updates");
    tokio::fs::create_dir_all(&candidate)
        .await
        .context("无法创建数据目录中的 updates 文件夹，已取消下载")?;
    Ok(candidate)
}

fn cache_paths(directory: &Path, identifier: Uuid, asset_name: &str) -> (PathBuf, PathBuf) {
    (
        directory.join(format!("{identifier}-{asset_name}.part")),
        directory.join(format!("{identifier}-{asset_name}")),
    )
}

/// Download the checked installer into a private temporary path and verify its
/// exact byte count and SHA-256 before it can be launched.
pub async fn download_and_verify(check: &UpdateCheck) -> Result<PreparedUpdate> {
    ensure!(check.update_available, "当前没有可安装的客户端更新");
    let asset = check
        .asset
        .as_ref()
        .context("更新资产尚未就绪，请重新检查")?;
    let directory = create_update_cache_dir().await?;
    let identifier = Uuid::new_v4();
    let (partial, destination) = cache_paths(&directory, identifier, &asset.name);
    let outcome = async {
        let (response, _) =
            request_with_proxy_fallback(&asset.url, DOWNLOAD_TIMEOUT, check.proxy_fallback_used)
                .await?;
        if let Some(length) = response.content_length() {
            ensure!(
                length == asset.size,
                "安装器下载长度与 GitHub Release 元数据不符"
            );
        }
        let mut file = tokio::fs::File::create(&partial)
            .await
            .context("无法创建安装器临时文件")?;
        let mut stream = response.bytes_stream();
        let mut bytes_written = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("下载安装器时连接中断")?;
            bytes_written = bytes_written
                .checked_add(chunk.len() as u64)
                .context("安装器大小溢出")?;
            ensure!(
                bytes_written <= asset.size,
                "安装器下载长度超过 GitHub Release 元数据"
            );
            digest.update(&chunk);
            file.write_all(&chunk).await.context("写入安装器失败")?;
        }
        file.flush().await.context("刷新安装器文件失败")?;
        drop(file);
        ensure!(bytes_written == asset.size, "安装器下载不完整");
        let actual = format!("{:x}", digest.finalize());
        ensure!(
            actual.eq_ignore_ascii_case(&asset.sha256),
            "安装器 SHA-256 与 GitHub Release 清单不匹配"
        );
        tokio::fs::rename(&partial, &destination)
            .await
            .context("无法完成安装器下载")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
    }
    outcome?;
    Ok(PreparedUpdate {
        version: check.latest_version.clone(),
        installer: destination,
    })
}

fn installation_directory(executable: &Path) -> Result<PathBuf> {
    ensure!(
        executable
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("wonderland-desktop.exe")),
        "当前客户端不是受支持的 Windows 桌面安装程序"
    );
    let directory = executable
        .parent()
        .context("无法确定当前客户端安装目录")?
        .to_path_buf();
    ensure!(
        directory.join("wonderland.exe").is_file()
            && directory.join("Start-Wonderland.ps1").is_file()
            && directory.join("data-location.txt").is_file(),
        "当前客户端不在完整安装目录中；请从 GitHub Release 手动安装"
    );
    Ok(directory)
}

const UPDATE_HELPER: &str = r#"param(
  [Parameter(Mandatory = $true)][int]$DesktopPid,
  [Parameter(Mandatory = $true)][string]$Installer,
  [Parameter(Mandatory = $true)][string]$InstallDir,
  [Parameter(Mandatory = $true)][string]$LogPath
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
function Write-UpdateLog([string]$Message) {
  [IO.File]::WriteAllText($LogPath, $Message, [System.Text.UTF8Encoding]::new($true))
}
try {
  $deadline = [DateTime]::UtcNow.AddMinutes(2)
  while ((Get-Process -Id $DesktopPid -ErrorAction SilentlyContinue) -and ([DateTime]::UtcNow -lt $deadline)) {
    Start-Sleep -Milliseconds 200
  }
  if (Get-Process -Id $DesktopPid -ErrorAction SilentlyContinue) {
    throw '桌面窗口未在两分钟内退出，更新已取消。'
  }
  $installTarget = if ($InstallDir.EndsWith('\')) { $InstallDir + '.' } else { $InstallDir }
  $setupArgs = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /DIR="{0}"' -f $installTarget
  $setup = Start-Process -FilePath $Installer -ArgumentList $setupArgs -PassThru -Wait -WindowStyle Hidden
  if ($setup.ExitCode -ne 0) {
    throw ('安装器退出码: {0}' -f $setup.ExitCode)
  }
  $launcher = Join-Path $InstallDir 'Start-Wonderland.ps1'
  if (-not (Test-Path -LiteralPath $launcher)) {
    throw '更新完成后找不到 Wonderland 启动器。'
  }
  $shell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
  $launchArgs = '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "{0}" -NonInteractive' -f $launcher
  $launch = Start-Process -FilePath $shell -ArgumentList $launchArgs -WorkingDirectory $InstallDir -WindowStyle Hidden -PassThru -Wait
  if ($launch.ExitCode -ne 0) {
    throw ('更新已安装，但新版本启动失败（退出码: {0}）。' -f $launch.ExitCode)
  }
  Remove-Item -LiteralPath $Installer -Force -ErrorAction SilentlyContinue
  Write-UpdateLog 'completed: installer and desktop restart succeeded; cached installer removed'
} catch {
  $message = 'failed: ' + $_.Exception.Message
  Write-UpdateLog $message
  try {
    Add-Type -AssemblyName PresentationFramework
    [System.Windows.MessageBox]::Show(
      "Wonderland 更新失败。详细信息已写入：`n$LogPath`n`n$message",
      'Wonderland 更新',
      'OK',
      'Error'
    ) | Out-Null
  } catch {}
}
"#;

fn update_helper_bytes() -> Vec<u8> {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(UPDATE_HELPER.as_bytes());
    bytes
}

#[cfg(windows)]
fn powershell_executable() -> PathBuf {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

/// Write and start the short-lived updater helper. It never terminates a
/// process: it only waits for this exact desktop PID to exit, then delegates
/// file-lock handling to Inno Setup's Restart Manager for this install path.
#[cfg(windows)]
pub fn launch(prepared: &PreparedUpdate) -> Result<()> {
    let executable = env::current_exe().context("无法定位当前桌面程序")?;
    let installation = installation_directory(&executable)?;
    let installer = prepared
        .installer
        .canonicalize()
        .context("已下载的安装器不存在，请重新下载")?;
    ensure!(installer.is_file(), "已下载的安装器不是文件");
    let helper_dir = installer.parent().context("无法确定安装器缓存目录")?;
    let helper = helper_dir.join(format!(
        "launch-{}-{}.ps1",
        prepared.version,
        Uuid::new_v4()
    ));
    let log = helper.with_extension("log");
    fs::write(&helper, update_helper_bytes()).context("无法写入更新启动器")?;
    let mut command = Command::new(powershell_executable());
    command
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-WindowStyle")
        .arg("Hidden")
        .arg("-File")
        .arg(&helper)
        .arg("-DesktopPid")
        .arg(std::process::id().to_string())
        .arg("-Installer")
        .arg(&installer)
        .arg("-InstallDir")
        .arg(&installation)
        .arg("-LogPath")
        .arg(log);
    command.spawn().context("无法启动安装器助手")?;
    Ok(())
}

#[cfg(not(windows))]
pub fn launch(_prepared: &PreparedUpdate) -> Result<()> {
    anyhow::bail!("一键更新当前仅支持 Windows 安装版")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool, prerelease: bool) -> GitHubRelease {
        let version = tag.strip_prefix('v').unwrap_or(tag);
        GitHubRelease {
            tag_name: tag.into(),
            draft,
            prerelease,
            assets: vec![
                GitHubAsset {
                    name: setup_name(version),
                    size: 42,
                },
                GitHubAsset {
                    name: CHECKSUM_FILE.into(),
                    size: 70,
                },
            ],
        }
    }

    #[test]
    fn version_parser_orders_stable_release_versions() {
        assert!(parse_version("0.11.2").unwrap() > parse_version("0.11.1").unwrap());
        assert!(parse_version("1.0.0").unwrap() > parse_version("0.99.99").unwrap());
        for invalid in ["v0.11.2", "0.11", "0.11.02", "0.11.x", "0.11.2-beta"] {
            assert!(parse_version(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn release_selection_requires_exact_stable_assets() {
        let (version, _, installer, manifest) =
            release_plan(release("v0.11.2", false, false)).unwrap();
        assert_eq!(version, "0.11.2");
        assert_eq!(installer.name, "Wonderland-Setup-0.11.2-x64.exe");
        assert_eq!(manifest.name, CHECKSUM_FILE);
        assert!(release_plan(release("v0.11.2", true, false)).is_err());
        assert!(release_plan(release("v0.11.2", false, true)).is_err());
        let mut missing = release("v0.11.2", false, false);
        missing.assets.remove(0);
        assert!(release_plan(missing).is_err());
    }

    #[test]
    fn checksum_parser_accepts_standard_manifest_lines_and_rejects_ambiguity() {
        let hash = "a".repeat(64);
        let file = "Wonderland-Setup-0.11.2-x64.exe";
        assert_eq!(
            checksum_from_manifest(&format!("{hash} *{file}\n"), file).unwrap(),
            hash
        );
        assert!(checksum_from_manifest(&format!("{}  {file}\n", "z".repeat(64)), file).is_err());
        assert!(
            checksum_from_manifest(&format!("{hash}  {file}\n{hash} *{file}\n"), file).is_err()
        );
        assert!(checksum_from_manifest("# no installer\n", file).is_err());
    }

    #[test]
    fn loopback_proxy_detection_never_bypasses_remote_proxy() {
        assert!(proxy_value_is_loopback("http://127.0.0.1:9"));
        assert!(proxy_value_is_loopback("localhost:8080"));
        assert!(proxy_value_is_loopback("http://[::1]:3128"));
        assert!(!proxy_value_is_loopback("https://proxy.example.test:443"));
        assert!(!proxy_value_is_loopback("not a proxy"));
    }

    #[test]
    fn installation_directory_requires_the_packaged_desktop_layout() {
        let directory = tempfile::tempdir().unwrap();
        let desktop = directory.path().join("wonderland-desktop.exe");
        fs::write(&desktop, "desktop").unwrap();
        assert!(installation_directory(&desktop).is_err());
        fs::write(directory.path().join("wonderland.exe"), "backend").unwrap();
        fs::write(directory.path().join("Start-Wonderland.ps1"), "launcher").unwrap();
        assert!(installation_directory(&desktop).is_err());
        fs::write(
            directory.path().join("data-location.txt"),
            "D:/WonderlandData",
        )
        .unwrap();
        assert_eq!(installation_directory(&desktop).unwrap(), directory.path());
    }

    #[test]
    fn helper_waits_for_one_pid_and_never_terminates_processes() {
        assert!(UPDATE_HELPER.contains("Get-Process -Id $DesktopPid"));
        assert!(UPDATE_HELPER.contains("/CLOSEAPPLICATIONS"));
        assert!(UPDATE_HELPER.contains("/DIR=\"{0}\""));
        assert!(UPDATE_HELPER.contains("$InstallDir.EndsWith('\\')"));
        assert!(UPDATE_HELPER.contains("-NonInteractive"));
        assert!(UPDATE_HELPER.contains("-PassThru -Wait"));
        assert!(!UPDATE_HELPER.contains("Stop-Process"));
        assert!(!UPDATE_HELPER.contains("taskkill"));
    }

    #[test]
    fn cache_destination_keeps_the_executable_extension() {
        let directory = Path::new("D:/WonderlandData/updates");
        let (partial, destination) =
            cache_paths(directory, Uuid::nil(), "Wonderland-Setup-0.11.2-x64.exe");
        assert_eq!(
            destination.extension().and_then(|value| value.to_str()),
            Some("exe")
        );
        assert_eq!(
            partial.extension().and_then(|value| value.to_str()),
            Some("part")
        );
        assert!(destination.starts_with(directory));
    }

    #[test]
    fn helper_is_utf8_bom_for_windows_powershell() {
        assert!(update_helper_bytes().starts_with(&[0xEF, 0xBB, 0xBF]));
    }
}
