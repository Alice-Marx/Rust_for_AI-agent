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
| `kimi-cli/` | Kimi CLI | 多厂商接入约定、工具续轮保留 reasoning_content、MCP OAuth、会话/技能布局 |
| `kimi-code/` | Kimi Code | 多厂商模型目录与端点配置方式 |
| `deepseek-harness/` | DeepSeek Harness | 工具执行管线分层、会话事件轨迹与用户隔离 |
| `dsh-desktop/` | anywhere-labs/dsh-desktop | 项目工作区、文件/变更/终端面板和外部 CLI 的桌面分层 |

## 0.6.0 参考映射

- Codex：Responses SSE 事件、function_call_output、加密 reasoning 原始封装、模型推理档位和 ApplyPatch 偏好。
- Claude Code：Messages thinking 签名、显式缓存、先读后写、权限确认、hooks、agents 与 commands。
- Kimi CLI/Code：thinking.type / effort、reasoning_content 续传、PKCE 登录、MCP 配置与令牌刷新。
- DeepSeek Harness：thinking/reasoning_effort 参数及工具续轮、会话查询层分离。
- CLIProxyAPI：以 MIT 原版 v7.3.7 sidecar 分发，完整管理路由通过本地 API 转发。主项目未复制账号令牌或上游参考目录。

本地参考目录是调研输入，不作为主项目编译依赖。Kimi 实测结果与其他提供商模拟回归验证分别记录在 docs/VALIDATION-0.6.0.md。参考协议不能证明与官方工具整体效果或效率相同。

## 0.7.0 桌面集成

桌面工作台由 Rust/egui 实现，通过独立的 `desktop_bridge`、`desktop_terminal`、`desktop_workspace` 分别管理 CLI 入口、原生 PTY 和项目文件/Git。参考 `dsh-desktop` 的工作区组织方式，保留 Wonderland 的多模型 API、订阅聚合和权限流程，没有将 DeepSeek 专用后端复制到其他提供商。

Codex、官方 Claude Code、Kimi CLI/Code 和 DeepSeek Harness 均通过原程序运行，源码导入只保存本地已构建入口。上游登录、配置、授权和更新保留在各自工具中，不由主项目重新实现。当前本地 `claude-code` checkout 是 Claude Code Best 分支，界面将它与官方 Claude Code 分开标示。主项目没有修改任何上述参考仓库。
