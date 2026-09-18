use std::{
    io::{self, Write},
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
    /// 列出当前项目的自定义斜杠命令
    Commands,
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
            client: Client::new(),
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
            .get(self.url("/v1/providers/cliproxyapi/models"))
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
    let default_session = match (&cli.session_id, cli.resume) {
        (Some(session_id), _) => session_id.clone(),
        (None, true) => resume_session(&api, &cli.user_id).await?,
        (None, false) => Uuid::new_v4().to_string(),
    };

    let settings = RunSettings {
        mode: cli.mode,
        cwd: cli.cwd.clone(),
        model: cli.model.clone(),
    };

    match cli.command.unwrap_or(Command::Chat { prompt: None }) {
        Command::Chat {
            prompt: Some(prompt),
        } => {
            print_response(
                &api.run(settings.request(&default_session, &cli.user_id, prompt, Vec::new()))
                    .await?,
            );
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
            print_response(
                &api.run(settings.request(session_id, &cli.user_id, input, skills))
                    .await?,
            );
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
    let response = api
        .run(settings.request(session_id, user_id, input, Vec::new()))
        .await?;
    println!("{}", response.output);
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

fn print_response(response: &AgentResponse) {
    println!("{}", response.output);
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
