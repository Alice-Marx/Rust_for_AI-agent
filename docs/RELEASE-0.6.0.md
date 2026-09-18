# Wonderland 0.6.0

本次升级完善了多模型编码工作区的实时输出、工具续轮、订阅登录与桌面操作，修复断流误判、模型推理丢失、订阅配置端口冲突和默认模型未生效等问题。

- 新桌面：薄荷绿深色主题、常规中文字体、原创矢量图标、代码块复制、模型/推理/权限选择和小窗口布局。
- OpenAI Responses、Claude Messages、DeepSeek/Kimi Chat SSE，保留提供商推理与缓存信息。
- Windows AppContainer 代码沙箱、MCP HTTP/旧 SSE/resources/prompts/OAuth、SQLite 中文会话检索。
- CLIProxyAPI 7.3.7 原版 sidecar 与管理代理，支持订阅授权、模型目录和账号管理。
- npm 包名为 `rust-ai-wonderland-cli`，保留 `wonderland`、`wonderland-cli` 命令。`wonderland-cli` npm 名称由其他作者占用，因此未使用。

Windows 下载 `Wonderland-Setup-0.6.0-x64.exe`，或解压 `Wonderland-0.6.0-windows-x64.zip`。运行前可用 `SHA256SUMS.txt` 核对文件。npm：`npm install -g rust-ai-wonderland-cli@0.6.0`，然后连接本地 Wonderland 服务。

验证：250 个 Rust 单元测试、7 个集成测试、2 个 npm 流式测试通过；真实 Kimi 2.5/2.8 账号完成读取、编辑和推理续聊。其他提供商尚无本次真实账号对照测试。完整范围与限制见 [验证记录](VALIDATION-0.6.0.md)。

升级保留用户数据。安装程序未签名。Bash/MCP/hooks 并未被全应用沙箱隔离；SandboxRun 的代码执行才使用 OS 隔离。高级 MCP sampling/elicitation 暂未提供。
