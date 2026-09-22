# anytool 上游工具受管接入评估

日期：2026-09-23。方法：按 H09 标准第 1–2 步（来源确认 + 本地只读研究），把候选工具的上游源码浅克隆到仓库外，逐个核对**结构化协议入口**。参考 [CodexHost 技术参考](CODEX_HOST_REFERENCE.md) 的做法：不同版本用不同协议 profile 严格解码，不把工具统一降级为无认证聊天 API。

## 核心结论：ACP 是行业收敛点

| 工具 | 入口 | 协议 | 受管接入结论 |
| --- | --- | --- | --- |
| **MiMo Code**（Xiaomi，`@mimo-ai/cli` 0.1.15，bin `mimo`） | `mimo acp` | **ACP** stdin/stdout NDJSON（`@agentclientprotocol/sdk`） | **本轮已接入**（`src/native_executor/mimo.rs`） |
| **grok-build**（xAI，Rust workspace） | 含 `xai-acp-lib` crate | **ACP** | 同框架可接：新增 `GrokDialect` + 二进制定位；协议细节需读 `xai-acp-lib` 定稿 |
| **kimi-code**（Moonshot，`@moonshot-ai/kimi-code` 2.0.2，bin `kimi`） | `kimi acp` 子命令（`src/cli/sub/acp.ts`） | **ACP** | 同框架可接；注意与 Python `kimi-cli` 的 `kimi` 命令同名冲突（已有身份区分记录） |
| **minimax-code**（`minimax-code` 0.5.2，bin `mcode`） | `packages/tui/src/acp/agent.ts` 用 `@agentclientprotocol/sdk` | **ACP** | 同框架可接：模型/登录门控（`TuiLoginRequiredError`）等 profile 细节待读 |
| **ZCode**（zai-org，`@zcode/cli` 3.14.0） | `zcode app-server` / `agent-server` | 自有 app-server JSON（与 Codex app-server 同类） | 可接但走**自有协议**路径（类似 codex 适配），工作量大于 ACP 方言 |
| opencode（opencode-ai，Go 重写版） | 仅 TUI | 无结构化入口 | **暂不接**：当前 Go 主干没有 serve/RPC 模式（TS 时代有 `opencode serve`；MiMo-Code 正是 TS 版 fork 并自带 ACP） |
| Kiro / KiroCrew / Hermes / pi / anomalyco | 未在本次深扫范围 | 待评估 | 按同方法逐个补评 |

**架构含义**：Wonderland 已把 DeepSeek Harness transport 泛化为通用 ACP 框架（`src/native_executor/acp.rs`：帧循环、initialize/session/prompt 生命周期、流式更新、一次性权限、取消；方言差异由 `AcpDialect` trait 承载）。新增一个 ACP 工具 = 写一个 dialect（身份校验、模型值格式、权限选项、停止原因、透传更新）+ 安装探测 + 注册，不再各自复制会话引擎。

## MiMo 接入的已核对细节（写进 `MimoDialect`）

- `initialize` 上报 `agentInfo.name = "OpenCode"`（上游 fork 遗留身份）与 `version = 0.1.15`——握手按官方二进制实际上报锁定；
- 模型配置 `configId = "model"`，值为 `providerID/modelID` 字符串（变体段如 `/high` 保留在 modelID 内由官方解析）；**无 reasoning-effort 选项**（`mode` 不是 effort，泛型 effort 值被拒绝）；
- 权限三选项 `once/always/reject`——受管任务只选**一次性** allow/reject，永不 `allow_always`；
- `sessionUpdate` 集合：`agent_message_chunk`、`agent_thought_chunk`、`tool_call`、`tool_call_update`、`usage_update`、`available_commands_update`（后者作为工具活动透传，不进输出）；
- `usage_update` 额外携带 `cost.amount`（USD）——记录为 `reported_cost_usd`，明确标注「harness 报告值，非提供商确认账单」；
- 分发：npm launcher 包 `@mimo-ai/cli`（stub）+ 平台二进制包 `@mimo-ai/mimocode-<platform>-<arch>`（Windows `mimo.exe`）；`MIMOCODE_BIN_PATH`/`WONDERLAND_MIMO_BIN` 可直指二进制；
- 版本双闸：npm 包 `package.json` 版本 == 0.1.15 且 `--version` banner 含 0.1.15；二进制摘要记录于 Identity 事件（**固定指纹常量待真实安装后回填**——诚实边界）。

## H09 步骤 7–8 的待办（等用户账号）

真实 `mimo acp` 握手、登录、真实推理、取消/失败注入与脱敏 fixture 归档尚未执行（用户将自行登录验证）；在此之前 `mimo` 的受管能力以本文件与代码测试为准，不宣称真实模型执行。
