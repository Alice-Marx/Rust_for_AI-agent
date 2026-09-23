# 任务报告：kimi-code 与 minimax-code 受管 ACP 接入

日期：2026-09-23
基线：`386d015`（`handoff-development`，已合入 main）
上游原则：anytool 子模块保持只读，全部适配代码在 Wonderland 侧（`src/native_executor/`），官方工具升级只需更新子模块指针并核对方言常量。

## 一、完成的工作

1. **Kimi Code · Node（`kimi-code`）受管接入**：第五个之外的第六个受管执行器。`node <npm>/@moonshot-ai/kimi-code/dist/main.mjs acp` 走通用 ACP 框架；方言按上游 2.0.2 的 `packages/acp-server` 源码核对——身份 `Kimi Code CLI`/2.0.2、模型值为裸目录 id、`thinking` 选项模型相关（off/on 或声明的 effort 级）故**不映射**通用档位、权限 `approve_once`/`reject`（`approve_always` 存在但受管任务永不选）、`plan` 与 `available_commands_update` 作为活动透传、`usage_update` 只有上下文占用（引擎不发成本）。
2. **MiniMax Code（`minimax`）受管接入**：`node <npm>/minimax-code/dist/cli.js acp`（bin `mcode`）。方言按上游 0.5.2 的 `packages/tui/src/acp` 核对——身份 `minimax-code`/0.5.2、模型值为 `m:<provider>:<model>[:v:<variant>]` wire 格式（`parseModelConfigValue` 五段式）、权限 `allow-once`/`deny`、`thinkingEffort` 同样不映射。登录入口 `mcode acp login --region cn|global`（等账号）。
3. **两工具全量注册**：capabilities（协议/权限/问题交互）、validate_binding（kimi-code 要求 `kimi-*`；minimax 要求 `m:` wire 格式）、dispatch 分支、desktop_bridge 目录（managed=true + 身份说明）、account_billing 各加 api+subscription 渠道（现 13 条）、pricing 状态计数同步。
4. **grok-build 评估结论更新**：`xai-acp-lib` 是 gateway 转发层、`xai-grok-pager` 是 ACP 客户端（leader），仓库内未见独立 stdio server 入口——接入前需先确认其 server 暴露方式（见评估报告）。

至此受管执行器达 **7 个**：codex、claude、kimi-cli（Python Wire）、kimi-code（ACP）、mimo（ACP）、minimax（ACP）、deepseek（ACP），其中 4 个走统一 ACP 框架。

## 二、各文件的说明

| 文件 | 说明 |
| --- | --- |
| `src/native_executor/kimi_code.rs`（新，约 470 行） | Kimi Code ACP 方言 + npm 入口探测（`WONDERLAND_KIMI_CODE_CLI` 或 npm root -g）+ Node 启动 + Windows 长路径归一化 + execute_with_control + 3 项测试 |
| `src/native_executor/minimax.rs`（新，约 560 行） | MiniMax ACP 方言（wire 模型 id 校验器）+ `WONDERLAND_MINIMAX_CLI`/npm 探测 + execute_with_control + 3 项测试（含完整会话） |
| `src/native_executor.rs` | 注册两个新模块、dispatch 分支、capabilities 协议映射、validate_binding 模型规则 |
| `src/desktop_bridge.rs` | kimi-code/minimax 从手动候选升为受管（managed=true + 说明）；目录断言更新为 7 个受管适配器 |
| `src/account_billing.rs` | 渠道目录 9→13（两工具各 api+subscription）；订阅渠道断言更新 |
| `src/pricing.rs` | 状态测试的 billing_channels 计数 9→13 |
| `docs/ANYTOOL-ADAPTER-ASSESSMENT-2026-09-23.md` | 三工具状态更新为已接入；grok 结论修正为「需先确认 server 暴露方式」 |

## 三、需要做的测试（含已做与待做）

**已做（本机全过）**：
- 单元：两工具的模型 id 格式校验（含负例）、方言锁定（身份/版本/权限一次性对/停止原因/配置漂移）、与 `AcpRunner` 的**完整会话集成测试**（握手→模型设置→流式/透传更新→权限只读自动拒绝→工具完成清理→end_turn）。
- 回归：全目标（含 ui-snapshots）437 库 + 3 CLI + 18 桌面 + 7 协议；失败仅为已基线复现的 desktop_bridge/desktop_terminal 本机环境问题（PowerShell CLIXML 污染等）。npm 14/14。`cargo fmt --all --check` 通过。

**待做（需要你登录账号后，按 H09 步骤 7-8）**：
1. 真实握手：`npm i -g @moonshot-ai/kimi-code@2.0.2 minimax-code@0.5.2 @mimo-ai/cli@0.1.15` 后各跑一次真实 ACP initialize（验证 agentInfo/version/banner 假设，回填固定指纹常量）。
2. 真实登录（`kimi acp` 数据目录登录 / `mcode acp login`）+ 各一次真实推理、一次取消、一次失败注入。
3. 脱敏协议 fixture 归档进 `tests/`，作为升级时的对照基线。
4. Automatic 无预算路径的跨厂商真实任务实例（用独立示例 Git 项目）。

## 四、还未完成的任务与完成思路

| 任务 | 思路与方式 |
| --- | --- |
| **grok-build 接入** | 先确认 server 暴露：读 `xai-grok-agent` 的启动方式（是否有 headless/serve 模式或可复用的 ACP agent 入口）；若有则写 `GrokDialect`（身份/模型值格式从其 proto 定义核对）+ 二进制定位（Rust 产物，非 npm）；若只有 pager 客户端形态，则记录为「不提供 server，只能终端手动」并更新评估报告 |
| **ZCode 接入** | 走自有 app-server 协议路径（同 codex 适配模式）：读 `apps/zcode-cli/packages/contracts` 的消息 schema，实现独立 transport（不复用 ACP 框架），版本锁定 `@zcode/cli` 3.14.0 |
| **两个 CLI 的路由策略命令** | Rust CLI 加 `team routing save <id> --file policy.json` / `team routing preview <id>` / `team routing replay <id>`（透传 HTTP 三接口）；npm CLI 同步等价命令。改 `src/bin/wonderland-cli.rs` 与 `packaging/npm/wonderland-cli/bin/wonderland.js` + 两边测试 |
| **桌面阻塞原因面板（H06 起步）** | Teams 界面读 `/api/v1/pricing` 的 `dispatch_readiness.missing` 与 `routing_decision` 事件，渲染「为什么阻塞/为什么选它」面板（`src/bin/desktop/teams.rs`），如实呈现后端事实 |
| **H04 预算闭环** | 派工前按 `estimated_cost_usd` 上浮安全系数走 `team_store` 既有微美元账本预留，终态/取消结算释放，重启扫未结算预留对账；适配器把 usage 事件映射到 attempt 账目（mimo 的 `reported_cost_usd` 标注为非确认值）。只有闭环后才解除 `budget_usd` 阻塞 |
| **H05 恢复/分叉、H07 插件定时远程 PR、H08 沙箱 MCP、H10 研究实验** | 按 [ROADMAP-2026-09-23](ROADMAP-2026-09-23.md) 既有分解执行 |
