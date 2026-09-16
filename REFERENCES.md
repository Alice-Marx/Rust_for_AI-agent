# Reference repositories

本项目基于以下 Git 仓库进行结构对照和扩展：

- `https://github.com/solenovex/rust-ai-agent`
  - 对照 Agent runtime、session/context、tool abstraction、callback 分层。
- `https://github.com/solenovex/expense-tracker-api`
  - 对照 Axum router、`Arc<RwLock<HashMap<...>>>` store、CRUD handlers、summary aggregation、`x-api-key` middleware 和 request tracing。
- `https://github.com/router-for-me/CLIProxyAPI`
  - 作为独立 sidecar 接入，复用其 OAuth 登录、订阅账号凭据管理、多账号轮换和 OpenAI-compatible API。

本地参考检出位置（开发过程使用，不随本仓库提交）：

```text
F:/codex/_refs/rust-ai-agent
F:/codex/_refs/expense-tracker-api-2
F:/codex/_refs/CLIProxyAPI
```

目标仓库只保留针对 Agent 平台的实现，避免把参考仓库当作 vendored dependency；具体映射见主 README 的“设计说明”。
