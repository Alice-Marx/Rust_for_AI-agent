# Wonderland

一个用 Rust 实现的 agentic 编码 Agent 框架。项目已从"单次问答流水线"升级为真正的 agentic 编码助手：模型在一个多轮 `tool_use → tool_result` 循环中自主读写文件、搜索代码、执行命令，配合细粒度权限管线、会话持久化与上下文自动压缩。核心设计（工具协议、权限模式、压缩策略等）移植自 Claude Code，仓库根目录的 `claude-code/` 为只读参考实现。工程结构参考了 [solenovex/wonderland](https://github.com/solenovex/wonderland) 的 Agent / session / tool 分层思路，以及 [solenovex/expense-tracker-api](https://github.com/solenovex/expense-tracker-api) 的 Axum API、健康检查和请求 tracing 方式。

当前实现把 agentic 编码助手的完整闭环打通：

- Agentic loop：模型响应中出现 `tool_use` 块时执行工具并把 `tool_result` 写回会话，直到模型停止调用工具或达到最大轮数（默认 25，可用 `with_max_turns` 调整）；工具失败与权限拒绝都会作为错误结果反馈给模型，不中断循环。
- 十五个内置工具：`FileRead` / `FileWrite` / `FileEdit` / `Glob` / `Grep` / `Bash` / `WebFetch` / `Task` / `TodoWrite` / `ApplyPatch` / `NotebookEdit` / `TaskOutput` / `TaskStop` / `EnterPlanMode` / `ExitPlanMode`。`FileEdit` 做精确字符串替换并带 mtime + 内容 hash 的 staleness 检查（文件在读取后被外部修改会拒绝写入）；`Bash` 复合命令按 `&&` / `||` / `;` / `|` 拆分后逐段做权限评估，超长输出自动落盘，`run_in_background=true` 把长驻命令放进后台并由 `TaskOutput` / `TaskStop` 管理（借鉴 Claude Code）；`WebFetch` 抓取网页并抽取正文（拒绝 localhost/私网地址，按 `WebFetch(domain:example.com)` 域名规则授权）；`Task` 把子任务委派给子 Agent（内置 research / expense，或 `.claude/agents/*.md` 与 `.wonderland/agents/*.md` 文件定义的子代理，按 `Task(agent:name)` 规则授权）；`TodoWrite` 维护会话级任务清单（免权限提示，清单作为动态段注入系统提示词并持久化到会话）；`ApplyPatch` 借鉴 Codex 的 V4A 多文件补丁（`*** Update File:` / `@@` 定位提示 + 三级容错匹配，无需行号）；`NotebookEdit` 编辑 Jupyter `.ipynb` 单元格（按 cell id 或序号 replace/insert/delete）；`EnterPlanMode` / `ExitPlanMode` 在 run 中途切换 plan 只读模式并可在退出时恢复原权限模式。
- 权限管线：五种权限模式（`default` / `plan` / `acceptEdits` / `bypassPermissions` / `dontAsk`）+ 从 `<cwd>/.claude/settings.json` 与 `<cwd>/.wonderland/settings.json` 加载的 `allow / ask / deny` 规则管线（首命中胜出），外加不可绕过的 `.git/` 内部与 `.claude/`、`.wonderland/` 目录写保护。
- Hooks：借鉴 Claude Code 的命令 hook 机制，`settings.json` 的 `hooks` 字段配置 `PreToolUse` / `PostToolUse` / `SessionStart` / `Stop` 事件（matcher 支持 `Bash|FileWrite` 多值与空通配）；hook 进程从 stdin 收到事件 JSON，退出码 2 拦截操作（stderr 作为拒绝理由），stdout JSON 的 `decision: block|approve` 与 `additionalContext` 参与决策或注入上下文。
- 文件定义子代理：`.claude/agents/*.md`（或 `.wonderland/agents/*.md`）用 YAML frontmatter（`name` / `description` / `tools` / `max_turns`）定义子代理，正文作为其系统提示词；`Task` 工具按名委派，frontmatter 的 `tools` 既是工具池也是白名单，Task/TodoWrite 禁止出现在子代理中（不允许嵌套委派）。
- 会话持久化与自动压缩：每个会话一个 JSON 文件，每轮落盘，崩溃后可恢复完整工具调用轨迹；上下文估算超过阈值时自动把历史压缩为摘要，并保证不切断 `tool_use / tool_result` 配对。
- 上下文构建：系统提示词按静态段（身份、工具规范、安全准则）在前、动态段（技能、环境信息、git 状态、项目指令）在后的顺序组织以保护 prompt cache；层级加载 `~/.claude/CLAUDE.md` 与从根到 `cwd` 各级的 `AGENTS.md` / `CLAUDE.md` / `.claude/CLAUDE.md`，支持 `@path` include。
- 可观测性：`tracing` + `tower-http::TraceLayer`，每次 HTTP 请求、Agent 执行、Agent 委派都有 span；通过 `RUST_LOG` 调整级别。
- 长期记忆：JSON 持久化的跨会话记忆，支持按 `user_id` 和关键词检索；Agent 每次运行会自动写入对话摘要，也可通过 API 显式记忆。
- 多智能体协作：`AgentDirectory` 支持注册和互调；内置 `research`、`expense` 两个示例 Agent。
- 规划与反思：启发式 Planner 生成步骤，Reflection 做失败检测；后续可替换为 LLM Planner。
- 代码执行沙箱：默认关闭，只允许策略声明的语言并限制输入、输出和超时；当前是开发原型，不是安全边界。
- 评估打分：每次运行落盘 `correctness / completeness / safety / latency` 和反馈，可通过 API 查询；后续可替换为真实评测集或人工标注。
- 模型能力档案：按模型 id 自动解析上下文窗口、输出上限、推理参数风格、提示缓存策略与编辑工具偏好（`src/model_profile.rs`），并把模型专属的工具使用指引写进系统提示词静态段。因此同一个工具集在 Claude、GPT、DeepSeek、Kimi、Qwen、GLM、Grok、Gemini 上都会用各家最擅长的方式编辑文件。
- 多厂商 API 适配：`AGENT_PROVIDER` 支持 `anthropic`（官方 Messages API 直连）以及 `deepseek` / `kimi` / `qwen` / `glm` / `grok` / `gemini` / `openrouter` / `ollama` 等 OpenAI-compatible 别名，配合 `openai`（任意兼容网关）与 `cliproxyapi`（订阅账号）共四条接入路径；每家的 base URL、API key 变量与默认模型都有内置约定，可用 `*_BASE_URL` / `*_MODEL` 覆盖。
- Anthropic 原生能力：`/v1/messages` 直连时启用提示缓存断点（system + 最后一个工具 + 会话前缀）、扩展思考（`thinking.budget_tokens`，并自动抬高 `max_tokens`）、thinking 块签名回传，使 Claude 模型在本项目里的缓存命中与工具调用行为与官方客户端一致。
- 推理参数按模型族下发：OpenAI 推理模型自动带 `reasoning_effort`（可按 `AGENT_REASONING_EFFORT` 调整，默认 `medium`），Anthropic 走 thinking 预算；DeepSeek / Kimi 等端点返回的 `reasoning_content` 会作为思维链记录进会话轨迹但不回流到请求体。
- MCP 工具生态：读取 `.mcp.json` / `.claude/settings.json` / `.wonderland/settings.json` 的 `mcpServers`，用 stdio JSON-RPC 连接 MCP 服务器并把工具以 `mcp__<server>__<tool>` 注册进同一张工具表，权限管线、hooks、会话轨迹与内置工具完全一致；`/v1/mcp/servers` 与 `wonderland-cli mcp` 可查看连接结果。
- 自定义斜杠命令：`.claude/commands/**/*.md`（或 `.wonderland/commands/`）定义提示词模板，支持 YAML frontmatter 的 `description` / `argument-hint`、子目录命名空间（`/frontend:component`）与 `$ARGUMENTS` / `$1`..`$9` 参数替换。
- 会话按用户隔离：会话文件记录 `user_id`，`/v1/sessions?user_id=` 与 CLI 的 `--continue` 都按用户过滤，与长期记忆的隔离策略一致。
- CLIProxyAPI 账号接入：Rust 服务通过 OpenAI-compatible `/v1` 调用本地 CLIProxyAPI，并通过 Management API 发起 OAuth、轮询登录状态和验证订阅模型；OAuth 凭据仍由 CLIProxyAPI 管理。
- 费用 API：沿用参考项目的 Axum `Expense / Store / Handler / Error` 分层，提供带 `x-api-key` 的 CRUD 和汇总接口，便于费用 Agent 作为真实协作者接入。

Contributors: **AliceMarx**

## 架构

```text
src/
├── agent.rs        AgentRuntime：多轮 agentic loop（tool_use → tool_result）、
│                   每轮会话落盘、上下文自动压缩（maybe_compact）、
│                   记忆写入、规划/反思与评估收尾
├── provider.rs     统一消息/工具调用协议：ChatMessage / ContentBlock
│                   （含 Thinking 思维链块）/ ToolDefinition / Usage /
│                   StopReason / ModelProvider；OpenAI-compatible 兼容层
│                   （请求构建与响应解析为纯函数，含 reasoning_effort 下发）
├── anthropic.rs    Anthropic Messages API 原生 provider：cache_control 提示
│                   缓存断点、thinking 预算、thinking 签名回传
├── vendors.rs      OpenAI-compatible 厂商预设（deepseek / kimi / qwen / glm /
│                   grok / gemini / openrouter / ollama）与参数解析
├── model_profile.rs 模型能力档案：上下文窗口、输出上限、推理风格、
│                   缓存策略、编辑工具偏好与模型专属工具指引
├── mcp.rs          MCP stdio 客户端（JSON-RPC 握手、tools/list 分页、
│                   tools/call、mcp__server__tool 工具适配）
├── commands.rs     自定义斜杠命令（.claude/commands/*.md、命名空间与参数替换）
├── tools/          工具注册表与十五个内置工具（含后台任务 / V4A 补丁 / notebook / plan mode）
│   ├── fs.rs       FileRead / FileWrite / FileEdit + 会话内已读文件状态
│   │               （FileWrite/FileEdit 要求先读后写，带 staleness 检查）
│   ├── search.rs   Glob / Grep（尊重 .gitignore，跳过 hidden 与 .git）
│   ├── shell.rs    Bash（复合命令拆分、超时、超长输出落盘、shell 自动选择）
│   ├── webfetch.rs WebFetch（HTML 抽正文、SSRF 防护、按域名授权）
│   ├── task.rs     Task（把子任务委派给 AgentDirectory 中的子 Agent）
│   └── todo.rs     TodoWrite（会话级任务清单，免权限提示）
├── permissions.rs  PermissionMode 五种模式、allow/ask/deny 规则管线
│                   （evaluate 为纯函数，首命中胜出）、.git/.claude 写保护、
│                   PermissionHandler（Ask 时的用户询问通道）
├── session.rs      SessionStore：每会话一个 JSON 文件、原子写入、
│                   摘要列表、id 净化防路径逃逸
├── context.rs      系统提示词静态/动态分段、AGENTS.md/CLAUDE.md 层级加载
│                   （含 @path include）、git 分支/状态/最近提交注入
├── memory.rs       长期记忆
├── planning.rs     启发式 Planner 与 Reflection
├── skills.rs       本地 SKILL.md 技能目录
├── evaluation.rs   运行评估打分
├── sandbox.rs      代码执行沙箱（默认关闭）
├── cliproxy.rs     CLIProxyAPI 客户端（模型列表、验证、OAuth 登录）
├── expenses.rs     费用 API
└── api.rs          Axum 路由表（含 /v1/tools、/v1/mcp/servers）
```

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

### 2. 下载并启动 Wonderland

```powershell
git clone https://github.com/Alice-Marx/Rust_for_AI-agent.git F:\codex\Rust_for_AI-agent
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

### 桌面安装包：零配置登录（推荐）

从 `v0.1.1` 开始，Windows 安装包已经内置 CLIProxyAPI Windows x64 sidecar。首次通过开始菜单或桌面快捷方式启动时，启动器会自动：

- 创建只监听 `127.0.0.1:18317` 的 CLIProxyAPI 配置；
- 生成仅供本机 Agent 使用的随机 API / Management 密钥；
- 在 `%LOCALAPPDATA%\WonderlandData\CLIProxyAPI` 保存配置、运行日志和 OAuth 凭据；
- 启动 Wonderland，并使“账号登录”“刷新模型”“验证模型”立即可用。

因此使用桌面安装包时，不需要安装 Go、不需要手写 YAML、也不需要设置 `CLIPROXYAPI_*` 环境变量。打开桌面版后选择服务，点击“账号登录”，在浏览器完成授权，再依次点击“检查登录”和“刷新模型”。未登录账号时模型列表为空是正常现象。

如果 sidecar 无法启动，桌面端会显示可读提示；详细原因位于 `%LOCALAPPDATA%\WonderlandData\launcher.log`。该目录不会在升级或卸载时删除，因此订阅账号 OAuth 凭据不会丢失。

### 3. 从源码运行时手动准备 CLIProxyAPI

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

保持这个窗口运行。CLIProxyAPI 默认监听 `http://127.0.0.1:8317`，Wonderland 使用 `/v1` API，登录管理使用 `/v0/management` API。

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

### 4. 在 Wonderland 中选择 API 并登录账号

先在一个新的 PowerShell 窗口配置 Provider：

```powershell
$env:AGENT_PROVIDER="cliproxyapi"
$env:CLIPROXYAPI_BASE_URL="http://127.0.0.1:8317/v1"
$env:CLIPROXYAPI_API_KEY="local-agent-key"
$env:CLIPROXYAPI_MODEL="gpt-5.4"
$env:CLIPROXYAPI_MANAGEMENT_URL="http://127.0.0.1:8317/v0/management"
$env:CLIPROXYAPI_MANAGEMENT_KEY="local-management-key"
```

再次启动 Wonderland：

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

保持 Wonderland 运行，调用 `/v1/agent/run`：

```powershell
$request = @{
  session_id = "demo-session"
  user_id = "alice"
  input = "请研究 Wonderland，并总结我的费用预算"
} | ConvertTo-Json

Invoke-RestMethod `
  -Method Post `
  -Uri http://127.0.0.1:8080/v1/agent/run `
  -ContentType "application/json" `
  -Body $request
```

请求和响应的完整字段见下文「API 示例」。除 `session_id` 和 `input` 外，`AgentRequest` 还支持：

- `mode`：权限模式，可选 `default` / `plan` / `acceptEdits` / `bypassPermissions` / `dontAsk`，缺省为 `default`。
- `cwd`：工具执行的工作目录，缺省为服务端当前目录；权限规则也从该目录加载。
- `model`：覆盖本次请求的模型名。
- `skills`：按目录名显式启用本地 SKILL.md 技能。

`AgentResponse` 在原有 `output / plan / reflection / evaluation` 等字段之外，新增 `turns`（工具调用轮数）、`tool_calls`（工具调用总次数）、`usage`（本次 run 累计的 token 用量，含缓存命中细分）和 `todos`（本次 run 结束时的任务清单，由 `TodoWrite` 工具维护）。

**关于 headless 权限**：HTTP 服务没有交互能力，权限评估为 `Ask` 时由 `DenyAllHandler` 一律拒绝（拒绝原因会作为工具错误反馈给模型，而不是中断请求）。要让 Agent 真正执行工具，请二选一：

- 请求中传 `"mode": "bypassPermissions"`（仍会受 `.git/` / `.claude/` 写保护约束）；或
- 在 `cwd` 下的 `.claude/settings.json` 或 `.wonderland/settings.json` 中配置 `permissions.allow` 规则，规则格式为 `"Bash(git *)"`（内容级前缀匹配）或 `"FileWrite"`（整工具放行）：

```json
{
  "permissions": {
    "allow": ["FileRead", "Glob", "Grep", "Bash(git *)", "FileWrite", "FileEdit"],
    "deny": ["Bash(rm *)"]
  }
}
```

此时模型调用路径是：

```text
Wonderland -> http://127.0.0.1:8317/v1/chat/completions -> CLIProxyAPI -> 已验证的订阅账号模型
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
| `openai` | 任意 OpenAI-compatible 服务或网关 | `OPENAI_API_KEY`（或 `AGENT_API_KEY`），可覆盖 `OPENAI_BASE_URL` / `AGENT_BASE_URL`、`OPENAI_MODEL` / `AGENT_MODEL` |
| `anthropic`（别名 `claude`） | Anthropic Messages API 原生直连 | `ANTHROPIC_API_KEY`，可选 `ANTHROPIC_BASE_URL`、`ANTHROPIC_MODEL` |
| `deepseek` | DeepSeek 官方端点 | `DEEPSEEK_API_KEY`，可选 `DEEPSEEK_BASE_URL` / `DEEPSEEK_MODEL` |
| `kimi`（别名 `moonshot`） | Moonshot / Kimi | `MOONSHOT_API_KEY` 或 `KIMI_API_KEY`，可选 `KIMI_BASE_URL` / `KIMI_MODEL` |
| `qwen`（别名 `dashscope`） | 阿里云百炼 | `DASHSCOPE_API_KEY`，可选 `QWEN_MODEL` |
| `glm`（别名 `zhipu`） | 智谱 GLM | `ZHIPU_API_KEY`，可选 `GLM_MODEL` |
| `grok`（别名 `xai`） | xAI Grok | `XAI_API_KEY`，可选 `XAI_MODEL` |
| `gemini`（别名 `google`） | Gemini OpenAI 兼容端点 | `GEMINI_API_KEY`，可选 `GEMINI_MODEL` |
| `openrouter` | OpenRouter 聚合网关 | `OPENROUTER_API_KEY`，可选 `OPENROUTER_MODEL` |
| `ollama`（别名 `local`） | 本地推理服务，不需要密钥 | 可选 `OLLAMA_BASE_URL` / `OLLAMA_MODEL` |

未设置 `AGENT_PROVIDER` 时会按已配置的密钥自动推断（CLIProxyAPI → OpenAI → Anthropic → 离线），修改环境变量后需要重启 Wonderland 进程。

**模型能力档案**：请求里带 `model` 时，`src/model_profile.rs` 会按模型 id 解析上下文窗口、输出上限、推理风格、缓存策略与编辑工具偏好，并把对应的工具使用指引写进系统提示词（例如 GPT 系优先 `ApplyPatch`、Claude 系优先 `FileEdit`）。可用 `AGENT_MODEL_PROFILE` 强制指定档案（`anthropic-claude` / `openai-reasoning` / `openai-chat` / `deepseek-chat` / `kimi` / `qwen` / `gemini` / `glm` / `grok` / `generic`）。

**提示缓存与推理参数**：

- Anthropic 直连时自动打 `cache_control` 断点（system、最后一个工具、会话前缀），`usage` 里的 `cache_read_tokens` / `cache_creation_tokens` 反映真实命中情况；CLI 的 `/cost` 会显示命中率。
- OpenAI 推理模型（`gpt-5*` / `codex` / `o3` / `o4` / `grok`）默认下发 `reasoning_effort=medium`，可用 `AGENT_REASONING_EFFORT=low|medium|high` 调整。
- Anthropic 的扩展思考只在显式配置 `AGENT_REASONING_EFFORT` 时开启（会产生额外计费），预算默认按档位换算，也可用 `AGENT_THINKING_BUDGET` 精确指定 token 数；开启时自动抬高 `max_tokens` 以满足 `max_tokens > budget_tokens`。
- DeepSeek / Kimi 等端点返回的 `reasoning_content` 会作为思维链记录进会话轨迹，但不会回流到请求体。

### 环境变量

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `AGENT_PROVIDER` | 模型后端：`offline` / `cliproxyapi` / `openai` | 按 `CLIPROXYAPI_*` / `OPENAI_API_KEY` 自动推断，否则 `offline` |
| `AGENT_CONTEXT_WINDOW` | 模型上下文窗口大小（token），覆盖能力档案取值 | 按模型档案（缺省 `200000`） |
| `AGENT_MAX_OUTPUT_TOKENS` | 单次模型调用的最大输出 token | 按模型档案（缺省 `8192`） |
| `AGENT_MODEL_PROFILE` | 强制指定模型能力档案 | 未设置（按模型 id 匹配） |
| `AGENT_REASONING_EFFORT` | 推理档位 `low` / `medium` / `high`；`off`/`none` 关闭 | 未设置（OpenAI 推理模型默认 `medium`） |
| `AGENT_THINKING_BUDGET` | Anthropic `thinking.budget_tokens` | 按档位换算（`low`=2048 / 其他=8000 / `high`=16000） |
| `AGENT_MODEL` | CLI/子代理使用的模型名 | 未设置（由 provider 决定） |
| `AGENT_CWD` | CLI 的工作目录（同时决定自定义命令加载位置） | 当前目录 |
| `AGENT_MCP_TIMEOUT_MS` | MCP 单次请求超时 | `30000` |
| `AGENT_PERMISSION_MODE` | CLI 的默认权限模式（等价于 `--mode`） | 未设置（即 `default`） |
| `AGENT_DATA_DIR` | 记忆、评估、会话等数据的保存目录 | `.agent-data/` |
| `AGENT_SHELL` | Bash 工具使用的 shell | Windows 上优先 PATH 中的 `bash`，否则 `cmd /C`；其他平台 `sh -c` |
| `AGENT_SERVER_URL` / `AGENT_USER_ID` / `AGENT_SESSION_ID` | CLI 的服务地址、用户、会话 | 见 `wonderland-cli --help` |
| `RUST_LOG` | tracing 日志级别 | `info` |

### 常见问题

- `503 CLIProxyAPI is not configured`：如果使用 `v0.1.1` 或更高版本的桌面安装包，请完全退出后重新从开始菜单启动桌面版；启动器会自动配置本机 sidecar。若仍失败，查看 `%LOCALAPPDATA%\WonderlandData\launcher.log`。从源码运行时，则需要在启动 Wonderland 的同一个 PowerShell 窗口设置 `AGENT_PROVIDER=cliproxyapi` 和 `CLIPROXYAPI_*` 环境变量。
- `CLIProxyAPI models request failed`：检查 CLIProxyAPI 是否运行在 `8317` 端口、`CLIPROXYAPI_API_KEY` 是否与 `config.yaml` 的 `api-keys` 一致。
- 登录接口返回 404：检查 CLIProxyAPI 是否配置了 `remote-management.secret-key`，并确认 `CLIPROXYAPI_MANAGEMENT_KEY` 一致；`allow-remote: false` 允许本机调用，但仍然要求管理密钥。
- 登录状态长时间为 `wait`：确认浏览器已经完成授权，并保持 CLIProxyAPI 进程运行；OAuth 状态可能因超时失效，需要重新发起登录。
- `/v1/providers/cliproxyapi/models` 没有模型：先完成至少一个订阅账号登录，再重新请求模型列表。

## CLI 终端客户端

CLI 是纯命令行客户端，适合服务器、远程 SSH、脚本和不需要图形界面的用户。它复用已经启动的 Wonderland HTTP 服务，因此长期记忆、规划、评估和 CLIProxyAPI 账号配置保持一致。

先保持后端服务运行，再执行：

```powershell
Set-Location F:\codex\Rust_for_AI-agent

# 查看所有命令
cargo run --bin wonderland-cli -- --help

# 进入连续聊天模式；输入 /exit 退出
cargo run --bin wonderland-cli

# 单次执行任务
cargo run --bin wonderland-cli -- run "分析我的 Rust 项目并给出改进计划"

# 指定用户和会话，确保长期记忆隔离和会话连续
cargo run --bin wonderland-cli -- --user-id alice --session-id project-001 chat

# 检查后端、查看订阅模型、验证模型
cargo run --bin wonderland-cli -- health
cargo run --bin wonderland-cli -- models
cargo run --bin wonderland-cli -- verify --model gpt-5.4

# 管理会话：列出全部会话、查看单个会话的消息历史与任务清单
cargo run --bin wonderland-cli -- sessions
cargo run --bin wonderland-cli -- session demo-session

# 继续该用户最近一次会话，而不是新建会话
cargo run --bin wonderland-cli -- --continue chat

# 指定工作目录（权限规则与自定义命令都从这里加载）和模型
cargo run --bin wonderland-cli -- --cwd F:/my-project --model deepseek-chat chat

# 查看工具面与 MCP 连接情况
cargo run --bin wonderland-cli -- tools
cargo run --bin wonderland-cli -- mcp
cargo run --bin wonderland-cli -- commands

# 发起 CLIProxyAPI OAuth 登录；--wait 会在终端轮询验证结果
cargo run --bin wonderland-cli -- login codex --wait
```

聊天模式内还提供：`/cost`（当前会话 token 用量与提示缓存命中率）、`/session`、`/tools`、`/mcp`、`/commands`、`/skills`、`/sessions`、`/health`、`/models`。

**自定义斜杠命令**（借鉴 Claude Code）：在项目里放 `.claude/commands/review.md`（或 `.wonderland/commands/`）：

```markdown
---
description: 审查当前分支的未提交改动
argument-hint: [重点模块]
---
请阅读 `git diff` 的全部改动，逐文件给出审查意见，重点关注 $ARGUMENTS 的并发与错误处理。
```

之后在聊天里输入 `/review 上下文压缩` 就会把模板展开成完整提示词发给 Agent；子目录形成命名空间（`.claude/commands/frontend/component.md` → `/frontend:component`），`$1`..`$9` 可取位置参数。`wonderland-cli commands` 可列出当前项目可用的命令。

**MCP 工具**（借鉴 Claude Code / Codex / Kimi CLI）：在项目里配置 `.mcp.json`：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "F:/my-project"]
    },
    "readonly-docs": {
      "command": "python",
      "args": ["tools/docs_mcp.py"],
      "read_only": true
    }
  }
}
```

后端启动时会连接这些服务器，把工具注册为 `mcp__filesystem__read_file` 这类名字；权限规则（`permissions.allow` / `deny`）、hooks、`/v1/tools` 与管理接口对它们与内置工具一视同仁。单个服务器启动失败只会打日志并跳过，不影响服务启动；`read_only: true` 会把该服务器的全部工具按只读处理。

CLI 提供全局 `--mode` 参数控制工具权限模式，也可用环境变量 `AGENT_PERMISSION_MODE` 设置。可选值与 Claude Code 对齐：

| 模式 | 行为 |
| --- | --- |
| `default` | 无 allow 规则命中的工具调用进入 Ask；HTTP 层 Ask 即拒绝 |
| `plan` | 只读：拒绝一切写入类工具调用 |
| `acceptEdits` | 自动放行非破坏性的文件读写工具（FileRead/FileWrite/FileEdit/Glob/Grep），Bash 仍需规则或授权 |
| `bypassPermissions` | 全部放行（`.git/` 内部与 `.claude/` 目录写保护不可绕过） |
| `dontAsk` | headless：不询问用户，未命中 allow 规则的调用直接拒绝 |

注意：CLI 通过 HTTP 调用后端，Ask 决策在后端一律被拒绝（`DenyAllHandler`），所以想让 Agent 自由使用工具，应传 `--mode bypassPermissions`，或在项目目录的 `.claude/settings.json` / `.wonderland/settings.json` 中配置 `permissions.allow` 规则：

```powershell
# 以 bypassPermissions 模式进入聊天
cargo run --bin wonderland-cli -- --mode bypassPermissions chat

# 等价的环境变量写法
$env:AGENT_PERMISSION_MODE = "bypassPermissions"
cargo run --bin wonderland-cli -- run "把 src 下的 TODO 注释汇总成 docs/todo.md"
```

每次回答尾部会显示本轮的 turns / 工具调用次数 / token 用量；如果模型维护了任务清单，还会在回答后列出当前 todos。聊天模式内输入 `/help` 查看可用命令。

也可以通过环境变量固定服务地址和用户：

```powershell
$env:AGENT_SERVER_URL = "http://127.0.0.1:8080"
$env:AGENT_USER_ID = "alice"
cargo run --bin wonderland-cli -- chat
```

终端登录命令会打印 OAuth URL。复制到浏览器完成账号授权；如果当前终端支持手动打开链接，也可以使用：

```powershell
$url = "上一步输出的 OAuth URL"
Start-Process $url
```

## Hooks 与文件定义子代理

### Hooks（借鉴 Claude Code）

在项目 `.claude/settings.json` 或 `.wonderland/settings.json` 中配置命令 hook，引擎会在对应事件点执行并把结果纳入决策：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|FileWrite",
        "hooks": [
          {"type": "command", "command": "python check_policy.py", "timeout": 30}
        ]
      }
    ],
    "SessionStart": [
      {"hooks": [{"type": "command", "command": "echo 记住团队约定：所有提交用中文"}]}
    ]
  }
}
```

- 支持事件：`PreToolUse`（可拦截）、`PostToolUse`（可附加上下文）、`SessionStart`（输出并入用户提示词）、`Stop`（仅通知）。
- `matcher` 为空或 `"*"` 匹配所有工具，`"Bash|FileWrite"` 多值精确匹配。
- hook 进程从 stdin 收到 `{"event","session_id","tool_name","tool_input","tool_output"}` JSON；环境变量 `WONDERLAND_PROJECT_DIR` 指向工作目录。
- 退出码 `2` = 拦截（stderr 作为拒绝理由反馈给模型）；退出码 `0` 时 stdout 若为 JSON，`{"decision": "block", "reason": "..."}` 拦截、`{"decision": "approve"}` 直接放行、`{"additionalContext": "..."}` 注入附加上下文；stdout 为纯文本时按附加上下文处理；其他退出码忽略。hook 超时（默认 60 秒）按非阻塞处理。

### 文件定义子代理（借鉴 Claude Code）

在 `.claude/agents/researcher.md`（或 `.wonderland/agents/`）中定义：

```markdown
---
name: explorer
description: 只读地探索代码库并回答结构问题
tools:
  - FileRead
  - Glob
  - Grep
max_turns: 10
---

你是代码库探索专家。用只读工具回答问题，引用具体文件路径与行号，不做任何修改。
```

之后模型就能通过 `Task(agent:explorer, task="...")` 委派。frontmatter 的 `tools` 既是该子代理的工具池也是权限白名单（缺省只有 `FileRead` / `Glob` / `Grep` / `WebFetch` 四个只读工具）；`Task` 与 `TodoWrite` 永远不进入子代理，避免嵌套委派。文件代理在每次 run 开始时从请求的 `cwd` 加载，与内置 research / expense 代理一起列在系统提示词中。

## 桌面版应用

桌面版是原生 Rust `egui` 应用，界面按 ChatGPT 类工作台设计：

- 左侧是历史任务列表，任务标题、聊天记录、执行计划和反思会保存到桌面端本地存储。
- “聊天式规划”显示当前任务的完整对话，右侧显示计划步骤和反思结果。
- “多任务并排”把多个任务同时展示在工作区，每个任务拥有独立 session，可以分别打开继续对话。
- 顶部可以修改 Wonderland 服务地址和用户 ID。
- 顶部可以选择 Codex/Claude/Kimi 等 OAuth 服务，生成授权链接、检查登录状态、读取模型并验证模型。
- 选择的模型会随当前 Agent 请求发送，桌面端不需要重启后端即可切换模型。
- Agent 请求在后台线程执行，窗口不会因为模型请求阻塞；后端不可用时会在当前任务中显示错误。

启动桌面版：

```powershell
Set-Location F:\codex\Rust_for_AI-agent
cargo run --bin wonderland-desktop
```

如果已经构建过，也可以直接运行：

```powershell
cargo build --release --bin wonderland-desktop
& "$env:CARGO_TARGET_DIR\release\wonderland-desktop.exe"
```

桌面版启动前需要先启动 Wonderland 后端：

```powershell
# 窗口 1：后端，先配置 OPENAI 或 CLIProxyAPI 环境变量
cargo run

# 窗口 2：桌面版
cargo run --bin wonderland-desktop
```

首次使用时，在顶部“API 登录”区域选择服务，点击“账号登录”，再点击 OAuth 链接完成浏览器授权；授权完成后点击“检查登录”，随后点击“刷新模型”和“验证模型”。桌面版不会单独保存模型密钥，也不会直接读取 CLIProxyAPI OAuth token；账号验证由 Wonderland 后端转发给 CLIProxyAPI 完成。

## 安装包和 npm CLI

### Windows 桌面安装包

安装包包含下列原生程序和运行组件：

- `wonderland.exe`：后端服务。
- `wonderland-desktop.exe`：桌面 GUI。
- `wonderland-cli.exe`：原生 Rust CLI。
- `cliproxyapi\cli-proxy-api.exe`：随安装包分发的本机 CLIProxyAPI sidecar。

桌面版使用 **Inno Setup 7** 制作标准 Windows 安装程序。安装包会把这三个程序安装到 `%LOCALAPPDATA%\Programs\Wonderland`，创建开始菜单快捷方式（可选桌面快捷方式），并把安装目录加入当前用户的 PATH。因此安装桌面版后，重新打开 PowerShell 就可以直接运行 `wonderland-cli`，不需要额外安装 CLI。

生成 Windows x64 安装包：

```powershell
Set-Location F:\codex\Rust_for_AI-agent
& .\packaging\windows\build-windows-package.ps1
```

构建脚本会使用 `D:\Jianwei_Li\rust` 中的 Rust/Cargo 缓存，并调用 Inno Setup 7 的 `ISCC.exe`。若编译器不在默认位置，可以显式指定：

```powershell
& .\packaging\windows\build-windows-package.ps1 -InnoCompiler "C:\Program Files\Inno Setup 7\ISCC.exe"
```

生成文件：

```text
dist\Wonderland-Setup-0.4.0-x64.exe
```

双击该 `.exe` 并按向导安装。安装完成页可直接启动桌面版；桌面版启动器会在需要时隐藏启动本机 CLIProxyAPI 和 Wonderland 后端服务。安装包会迁移并移除旧 ZIP 版的 `%LOCALAPPDATA%\RustAIAgent` 程序目录，但不会删除 `%LOCALAPPDATA%\WonderlandData` 或 CLIProxyAPI 的 OAuth 账号凭据。

安装完成后可以双击桌面快捷方式启动后端和桌面版，也可以手动运行：

```powershell
wonderland-cli health
wonderland-cli chat
```

卸载：

在 Windows 的“已安装的应用”中选择 **Wonderland** 卸载，或运行安装目录中的 `unins000.exe`。卸载不会删除 `%LOCALAPPDATA%\WonderlandData` 或 CLIProxyAPI 的 `auth-dir` 账号凭据。

### npm 独立 CLI

`packaging/npm/wonderland-cli` 是零依赖 npm 包，不要求安装 Rust。它通过 HTTP 调用已经启动的 Wonderland 后端，适合单独安装 CLI：

```powershell
npm install --global rust-ai-wonderland-cli
wonderland-cli --help
wonderland-cli chat
wonderland-cli run "分析我的项目"
```

如果还没有发布到 npm，也可以直接安装本地 tarball：

```powershell
Set-Location F:\codex\Rust_for_AI-agent
npm.cmd pack .\packaging\npm\wonderland-cli --pack-destination .\dist
npm.cmd install --global .\dist\wonderland-cli-0.4.0.tgz
```

需要发布 npm 包时：

```powershell
Set-Location F:\codex\Rust_for_AI-agent\packaging\npm\wonderland-cli
npm login
npm publish
```

CLI 默认连接 `http://127.0.0.1:8080`，也可以设置：

```powershell
$env:AGENT_SERVER_URL = "http://127.0.0.1:8080"
$env:AGENT_USER_ID = "alice"
wonderland-cli health
```

## API 示例

健康检查：

```bash
curl http://127.0.0.1:8080/health
```

运行 Agent（`mode` 和 `cwd` 可选；响应中包含 `turns`、`tool_calls` 和 `usage` 字段）：

```bash
curl -X POST http://127.0.0.1:8080/v1/agent/run \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo-session","user_id":"alice","input":"请研究 Wonderland，并总结我的费用预算","mode":"bypassPermissions","cwd":"F:/codex/Rust_for_AI-agent"}'
```

会话查询（会话由后端按 `session_id` 持久化，包含完整的多轮消息与工具调用轨迹）：

```bash
# 列出所有会话摘要（id / message_count / updated_at / user_id，按更新时间倒序）
curl http://127.0.0.1:8080/v1/sessions

# 按用户过滤会话（CLI 的 --continue 使用同一接口）
curl 'http://127.0.0.1:8080/v1/sessions?user_id=alice'

# 当前注册的全部工具（内置 + MCP）：name / source / read_only
curl http://127.0.0.1:8080/v1/tools

# 已连接的 MCP 服务器及其工具名
curl http://127.0.0.1:8080/v1/mcp/servers

# 读取单个会话的完整内容
curl http://127.0.0.1:8080/v1/sessions/demo-session
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
cargo test --lib   # 191 个测试
cargo clippy --all-targets --all-features -- -D warnings
```

## 设计来源

项目根目录下的 `claude-code/`、`codex/`、`kimi-cli/`、`kimi-code/`、`deepseek-harness/` 都是只读的上游参考检出（已在 `.gitignore` 中忽略，不参与编译）。借鉴的目的不是复制代码，而是让主程序在调用各家模型 API 时，行为尽量贴近该模型官方客户端的效率。

来自 Claude Code 的部分：

- **工具协议**：`ChatMessage / ContentBlock（text / tool_use / tool_result）/ ToolDefinition / Usage / StopReason` 的中立消息格式，以及 OpenAI-compatible 端点上的 tool_calls 双向转换。
- **权限管线**：五种 `PermissionMode`、`allow / ask / deny` 规则（`"Bash(git *)"` 前缀匹配语法）、首命中胜出的评估顺序、`.git/` 内部写保护，以及 Bash 复合命令逐段评估（任何一段 Deny 则整体 Deny）。
- **上下文压缩**：阈值公式 `context_window - min(max_output_tokens, 20000) - 13000`（13000 对应 Claude Code 的 `AUTOCOMPACT_BUFFER_TOKENS`），压缩时保留最后一个「干净」user 消息及其后缀，保证不切断 tool_use/tool_result 配对。
- **上下文构建**：系统提示词静态段在前、动态段在后以保护 prompt cache；`CLAUDE.md` / `AGENTS.md` 层级加载与 `@path` include 语义。
- **TodoWrite / WebFetch / Task**：会话级任务清单的 content + activeForm 双形态与单一 in_progress 约束；WebFetch 的域名级权限规则与本地/私网地址拒绝；Task 的子代理委派语义。
- **自定义斜杠命令**：`.claude/commands/**/*.md` 的 frontmatter、命名空间与参数替换语义。
- **MCP**：`mcpServers` 配置形状与 `mcp__<server>__<tool>` 的工具命名约定。

来自 Codex 的部分：

- **apply_patch（V4A）**：`*** Update File:` / `@@` 定位提示与三级容错匹配的多文件补丁。
- **推理档位**：`model_reasoning_effort` 的默认档位与「按模型族切换工具偏好」的做法（GPT 系优先补丁、其他优先精确替换）。
- **模型能力档案**：把上下文窗口与输出上限从「全局常量」改为按模型解析，避免不同厂商模型共用一套错误阈值。

来自 Kimi CLI / Kimi Code 的部分：

- **多厂商接入约定**：一套 OpenAI-compatible 协议适配多家端点，各自独立的 base URL / 密钥 / 模型变量命名。
- **思维链处理**：`reasoning_content` 只读回显、不回传给端点，避免把推理内容当成事实。

来自 DeepSeek Harness 的部分：

- **工具执行管线分层**：权限、hooks、结果规范化与上下文注入各司其职，工具定义本身不耦合策略。
- **会话按用户隔离与事件轨迹可回放**：会话记录 `user_id`、消息与工具调用全量落盘，便于按用户检索与复盘。

## 设计说明

工程刻意把 `ModelProvider`、`Planner`、`Tool`、`PermissionHandler`、`MemoryStore` 和 `EvaluationStore` 做成可替换接口：`ModelProvider` 已有 offline / OpenAI-compatible（含 CLIProxyAPI）实现，`PermissionHandler` 在 HTTP headless 场景使用 `DenyAllHandler`，交互式前端可注入自己的实现弹窗询问用户。后续可在此基础上接入向量检索、OpenTelemetry、领域评测集和隔离执行服务。
