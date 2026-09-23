# anytool 上游工具受管接入评估

日期：2026-09-23。方法：按 H09 标准第 1–2 步（来源确认 + 本地只读研究），把候选工具的上游源码浅克隆到仓库外，逐个核对**结构化协议入口**。参考 [CodexHost 技术参考](CODEX_HOST_REFERENCE.md) 的做法：不同版本用不同协议 profile 严格解码，不把工具统一降级为无认证聊天 API。

## 核心结论：ACP 是行业收敛点

| 工具 | 入口 | 协议 | 受管接入结论 |
| --- | --- | --- | --- |
| **MiMo Code**（Xiaomi，`@mimo-ai/cli` 0.1.15，bin `mimo`） | `mimo acp` | **ACP** stdin/stdout NDJSON（`@agentclientprotocol/sdk`） | **本轮已接入**（`src/native_executor/mimo.rs`） |
| **Grok Build**（xAI，`grok` 1.0.38，Rust workspace） | `grok agent stdio` | **ACP** stdin/stdout JSON-RPC；另有 leader/serve/headless 模式 | **离线适配已完成**（`src/native_executor/grok.rs`）：验证 `grokShell`/`agentVersion`、只选广告的 `xai.api_key` 或 `cached_token` 并用 headless 认证、固定裸模型 id 与 `reasoning_effort`、限制一次性权限、透传 x.ai 扩展、核对提示级 usage。真实安装握手与推理待 H01/H09 验证 |
| **kimi-code**（Moonshot，`@moonshot-ai/kimi-code` 2.0.2，bin `kimi`） | `kimi acp`（`dist/main.mjs acp`，`@moonshot-ai/acp-server` 驱动） | **ACP** | **已接入**（`src/native_executor/kimi_code.rs`）：身份 "Kimi Code CLI" 2.0.2、裸模型 id、`thinking` 选项模型相关故不映射、权限 `approve_once`/`reject`、`plan` 与 `available_commands_update` 透传、usage 无 cost。只认 npm `dist/main.mjs` 入口，绝不解析 PATH 裸 `kimi`（与 Python kimi-cli 区分） |
| **minimax-code**（`minimax-code` 0.5.2，bin `mcode`） | `mcode acp`（`node dist/cli.js acp`） | **ACP** | **已接入**（`src/native_executor/minimax.rs`）：身份 "minimax-code" 0.5.2、模型值 `m:<provider>:<model>[:v:<variant>]` wire 格式、权限 `allow-once`/`deny`、`thinkingEffort` 模型相关故不映射。登录走 `mcode acp login --region cn\|global`（真实登录待账号验证） |
| **ZCode**（zai-org，`@zcode/cli` 3.14.0） | `zcode app-server` / `agent-server` | 自有 app-server JSON（与 Codex app-server 同类） | 可接但走**自有协议**路径（类似 codex 适配），工作量大于 ACP 方言 |
| opencode（opencode-ai，Go 重写版） | 仅 TUI | 无结构化入口 | **暂不接**：当前 Go 主干没有 serve/RPC 模式（TS 时代有 `opencode serve`；MiMo-Code 正是 TS 版 fork 并自带 ACP） |
| Kiro / KiroCrew / Hermes / pi / anomalyco | 未在本次深扫范围 | 待评估 | 按同方法逐个补评 |

**架构含义**：Wonderland 已把 DeepSeek Harness transport 泛化为通用 ACP 框架（`src/native_executor/acp.rs`：帧循环、可选 authenticate、initialize/session/prompt 生命周期、流式更新、一次性权限、取消；方言差异由 `AcpDialect` trait 承载）。新增一个 ACP 工具 = 写一个 dialect（身份校验、可选认证、模型值格式、权限选项、停止原因、透传更新）+ 安装探测 + 注册，不再各自复制会话引擎。

## Grok Build 接入的已核对细节（写进 `GrokDialect`）

- 固定研究归档为 GitHub 提交 `4247f661689354b831191f11eeeac8424993fe3d`；归档内 `SOURCE_REV=9bb727ccdff0a793ee73bcde4e2e09cbef6b5387` 是 monorepo 同步点，两个 SHA 属于不同命名空间，已在仓库外 `upstream-study/PROVENANCE.md` 对账；
- 官方发布版本 1.0.38，`grok --version` 输出以 `grok 1.0.38` 开头；受管服务入口是 `grok agent stdio`，`WONDERLAND_GROK_BIN` 可显式指定绝对二进制路径；
- `initialize` 必须返回 `protocolVersion=1`、`_meta.grokShell=true`、`_meta.agentVersion=1.0.38` 与 session close 能力；缺失、版本漂移或认证方法重复均失败关闭；
- `XAI_API_KEY` 存在时只选择且要求首项为 `xai.api_key`；否则只允许实际广告的 `cached_token`。请求仅携带 `_meta.headless=true`，不设置 `force_interactive`、`reauth` 或 `use_oauth`；
- 新建会话可从其他默认模型开始，随后必须经 `session/set_config_option` 把 `model` 和可选 `reasoning_effort` 回读为请求值；提示结果 `_meta.modelId` 再确认实际模型；
- tool 起始状态兼容上游 `pending`/`in_progress`，中间更新不会提前结算；权限只取恰好一个 `allow_once` 与一个 `reject_once`，只读任务仍自动拒绝；
- `x.ai/session_notification` 及包装形式 `_x.ai/session_notification` 只作为 `ToolActivity`，不混入正文。提示 `_meta.usage` 按 input/output/total/cache/reasoning 字段校验；只有完整且非 partial 的 `costUsdTicks` 才换算为工具自报 USD，缺失时不估算；
- `resume/fork` 继续为 false；Grok 没有当前 LiveBench 身份 attestation，未加入 Automatic 身份种子，用户显式指定执行不受此限制。

## MiMo 接入的已核对细节（写进 `MimoDialect`）

- `initialize` 上报 `agentInfo.name = "OpenCode"`（上游 fork 遗留身份）与 `version = 0.1.15`——握手按官方二进制实际上报锁定；
- 模型配置 `configId = "model"`，值为 `providerID/modelID` 字符串（变体段如 `/high` 保留在 modelID 内由官方解析）；**无 reasoning-effort 选项**（`mode` 不是 effort，泛型 effort 值被拒绝）；
- 权限三选项 `once/always/reject`——受管任务只选**一次性** allow/reject，永不 `allow_always`；
- `sessionUpdate` 集合：`agent_message_chunk`、`agent_thought_chunk`、`tool_call`、`tool_call_update`、`usage_update`、`available_commands_update`（后者作为工具活动透传，不进输出）；
- `usage_update` 额外携带 `cost.amount`（USD）——记录为 `reported_cost_usd`，明确标注「harness 报告值，非提供商确认账单」；
- 分发：npm launcher 包 `@mimo-ai/cli`（stub）+ 平台二进制包 `@mimo-ai/mimocode-<platform>-<arch>`（Windows `mimo.exe`）；`MIMOCODE_BIN_PATH`/`WONDERLAND_MIMO_BIN` 可直指二进制；
- 版本双闸：npm 包 `package.json` 版本 == 0.1.15 且 `--version` banner 含 0.1.15；二进制摘要记录于 Identity 事件（**固定指纹常量待真实安装后回填**——诚实边界）。

## H09 步骤 7–8 的待办（等用户账号）

真实 `mimo acp` 与 `grok agent stdio` 握手、登录、真实推理、取消/失败注入与脱敏 fixture 归档尚未执行（用户将自行登录验证）；在此之前两者的受管能力以本文件与离线协议测试为准，不宣称真实模型执行。Grok 的固定发布二进制指纹也须在真实安装后回填。
