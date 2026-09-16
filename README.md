# Rust for AI Agent

一个可运行的 Rust AI Agent 基础工程，参考了 [solenovex/rust-ai-agent](https://github.com/solenovex/rust-ai-agent) 的 Agent / session / tool 分层思路，以及 [solenovex/expense-tracker-api](https://github.com/solenovex/expense-tracker-api) 的 Axum API、健康检查和请求 tracing 方式。

当前实现把后续迭代需要的闭环先打通：

- 可观测性：`tracing` + `tower-http::TraceLayer`，每次 HTTP 请求、Agent 执行、Agent 委派都有 span；通过 `RUST_LOG` 调整级别。
- 长期记忆：JSON 持久化的跨会话记忆，支持按 `user_id` 和关键词检索；Agent 每次运行会自动写入对话摘要，也可通过 API 显式记忆。
- 多智能体协作：`AgentDirectory` 支持注册和互调；内置 `research`、`expense` 两个示例 Agent。
- 规划与反思：启发式 Planner 生成步骤，Reflection 做失败检测，失败时自动重试一次；后续可替换为 LLM Planner。
- 代码执行沙箱：默认关闭，只允许策略声明的语言并限制输入、输出和超时；当前是开发原型，不是安全边界。
- 评估打分：每次运行落盘 `correctness / completeness / safety / latency` 和反馈，可通过 API 查询；后续可替换为真实评测集或人工标注。
- CLIProxyAPI 账号接入：Rust 服务通过 OpenAI-compatible `/v1` 调用本地 CLIProxyAPI，并通过 Management API 发起 OAuth、轮询登录状态和验证订阅模型；OAuth 凭据仍由 CLIProxyAPI 管理。
- 费用 API：沿用参考项目的 Axum `Expense / Store / Handler / Error` 分层，提供带 `x-api-key` 的 CRUD 和汇总接口，便于费用 Agent 作为真实协作者接入。

Contributors: **AliceMarx**

## 快速开始

需要 Rust stable（建议 Rust 1.85+）：

```bash
cargo run
```

## CLIProxyAPI 订阅账号登录

CLIProxyAPI 作为独立的 Go sidecar 运行，负责 OAuth 登录、订阅账号轮换和凭据保存；本 Rust 项目不读取或保存 OAuth token。参考实现来自 [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)。

先按 CLIProxyAPI 的说明启动服务，并在配置中开启本机 Management API。例如：

```yaml
port: 8317
auth-dir: "~/.cli-proxy-api"
api-keys:
  - "local-agent-key"
remote-management:
  allow-remote: false
  secret-key: "local-management-key"
```

可以直接使用 CLIProxyAPI 的 OAuth 登录参数，或通过下面的 Rust API 发起登录：

```text
codex:       -codex-login
claude:      -claude-login
antigravity: -antigravity-login
kimi:        -kimi-login
xai:         -xai-login
devin:       -devin-login
meta:        -meta-login
```

Windows PowerShell 配置 Rust Agent：

```powershell
$env:AGENT_PROVIDER="cliproxyapi"
$env:CLIPROXYAPI_BASE_URL="http://127.0.0.1:8317/v1"
$env:CLIPROXYAPI_API_KEY="local-agent-key"
$env:CLIPROXYAPI_MODEL="gpt-5.4"
$env:CLIPROXYAPI_MANAGEMENT_URL="http://127.0.0.1:8317/v0/management"
$env:CLIPROXYAPI_MANAGEMENT_KEY="local-management-key"
cargo run
```

登录和验证接口：

```bash
# 选择订阅服务并获取 OAuth URL；在浏览器完成账号授权
curl -X POST http://127.0.0.1:8080/v1/providers/cliproxyapi/login \
  -H 'content-type: application/json' \
  -d '{"provider":"codex"}'

# 用上一步返回的 state 轮询，status=ok 表示凭据已保存
curl 'http://127.0.0.1:8080/v1/providers/cliproxyapi/login/status?state=STATE'

# 查看 CLIProxyAPI 当前可用的订阅模型
curl http://127.0.0.1:8080/v1/providers/cliproxyapi/models

# 验证代理可访问，并检查指定模型是否可用
curl -X POST http://127.0.0.1:8080/v1/providers/cliproxyapi/verify \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5.4"}'
```

登录完成后，Agent 的 `/v1/agent/run` 会自动经由 CLIProxyAPI 使用订阅账号模型。`CLIPROXYAPI_API_KEY` 是本机代理访问密钥，不是订阅账号的 OAuth token；`CLIPROXYAPI_MANAGEMENT_KEY` 只用于登录状态管理。

默认启动 `http://127.0.0.1:8080`。没有配置模型密钥时使用离线演示 Provider，因此可以先验证完整链路。

使用真实 OpenAI-compatible 服务时：

```bash
export OPENAI_API_KEY="..."
export OPENAI_BASE_URL="https://api.openai.com/v1"
export OPENAI_MODEL="gpt-4o-mini"
cargo run
```

Windows PowerShell 对应：

```powershell
$env:OPENAI_API_KEY="..."
$env:OPENAI_BASE_URL="https://api.openai.com/v1"
$env:OPENAI_MODEL="gpt-4o-mini"
cargo run
```

数据默认保存在 `.agent-data/`，可用 `AGENT_DATA_DIR` 更换。

## API 示例

健康检查：

```bash
curl http://127.0.0.1:8080/health
```

运行 Agent：

```bash
curl -X POST http://127.0.0.1:8080/v1/agent/run \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo-session","user_id":"alice","input":"请研究 Rust AI Agent，并总结我的费用预算"}'
```

显式写入和检索长期记忆：

```bash
curl -X POST http://127.0.0.1:8080/v1/memory \
  -H 'content-type: application/json' \
  -d '{"user_id":"alice","content":"我偏好简洁的 Rust 示例","tags":["preference","rust"],"importance":0.9}'

curl 'http://127.0.0.1:8080/v1/memory/search?user_id=alice&q=Rust&limit=5'
```

评估记录：

```bash
curl 'http://127.0.0.1:8080/v1/evaluations?session_id=demo-session'
```

费用 API（参考项目的 `x-api-key` 约定）：

```bash
curl -H 'x-api-key: dev-secret-key' http://127.0.0.1:8080/expenses
curl -H 'x-api-key: dev-secret-key' 'http://127.0.0.1:8080/expenses/summary?month=2026-07'
```

## 沙箱安全边界

沙箱默认关闭。只有在隔离容器或虚拟机中，确认运行用户、文件系统、网络和资源配额均已限制后，才可临时启用：

```bash
AGENT_ENABLE_SANDBOX=true AGENT_SANDBOX_TIMEOUT_MS=2000 cargo run
```

当前实现只允许 `python`，使用独立临时目录、无环境变量、输入/输出大小限制、超时和进程退出清理，但 Python 本身仍可能访问宿主能力。生产环境必须额外使用容器/VM、非特权用户、网络隔离、seccomp/AppContainer 和 CPU/内存配额。

## 开发检查

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## 设计说明

工程刻意把 `ModelProvider`、`Planner`、`AgentWorker`、`MemoryStore` 和 `EvaluationStore` 做成可替换接口，先用确定性实现把数据流和观测闭环跑通，再逐步接入向量检索、真正的工具调用、OpenTelemetry、领域评测集和隔离执行服务。
