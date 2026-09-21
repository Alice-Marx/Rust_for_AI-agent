# Wonderland 0.10.0 · 官方执行器扩展预览版

Work、Chat 和 Teams 新增 Claude Code 与 DeepSeek Harness 受管执行器。它们使用官方工具的原生协议，保留厂商提示、上下文处理及工具执行循环；不修改上游源码，不以直接模型 API 代替官方工具。CodexHost 的协议分层、版本隔离与用量区分设计用于指导适配。

## 使用方式

在“应用”选择“检测安装”，核对版本与实际入口，然后创建任务或在 Teams 中指定执行器。检测只运行版本检查与文件指纹读取，不读取账号凭据，不调用模型；登录状态和模型可用性不会根据安装结果推测。

| 执行器 | 接口与版本 | 登录与限制 |
| --- | --- | --- |
| Codex | 现有 app-server | 官方账号配置；执行前核验配置 |
| Kimi Python | 现有 Wire | 官方配置与订阅；必须使用精确模型 ID |
| Claude Code | 双向 stream-json，2.1.193 原生 npm 包 | 官方 OAuth/API 账号；适配器不读取凭据文件 |
| DeepSeek Harness | ACP，0.1.6-alpha.2 官方 npm 包 | 继承 `DEEPSEEK_API_KEY`；临时隔离配置，不宣称支持订阅登录 |

Claude、DeepSeek 的未知版本、源码构建或不匹配入口会阻止受管启动，仍可从手动终端使用。官方程序需要单独安装；Wonderland 安装包不分发这些厂商程序。

Rust CLI 示例（npm CLI 的任务参数见其 README）：

```powershell
wonderland-cli apps --probe claude
wonderland-cli apps --probe deepseek
wonderland-cli --model claude-sonnet-4-6 --cwd E:/projects/demo work create "检查错误处理" --app claude --chat
```

Rust 与 npm CLI 均支持 `apps --probe <app>`。诊断包括安装路径、版本、身份观察、SHA-256 及其覆盖范围。它不是发行商签名认证，也不证明远端模型身份。

推理档位可在 Teams 的执行器绑定中指定。单个 Work/Chat 受管任务仍使用官方默认档位；CLI 对其显式 `--reasoning` 参数报错，避免忽略用户选择。

## Claude

适配器核验第一方 provider、应用生效的模型和推理档位；每条助手消息与最终用量再次验证模型。支持正文与思考流、工具活动、单次权限确认及官方提问。官方 CLI 会为每个内容块发送快照，适配器逐块去重，避免思考之后的正文丢失。

Chat 只开放 Read、Glob、Grep、AskUserQuestion。Work 增加 Bash、Edit、Write、NotebookEdit 并要求逐次审批。通过官方 safe mode、显式工具集合及受限设置关闭插件、子代理、备用模型和 MCP；无法确认设置时拒绝开始。此限制是工具边界，不是额外的操作系统沙箱。

## DeepSeek

适配器核验整个 DSH 包版本、官方 CLI/profile 指纹及 ACP 身份，创建新会话并锁定 `deepseek-official` 与精确模型，推理选项采用其原生 `off/low/high/max`。临时官方 profile 关闭其它模型路由、子代理、插件、动态设置、MCP 和辅助模型调用。

ACP 推送的是已提交的助手文本块，不能宣称等同原始 token 流。上下文占用单独记录，不当作计费 token。权限只接受当前工具的一次性决定，工具结束撤销旧请求，仍有活动工具时不能标记完成。取消、超时、协议失败均进行有时限的进程树清理。

只读和写入边界依赖官方文件/命令沙箱；上游 Windows 隔离存在限制，无密钥握手不能证明实际工具隔离。DeepSeek 当前不提供受管交互问题、恢复与分叉能力。

## 本版边界

新增适配器已进行离线协议回归与真实官方程序检查，未进行真实 Claude/DeepSeek 付费推理或跨厂商效率对照。现有 Kimi 订阅协作成功记录仍见 0.9.0 验证说明。自动质量/成本选模、预算硬限制、其它厂商受管适配、插件市场、定时任务、远程主机、PR 和网站专用面板仍需继续完成。

详细资料：[Claude 适配](CLAUDE_NATIVE_ADAPTER.md)、[DeepSeek 适配](DEEPSEEK_NATIVE_ADAPTER.md)、[验证记录](VALIDATION-0.10.0.md)。
