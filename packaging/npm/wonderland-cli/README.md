# rust-ai-wonderland-cli

Wonderland 0.7.0 的零依赖终端客户端。Node.js 18+，连接已启动的 Wonderland Rust 后端；后端来自桌面安装包、便携包或源码构建。npm 包不包含后端。

```sh
npm install -g rust-ai-wonderland-cli
wonderland-cli --version
wonderland-cli health
wonderland-cli login kimi --wait
wonderland-cli models
wonderland-cli --cwd /path/to/project --model kimi-k2.5 --reasoning on chat
wonderland-cli --continue chat
wonderland-cli search "会话关键词"
wonderland-cli profile --model gpt-5.4
wonderland-cli mcp-login remote --wait
wonderland-cli mcp-reload
```

默认 SSE 流式输出；`--no-stream` 获取完整响应。交互式工具权限提示展示完整参数，只有手动输入 y 才批准。工作区内读取默认允许，明确拒绝/询问规则优先。`--mode plan` 只读，`--mode acceptEdits` 允许编辑。

环境变量：`AGENT_SERVER_URL`（默认 http://127.0.0.1:8080）、`WONDERLAND_SERVER_TOKEN`、`AGENT_USER_ID`、`AGENT_SESSION_ID`、`AGENT_CWD`、`AGENT_MODEL`、`AGENT_PERMISSION_MODE`、`AGENT_REASONING_EFFORT`。

提供 `wonderland` 和 `wonderland-cli` 两个命令名；与原生安装版一起使用时优先运行 `wonderland-cli`，避免后端 `wonderland.exe` 的同名冲突。

聊天命令：`/help`、`/exit`、`/health`、`/models`、`/sessions`、`/session`、`/cost`、`/tools`、`/mcp`、`/skills`、`/commands`。支持项目 `.claude/commands`、`.wonderland/commands` 的 Markdown 模板。

[项目与完整文档](https://github.com/Alice-Marx/Rust_for_AI-agent)
