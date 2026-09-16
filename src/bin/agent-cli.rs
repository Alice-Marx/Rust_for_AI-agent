use std::{
    io::{self, Write},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use rust_ai_agent::{
    cliproxy::{CliProxyLoginStart, CliProxyLoginStatus, CliProxyModel},
    model::{AgentRequest, AgentResponse},
};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "agent-cli", version, about = "Rust AI Agent 终端客户端")]
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

    #[command(subcommand)]
    command: Option<Command>,
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
}

#[derive(Debug, Serialize)]
struct VerifyRequest {
    model: Option<String>,
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

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let api = AgentApi::new(cli.server);
    let default_session = cli.session_id.unwrap_or_else(|| Uuid::new_v4().to_string());

    match cli.command.unwrap_or(Command::Chat { prompt: None }) {
        Command::Chat {
            prompt: Some(prompt),
        } => {
            print_response(
                &api.run(request(&default_session, &cli.user_id, prompt))
                    .await?,
            );
        }
        Command::Chat { prompt: None } => {
            interactive_chat(&api, &cli.user_id, &default_session).await?
        }
        Command::Run { input, session_id } => {
            let session_id = session_id.as_deref().unwrap_or(&default_session);
            print_response(&api.run(request(session_id, &cli.user_id, input)).await?);
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
    }
    Ok(())
}

async fn interactive_chat(api: &AgentApi, user_id: &str, session_id: &str) -> Result<()> {
    println!("Rust AI Agent CLI | session={session_id}");
    println!("输入消息开始对话，输入 /help 查看命令，输入 /exit 退出。\n");
    let stdin = io::stdin();
    loop {
        print!("you> ");
        io::stdout().flush()?;
        let mut line = String::new();
        stdin.read_line(&mut line)?;
        let input = line.trim();
        match input {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => {
                println!(
                    "/exit 退出；/health 检查服务；/models 查看模型；其他文本发送给 Agent。\n"
                );
            }
            "/health" => println!("{}", serde_json::to_string_pretty(&api.health().await?)?),
            "/models" => {
                for model in api.models().await? {
                    println!("- {}", model.id);
                }
            }
            message => {
                print!("agent> ");
                io::stdout().flush()?;
                let response = api
                    .run(request(session_id, user_id, message.to_string()))
                    .await?;
                println!("{}\n", response.output);
                println!(
                    "plan: {} steps | reflection: {}\n",
                    response.plan.steps.len(),
                    response.reflection.passed
                );
            }
        }
    }
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
        println!("登录后执行：agent-cli --server ... login {provider} --wait 不会复用本次 state，请使用 HTTP status 接口或桌面版轮询。 ");
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

fn request(session_id: &str, user_id: &str, input: String) -> AgentRequest {
    AgentRequest {
        session_id: session_id.to_string(),
        user_id: Some(user_id.to_string()),
        model: None,
        input,
    }
}

fn print_response(response: &AgentResponse) {
    println!("{}", response.output);
    println!(
        "\n[session={} execution={} score={:.1}]",
        response.session_id, response.execution_id, response.evaluation.total_score
    );
}
