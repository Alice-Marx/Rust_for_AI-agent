use std::{
    io::{self, Read, Write},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use uuid::Uuid;
use wonderland::{
    cliproxy::{CliProxyLoginStart, CliProxyLoginStatus, CliProxyModel},
    model::{AgentRequest, AgentResponse},
    permissions::PermissionMode,
};

#[derive(Debug, Parser)]
#[command(name = "wonderland-cli", version, about = "Wonderland 终端客户端")]
struct Cli {
    /// Rust Agent HTTP 服务地址
    #[arg(
        long,
        env = "AGENT_SERVER_URL",
        default_value = "http://127.0.0.1:8080"
    )]
    server: String,

    /// 当前用户，用于长期记忆隔离
    #[arg(long, env = "AGENT_USER_ID", default_value = "local-user")]
    user_id: String,

    /// 当前会话 ID；不指定时自动生成
    #[arg(long, env = "AGENT_SESSION_ID")]
    session_id: Option<String>,

    /// 权限模式：default / plan / acceptEdits / bypassPermissions / dontAsk
    #[arg(long, env = "AGENT_PERMISSION_MODE", value_parser = parse_permission_mode)]
    mode: Option<PermissionMode>,

    /// 工具执行与权限规则的工作目录；同时决定自定义命令的加载位置
    #[arg(long, env = "AGENT_CWD")]
    cwd: Option<String>,

    /// 复用该用户最近一次会话，而不是新建会话
    #[arg(long = "continue", default_value_t = false)]
    resume: bool,

    /// 单次请求使用的模型；缺省由后端按 provider 决定
    #[arg(long, env = "AGENT_MODEL")]
    model: Option<String>,

    /// 关闭流式输出（默认开启：逐字显示回答与工具进度）
    #[arg(long = "no-stream", default_value_t = false)]
    no_stream: bool,

    /// 推理档位：low / medium / high（off 关闭）。仅对支持推理的模型生效
    #[arg(long, env = "AGENT_REASONING_EFFORT")]
    reasoning: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

fn parse_permission_mode(value: &str) -> Result<PermissionMode, String> {
    match value {
        "default" => Ok(PermissionMode::Default),
        "plan" => Ok(PermissionMode::Plan),
        "acceptEdits" => Ok(PermissionMode::AcceptEdits),
        "bypassPermissions" => Ok(PermissionMode::BypassPermissions),
        "dontAsk" => Ok(PermissionMode::DontAsk),
        other => Err(format!(
            "未知权限模式：{other}（可选 default / plan / acceptEdits / bypassPermissions / dontAsk）"
        )),
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 官方应用目录与适配能力
    Apps {
        /// Probe one registered local executable without a model call
        #[arg(long)]
        probe: Option<String>,
    },
    /// 查看或操作由后端持有的任务
    Work {
        #[command(subcommand)]
        action: WorkAction,
    },
    /// 创建和管理多模型团队任务（创建不会自动启动）
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },
    /// 查看官方价格快照、在线刷新或查询精确渠道报价
    Pricing {
        #[command(subcommand)]
        action: PricingAction,
    },
    /// 查看模型数据来源；--refresh 每次在线刷新 LiveBench
    Intelligence {
        #[arg(long)]
        refresh: bool,
    },
    /// 进入连续聊天模式；传入 prompt 时执行一次后退出
    Chat { prompt: Option<String> },
    /// 执行一次 Agent 请求
    Run {
        input: String,
        #[arg(long)]
        session_id: Option<String>,
        /// 显式启用本地 SKILL.md；可重复传入 --skill
        #[arg(long = "skill")]
        skills: Vec<String>,
    },
    /// 检查 Rust Agent 服务状态
    Health,
    /// 查看 CLIProxyAPI 可用模型
    Models,
    /// 验证 CLIProxyAPI 和指定模型
    Verify {
        #[arg(long)]
        model: Option<String>,
    },
    /// 发起 CLIProxyAPI OAuth 登录
    Login {
        provider: String,
        #[arg(long, default_value_t = false)]
        wait: bool,
    },
    /// 列出服务发现的本地 SKILL.md
    Skills,
    /// 列出后端保存的所有会话摘要
    Sessions,
    /// 查看指定会话的完整消息历史
    Session { id: String },
    /// 列出后端注册的全部工具（内置 + MCP）
    Tools,
    /// 列出已连接的 MCP 服务器及其工具
    Mcp,
    /// OAuth 登录配置中的 MCP 服务器，然后自动重新连接
    McpLogin { name: String },
    /// 重新加载当前目录的 MCP 配置
    McpReload,
    /// 查看模型能力档案
    Profile,
    /// 列出当前项目的自定义斜杠命令
    Commands,
    /// 列出已登录的订阅账号
    Accounts,
    /// 检索历史会话（关键词，走 SQLite 索引）
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum WorkAction {
    List,
    Get {
        id: String,
    },
    Events {
        id: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
    },
    Create {
        prompt: String,
        #[arg(long)]
        app: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        chat: bool,
        #[arg(long)]
        read_only: bool,
        #[arg(long)]
        start: bool,
    },
    Start {
        id: String,
    },
    Cancel {
        id: String,
    },
    Approve {
        id: String,
        request_id: String,
        #[arg(long)]
        allow: bool,
    },
    Answer {
        id: String,
        request_id: String,
        answers_json: String,
    },
    Accept {
        id: String,
        evidence: String,
    },
}

#[derive(Debug, Subcommand)]
enum TeamAction {
    List,
    /// 从 TeamCreate JSON 文件创建计划，随后使用 team start 启动
    Create {
        #[arg(long)]
        file: std::path::PathBuf,
    },
    Get {
        id: String,
    },
    Start {
        id: String,
    },
    Cancel {
        id: String,
    },
    Events {
        id: String,
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
        after: i64,
    },
}

#[derive(Debug, Subcommand)]
enum PricingAction {
    /// 显示缓存及核验状态，不发起官方来源刷新
    Status,
    /// 重新下载并核验官方来源
    Refresh,
    Quote {
        #[arg(long)]
        app: String,
        #[arg(long)]
        model: String,
        /// 显式计费渠道，例如 api 或 subscription；不从登录状态推断
        #[arg(long)]
        billing: String,
    },
}

fn team_request(action: TeamAction) -> Result<(String, Option<serde_json::Value>)> {
    let segment = |id: &str| -> Result<String> {
        anyhow::ensure!(!id.is_empty() && id != "." && id != "..", "团队 ID 无效");
        Ok(url::form_urlencoded::byte_serialize(id.as_bytes())
            .collect::<String>()
            .replace('+', "%20"))
    };
    Ok(match action {
        TeamAction::List => ("/api/v1/teams".into(), None),
        TeamAction::Create { file } => {
            const MAX_TEAM_FILE: usize = 8 * 1024 * 1024;
            let file = std::fs::File::open(&file)
                .with_context(|| format!("无法读取团队计划：{}", file.display()))?;
            anyhow::ensure!(
                file.metadata()?.is_file() && file.metadata()?.len() <= MAX_TEAM_FILE as u64,
                "团队计划必须是最多 8 MiB 的 JSON 文件"
            );
            let mut bytes = Vec::new();
            file.take(MAX_TEAM_FILE as u64 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= MAX_TEAM_FILE, "团队计划超过 8 MiB");
            let request: wonderland::team_store::TeamCreate =
                serde_json::from_slice(&bytes).context("团队计划 JSON 不符合 TeamCreate 格式")?;
            ("/api/v1/teams".into(), Some(serde_json::to_value(request)?))
        }
        TeamAction::Get { id } => (format!("/api/v1/teams/{}", segment(&id)?), None),
        TeamAction::Start { id } => (
            format!("/api/v1/teams/{}/start", segment(&id)?),
            Some(serde_json::json!({})),
        ),
        TeamAction::Cancel { id } => (
            format!("/api/v1/teams/{}/cancel", segment(&id)?),
            Some(serde_json::json!({})),
        ),
        TeamAction::Events { id, after } => (
            format!("/api/v1/teams/{}/events?after={after}", segment(&id)?),
            None,
        ),
    })
}

fn pricing_request(action: PricingAction) -> Result<(String, Option<serde_json::Value>)> {
    Ok(match action {
        PricingAction::Status => ("/api/v1/pricing".into(), None),
        PricingAction::Refresh => (
            "/api/v1/pricing/refresh".into(),
            Some(serde_json::json!({})),
        ),
        PricingAction::Quote {
            app,
            model,
            billing,
        } => {
            anyhow::ensure!(
                !app.trim().is_empty() && !model.trim().is_empty() && !billing.trim().is_empty(),
                "报价需要明确的应用、模型和计费渠道"
            );
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("app_id", &app)
                .append_pair("model", &model)
                .append_pair("billing_channel", &billing)
                .finish();
            (format!("/api/v1/pricing/quote?{query}"), None)
        }
    })
}

impl AgentApi {
    async fn work_request(
        &self,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let request = match body {
            Some(body) => self.client.post(self.url(path)).json(&body),
            None => self.client.get(self.url(path)),
        };
        let response = request.send().await.context("无法连接任务服务")?;
        let status = response.status();
        let value: serde_json::Value = response.json().await?;
        anyhow::ensure!(
            status.is_success(),
            "{status}: {}",
            value["error"].as_str().unwrap_or("请求失败")
        );
        Ok(value)
    }
}

#[derive(Clone)]
struct AgentApi {
    client: Client,
    base_url: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct HealthResponse {
    status: String,
    provider: String,
    agents: Vec<String>,
    sandbox_enabled: bool,
    cliproxyapi_configured: bool,
    skills: usize,
}

#[derive(Debug, Serialize)]
struct VerifyRequest {
    model: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionSummary {
    id: String,
    message_count: usize,
    updated_at: String,
    #[serde(default)]
    user_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionHit {
    id: String,
    message_count: usize,
    updated_at: String,
    #[serde(default)]
    snippet: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionDetail {
    id: String,
    todos: Vec<wonderland::model::TodoItem>,
    usage: wonderland::provider::Usage,
    updated_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct VerifyResponse {
    reachable: bool,
    model_count: usize,
    models: Vec<CliProxyModel>,
    selected_model: Option<String>,
    selected_model_available: Option<bool>,
}

impl AgentApi {
    fn new(server: String) -> Self {
        Self {
            client: wonderland::connection::service_client(),
            base_url: server.trim_end_matches('/').to_string(),
        }
    }

    async fn run(&self, request: AgentRequest) -> Result<AgentResponse> {
        self.client
            .post(self.url("/v1/agent/run"))
            .json(&request)
            .send()
            .await
            .context("无法连接 Rust Agent 服务")?
            .error_for_status()
            .context("Agent 请求失败")?
            .json()
            .await
            .context("无法解析 Agent 响应")
    }

    /// 流式运行：逐帧消费 SSE，把增量直接写到终端，最后返回完整响应。
    async fn run_stream(&self, request: AgentRequest) -> Result<AgentResponse> {
        use futures_util::StreamExt;

        let response = self
            .client
            .post(self.url("/v1/agent/stream"))
            .header("x-wonderland-interactive", {
                use std::io::IsTerminal;
                if io::stdin().is_terminal() {
                    "true"
                } else {
                    "false"
                }
            })
            .json(&request)
            .send()
            .await
            .context("无法连接 Rust Agent 流式接口")?;
        let status = response.status();
        if !status.is_success() {
            let raw = response.text().await.unwrap_or_default();
            anyhow::bail!("流式请求失败 {status}: {raw}");
        }
        let mut stream = response.bytes_stream();
        let mut decoder = wonderland::provider::SseBuffer::new();
        let mut final_response: Option<AgentResponse> = None;
        let mut failure: Option<String> = None;
        let mut printed_text = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("流式响应中断")?;
            for payload in decoder.push_bytes(&chunk) {
                let trimmed = payload.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let frame: serde_json::Value = serde_json::from_str(trimmed)
                    .with_context(|| format!("无法解析流式帧：{trimmed}"))?;
                match frame
                    .get("frame")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                {
                    "event" => {
                        let event = &frame["event"];
                        let kind = event
                            .get("type")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default();
                        match kind {
                            "permission_request" => {
                                eprintln!(
                                    "\n{} 请求执行：\n{}",
                                    event["tool"],
                                    serde_json::to_string_pretty(&event["input"])?
                                );
                                eprint!("允许这一次？[y/N] ");
                                io::stderr().flush()?;
                                let mut answer = String::new();
                                io::stdin().read_line(&mut answer)?;
                                let id = event["id"].as_str().context("missing approval ID")?;
                                self.client.post(self.url(&format!("/v1/permissions/{id}"))).json(&serde_json::json!({"allow":matches!(answer.trim(),"y"|"Y"|"yes")})).send().await?.error_for_status()?;
                            }
                            "text_delta" => {
                                print!(
                                    "{}",
                                    event
                                        .get("text")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or_default()
                                );
                                io::stdout().flush()?;
                                printed_text = true;
                            }
                            "reasoning_delta" => {
                                // 思维链写 stderr，避免污染可复制的回答正文。
                                eprint!(
                                    "{}",
                                    event
                                        .get("text")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or_default()
                                );
                            }
                            "tool_call" => {
                                eprintln!(
                                    "[工具] {}",
                                    event
                                        .get("name")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("?")
                                );
                            }
                            "tool_result" => {
                                let error = event
                                    .get("is_error")
                                    .and_then(|value| value.as_bool())
                                    .unwrap_or(false);
                                let head = event
                                    .get("content")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or_default()
                                    .lines()
                                    .next()
                                    .unwrap_or_default();
                                eprintln!("[工具结果{}] {head}", if error { " 失败" } else { "" });
                            }
                            "failed" => {
                                failure = event
                                    .get("message")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_string);
                            }
                            _ => {}
                        }
                    }
                    "response" => {
                        final_response = serde_json::from_value(frame["response"].clone()).ok();
                    }
                    "error" => {
                        failure = frame
                            .get("message")
                            .and_then(|value| value.as_str())
                            .map(str::to_string);
                    }
                    _ => {}
                }
            }
        }
        if printed_text {
            println!();
        }
        if let Some(response) = final_response {
            if !printed_text {
                println!("{}", response.output);
            }
            return Ok(response);
        }
        if let Some(message) = failure {
            anyhow::bail!("Agent 运行失败：{message}");
        }
        anyhow::bail!("流式响应提前结束，没有收到最终结果")
    }

    async fn health(&self) -> Result<HealthResponse> {
        self.client
            .get(self.url("/health"))
            .send()
            .await
            .context("无法连接 Rust Agent 服务")?
            .error_for_status()?
            .json()
            .await
            .context("无法解析健康检查响应")
    }

    async fn models(&self) -> Result<Vec<CliProxyModel>> {
        self.client
            .get(self.url("/v1/models"))
            .send()
            .await
            .context("无法连接 CLIProxyAPI 模型接口")?
            .error_for_status()
            .context("查询 CLIProxyAPI 模型失败")?
            .json()
            .await
            .context("无法解析模型列表")
    }

    async fn verify(&self, model: Option<String>) -> Result<VerifyResponse> {
        self.client
            .post(self.url("/v1/providers/cliproxyapi/verify"))
            .json(&VerifyRequest { model })
            .send()
            .await
            .context("无法连接 CLIProxyAPI 验证接口")?
            .error_for_status()
            .context("CLIProxyAPI 验证失败")?
            .json()
            .await
            .context("无法解析验证响应")
    }

    async fn login(&self, provider: &str) -> Result<CliProxyLoginStart> {
        self.client
            .post(self.url("/v1/providers/cliproxyapi/login"))
            .json(&serde_json::json!({ "provider": provider }))
            .send()
            .await
            .context("无法连接 CLIProxyAPI 登录接口")?
            .error_for_status()
            .context("CLIProxyAPI 登录启动失败")?
            .json()
            .await
            .context("无法解析登录响应")
    }

    async fn login_status(&self, state: &str) -> Result<CliProxyLoginStatus> {
        self.client
            .get(self.url("/v1/providers/cliproxyapi/login/status"))
            .query(&[("state", state)])
            .send()
            .await
            .context("无法连接 CLIProxyAPI 登录状态接口")?
            .error_for_status()
            .context("查询 CLIProxyAPI 登录状态失败")?
            .json()
            .await
            .context("无法解析登录状态响应")
    }

    async fn skills(&self) -> Result<Vec<serde_json::Value>> {
        self.client
            .get(self.url("/v1/skills"))
            .send()
            .await
            .context("无法连接本地技能接口")?
            .error_for_status()
            .context("查询本地技能失败")?
            .json()
            .await
            .context("无法解析本地技能列表")
    }

    async fn tools(&self) -> Result<Vec<serde_json::Value>> {
        self.client
            .get(self.url("/v1/tools"))
            .send()
            .await
            .context("无法连接工具列表接口")?
            .error_for_status()
            .context("查询工具列表失败")?
            .json()
            .await
            .context("无法解析工具列表")
    }

    async fn mcp_servers(&self) -> Result<Vec<serde_json::Value>> {
        self.client
            .get(self.url("/v1/mcp/servers"))
            .send()
            .await
            .context("无法连接 MCP 接口")?
            .error_for_status()
            .context("查询 MCP 服务器失败")?
            .json()
            .await
            .context("无法解析 MCP 服务器列表")
    }

    async fn search_sessions(
        &self,
        query: &str,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionHit>> {
        self.client
            .get(self.url("/v1/sessions/search"))
            .query(&[
                ("q", query.to_string()),
                ("user_id", user_id.to_string()),
                ("limit", limit.to_string()),
            ])
            .send()
            .await
            .context("无法连接会话检索接口")?
            .error_for_status()
            .context("会话检索失败")?
            .json()
            .await
            .context("无法解析会话检索结果")
    }

    async fn accounts(&self) -> Result<Vec<wonderland::cliproxy::CliProxyAccount>> {
        self.client
            .get(self.url("/v1/providers/cliproxyapi/accounts"))
            .send()
            .await
            .context("无法连接订阅账号接口")?
            .error_for_status()
            .context("查询订阅账号失败")?
            .json()
            .await
            .context("无法解析订阅账号列表")
    }

    async fn sessions(&self, user_id: Option<&str>) -> Result<Vec<SessionSummary>> {
        let mut request = self.client.get(self.url("/v1/sessions"));
        if let Some(user_id) = user_id.filter(|value| !value.is_empty()) {
            request = request.query(&[("user_id", user_id)]);
        }
        request
            .send()
            .await
            .context("无法连接会话列表接口")?
            .error_for_status()
            .context("查询会话列表失败")?
            .json()
            .await
            .context("无法解析会话列表")
    }

    async fn session_detail(&self, id: &str) -> Result<SessionDetail> {
        self.client
            .get(self.url(&format!("/v1/sessions/{id}")))
            .send()
            .await
            .context("无法连接会话详情接口")?
            .error_for_status()
            .context("查询会话详情失败")?
            .json()
            .await
            .context("无法解析会话详情")
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let api = AgentApi::new(cli.server);
    let cwd = cli
        .cwd
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let commands = wonderland::commands::load_commands(&cwd);
    let uses_chat = matches!(
        cli.command.as_ref(),
        None | Some(Command::Chat { .. } | Command::Run { .. })
    );
    let default_session = match (&cli.session_id, cli.resume && uses_chat) {
        (Some(session_id), _) => session_id.clone(),
        (None, true) => resume_session(&api, &cli.user_id).await?,
        (None, false) => Uuid::new_v4().to_string(),
    };

    let settings = RunSettings {
        mode: cli.mode,
        cwd: cli.cwd.clone(),
        model: cli.model.clone(),
        stream: !cli.no_stream,
        reasoning_effort: cli.reasoning.clone(),
    };

    match cli.command.unwrap_or(Command::Chat { prompt: None }) {
        Command::Apps { probe } => println!(
            "{}",
            serde_json::to_string_pretty(&match probe {
                Some(id) =>
                    api.work_request(
                        &format!(
                            "/api/v1/apps/{}/probe",
                            url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>()
                        ),
                        Some(serde_json::json!({}))
                    )
                    .await?,
                None => api.work_request("/api/v1/apps", None).await?,
            })?
        ),
        Command::Intelligence { refresh } => {
            let (path, body) = if refresh {
                ("/api/v1/intelligence/refresh", Some(serde_json::json!({})))
            } else {
                ("/api/v1/intelligence", None)
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&api.work_request(path, body).await?)?
            );
        }
        Command::Team { action } => {
            let (path, body) = team_request(action)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&api.work_request(&path, body).await?)?
            );
        }
        Command::Pricing { action } => {
            let (path, body) = pricing_request(action)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&api.work_request(&path, body).await?)?
            );
        }
        Command::Work { action } => {
            use serde_json::json;
            anyhow::ensure!(
                cli.reasoning.is_none(),
                "单个 Work 任务暂不接受 --reasoning；请在 Teams 计划的 executor.reasoning_effort 中指定，或不传此参数使用官方默认档位"
            );
            let (path, body, start_after) = match action {
                WorkAction::List => ("/api/v1/workflows".into(), None, false),
                WorkAction::Create {
                    prompt,
                    app,
                    title,
                    chat,
                    read_only,
                    start,
                } => (
                    "/api/v1/workflows".into(),
                    Some(
                        json!({"prompt":prompt,"title":title.unwrap_or_default(),"cwd":cwd.to_string_lossy(),"mode":if chat {"chat"} else {"work"},"app_id":app,"model":cli.model.context("请通过全局 --model 参数指定模型")?,"read_only":read_only}),
                    ),
                    start,
                ),
                WorkAction::Get { id } => (format!("/api/v1/workflows/{id}"), None, false),
                WorkAction::Events { id, after } => (
                    format!("/api/v1/workflows/{id}/events?after={after}"),
                    None,
                    false,
                ),
                WorkAction::Start { id } => (
                    format!("/api/v1/workflows/{id}/start"),
                    Some(json!({})),
                    false,
                ),
                WorkAction::Cancel { id } => (
                    format!("/api/v1/workflows/{id}/cancel"),
                    Some(json!({})),
                    false,
                ),
                WorkAction::Approve {
                    id,
                    request_id,
                    allow,
                } => (
                    format!("/api/v1/workflows/{id}/approve"),
                    Some(json!({"request_id":request_id,"approve":allow})),
                    false,
                ),
                WorkAction::Answer {
                    id,
                    request_id,
                    answers_json,
                } => (
                    format!("/api/v1/workflows/{id}/answer"),
                    Some(
                        json!({"request_id":request_id,"answers":serde_json::from_str::<serde_json::Value>(&answers_json)?}),
                    ),
                    false,
                ),
                WorkAction::Accept { id, evidence } => (
                    format!("/api/v1/workflows/{id}/accept"),
                    Some(json!({"evidence":evidence})),
                    false,
                ),
            };
            let mut value = api.work_request(&path, body).await?;
            if start_after {
                let id = value["id"].as_str().context("missing task ID")?;
                eprintln!("任务草稿: {id}");
                value = api
                    .work_request(&format!("/api/v1/workflows/{id}/start"), Some(json!({})))
                    .await?;
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::Chat {
            prompt: Some(prompt),
        } => {
            let request = settings.request(&default_session, &cli.user_id, prompt, Vec::new());
            let response = if settings.stream {
                api.run_stream(request).await?
            } else {
                api.run(request).await?
            };
            print_response(&response, !settings.stream);
        }
        Command::Chat { prompt: None } => {
            interactive_chat(
                &api,
                &cli.user_id,
                &default_session,
                &settings,
                &cwd,
                &commands,
            )
            .await?
        }
        Command::Run {
            input,
            session_id,
            skills,
        } => {
            let session_id = session_id.as_deref().unwrap_or(&default_session);
            let request = settings.request(session_id, &cli.user_id, input, skills);
            let response = if settings.stream {
                api.run_stream(request).await?
            } else {
                api.run(request).await?
            };
            print_response(&response, !settings.stream);
        }
        Command::Health => {
            let health = api.health().await?;
            println!("{}", serde_json::to_string_pretty(&health)?);
        }
        Command::Models => {
            let models = api.models().await?;
            for model in models {
                println!(
                    "{}{}",
                    model.id,
                    model
                        .owned_by
                        .map(|v| format!(" ({v})"))
                        .unwrap_or_default()
                );
            }
        }
        Command::Verify { model } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&api.verify(model).await?)?
            );
        }
        Command::Login { provider, wait } => login_flow(&api, &provider, wait).await?,
        Command::Skills => println!("{}", serde_json::to_string_pretty(&api.skills().await?)?),
        Command::Sessions => {
            let sessions = api.sessions(Some(&cli.user_id)).await?;
            if sessions.is_empty() {
                println!("（暂无会话）");
            }
            for session in sessions {
                println!(
                    "{}  messages={}  updated={}",
                    session.id, session.message_count, session.updated_at
                );
            }
        }
        Command::Session { id } => {
            let session = api.session_detail(&id).await?;
            println!("session: {}", session.id);
            print_usage(&session.usage);
            if !session.todos.is_empty() {
                println!("\ntodos:");
                for (index, todo) in session.todos.iter().enumerate() {
                    println!("{}. {}", index + 1, todo.render());
                }
            }
        }
        Command::Tools => {
            for tool in api.tools().await? {
                println!(
                    "{:26} {:9} {}",
                    tool["name"].as_str().unwrap_or("<unknown>"),
                    tool["source"].as_str().unwrap_or("builtin"),
                    if tool["read_only"].as_bool().unwrap_or(false) {
                        "read-only"
                    } else {
                        "mutating"
                    },
                );
            }
        }
        Command::Profile => {
            let value: serde_json::Value = api
                .client
                .get(api.url("/v1/models/profile"))
                .query(&[("model", cli.model.as_deref().unwrap_or(""))])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::McpReload => {
            let value: serde_json::Value = api
                .client
                .post(api.url("/v1/mcp/reload"))
                .json(&serde_json::json!({"cwd":settings.cwd}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::McpLogin { name } => {
            let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
            let value: serde_json::Value = api
                .client
                .post(api.url(&format!("/v1/mcp/{encoded}/login")))
                .json(&serde_json::json!({"cwd":settings.cwd}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", value["url"].as_str().unwrap_or_default());
            let state = value["state"]
                .as_str()
                .context("MCP login returned no state")?;
            for _ in 0..150 {
                sleep(Duration::from_secs(2)).await;
                let status: serde_json::Value = api
                    .client
                    .get(api.url("/v1/mcp/login/status"))
                    .query(&[("state", state)])
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if status["status"] == "error" {
                    anyhow::bail!("{}", status["error"]);
                }
                if status["status"] == "ok" {
                    api.client
                        .post(api.url("/v1/mcp/reload"))
                        .json(&serde_json::json!({"cwd":settings.cwd}))
                        .send()
                        .await?
                        .error_for_status()?;
                    println!("MCP 登录成功，已重新连接");
                    return Ok(());
                }
            }
            anyhow::bail!("MCP 登录超时");
        }
        Command::Mcp => {
            let servers = api.mcp_servers().await?;
            if servers.is_empty() {
                println!("（没有已连接的 MCP 服务器；在 .mcp.json 或 settings.json 的 mcpServers 中配置）");
            }
            for server in servers {
                println!("{}:", server["name"].as_str().unwrap_or("<unknown>"));
                if let Some(tools) = server["tools"].as_array() {
                    for tool in tools {
                        println!("  - {}", tool.as_str().unwrap_or_default());
                    }
                }
            }
        }
        Command::Commands => println!("{}", wonderland::commands::render_command_list(&commands)),
        Command::Search { query, limit } => {
            let hits = api.search_sessions(&query, &cli.user_id, limit).await?;
            if hits.is_empty() {
                println!("没有匹配「{query}」的会话");
            }
            for hit in hits {
                println!(
                    "{}  messages={}  updated={}",
                    hit.id, hit.message_count, hit.updated_at
                );
                println!(
                    "    {}",
                    hit.snippet.replace(char::from_u32(10).unwrap(), " ")
                );
            }
        }
        Command::Accounts => {
            let accounts = api.accounts().await?;
            if accounts.is_empty() {
                println!("（没有已登录的订阅账号；用 wonderland-cli login codex 等命令登录）");
            }
            for account in accounts {
                println!(
                    "{:32} {:12} {}{}",
                    account.name,
                    account.provider.unwrap_or_else(|| "?".to_string()),
                    account.email.unwrap_or_default(),
                    if account.disabled {
                        "（已禁用）"
                    } else {
                        ""
                    },
                );
            }
        }
    }
    Ok(())
}

/// 打印 token 用量（含缓存细分与缓存命中率）。
fn print_usage(usage: &wonderland::provider::Usage) {
    let cached = usage.cache_read_tokens;
    let total_input = usage.input_tokens + cached + usage.cache_creation_tokens;
    let hit_rate = if total_input == 0 {
        0.0
    } else {
        cached as f64 * 100.0 / total_input as f64
    };
    println!(
        "tokens: {} in ({} cached read / {} cache write) / {} out | cache 命中 {:.0}%",
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.cache_creation_tokens,
        usage.output_tokens,
        hit_rate
    );
}

async fn interactive_chat(
    api: &AgentApi,
    user_id: &str,
    session_id: &str,
    settings: &RunSettings,
    cwd: &std::path::Path,
    commands: &[wonderland::commands::CustomCommand],
) -> Result<()> {
    println!(
        "Wonderland CLI | session={session_id} | cwd={}",
        cwd.display()
    );
    println!("输入消息开始对话，输入 /help 查看命令，输入 /exit 退出。");
    if !commands.is_empty() {
        println!(
            "可用自定义命令：{}",
            commands
                .iter()
                .map(|command| format!("/{}", command.name))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    println!();
    let stdin = io::stdin();
    loop {
        print!("you> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim();
        match input {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => {
                println!("/exit 退出；/health 检查服务；/models 查看模型；/skills 列出本地技能；");
                println!("/sessions 列出会话；/session 或 /cost 查看当前会话用量与清单；");
                println!(
                    "/tools 列出全部工具（含 MCP）；/mcp 列出 MCP 服务器；/commands 列出自定义命令；"
                );
                println!(
                    "其他文本发送给 Agent；全局参数 --mode 控制工具权限，--cwd 指定工作目录，--model 覆盖模型。"
                );
                if !commands.is_empty() {
                    println!();
                    println!("自定义命令：");
                    println!("{}", wonderland::commands::render_command_list(commands));
                }
                println!();
            }
            "/health" => println!("{}", serde_json::to_string_pretty(&api.health().await?)?),
            "/models" => {
                for model in api.models().await? {
                    println!("- {}", model.id);
                }
            }
            "/skills" => println!("{}", serde_json::to_string_pretty(&api.skills().await?)?),
            "/sessions" => {
                for session in api.sessions(Some(user_id)).await? {
                    println!(
                        "{}  messages={}  updated={}",
                        session.id, session.message_count, session.updated_at
                    );
                }
            }
            "/session" | "/cost" => {
                let session = api.session_detail(session_id).await?;
                println!("session: {}", session.id);
                print_usage(&session.usage);
                for (index, todo) in session.todos.iter().enumerate() {
                    println!("{}. {}", index + 1, todo.render());
                }
            }
            "/tools" => {
                for tool in api.tools().await? {
                    println!(
                        "{:26} {:9} {}",
                        tool["name"].as_str().unwrap_or("<unknown>"),
                        tool["source"].as_str().unwrap_or("builtin"),
                        if tool["read_only"].as_bool().unwrap_or(false) {
                            "read-only"
                        } else {
                            "mutating"
                        },
                    );
                }
            }
            "/mcp" => {
                for server in api.mcp_servers().await? {
                    println!("- {}", server["name"].as_str().unwrap_or("<unknown>"));
                }
            }
            "/commands" => println!("{}", wonderland::commands::render_command_list(commands)),
            command if command.starts_with('/') => {
                let (name, arguments) = match command.split_once(char::is_whitespace) {
                    Some((name, arguments)) => (name, arguments),
                    None => (command, ""),
                };
                match wonderland::commands::find_command(commands, name) {
                    Some(custom) => {
                        let expanded = wonderland::commands::expand(custom, arguments);
                        println!("[命令 /{}] {}", custom.name, custom.description);
                        run_turn(api, session_id, user_id, settings, expanded).await?;
                    }
                    None => {
                        println!(
                            "未知命令 {name}；输入 /help 查看内置命令，或 /commands 查看项目自定义命令。"
                        );
                    }
                }
            }
            message => run_turn(api, session_id, user_id, settings, message.to_string()).await?,
        }
    }
    Ok(())
}

/// 发送一条消息并打印回答、任务清单与用量。
async fn run_turn(
    api: &AgentApi,
    session_id: &str,
    user_id: &str,
    settings: &RunSettings,
    input: String,
) -> Result<()> {
    print!("agent> ");
    io::stdout().flush()?;
    let request = settings.request(session_id, user_id, input, Vec::new());
    let response = if settings.stream {
        api.run_stream(request).await?
    } else {
        let response = api.run(request).await?;
        println!("{}", response.output);
        response
    };
    println!();
    if !response.todos.is_empty() {
        println!("todos:");
        for (index, todo) in response.todos.iter().enumerate() {
            println!("  {}. {}", index + 1, todo.render());
        }
        println!();
    }
    println!(
        "plan: {} steps | reflection: {} | turns: {} | tool calls: {} | tokens: {} in / {} out",
        response.plan.steps.len(),
        response.reflection.passed,
        response.turns,
        response.tool_calls,
        response.usage.input_tokens,
        response.usage.output_tokens,
    );
    println!();
    Ok(())
}

async fn login_flow(api: &AgentApi, provider: &str, wait: bool) -> Result<()> {
    let login = api.login(provider).await?;
    println!("provider: {provider}");
    println!("state: {}", login.state.as_deref().unwrap_or("<missing>"));
    println!(
        "请在浏览器打开 OAuth URL：\n{}",
        login.url.as_deref().unwrap_or("<missing>")
    );
    let Some(state) = login.state else {
        return Ok(());
    };
    if !wait {
        println!("登录后执行：wonderland-cli --server ... login {provider} --wait 不会复用本次 state，请使用 HTTP status 接口或桌面版轮询。 ");
        return Ok(());
    }

    loop {
        sleep(Duration::from_secs(2)).await;
        let status = api.login_status(&state).await?;
        println!(
            "status={} authenticated={}",
            status.status, status.authenticated
        );
        if status.status != "wait" {
            if let Some(error) = status.error {
                println!("error: {error}");
            }
            break;
        }
    }
    Ok(())
}

/// 一次请求的公共设置（权限模式 / 工作目录 / 模型）。
#[derive(Clone, Debug)]
struct RunSettings {
    mode: Option<PermissionMode>,
    cwd: Option<String>,
    model: Option<String>,
    /// 是否使用流式接口（--no-stream 关闭）。
    stream: bool,
    /// 推理档位覆盖。
    reasoning_effort: Option<String>,
}

impl RunSettings {
    fn request(
        &self,
        session_id: &str,
        user_id: &str,
        input: String,
        skills: Vec<String>,
    ) -> AgentRequest {
        AgentRequest {
            session_id: session_id.to_string(),
            user_id: Some(user_id.to_string()),
            model: self.model.clone(),
            skills,
            mode: self.mode,
            cwd: self.cwd.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            input,
        }
    }
}

/// `--continue`：复用该用户最近更新的会话。
async fn resume_session(api: &AgentApi, user_id: &str) -> Result<String> {
    match api.sessions(Some(user_id)).await?.into_iter().next() {
        Some(session) => {
            println!(
                "继续会话 {}（messages={} updated={}）",
                session.id, session.message_count, session.updated_at
            );
            Ok(session.id)
        }
        None => {
            println!("没有可继续的会话，新建一个。");
            Ok(Uuid::new_v4().to_string())
        }
    }
}

fn print_response(response: &AgentResponse, include_output: bool) {
    if include_output {
        println!("{}", response.output);
    }
    if !response.todos.is_empty() {
        println!("\ntodos:");
        for (index, todo) in response.todos.iter().enumerate() {
            println!("  {}. {}", index + 1, todo.render());
        }
    }
    println!(
        "\n[session={} execution={} score={:.1} turns={} tool_calls={} tokens={}in/{}out]",
        response.session_id,
        response.execution_id,
        response.evaluation.total_score,
        response.turns,
        response.tool_calls,
        response.usage.input_tokens,
        response.usage.output_tokens,
    );
}

#[cfg(test)]
mod management_tests {
    use super::*;

    #[test]
    fn pricing_requires_billing_and_preserves_exact_query_values() {
        assert!(Cli::try_parse_from([
            "wonderland-cli",
            "pricing",
            "quote",
            "--app",
            "codex",
            "--model",
            "gpt-test"
        ])
        .is_err());
        let cli = Cli::try_parse_from([
            "wonderland-cli",
            "pricing",
            "quote",
            "--app",
            "codex",
            "--model",
            "gpt-test+variant&reason=high",
            "--billing",
            "subscription",
        ])
        .unwrap();
        let Some(Command::Pricing { action }) = cli.command else {
            panic!("wrong command")
        };
        let (path, body) = pricing_request(action).unwrap();
        assert!(body.is_none());
        let url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
        assert_eq!(
            url.query_pairs().into_owned().collect::<Vec<_>>(),
            vec![
                ("app_id".into(), "codex".into()),
                ("model".into(), "gpt-test+variant&reason=high".into()),
                ("billing_channel".into(), "subscription".into())
            ]
        );
    }

    #[test]
    fn team_events_reject_negative_cursor_and_escape_path_id() {
        assert!(Cli::try_parse_from([
            "wonderland-cli",
            "team",
            "events",
            "team-1",
            "--after",
            "-1"
        ])
        .is_err());
        let (path, body) = team_request(TeamAction::Events {
            id: "team/a b?x=1".into(),
            after: 42,
        })
        .unwrap();
        assert_eq!(path, "/api/v1/teams/team%2Fa%20b%3Fx%3D1/events?after=42");
        assert!(body.is_none());
        assert!(team_request(TeamAction::Get { id: "..".into() }).is_err());
    }

    #[test]
    fn team_plan_must_match_typed_schema_and_creation_does_not_start() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("team.json");
        std::fs::write(&file, r#"{"prompt":"repair tests","cwd":"E:/projects/demo","planner":{"app_id":"kimi-cli","model":"kimi-for-coding"},"checks":[{"program":"cargo","args":["test"],"timeout_secs":300}]}"#).unwrap();
        let (path, body) = team_request(TeamAction::Create { file: file.clone() }).unwrap();
        assert_eq!(path, "/api/v1/teams");
        assert_eq!(body.unwrap()["planner"]["model"], "kimi-for-coding");
        std::fs::write(&file, r#"{"prompt":"repair tests","unexpected":true}"#).unwrap();
        assert!(team_request(TeamAction::Create { file }).is_err());
    }
}
