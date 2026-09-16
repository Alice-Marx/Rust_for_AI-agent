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

## 如何使用

### 1. 环境要求

- Windows 10/11、Rust stable（建议 Rust 1.85+）、Git。
- 如果从 CLIProxyAPI 源码构建，还需要 Go。也可以直接使用它发布的可执行文件。
- 本项目不会把 Cargo 缓存和编译产物放到 C 盘，推荐使用下面的 D 盘配置：

```powershell
$rustRoot = "D:\Jianwei_Li\rust"
$env:CARGO_HOME = "$rustRoot\cargo"
$env:RUSTUP_HOME = "$rustRoot\rustup"
$env:CARGO_TARGET_DIR = "$rustRoot\targets"
$env:Path = "$rustRoot\cargo\bin;$env:Path"
```

### 2. 下载并启动 Rust Agent

```powershell
git clone https://github.com/ljwei-stak/Rust_for_AI-agent.git F:\codex\Rust_for_AI-agent
Set-Location F:\codex\Rust_for_AI-agent

# 第一次运行会编译依赖；默认没有模型密钥也可以启动离线演示模式
cargo run
```

默认地址是 `http://127.0.0.1:8080`。确认服务启动：

```powershell
Invoke-RestMethod http://127.0.0.1:8080/health
```

没有设置 `AGENT_PROVIDER` 时，服务使用离线 Provider。生产使用时请选择 `cliproxyapi` 或 `openai`。

## CLIProxyAPI 订阅账号登录

CLIProxyAPI 作为独立的 Go sidecar 运行，负责 OAuth 登录、订阅账号轮换和凭据保存；本 Rust 项目不读取或保存 OAuth token。参考实现来自 [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)。

### 3. 准备 CLIProxyAPI

可以从 [CLIProxyAPI Releases](https://github.com/router-for-me/CLIProxyAPI/releases) 下载 Windows 可执行文件；或者从源码构建：

```powershell
Set-Location F:\codex\_refs
git clone https://github.com/router-for-me/CLIProxyAPI.git
Set-Location F:\codex\_refs\CLIProxyAPI
go build -o cliproxyapi.exe .\cmd\server
```

在 CLIProxyAPI 目录创建 `config.yaml`。`secret-key` 和 `api-keys` 仅用于本机服务访问，不是订阅账号 OAuth token：

```yaml
port: 8317
auth-dir: "D:/Jianwei_Li/cli-proxy-api"
api-keys:
  - "local-agent-key"
remote-management:
  allow-remote: false
  secret-key: "local-management-key"
```

启动 CLIProxyAPI：

```powershell
Set-Location F:\codex\_refs\CLIProxyAPI
.\cliproxyapi.exe -config .\config.yaml
```

保持这个窗口运行。CLIProxyAPI 默认监听 `http://127.0.0.1:8317`，Rust Agent 使用 `/v1` API，登录管理使用 `/v0/management` API。

如果只是想使用 CLIProxyAPI 自带的命令行 OAuth，也可以运行以下命令后按浏览器提示登录：

```text
codex:       -codex-login
claude:      -claude-login
antigravity: -antigravity-login
kimi:        -kimi-login
xai:         -xai-login
devin:       -devin-login
meta:        -meta-login
```

### 4. 在 Rust Agent 中选择 API 并登录账号

先在一个新的 PowerShell 窗口配置 Provider：

```powershell
$env:AGENT_PROVIDER="cliproxyapi"
$env:CLIPROXYAPI_BASE_URL="http://127.0.0.1:8317/v1"
$env:CLIPROXYAPI_API_KEY="local-agent-key"
$env:CLIPROXYAPI_MODEL="gpt-5.4"
$env:CLIPROXYAPI_MANAGEMENT_URL="http://127.0.0.1:8317/v0/management"
$env:CLIPROXYAPI_MANAGEMENT_KEY="local-management-key"
```

再次启动 Rust Agent：

```powershell
Set-Location F:\codex\Rust_for_AI-agent
cargo run
```

现在可以通过 API 选择登录服务。下面的例子选择 Codex；可选值还有 `claude`、`antigravity`、`kimi`、`xai`、`devin` 和 `meta`：

```powershell
$login = Invoke-RestMethod `
  -Method Post `
  -Uri http://127.0.0.1:8080/v1/providers/cliproxyapi/login `
  -ContentType "application/json" `
  -Body (@{ provider = "codex" } | ConvertTo-Json)

# 在浏览器中完成订阅账号授权
Start-Process $login.url

# 轮询登录结果；status=ok 且 authenticated=true 表示验证成功
do {
  Start-Sleep -Seconds 2
  $status = Invoke-RestMethod `
    "http://127.0.0.1:8080/v1/providers/cliproxyapi/login/status?state=$($login.state)"
  $status | ConvertTo-Json
} while ($status.status -eq "wait")
```

登录成功后，CLIProxyAPI 会把账号凭据保存到 `auth-dir`，后续重启服务不需要重复登录。已经登录过的账号可以跳过上面的登录步骤，直接执行模型验证。

验证 CLIProxyAPI 和订阅模型：

```bash
# 查看 CLIProxyAPI 当前可用的订阅模型
curl http://127.0.0.1:8080/v1/providers/cliproxyapi/models

# 验证代理可访问，并检查指定模型是否可用
curl -X POST http://127.0.0.1:8080/v1/providers/cliproxyapi/verify \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5.4"}'
```

PowerShell 也可以直接查看模型并选择其中一个作为 Agent 模型：

```powershell
$models = Invoke-RestMethod http://127.0.0.1:8080/v1/providers/cliproxyapi/models
$models | Format-Table id, object, owned_by

# 将这里替换为 /models 返回的真实模型 id
$env:CLIPROXYAPI_MODEL = "gpt-5.4"
```

`CLIPROXYAPI_API_KEY` 是本机代理访问密钥，不是订阅账号 OAuth token；`CLIPROXYAPI_MANAGEMENT_KEY` 只用于登录、查询登录状态和取消登录。

### 5. 调用 Agent

保持 Rust Agent 运行，调用 `/v1/agent/run`：

```powershell
$request = @{
  session_id = "demo-session"
  user_id = "alice"
  input = "请研究 Rust AI Agent，并总结我的费用预算"
} | ConvertTo-Json

Invoke-RestMethod `
  -Method Post `
  -Uri http://127.0.0.1:8080/v1/agent/run `
  -ContentType "application/json" `
  -Body $request
```

此时模型调用路径是：

```text
Rust Agent -> http://127.0.0.1:8317/v1/chat/completions -> CLIProxyAPI -> 已验证的订阅账号模型
```

如果需要取消尚未完成的登录：

```powershell
Invoke-RestMethod `
  -Method Delete `
  -Uri "http://127.0.0.1:8080/v1/providers/cliproxyapi/login?state=$($login.state)"
```

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

### Provider 选择

通过 `AGENT_PROVIDER` 选择模型后端：

| 值 | 说明 | 必需配置 |
| --- | --- | --- |
| `offline` | 离线演示，不访问外部模型 | 无 |
| `cliproxyapi` | 使用本机 CLIProxyAPI 和订阅账号 | `CLIPROXYAPI_BASE_URL`、`CLIPROXYAPI_API_KEY`、`CLIPROXYAPI_MODEL` |
| `openai` | 直接调用 OpenAI-compatible 服务 | `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`OPENAI_MODEL` |

修改环境变量后需要重启 Rust Agent 进程。

### 常见问题

- `503 CLIProxyAPI is not configured`：当前 Rust Agent 进程没有读取到 `AGENT_PROVIDER=cliproxyapi` 或 CLIProxyAPI 相关环境变量，请在启动 Rust Agent 的同一个 PowerShell 窗口重新设置变量。
- `CLIProxyAPI models request failed`：检查 CLIProxyAPI 是否运行在 `8317` 端口、`CLIPROXYAPI_API_KEY` 是否与 `config.yaml` 的 `api-keys` 一致。
- 登录接口返回 404：检查 CLIProxyAPI 是否配置了 `remote-management.secret-key`，并确认 `CLIPROXYAPI_MANAGEMENT_KEY` 一致；`allow-remote: false` 允许本机调用，但仍然要求管理密钥。
- 登录状态长时间为 `wait`：确认浏览器已经完成授权，并保持 CLIProxyAPI 进程运行；OAuth 状态可能因超时失效，需要重新发起登录。
- `/v1/providers/cliproxyapi/models` 没有模型：先完成至少一个订阅账号登录，再重新请求模型列表。

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
