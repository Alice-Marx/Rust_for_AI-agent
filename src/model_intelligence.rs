//! Live, source-backed model intelligence.
//!
//! This module deliberately does not manufacture prices or turn a benchmark
//! score into a routing guarantee. Every refresh contacts both upstream GitHub
//! repositories, discovers the newest published release from `constants.js`,
//! validates the table/categories pair, and writes an immutable snapshot. A
//! failed refresh leaves the previous snapshot's original timestamp intact and
//! records the failure separately.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use chrono::{SecondsFormat, Utc};
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

const NEW_LIVEBENCH_REPO: &str = "LiveBench/new-livebench";
const LIVEBENCH_REPO: &str = "LiveBench/LiveBench";
const GITHUB_API: &str = "https://api.github.com/repos";
const RAW_GITHUB: &str = "https://raw.githubusercontent.com";
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONSTANTS_BYTES: usize = 2 * 1024 * 1024;
const MAX_ROWS: usize = 100_000;
const MAX_MODEL_BYTES: usize = 512;
const MAX_CATEGORY_BYTES: usize = 512;
const MAX_STATUS_ERROR_BYTES: usize = 4_096;
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
const MAX_CATEGORIES: usize = 128;
const MAX_TASKS: usize = 1_024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelScore {
    pub model: String,
    pub scores: BTreeMap<String, f64>,
    #[serde(default)]
    pub missing_tasks: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub parser_version: u32,
    pub id: String,
    pub release: String,
    pub checked_at: String,
    pub new_livebench_sha: String,
    pub livebench_sha: String,
    pub table_url: String,
    pub categories_url: String,
    pub constants_url: String,
    pub table_sha256: String,
    pub categories_sha256: String,
    pub constants_sha256: String,
    pub models: Vec<ModelScore>,
    pub categories: Vec<String>,
    pub category_tasks: BTreeMap<String, Vec<String>>,
    /// The source bytes are retained so a decision can be audited or re-parsed
    /// when the decoder changes. They are bounded by MAX_SOURCE_BYTES.
    pub raw_table: String,
    pub raw_categories: Value,
    pub raw_categories_json: String,
    pub raw_constants: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Failure {
    checked_at: String,
    error: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedStatus {
    latest_snapshot: Option<StatusSnapshot>,
    latest_failure: Option<Failure>,
    last_checked_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StatusSnapshot {
    id: String,
    release: String,
    checked_at: String,
    new_livebench_sha: String,
    livebench_sha: String,
    table_sha256: String,
    categories_sha256: String,
    constants_sha256: String,
}

struct StatusState {
    persisted: PersistedStatus,
    latest: Option<Snapshot>,
}

pub struct ModelIntelligence {
    data_dir: PathBuf,
    refresh_lock: Arc<Mutex<()>>,
    state: Arc<RwLock<StatusState>>,
    client: Client,
}

impl ModelIntelligence {
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.into();
        let snapshots_dir = data_dir.join("model-intelligence").join("snapshots");
        fs::create_dir_all(&snapshots_dir).context("无法创建模型智能快照目录")?;
        let status_path = data_dir.join("model-intelligence").join("status.json");
        let mut persisted: PersistedStatus =
            match read_bounded_file(&status_path, MAX_CONSTANTS_BYTES) {
                Ok(bytes) if !bytes.is_empty() => serde_json::from_slice(&bytes)
                    .context("模型智能状态文件损坏；请移除 status.json 后重试")?,
                Ok(_) => PersistedStatus::default(),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    PersistedStatus::default()
                }
                Err(error) => return Err(error).context("无法读取模型智能状态文件"),
            };
        let latest = match persisted.latest_snapshot.as_ref() {
            Some(reference) => match load_snapshot(&snapshots_dir, reference) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    persisted.latest_failure = Some(Failure {
                        checked_at: timestamp(),
                        error: format!("本地快照无法验证：{error:#}"),
                    });
                    None
                }
            },
            None => None,
        };
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static("Wonderland-ModelIntelligence/0.1"),
        );
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-cache"),
        );
        let client = Client::builder()
            .default_headers(headers)
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 {
                    return attempt.error("LiveBench 重定向次数超过上限");
                }
                let target = attempt.url();
                if target.scheme() != "https"
                    || !matches!(
                        target.host_str(),
                        Some("api.github.com" | "raw.githubusercontent.com" | "github.com")
                    )
                {
                    return attempt.error("LiveBench 重定向离开了允许的 HTTPS 来源");
                }
                attempt.follow()
            }))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .context("无法创建 LiveBench HTTP 客户端")?;
        Ok(Self {
            data_dir,
            refresh_lock: Arc::new(Mutex::new(())),
            state: Arc::new(RwLock::new(StatusState { persisted, latest })),
            client,
        })
    }

    /// Refresh is intentionally online every time. Cached data can be viewed
    /// through `status`, but it never satisfies a new routing epoch by itself.
    pub async fn refresh(&self) -> Result<Snapshot> {
        let _guard = self.refresh_lock.lock().await;
        let checked_at = timestamp();
        let result = self.refresh_online(&checked_at).await;
        match result {
            Ok(snapshot) => {
                if let Err(error) = self.persist_success(&snapshot).await {
                    self.persist_failure(&checked_at, &error);
                    return Err(error);
                }
                Ok(snapshot)
            }
            Err(error) => {
                self.persist_failure(&checked_at, &error);
                Err(error)
            }
        }
    }

    pub fn status(&self) -> Value {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let latest = state.persisted.latest_snapshot.as_ref().map(|snapshot| {
            json!({
                "id": snapshot.id,
                "release": snapshot.release,
                "checked_at": snapshot.checked_at,
                "new_livebench_sha": snapshot.new_livebench_sha,
                "livebench_sha": snapshot.livebench_sha,
                "table_sha256": snapshot.table_sha256,
                "categories_sha256": snapshot.categories_sha256,
                "constants_sha256": snapshot.constants_sha256,
            })
        });
        json!({
            "source": {
                "new_livebench": format!("https://github.com/{NEW_LIVEBENCH_REPO}"),
                "livebench": format!("https://github.com/{LIVEBENCH_REPO}"),
                "table_format": "public/table_<published-release>.csv",
                "categories_format": "public/categories_<published-release>.json",
            },
            "latest_snapshot": latest,
            "cached_snapshot_available": state.latest.is_some(),
            "latest_failure": state.persisted.latest_failure,
            "last_checked_at": state.persisted.last_checked_at,
            "cache_is_stale_after_failure": state.persisted.latest_failure.is_some(),
            "pricing": {"status":"unverified", "quotes":[], "official_sources":pricing_sources(), "note":"仅登记官方来源链接，尚未在线解析并核验适用渠道报价"},
            "identity_mapping_status":"unverified",
            "auto_dispatch_ready": false,
        })
    }

    pub fn latest_snapshot(&self) -> Option<Snapshot> {
        self.state
            .read()
            .ok()
            .and_then(|state| state.latest.clone())
    }

    async fn refresh_online(&self, checked_at: &str) -> Result<Snapshot> {
        let (new_sha, live_sha) = tokio::join!(
            self.github_main_sha(NEW_LIVEBENCH_REPO),
            self.github_main_sha(LIVEBENCH_REPO)
        );
        let (new_sha, live_sha) = match (new_sha, live_sha) {
            (Ok(new_sha), Ok(live_sha)) => (new_sha, live_sha),
            (new_sha, live_sha) => {
                let failures = [new_sha.err(), live_sha.err()]
                    .into_iter()
                    .flatten()
                    .map(|error| format!("{error:#}"))
                    .collect::<Vec<_>>();
                anyhow::bail!("LiveBench main SHA 核验失败：{}", failures.join("; "));
            }
        };
        let constants_url =
            format!("{RAW_GITHUB}/{NEW_LIVEBENCH_REPO}/{new_sha}/src/lib/constants.js");
        let constants_bytes = self
            .fetch_bounded(&constants_url, MAX_CONSTANTS_BYTES)
            .await?;
        let constants = String::from_utf8(constants_bytes.clone())
            .context("LiveBench constants.js 不是 UTF-8")?;
        let release = discover_release(&constants)?;
        // The published constants use hyphens while repository filenames use
        // underscores; derive this path convention rather than hardcoding a date.
        let release_file = release.replace('-', "_");
        let table_url =
            format!("{RAW_GITHUB}/{NEW_LIVEBENCH_REPO}/{new_sha}/public/table_{release_file}.csv");
        let categories_url = format!(
            "{RAW_GITHUB}/{NEW_LIVEBENCH_REPO}/{new_sha}/public/categories_{release_file}.json"
        );
        let table_bytes = self.fetch_bounded(&table_url, MAX_SOURCE_BYTES).await?;
        let categories_bytes = self
            .fetch_bounded(&categories_url, MAX_SOURCE_BYTES)
            .await?;
        ensure!(!table_bytes.is_empty(), "LiveBench table 为空");
        let raw_table =
            String::from_utf8(table_bytes.clone()).context("LiveBench table 不是 UTF-8 CSV")?;
        ensure!(
            raw_table.len() <= MAX_SOURCE_BYTES,
            "LiveBench table 超过大小上限"
        );
        let raw_categories = parse_categories_json(&categories_bytes)?;
        let category_tasks = parse_category_map(&raw_categories)?;
        let categories = category_tasks.keys().cloned().collect();
        let subtasks = category_tasks
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        let models = parse_table(&raw_table, &subtasks)?;
        Ok(Snapshot {
            parser_version: 1,
            id: Uuid::new_v4().to_string(),
            release,
            checked_at: checked_at.to_owned(),
            new_livebench_sha: new_sha,
            livebench_sha: live_sha,
            table_url,
            categories_url,
            constants_url,
            table_sha256: sha256_hex(&table_bytes),
            categories_sha256: sha256_hex(&categories_bytes),
            constants_sha256: sha256_hex(&constants_bytes),
            models,
            categories,
            category_tasks,
            raw_table,
            raw_categories,
            raw_categories_json: String::from_utf8(categories_bytes)
                .context("LiveBench categories 不是 UTF-8")?,
            raw_constants: constants,
        })
    }

    async fn github_main_sha(&self, repository: &str) -> Result<String> {
        ensure!(
            matches!(repository, NEW_LIVEBENCH_REPO | LIVEBENCH_REPO),
            "unrecognized LiveBench repository"
        );
        let url = format!("{GITHUB_API}/{repository}/commits/main");
        let bytes = match self.fetch_bounded(&url, 1_048_576).await {
            Ok(bytes) => bytes,
            Err(api_error) => {
                // GitHub's public REST quota is independent of its official Git
                // transport. Fetch fresh advertised refs from the same repository;
                // no cached SHA, subprocess credential lookup or branch guess.
                let refs_url = format!(
                    "https://github.com/{repository}.git/info/refs?service=git-upload-pack"
                );
                let refs = self
                    .fetch_bounded(&refs_url, MAX_SOURCE_BYTES)
                    .await
                    .with_context(|| {
                        format!("GitHub REST failed ({api_error}); official Git refs also failed")
                    })?;
                return parse_advertised_main(&refs);
            }
        };
        let value: Value = serde_json::from_slice(&bytes).context("GitHub commit 响应不是 JSON")?;
        let sha = value
            .get("sha")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        ensure!(is_sha(sha), "GitHub 返回了无效的 {repository} main SHA");
        Ok(sha.to_owned())
    }

    async fn fetch_bounded(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("LiveBench 在线请求失败")?;
        let status = response.status();
        ensure!(
            status == StatusCode::OK,
            "LiveBench 在线请求返回 HTTP {status}: {url}"
        );
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("LiveBench 在线响应读取失败")?;
            ensure!(
                body.len().saturating_add(chunk.len()) <= max_bytes,
                "LiveBench 在线响应超过大小上限"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn persist_success(&self, snapshot: &Snapshot) -> Result<()> {
        let directory = self.data_dir.join("model-intelligence");
        let snapshots = directory.join("snapshots");
        ensure!(Uuid::parse_str(&snapshot.id).is_ok(), "快照 ID 无效");
        let bytes = serde_json::to_vec(snapshot)?;
        ensure!(bytes.len() <= MAX_SNAPSHOT_BYTES, "快照超过大小上限");
        write_snapshot(&snapshots.join(format!("{}.json", snapshot.id)), &bytes)?;
        let mut guard = self
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("模型智能状态锁已损坏"))?;
        let mut next = guard.persisted.clone();
        next.latest_snapshot = Some(StatusSnapshot::from(snapshot));
        next.latest_failure = None;
        next.last_checked_at = Some(snapshot.checked_at.clone());
        let status = serde_json::to_vec_pretty(&next)?;
        atomic_write(&directory.join("status.json"), &status)?;
        guard.persisted = next;
        guard.latest = Some(snapshot.clone());
        Ok(())
    }

    fn persist_failure(&self, checked_at: &str, error: &anyhow::Error) {
        let error_text = truncate_error(&format!("{error:#}"));
        if let Ok(mut guard) = self.state.write() {
            guard.persisted.last_checked_at = Some(checked_at.to_owned());
            guard.persisted.latest_failure = Some(Failure {
                checked_at: checked_at.to_owned(),
                error: error_text,
            });
            if let Ok(bytes) = serde_json::to_vec_pretty(&guard.persisted) {
                if let Err(error) = atomic_write(
                    &self.data_dir.join("model-intelligence").join("status.json"),
                    &bytes,
                ) {
                    tracing::warn!(%error, "Unable to persist model intelligence failure status");
                }
            }
        }
    }
}

fn parse_advertised_main(bytes: &[u8]) -> Result<String> {
    let mut cursor = 0usize;
    let mut found = None;
    let mut service = false;
    while cursor < bytes.len() {
        ensure!(cursor + 4 <= bytes.len(), "truncated Git ref frame");
        let size = usize::from_str_radix(std::str::from_utf8(&bytes[cursor..cursor + 4])?, 16)
            .context("invalid Git ref frame size")?;
        cursor += 4;
        if size == 0 {
            continue;
        }
        ensure!(
            size >= 4 && cursor + size - 4 <= bytes.len(),
            "invalid Git ref frame length"
        );
        let frame = std::str::from_utf8(&bytes[cursor..cursor + size - 4])?;
        cursor += size - 4;
        if frame == "# service=git-upload-pack\n" {
            service = true;
            continue;
        }
        let reference = frame
            .split('\0')
            .next()
            .unwrap_or("")
            .trim_end_matches('\n');
        if let Some((sha, name)) = reference.split_once(' ') {
            if name == "refs/heads/main" {
                ensure!(
                    is_sha(sha) && found.is_none(),
                    "invalid or duplicate advertised main revision"
                );
                found = Some(sha.to_owned());
            }
        }
    }
    ensure!(service, "unexpected Git ref service");
    found.context("official Git transport did not advertise refs/heads/main")
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

impl From<&Snapshot> for StatusSnapshot {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            id: snapshot.id.clone(),
            release: snapshot.release.clone(),
            checked_at: snapshot.checked_at.clone(),
            new_livebench_sha: snapshot.new_livebench_sha.clone(),
            livebench_sha: snapshot.livebench_sha.clone(),
            table_sha256: snapshot.table_sha256.clone(),
            categories_sha256: snapshot.categories_sha256.clone(),
            constants_sha256: snapshot.constants_sha256.clone(),
        }
    }
}

fn pricing_sources() -> Value {
    json!([
        {"provider":"OpenAI","billing_channel":"api","url":"https://openai.com/api/pricing/","status":"unverified"},
        {"provider":"Anthropic","billing_channel":"api","url":"https://platform.claude.com/docs/en/about-claude/pricing","status":"unverified"},
        {"provider":"Kimi","billing_channel":"api","url":"https://platform.moonshot.ai/docs/pricing/chat","status":"unverified"},
        {"provider":"DeepSeek","billing_channel":"api","url":"https://api-docs.deepseek.com/quick_start/pricing","status":"unverified"}
    ])
}

fn read_bounded_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    ensure!(file.metadata()?.len() <= limit as u64, "文件超过大小上限");
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "文件超过大小上限");
    Ok(bytes)
}

fn load_snapshot(directory: &Path, reference: &StatusSnapshot) -> Result<Snapshot> {
    let id = Uuid::parse_str(&reference.id).context("快照 ID 无效")?;
    ensure!(id.to_string() == reference.id, "快照 ID 格式不规范");
    let bytes = read_bounded_file(&directory.join(format!("{id}.json")), MAX_SNAPSHOT_BYTES)?;
    let snapshot: Snapshot = serde_json::from_slice(&bytes).context("快照 JSON 无效")?;
    ensure!(snapshot.parser_version == 1, "快照解码版本不受支持");
    ensure!(
        StatusSnapshot::from(&snapshot) == *reference,
        "快照元数据不匹配"
    );
    chrono::DateTime::parse_from_rfc3339(&snapshot.checked_at).context("快照核验时间无效")?;
    ensure!(
        is_sha(&snapshot.new_livebench_sha) && is_sha(&snapshot.livebench_sha),
        "快照来源提交无效"
    );
    ensure!(
        snapshot.raw_table.len() <= MAX_SOURCE_BYTES
            && snapshot.raw_categories_json.len() <= MAX_SOURCE_BYTES
            && snapshot.raw_constants.len() <= MAX_CONSTANTS_BYTES,
        "快照原始来源超过大小上限"
    );
    ensure!(
        sha256_hex(snapshot.raw_table.as_bytes()) == snapshot.table_sha256
            && sha256_hex(snapshot.raw_categories_json.as_bytes()) == snapshot.categories_sha256
            && sha256_hex(snapshot.raw_constants.as_bytes()) == snapshot.constants_sha256,
        "快照原始来源散列不匹配"
    );
    ensure!(
        discover_release(&snapshot.raw_constants)? == snapshot.release,
        "快照发布日期与来源不匹配"
    );
    let base = format!(
        "{RAW_GITHUB}/{NEW_LIVEBENCH_REPO}/{}",
        snapshot.new_livebench_sha
    );
    let release_file = snapshot.release.replace('-', "_");
    ensure!(
        snapshot.constants_url == format!("{base}/src/lib/constants.js")
            && snapshot.table_url == format!("{base}/public/table_{release_file}.csv")
            && snapshot.categories_url == format!("{base}/public/categories_{release_file}.json"),
        "快照来源 URL 未绑定到核验提交"
    );
    let categories = parse_categories_json(snapshot.raw_categories_json.as_bytes())?;
    ensure!(
        categories == snapshot.raw_categories,
        "快照类别原始 JSON 不匹配"
    );
    let category_tasks = parse_category_map(&categories)?;
    ensure!(
        category_tasks == snapshot.category_tasks
            && category_tasks.keys().cloned().collect::<Vec<_>>() == snapshot.categories,
        "快照类别映射不匹配"
    );
    let tasks = category_tasks
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        parse_table(&snapshot.raw_table, &tasks)? == snapshot.models,
        "快照模型评分不匹配"
    );
    Ok(snapshot)
}

fn parse_categories_json(bytes: &[u8]) -> Result<Value> {
    struct CategoryObject;
    impl<'de> serde::de::Visitor<'de> for CategoryObject {
        type Value = Value;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an object with unique category names")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Value, M::Error> {
            let mut result = serde_json::Map::new();
            while let Some((key, value)) = map.next_entry::<String, Value>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate category name"));
                }
            }
            Ok(Value::Object(result))
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = serde::Deserializer::deserialize_map(&mut deserializer, CategoryObject)
        .context("LiveBench categories 不是有效的唯一类别 JSON 对象")?;
    deserializer
        .end()
        .context("LiveBench categories 包含多余内容")?;
    Ok(value)
}

fn is_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    publish_file(path, bytes, false)
}

fn write_snapshot(path: &Path, bytes: &[u8]) -> Result<()> {
    publish_file(path, bytes, true)
}

fn publish_file(path: &Path, bytes: &[u8], immutable: bool) -> Result<()> {
    let parent = path.parent().context("快照没有父目录")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if immutable {
            // A hard link publishes the complete file without replacing an
            // existing snapshot, including across concurrent processes.
            fs::hard_link(&temporary, path).context("无法发布不可变模型智能快照")?;
        } else {
            replace_file(&temporary, path).context("无法原子发布模型智能状态")?;
        }
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let ok = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn truncate_error(value: &str) -> String {
    let mut end = value.len().min(MAX_STATUS_ERROR_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Read only the release assignment; do not execute the JavaScript source.
fn discover_release(constants: &str) -> Result<String> {
    ensure!(
        constants.len() <= MAX_CONSTANTS_BYTES,
        "constants.js 超过大小上限"
    );
    let masked = mask_js_strings_and_comments(constants)?;
    let assignment = Regex::new(r"(?m)^[ \t]*(?:export\s+)?const\s+RELEASES\s*=\s*\[")?;
    let assignments = assignment.find_iter(&masked).collect::<Vec<_>>();
    ensure!(
        assignments.len() == 1,
        "constants.js 必须包含唯一的 RELEASES 字面量数组声明"
    );
    let bytes = constants.as_bytes();
    let mut index = assignments[0].end();
    let mut releases = BTreeSet::new();
    loop {
        skip_js_trivia(bytes, &mut index)?;
        if bytes.get(index) == Some(&b']') {
            index += 1;
            break;
        }
        let quote = *bytes.get(index).context("RELEASES 数组未闭合")?;
        ensure!(
            matches!(quote, b'\'' | b'"'),
            "RELEASES 仅支持日期字符串字面量"
        );
        index += 1;
        let start = index;
        while let Some(byte) = bytes.get(index) {
            if *byte == quote {
                break;
            }
            ensure!(
                byte.is_ascii_digit() || *byte == b'-',
                "RELEASES 包含非日期字符串"
            );
            index += 1;
        }
        ensure!(
            bytes.get(index) == Some(&quote),
            "RELEASES 日期字符串未闭合"
        );
        let date = &constants[start..index];
        validate_release(date)?;
        ensure!(releases.insert(date.to_owned()), "RELEASES 包含重复日期");
        index += 1;
        skip_js_trivia(bytes, &mut index)?;
        match bytes.get(index) {
            Some(b',') => index += 1,
            Some(b']') => {
                index += 1;
                break;
            }
            _ => anyhow::bail!("RELEASES 数组必须由逗号分隔的日期字面量组成"),
        }
    }
    skip_js_trivia(bytes, &mut index)?;
    ensure!(
        index == bytes.len() || bytes.get(index) == Some(&b';'),
        "RELEASES 不支持动态表达式"
    );
    releases
        .into_iter()
        .next_back()
        .context("constants.js 未找到有效的 LiveBench RELEASES 日期")
}

fn validate_release(value: &str) -> Result<()> {
    ensure!(
        value.len() == 10 && value.as_bytes()[4] == b'-' && value.as_bytes()[7] == b'-',
        "RELEASES 日期格式必须为 YYYY-MM-DD"
    );
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").context("RELEASES 日期无效")?;
    ensure!(
        date.format("%Y-%m-%d").to_string() == value,
        "RELEASES 日期格式不规范"
    );
    Ok(())
}

fn skip_js_trivia(bytes: &[u8], index: &mut usize) -> Result<()> {
    loop {
        while bytes.get(*index).is_some_and(u8::is_ascii_whitespace) {
            *index += 1;
        }
        if bytes.get(*index..*index + 2) == Some(b"//") {
            while bytes.get(*index).is_some_and(|byte| *byte != b'\n') {
                *index += 1;
            }
        } else if bytes.get(*index..*index + 2) == Some(b"/*") {
            *index += 2;
            while *index < bytes.len() && bytes.get(*index..*index + 2) != Some(b"*/") {
                *index += 1;
            }
            ensure!(*index < bytes.len(), "constants.js 块注释未闭合");
            *index += 2;
        } else {
            return Ok(());
        }
    }
}

fn mask_js_strings_and_comments(source: &str) -> Result<String> {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if matches!(bytes[index], b'\'' | b'"' | b'`') {
            let quote = bytes[index];
            index += 1;
            while index < bytes.len() && bytes[index] != quote {
                if bytes[index] == b'\\' {
                    index += 1;
                }
                index += 1;
            }
            ensure!(index < bytes.len(), "constants.js 字符串未闭合");
            index += 1;
        } else if bytes.get(index..index + 2) == Some(b"//")
            || bytes.get(index..index + 2) == Some(b"/*")
        {
            skip_js_trivia(bytes, &mut index)?;
        } else {
            index += 1;
            continue;
        }
        for byte in &mut masked[start..index] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(masked).context("constants.js UTF-8 无效")
}

fn parse_category_map(value: &Value) -> Result<BTreeMap<String, Vec<String>>> {
    let object = value
        .as_object()
        .context("LiveBench categories 必须是类别到子任务列表的对象")?;
    ensure!(
        !object.is_empty() && object.len() <= MAX_CATEGORIES,
        "LiveBench 类别数量无效"
    );
    let mut mapping = BTreeMap::new();
    let mut seen_tasks = BTreeSet::new();
    for (category, tasks) in object {
        validate_nonempty_bounded(category, "类别名称", MAX_CATEGORY_BYTES)?;
        let tasks = tasks
            .as_array()
            .context("LiveBench 类别必须包含子任务数组")?;
        ensure!(!tasks.is_empty(), "LiveBench 类别 {category} 没有子任务");
        let mut names = Vec::new();
        for task in tasks {
            let task = task.as_str().context("LiveBench 子任务名称必须是字符串")?;
            validate_nonempty_bounded(task, "子任务名称", MAX_CATEGORY_BYTES)?;
            ensure!(task != "model", "LiveBench 子任务不能使用模型标识列名");
            ensure!(
                seen_tasks.insert(task.to_owned()),
                "LiveBench 重复映射子任务：{task}"
            );
            ensure!(
                seen_tasks.len() <= MAX_TASKS,
                "LiveBench 子任务数量超过上限"
            );
            names.push(task.to_owned());
        }
        mapping.insert(category.to_owned(), names);
    }
    Ok(mapping)
}

fn parse_table(raw: &str, categories: &[String]) -> Result<Vec<ModelScore>> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(raw.as_bytes());
    let headers = reader
        .headers()
        .context("LiveBench table 缺少 CSV 表头")?
        .clone();
    let header_set = headers.iter().collect::<BTreeSet<_>>();
    ensure!(
        header_set.len() == headers.len(),
        "LiveBench table 包含重复列名"
    );
    let model_column = headers
        .iter()
        .position(|header| header == "model")
        .context("LiveBench table 缺少模型名称列")?;
    let category_names = categories
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    ensure!(
        !category_names.is_empty() && category_names.len() == categories.len(),
        "LiveBench 子任务映射无效"
    );
    ensure!(
        headers.len() == category_names.len() + 1
            && category_names.iter().all(|name| header_set.contains(name)),
        "LiveBench table 列与已发布的子任务映射不一致"
    );
    let mut score_columns = Vec::new();
    for (index, header) in headers.iter().enumerate() {
        if index == model_column {
            continue;
        }
        score_columns.push((index, header.to_owned()));
    }
    ensure!(
        !score_columns.is_empty(),
        "LiveBench table 没有可识别的评分列"
    );
    let mut models = Vec::new();
    let mut names = BTreeSet::new();
    for (row_number, result) in reader.records().enumerate() {
        ensure!(row_number < MAX_ROWS, "LiveBench table 行数超过上限");
        let record = result.context("LiveBench table CSV 行无效")?;
        let name = record.get(model_column).unwrap_or("");
        validate_model_name(name)?;
        ensure!(
            names.insert(name.to_owned()),
            "LiveBench table 存在重复模型名称：{name}"
        );
        let mut scores = BTreeMap::new();
        let mut missing_tasks = Vec::new();
        for (index, category) in &score_columns {
            let value = record.get(*index).unwrap_or("").trim();
            if value.is_empty() {
                missing_tasks.push(category.clone());
                continue;
            }
            let score: f64 = value
                .parse()
                .with_context(|| format!("LiveBench 模型 {name} 的评分 {category} 不是数字"))?;
            ensure!(
                score.is_finite() && (0.0..=100.0).contains(&score),
                "LiveBench 模型 {name} 的评分 {category} 超出 0-100"
            );
            scores.insert(category.clone(), score);
        }
        ensure!(!scores.is_empty(), "LiveBench 模型 {name} 没有任何评分");
        models.push(ModelScore {
            model: name.to_owned(),
            scores,
            missing_tasks,
        });
    }
    ensure!(!models.is_empty(), "LiveBench table 没有模型行");
    Ok(models)
}

fn validate_model_name(name: &str) -> Result<()> {
    validate_nonempty_bounded(name, "模型名称", MAX_MODEL_BYTES)
}

fn validate_nonempty_bounded(value: &str, label: &str, max: usize) -> Result<()> {
    ensure!(!value.is_empty(), "{label}不能为空");
    ensure!(value.len() <= max, "{label}超过大小上限");
    ensure!(value.trim() == value, "{label}包含首尾空白");
    ensure!(!value.chars().any(char::is_control), "{label}包含控制字符");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_git_refs_require_framing_service_and_exact_main() {
        let frame = |text: &str| format!("{:04x}{text}", text.len() + 4);
        let sha = "1234567890123456789012345678901234567890";
        let refs = format!(
            "{}0000{}{}0000",
            frame("# service=git-upload-pack\n"),
            frame(&format!("{sha} HEAD\0symref=HEAD:refs/heads/main\n")),
            frame(&format!("{sha} refs/heads/main\n"))
        );
        assert_eq!(parse_advertised_main(refs.as_bytes()).unwrap(), sha);
        assert!(parse_advertised_main(&refs.as_bytes()[..refs.len() - 1]).is_err());
        assert!(parse_advertised_main(
            refs.replace("refs/heads/main", "refs/heads/fake")
                .as_bytes()
        )
        .is_err());
        assert!(parse_advertised_main(
            format!("{refs}{}", frame(&format!("{sha} refs/heads/main\n"))).as_bytes()
        )
        .is_err());
    }

    #[test]
    fn release_discovery_uses_only_published_literal_dates() {
        let source = r#"// const RELEASES = ['2099-01-01'];
          export const RELEASES = ['2025-01-01', /* 2098-01-01 */ '2026-06-25',];
          const note = '2099-01-01';"#;
        assert_eq!(discover_release(source).unwrap(), "2026-06-25");
        assert!(discover_release("const RELEASES = ['yesterday'];").is_err());
        assert!(discover_release("const note = '2099-01-01';").is_err());
        assert!(discover_release("const RELEASES = ['bad-2027-01-01'];").is_err());
        assert!(discover_release("const RELEASES = ['2026-02-30'];").is_err());
        assert!(
            discover_release("const RELEASES = ['2026-06-25'].concat(['2099-01-01']);").is_err()
        );
        assert!(discover_release("const RELEASES = [computeDate()];").is_err());
        assert!(discover_release(
            "const RELEASES = ['2026-06-25'];\nconst RELEASES = ['2027-01-01'];"
        )
        .is_err());
        assert!(discover_release("const note = `\nconst RELEASES = ['2027-01-01'];\n`;").is_err());
    }

    #[test]
    fn table_parser_validates_names_scores_and_categories() {
        let raw = "model,Coding,Math\nkimi-k2.5,88.5,90\nclaude-sonnet,91,89\n";
        let rows = parse_table(raw, &["Coding".into(), "Math".into()]).unwrap();
        assert_eq!(rows[0].scores["Coding"], 88.5);
        assert!(parse_table("model,Coding\nx,101\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding\nx,\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding\nx,1\nx,2\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding\nx,NaN\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding\nx,-1\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding,Coding\nx,1,2\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding\nx,1,2\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding\nx,1\n", &["Math".into()]).is_err());
        assert!(parse_table("model,Coding,rank\nx,1,2\n", &["Coding".into()]).is_err());
        assert!(parse_table("model,Coding\n x,1\n", &["Coding".into()]).is_err());
    }

    #[test]
    fn table_keeps_exact_variants_and_explicit_missing_scores() {
        let rows = parse_table(
            "model,Coding,Math\n\"x,thinking-64k-high\",90,\nx,10,0\n",
            &["Coding".into(), "Math".into()],
        )
        .unwrap();
        assert_eq!(rows[0].model, "x,thinking-64k-high");
        assert_eq!(rows[0].missing_tasks, vec!["Math"]);
        assert!(!rows[0].scores.contains_key("Math"));
        assert_eq!(rows[1].scores["Math"], 0.0);
    }

    #[test]
    fn category_parser_rejects_duplicates_and_invalid_shapes() {
        assert_eq!(
            parse_category_map(&json!({"Coding":["code_generation"]})).unwrap()["Coding"],
            vec!["code_generation"]
        );
        assert!(parse_category_map(&json!([])).is_err());
        assert!(parse_category_map(&json!({"x":"\0"})).is_err());
        assert!(parse_category_map(&json!({"x":[]})).is_err());
        assert!(parse_category_map(&json!({"x":["a","a"]})).is_err());
        assert!(parse_category_map(&json!({"x":["a"],"y":["a"]})).is_err());
        assert!(parse_category_map(&json!({"x":["model"]})).is_err());
        assert!(parse_categories_json(br#"{"x":["a"],"x":["b"]}"#).is_err());
    }

    #[test]
    fn status_marks_cached_snapshot_stale_after_failure() {
        let state = PersistedStatus {
            latest_snapshot: Some(StatusSnapshot {
                id: "snap".into(),
                release: "2026-06-25".into(),
                checked_at: "2026-09-20T00:00:00Z".into(),
                new_livebench_sha: "a".repeat(40),
                livebench_sha: "b".repeat(40),
                table_sha256: "c".repeat(64),
                categories_sha256: "d".repeat(64),
                constants_sha256: "e".repeat(64),
            }),
            latest_failure: Some(Failure {
                checked_at: "2026-09-21T00:00:00Z".into(),
                error: "rate limited".into(),
            }),
            last_checked_at: Some("2026-09-21T00:00:00Z".into()),
        };
        assert!(state.latest_failure.is_some());
        assert_ne!(
            state.latest_snapshot.as_ref().unwrap().checked_at,
            state.last_checked_at.clone().unwrap()
        );
    }

    #[test]
    fn hash_is_stable_and_sha_validation_is_strict() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_sha(&"a".repeat(40)));
        assert!(!is_sha(&"a".repeat(39)));
        assert!(!is_sha(&"a".repeat(41)));
        assert!(!is_sha(&"g".repeat(40)));
        assert!(truncate_error(&"字".repeat(2_000)).len() <= MAX_STATUS_ERROR_BYTES);
    }

    fn fixture_snapshot() -> Snapshot {
        let raw_table = "model,code_generation,math_comp\nx-thinking-64k,88.5,90\n".to_owned();
        let raw_categories_json =
            r#"{"Coding":["code_generation"],"Mathematics":["math_comp"]}"#.to_owned();
        let raw_categories = parse_categories_json(raw_categories_json.as_bytes()).unwrap();
        let category_tasks = parse_category_map(&raw_categories).unwrap();
        let tasks = category_tasks
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        let raw_constants = "export const RELEASES = ['2026-06-25'];".to_owned();
        let new_livebench_sha = "a".repeat(40);
        let base = format!("{RAW_GITHUB}/{NEW_LIVEBENCH_REPO}/{new_livebench_sha}");
        Snapshot {
            parser_version: 1,
            id: Uuid::new_v4().to_string(),
            release: "2026-06-25".into(),
            checked_at: "2026-09-20T00:00:00Z".into(),
            new_livebench_sha,
            livebench_sha: "b".repeat(40),
            table_url: format!("{base}/public/table_2026_06_25.csv"),
            categories_url: format!("{base}/public/categories_2026_06_25.json"),
            constants_url: format!("{base}/src/lib/constants.js"),
            table_sha256: sha256_hex(raw_table.as_bytes()),
            categories_sha256: sha256_hex(raw_categories_json.as_bytes()),
            constants_sha256: sha256_hex(raw_constants.as_bytes()),
            models: parse_table(&raw_table, &tasks).unwrap(),
            categories: category_tasks.keys().cloned().collect(),
            category_tasks,
            raw_table,
            raw_categories,
            raw_categories_json,
            raw_constants,
        }
    }

    #[tokio::test]
    async fn failed_refresh_preserves_snapshot_timestamp_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let service = ModelIntelligence::open(directory.path()).unwrap();
        let snapshot = fixture_snapshot();
        service.persist_success(&snapshot).await.unwrap();
        service.persist_failure("2026-09-21T00:00:00Z", &anyhow::anyhow!("network offline"));
        drop(service);
        let service = ModelIntelligence::open(directory.path()).unwrap();
        assert_eq!(service.latest_snapshot(), Some(snapshot.clone()));
        let status = service.status();
        assert_eq!(status["latest_snapshot"]["checked_at"], snapshot.checked_at);
        assert_eq!(
            status["latest_failure"]["checked_at"],
            "2026-09-21T00:00:00Z"
        );
        assert_eq!(status["cache_is_stale_after_failure"], true);
        assert_eq!(status["auto_dispatch_ready"], false);
        assert_eq!(status["pricing"]["quotes"], json!([]));
    }

    #[tokio::test]
    async fn successful_epochs_keep_immutable_sources_and_reject_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let service = ModelIntelligence::open(directory.path()).unwrap();
        let first = fixture_snapshot();
        service.persist_success(&first).await.unwrap();
        let second = fixture_snapshot();
        service.persist_success(&second).await.unwrap();
        let snapshots = directory.path().join("model-intelligence/snapshots");
        assert_eq!(
            load_snapshot(&snapshots, &StatusSnapshot::from(&first)).unwrap(),
            first
        );
        assert_eq!(
            load_snapshot(&snapshots, &StatusSnapshot::from(&second)).unwrap(),
            second
        );
        assert!(service.persist_success(&second).await.is_err());
        assert_eq!(service.latest_snapshot().unwrap().id, second.id);
    }

    #[tokio::test]
    async fn status_write_failure_does_not_promote_new_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let service = ModelIntelligence::open(directory.path()).unwrap();
        let first = fixture_snapshot();
        service.persist_success(&first).await.unwrap();
        let status_path = directory.path().join("model-intelligence/status.json");
        fs::remove_file(&status_path).unwrap();
        fs::create_dir(&status_path).unwrap();
        assert!(service.persist_success(&fixture_snapshot()).await.is_err());
        assert_eq!(service.latest_snapshot(), Some(first.clone()));
        assert_eq!(service.status()["latest_snapshot"]["id"], first.id);
    }

    #[tokio::test]
    async fn cache_loading_detects_tampering_and_reference_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let service = ModelIntelligence::open(directory.path()).unwrap();
        let mut snapshot = fixture_snapshot();
        service.persist_success(&snapshot).await.unwrap();
        snapshot.models[0]
            .scores
            .insert("code_generation".into(), 100.0);
        let path = directory
            .path()
            .join("model-intelligence/snapshots")
            .join(format!("{}.json", snapshot.id));
        fs::write(path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let reopened = ModelIntelligence::open(directory.path()).unwrap();
        assert!(reopened.latest_snapshot().is_none());
        assert_eq!(reopened.status()["cached_snapshot_available"], false);
        assert!(reopened.status()["latest_failure"]["error"]
            .as_str()
            .unwrap()
            .contains("评分不匹配"));
        let mut reference = StatusSnapshot::from(&snapshot);
        reference.id = "../../outside".into();
        assert!(load_snapshot(directory.path(), &reference).is_err());
    }

    #[tokio::test]
    #[ignore = "requires an explicit online GitHub smoke test"]
    async fn live_refresh_fetches_and_persists_two_online_epochs() {
        let directory = tempfile::tempdir().unwrap();
        let service = ModelIntelligence::open(directory.path()).unwrap();
        let first = service.refresh().await.unwrap();
        let second = service.refresh().await.unwrap();
        assert_ne!(first.id, second.id);
        assert_ne!(first.checked_at, second.checked_at);
        assert!(!second.models.is_empty());
        let reopened = ModelIntelligence::open(directory.path()).unwrap();
        assert_eq!(reopened.latest_snapshot().unwrap(), second);
        println!(
            "release={} models={} new-livebench={} LiveBench={}",
            first.release,
            first.models.len(),
            first.new_livebench_sha,
            first.livebench_sha
        );
    }
}
