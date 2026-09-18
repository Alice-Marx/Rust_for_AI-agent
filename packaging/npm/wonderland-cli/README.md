# rust-ai-wonderland-cli

Wonderland 的 npm 终端客户端。该包不要求安装 Rust，使用 Node.js 调用已经启动的 Rust Agent HTTP 服务。

## 安装

发布到 npm 后：

```powershell
npm install --global rust-ai-wonderland-cli
wonderland-cli --help
```

在本地从源码安装：

```powershell
Set-Location F:\codex\Rust_for_AI-agent\packaging\npm\wonderland-cli
npm install --global .
```

## 使用

Rust Agent 后端默认地址是 `http://127.0.0.1:8080`：

```powershell
wonderland-cli                                                  # 交互式聊天
wonderland-cli run "分析我的 Rust 项目"                            # 单次任务
wonderland-cli --continue chat                                  # 复用该用户最近一次会话
wonderland-cli --cwd F:/my-project --model deepseek-chat chat    # 指定工作目录与模型
wonderland-cli health / models / sessions / session <id>         # 服务与模型、会话管理
wonderland-cli tools / mcp / commands                            # 工具面、MCP 服务器、自定义命令
wonderland-cli verify --model gpt-5.4
wonderland-cli login codex --wait
```

聊天模式内置命令：`/help` `/exit` `/health` `/models` `/skills` `/sessions` `/session` `/cost` `/tools` `/mcp` `/commands`。

自定义斜杠命令与主程序一致：项目里 `.claude/commands/**/*.md`（或 `.wonderland/commands/`）定义模板，支持 frontmatter 的 `description` / `argument-hint`、子目录命名空间（`/frontend:component`）与 `$ARGUMENTS` / `$1`..`$9` 参数替换。

可以通过 `AGENT_SERVER_URL`、`AGENT_USER_ID`、`AGENT_SESSION_ID`、`AGENT_CWD`、`AGENT_MODEL` 和 `AGENT_PERMISSION_MODE` 配置服务、用户、会话、工作目录、模型与权限模式。
