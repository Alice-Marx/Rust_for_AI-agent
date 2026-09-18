# Wonderland 0.6.0 验证记录

验证日期：2026-09-18。所有数字均来自本次执行，不能作为与官方客户端等效的性能承诺。

## 环境与自动回归

- Windows x64，Rust 1.98.1（GNU/MinGW），Node.js 24.18，Python 3.13。
- `cargo test --locked --all-targets`：250 个单元测试、7 个集成测试通过。
- `cargo test --locked --all-targets --features ui-snapshots`：同样覆盖开发用桌面渲染入口。
- `npm test --prefix packaging/npm/wonderland-cli`：2 个流式客户端测试通过。
- `cargo fmt --all --check`、`git diff --check` 为发布前检查。

回归覆盖：UTF-8 任意切片/CR/LF SSE、终态缺失、交错 Responses 工具调用、加密推理跨提供商隔离、Claude 签名与 adaptive thinking、DeepSeek/Kimi 推理续轮、模型推理参数、MCP HTTP 会话头与持续连接、旧版 SSE 握手、OAuth 模拟服务端 PKCE/状态校验/刷新/登出、一次性权限批准、中文 SQLite 检索和索引清理。

Windows AppContainer 测试在沙箱外创建测试文件，验证沙箱代码无法读取/覆盖该文件或联网，同时可以写入自己的临时目录。该测试不证明整个应用均被隔离：宿主 Bash、文件工具、MCP、hooks 有各自权限边界。

## Kimi 订阅账号实测

通过 CLIProxyAPI **7.3.7** 原版 sidecar 完成设备授权；账号与令牌只保存在本机用户目录。以下任务使用真实模型、真实 SSE、实际工具和磁盘文件，不是模拟响应。

| 测试 | 结果 |
| --- | --- |
| kimi-k2.5，acceptEdits，推理关闭 | FileRead → FileEdit → FileRead，3 次工具调用；将 `sum(a,b)` 的减号改为加号 |
| 独立执行验证 | Node.js 执行 `sum(2,3) === 5` 通过 |
| kimi-k2.5，同一会话，plan，推理开启 | 再读取文件，正确确认 `sum(-2,5) === 3`，无写入 |
| kimi-k2.8，plan，low 推理 | 读取文件并正确确认结果，1 次工具调用，无写入 |
| 最终服务，kimi-k2.5，plan，推理关闭 | 首事件 83 ms、首段正文 3002 ms、总耗时 3461 ms、14 段正文增量、1 次工具调用 |

最后一项上报 input_tokens=3310、output_tokens=72、cache_read_tokens=2816。首事件可能是计划/工具等非正文事件；首段正文包含一次文件工具往返时间。仅为单次本机网络样本，未做 p50/p95 或官方客户端对照测试。缓存统计按提供商响应记录，不混算输入与缓存口径。

真实服务另外验证了：模型目录返回 10 个 Kimi 模型；不设置端口覆盖时沿用已有 sidecar 配置；连接配置 API 不返回密钥；Windows DPAPI 文件不含测试密钥明文；offline/subscription 可实时切换；保存的订阅默认模型能用于后续请求。

## 桌面与发布产物

桌面使用 egui 实际 framebuffer 进行欢迎页、对话/代码块、连接设置和 940×620 小窗口检查。截图中的对话是渲染测试夹具；真实模型验证以以上工具执行记录为准。

- 深色主题、常规字重中文字体、薄荷绿强调色、原创矢量图标、响应式侧栏。
- 模型目录、按模型约束的推理选择、逐项确认/只读/允许编辑、流式输出与取消。
- API 密钥配置、订阅授权、模型档案、MCP 状态/OAuth/重载、会话检索恢复。
- Windows 正式构建不启用 `ui-snapshots`；安装包只包含允许的二进制、DLL、启动脚本、文档和许可证。
- npm 为 Node.js 服务客户端，需有正在运行的 Wonderland 后端；不把它描述为独立打包的 Rust 服务。
- Inno Setup 6.3.1 编译成功，当前用户静默安装退出码 0；文件、开始菜单快捷方式和卸载注册完成，无须重启。
- 正式原生 CLI 在仅保留 Windows 系统目录的 PATH 下启动成功；正式后端/CLI 通过真实 Kimi 文件读取测试。

## 验证范围与限制

- OpenAI Responses、Anthropic Messages、DeepSeek 已做协议回归，尚无本次对应真实账号端到端测试，也没有“达到 Codex/Claude Code/DeepSeek Harness 相同效率”的证据。
- Linux/macOS 编译由 GitHub Actions 检查；本次 Windows 主机未实测这两个平台的沙箱。Unix 内存/进程数量配额尚不等同 Windows Job Object。
- MCP 已实现 tools/resources/prompts、stdio/HTTP/旧 SSE/OAuth；sampling、elicitation、通知驱动的工具更新尚未实现。
- CLIProxyAPI 以原版 sidecar 集成并提供管理 API 转发，其所有管理选项并未逐项做成 GUI。订阅能力仍取决于上游授权、账号额度和可用模型。
- Windows 安装程序未进行代码签名。沙箱 Python/Node 执行要求本机已安装对应解释器。

后续建议用固定任务集、相同模型/账号/网络分别测试成功率、工具往返、首字延迟、总耗时和 token 消耗，再讨论官方工具的效率对齐。
