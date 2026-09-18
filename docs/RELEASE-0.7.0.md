# Wonderland 0.7.0

0.7.0 将桌面应用升级为面向项目的工作台：对话、文件、Git 变更、内嵌终端和官方 CLI 可以在同一个工作区中协作。

- 新增 Rust 原生 PTY 终端：Windows ConPTY、Unix PTY、VT100/xterm、Unicode、中文输入、粘贴、Ctrl+C、鼠标协议、替代屏幕、有限回滚和多会话。
- 新增 CLI 工作台：检测并直接调用 Codex、Claude Code、Kimi CLI、Kimi Code、DeepSeek Harness、Wonderland CLI 以及自定义入口；支持内置终端和系统终端。
- 新增项目文件浏览与编辑、Git 状态和 diff、未跟踪文本预览、最近工作区、外部修改提示和原子保存。
- 切换工作区时隔离异步响应和任务目录；运行中的任务不会被重定向，关闭窗口会处理未保存编辑和子进程。
- 继续复用上游官方 CLI，不修改 `claude-code`、`codex`、`kimi-cli`、`kimi-code` 和 `deepseek-harness` 源码，便于后续同步更新。
- 桌面主题补充原创矢量图标、终端标签滚动和退出码显示，并保持 940×620 小窗口可用。

下载 Windows 安装程序 `Wonderland-Setup-0.7.0-x64.exe` 或便携包 `Wonderland-0.7.0-windows-x64.zip`。npm 客户端安装：

```powershell
npm install -g rust-ai-wonderland-cli@0.7.0
```

账号、登录、权限和模型行为由各自官方 CLI 或 CLIProxyAPI 管理。内嵌终端是用户级终端，不等同于 Agent 的 SandboxRun 隔离。完整验证范围见 [VALIDATION-0.7.0.md](VALIDATION-0.7.0.md)。
