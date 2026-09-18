# Reference repositories

本项目基于以下 Git 仓库进行结构对照和扩展：

- `https://github.com/solenovex/rust-ai-agent`
  - 对照 Agent runtime、session/context、tool abstraction、callback 分层。
- `https://github.com/solenovex/expense-tracker-api`
  - 对照 Axum router、`Arc<RwLock<HashMap<...>>>` store、CRUD handlers、summary aggregation、`x-api-key` middleware 和 request tracing。
- `https://github.com/router-for-me/CLIProxyAPI`
  - 作为独立 sidecar 接入，复用其 OAuth 登录、订阅账号凭据管理、多账号轮换和 OpenAI-compatible API。

## 上游客户端参考实现

为了在同一个工具集上获得接近各家官方客户端的效率，仓库根目录还检出了以下只读参考实现（`.gitignore` 已忽略，不参与编译，也不随本仓库提交）：

| 目录 | 上游 | 借鉴内容 |
| --- | --- | --- |
| `claude-code/` | Claude Code | 工具协议、权限管线、hooks、子代理、自定义命令、MCP 约定、上下文压缩 |
| `codex/` | OpenAI Codex CLI | V4A `apply_patch`、`model_reasoning_effort`、模型能力分级、execpolicy 前缀规则 |
| `kimi-cli/` | Kimi CLI | 多厂商接入约定、思维链只读回显、会话/技能布局 |
| `kimi-code/` | Kimi Code | 多厂商模型目录与端点配置方式 |
| `deepseek-harness/` | DeepSeek Harness | 工具执行管线分层、会话事件轨迹与用户隔离 |

本地参考检出位置（开发过程使用，不随本仓库提交）：

```text
F:/codex/_refs/rust-ai-agent
F:/codex/_refs/expense-tracker-api-2
F:/codex/_refs/CLIProxyAPI
```

目标仓库只保留针对 Agent 平台的实现，避免把参考仓库当作 vendored dependency；具体映射见主 README 的“设计说明”。
