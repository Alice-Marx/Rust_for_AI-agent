# rust-ai-wonderland-cli

Wonderland 0.8.0 工作台预览版的零依赖终端客户端。Node.js 18+，连接已启动的 Wonderland Rust 后端；后端来自桌面安装包、便携包或源码构建。npm 包不包含后端。

新增官方应用任务命令：

```sh
wonderland-cli apps
wonderland-cli --cwd /path/to/project --model kimi-for-coding work create kimi-cli "修复测试"
wonderland-cli work start <id>
wonderland-cli work events <id>
wonderland-cli work approve <id> <request-id> allow
wonderland-cli work cancel <id>
wonderland-cli work accept <id> "独立检查变更和测试结果"
wonderland-cli intelligence refresh
```

受管任务目前支持官方 Codex 与 Kimi CLI（Python），沿用原工具登录；其余工具提供终端入口。任务完成执行后进入待验收。LiveBench 已支持在线刷新，实时价格解析及自动多模型调度尚未启用。API 对话与官方工具任务是独立执行路径。

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
