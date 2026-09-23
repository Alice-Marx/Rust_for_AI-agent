# 任务报告：Grok Build 受管 ACP 接入与小改进批

日期：2026-09-23。实施依据为 [改进检查与后续规划](REVIEW-AND-PLAN-2026-09-23.md) 的 G1/G3；其中的技术结论用于限定实现，用户请求是继续完成项目工作。

## 一、完成的工作

1. **通用 ACP 认证生命周期**：`AcpDialect` 增加默认空实现的 `authenticate` 钩子，`AcpRunner` 在身份握手通过后、创建 session 前执行认证。认证错误只保留 JSON-RPC code，服务端任意消息不进入错误文本。初始化请求携带非交互 startup hint。
2. **Grok Build 成为第 8 个受管执行器**：新增 `grok-acp` 方言和 `grok agent stdio` 进程适配，固定官方版本 1.0.38，支持 `WONDERLAND_GROK_BIN`，记录本地二进制 SHA-256，并把版本 banner、源码归档 SHA 和 `SOURCE_REV` 写入身份说明。
3. **身份与认证失败关闭**：要求 `_meta.grokShell=true`、`_meta.agentVersion=1.0.38` 和 session close 能力；`XAI_API_KEY` 存在时只选首项 `xai.api_key`，否则只选已广告的 `cached_token`。认证参数仅含 `_meta.headless=true`，不触发强制交互、重新登录或 OAuth 选择。API key 认证成功可证明 `billing_channel=api`；缓存令牌的计费渠道继续保持 unknown。
4. **模型、档位和 provider 回读**：允许新 session 先处于官方默认模型，再依次设置裸 `grok-*` 模型 ID 与 `reasoning_effort`；每次以 `configOptions.currentValue` 回读。提示终态还要求 `_meta.modelId` 与请求相同，模型前缀可证明时记录 provider `xai`。
5. **工具、权限、扩展和取消**：兼容 Grok 的 `pending → in_progress → completed/failed` 工具状态；中间更新不提前清除工具。权限只接受恰好一个 `allow_once` 和一个 `reject_once`，只读任务自动拒绝。直接及包装的 `x.ai/session_notification` 均作为工具活动透传。取消仍按通用 ACP 的 `session/cancel → session/close → 进程树回收` 收口。
6. **usage 映射**：读取提示结果 `_meta.usage`，校验 input/output/total 一致性并保存 cache、reasoning、model call 数据。只有完整且非 partial 的 `costUsdTicks` 才换算为工具自报 USD；无金额时不估算，金额不升级为提供商发票确认值。
7. **应用注册与诊断**：应用目录、默认 CLI profile、绑定校验和 native capabilities 已登记 Grok；reasoning 档位为 `none/minimal/low/medium/high/xhigh/max`，`resume/fork=false`。安装诊断识别带 commit/channel 后缀的 `grok 1.0.38` banner。
8. **G3 小改进**：npm README 补齐 `work duplicate` 和 `team routing preview/saved/replay` 的语法与严格 JSON 示例；仓库外新增 `upstream-study/PROVENANCE.md`；版本探测成功时优先解析 stdout，避免 PowerShell CLIXML stderr 污染；Kimi 同名识别改为不依赖 PATH/PowerShell 的纯函数测试；真实 PTY 时序测试明确列为外部环境测试；清除了 native executor 现有编译警告。

## 二、各文件说明

| 文件 | 本次职责 |
| --- | --- |
| `src/native_executor/acp.rs` | 可选 authenticate、初始化结构校验分层、厂商扩展通知、方言化工具状态、提示结果/usage 钩子 |
| `src/native_executor/grok.rs` | Grok 安装、版本、认证、模型/档位、权限、扩展、usage、取消和离线协议 fixture |
| `src/native_executor.rs` | 注册 `grok-acp`、模型前缀、档位、执行和请求校验分派 |
| `src/desktop_bridge.rs` | 应用目录与默认 `grok` profile；版本探测 stdout/stderr 分离；环境敏感测试改进 |
| `src/app_diagnostics.rs` | 识别 Grok 官方版本 banner 并补离线断言 |
| `src/native_executor/deepseek.rs` | 删除已无调用者的测试 writer/read helper 与未使用导入 |
| `src/native_executor/mimo.rs` | 删除不需要的 mutable 参数 |
| `src/desktop_terminal.rs` | 将真实 PTY/ConPTY 测试标为显式外部集成测试 |
| `packaging/npm/wonderland-cli/README.md` | 同步新 workflow/routing 命令、RoutingPolicy JSON 合同和当前 8 个受管执行器 |
| `docs/ANYTOOL-ADAPTER-ASSESSMENT-2026-09-23.md` | Grok 从“待确认入口”更新为“离线适配完成、待真实握手”并记录协议依据 |
| `docs/PROJECT-TASK-REPORT-2026-09-23.md` | 总项目进度、文件、测试、未完成任务与后续方法同步 |
| `docs/DOCUMENT-GUIDE-2026-09-23.md` / `docs/README.md` | 登记本报告 |
| `F:/everyAI/all/upstream-study/PROVENANCE.md` | 仓库外 Grok/ZCode 固定归档来源、SHA、URL、取得时间与验证边界 |

## 三、测试结果与仍需做的测试

已执行：

| 命令 | 结果 |
| --- | --- |
| `cargo test --locked native_executor::grok --lib` | 6/6 通过：完整认证/建会话/配置/提示/流/权限/usage 生命周期及负路径 |
| `cargo test --locked native_executor::acp --lib` | 11/11 通过：既有方言未因认证钩子回归 |
| `cargo test --locked desktop_bridge::tests --lib` | 19 通过，1 个安装型测试按设计忽略；此前 Kimi/CLIXML/超时波动在本机通过 |
| `cargo test --locked app_diagnostics::tests --lib` | 8 通过，1 个安装型测试按设计忽略 |
| `cargo test --locked native_executor::tests --lib` | 16/16 通过 |
| `cargo test --locked --all-targets --features ui-snapshots` | 库 454 通过、8 个外部条件测试忽略；Rust CLI 4、桌面 18、协议集成 7 通过，0 失败 |
| `npm test --prefix packaging/npm/wonderland-cli` | 16/16 通过 |
| `cargo fmt --all -- --check` / `cargo check --locked --all-targets --features ui-snapshots` | 通过；无编译警告 |

最终回归已完成。PowerShell CLIXML、真实 PATH 上 Kimi 同名程序和 PTY 时序不再把环境差异误报成产品回归；真实 PTY 用例仍保留为显式外部集成测试。

真实环境测试仍未执行：安装官方 Grok 1.0.38 后核对 banner 与固定二进制摘要；分别用 `XAI_API_KEY` 和缓存令牌完成握手；真实推理、权限允许/拒绝、取消、认证失败和 usage 脱敏归档各一次。完成这些步骤前不宣称真实 Grok 模型执行已验证。

## 四、还未完成的任务与完成思路

| 任务 | 当前边界 | 完成思路 |
| --- | --- | --- |
| Grok H01/H09 真实闭环 | 代码和离线 fixture 完成；无本机官方安装、账号握手或真实推理，固定发布指纹为空 | 用户登录后按上节矩阵执行；把脱敏帧固化到测试，回填官方二进制摘要；若真实 wire 与固定合同不符则继续失败关闭并升级显式版本 profile |
| Grok 恢复/分叉 | 上游广告 resume/list/load，但 Wonderland `resume/fork=false` | 按 H05 保存 session ID、工具版本/摘要、模型/档位和 workspace revision；会话存在、版本兼容、目录一致、审批可重建四项同时通过后再开放 |
| Automatic 身份种子 | Grok 未加入 LiveBench attestation，因此不能成为 Automatic 候选 | 找到官方精确模型名与同一榜单 byte-identical 行，按 identity v1.1 双证据合同加入；无证据继续 unknown，显式指定 Grok 仍可用 |
| ZCode G2 | 固定源码归档已取得，npm 3.14.0 与源码 0.16.9 对应关系未证实 | 先下载 npm tarball、记录 integrity/文件清单并对照固定提交；证实后再按自有 stdio app-server 写独立 transport，不套 ACP 假设 |
| H04/H05/H07/H08/H10 | 与本次 Grok 适配相互独立，仍按总路线图推进 | 先完成账号闭环和硬预算预留/结算，再做恢复分叉；插件/远程/PR/网站、沙箱/MCP 和实验分别按既有验收门槛实施 |
