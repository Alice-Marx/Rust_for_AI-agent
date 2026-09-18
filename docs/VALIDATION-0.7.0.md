# Wonderland 0.7.0 验证记录

## 自动化检查

Windows 本机通过：278 个库测试、8 个桌面状态测试、7 个协议集成测试；2 个依赖已安装 CLI 的检查默认忽略，另行手动执行。npm 的 2 个 SSE 测试通过。以下测试、正式构建和 npm 打包预览均已完成。

- `cargo fmt --all --check`
- `cargo test --locked --all-targets --features ui-snapshots`
- `cargo build --locked --release --bins`
- `npm test --prefix packaging/npm/wonderland-cli`
- `npm pack packaging/npm/wonderland-cli --dry-run`

桌面工作台覆盖终端会话状态、ANSI/Unicode、滚动回滚、输入控制序列、PTY 退出清理、文件编辑关闭确认、工作区切换、Git diff 和异步请求乱序。桌面渲染回归在欢迎页、对话、设置、CLI 工作台、文件编辑、Git 变更和终端页使用 940×620 小窗口检查。

## CLI 与账号

CLI 桥接只调用原程序，不复制或修改参考项目源码。已验证 PATH/源码入口识别、Windows 参数处理、版本探测，以及 Codex、Claude Code 和 Wonderland CLI 的本机版本输出。Kimi 订阅账号的真实流式、工具读取/修改和推理续聊测试沿用上一版记录；本次桌面工作台没有自动发送模型请求。

官方 CLI 的登录、订阅额度、模型目录、权限提示和工具行为仍由其自身控制。Kimi Python 与 Kimi Node 可能共用 `kimi` 命令，工作台通过入口指纹和版本检查选择正确配置。

## 边界

- 内嵌终端运行在当前用户权限下，支持常见 VT100/xterm 控制序列，不提供 Kitty/sixel 等所有终端扩展。
- 终端采用固定字符网格。运行时缩窄窗口可能截断已有长行，TUI 会通过自身重绘适配；退出后的最终记录固定保存。需要完整的高级终端能力时可使用系统终端入口。
- Linux 清理同一 OS 会话的子作业，使用 pidfd 绑定进程；要求内核 5.3+。macOS 每次发信号前核对进程启动时间、用户与会话，但系统没有等同 pidfd 的原子信号接口。主动脱离会话或切换用户的进程不在此清理范围。
- Agent 的 SandboxRun 与桌面终端是两个边界；Bash、MCP 子进程和 hooks 不会因为打开内嵌终端而获得额外隔离。
- Windows 安装程序未进行代码签名。Linux/macOS PTY 和安装包需要各平台工具链，CI 构建不替代目标系统上的实机验证。
