# 总项目任务报告

日期：2026-09-23（含适配器身份回读与 H04 usage 归档增量）
仓库：[Alice-Marx/Rust_for_AI-agent](https://github.com/Alice-Marx/Rust_for_AI-agent)，当前分支 `handoff-development`；身份回读提交 `09a1014` 后继续；CLI/桌面交付已提交为 `03a8ff9`
版本基线：0.11.0（`24dbf54`）+ 本阶段 13 个功能提交；本轮 CLI、桌面面板、Codex 身份回读与报告交付见「一·本轮交付」
配套文档：[文档指南](DOCUMENT-GUIDE-2026-09-23.md)（每份文档的用途）、[后续路线图](ROADMAP-2026-09-23.md)（H01–H10 分解）

## 一、完成的工作（总项目视角）

**产品**：Wonderland——Rust 桌面多模型工作台。复杂任务由系统拆解，按质量/价格/账号条件分配给不同模型，**每个模型通过自己的官方工具执行**；用户也可指定官方工具完成整个任务。三条执行路径语义严格区分：自有 API 对话、受管 Work/Chat/Teams（官方 CLI 进程）、手动终端。

### 0.11.0 基线已交付（历史，详见 [RELEASE-0.11.0](RELEASE-0.11.0.md)）

API Agent 与 SSE、Rust egui 桌面、Rust/npm 双 CLI、官方工具 Work/Chat、Teams DAG 与独立 Git 工作区、持久事件与权限交互、验收检查、LiveBench/价格来源快照、SQLite v1→v2 迁移、Windows 打包、npm 发布（`next=0.11.0`）。Kimi 订阅账号完成过真实 Teams 协作实测（12 项主机验收）。

### 本阶段新增（`24dbf54`..`d8c3461`，13 个提交）

**H02 数据合同——全部四项完成：**
1. **模型身份注册表**（`model_identity.rs` + 内嵌种子 v1.1）：精确 (app_id, model, reasoning_effort) → LiveBench 条目的 attestation 或显式 unknown；种子 6 条记录（3 attested：kimi-k2.7-code、kimi-k3、deepseek-v4-pro，均带官方定价文档 + 榜单 byte-identical 行双证据）。
2. **五个官方价格源全部 verified**（`pricing.rs`）：OpenAI 37 + Kimi 4 + Anthropic 14（同 epoch 双源 join 到 attestation 的精确 API model ID/alias、缓存乘数不变量、退役/受限语义）+ DeepSeek 4（峰谷两档逐字 UTC 窗口、退役别名按官方脚注计价）。真实在线刷新端到端验证过（59 条报价、快照持久化重开复验）。
3. **账号计费语义合同**（`account_billing.rs`）：13 条渠道合同；api=USD/token 列价（列价≠实付）、订阅=提供商额度单位（明文禁止订阅费÷token）；实付/额度/限速/重置周期四类账号字段显式 unknown；`dispatch_blockers` 结构化暴露。
4. **epoch 不可变绑定**：routing-v2 决策携带 `benchmark_snapshot_id:price_snapshot_id` + `identity_mapping_version` 完整证据链。

**H03 决策与派工——代码侧完成：**
- routing-v2：榜单行必须经身份注册表 attestation（未映射/不在榜/条目过期/声明不一致全部拒绝）。
- **Automatic 团队启动路径打通**：在线刷新→身份→决策（事件持久化）→决策 selected 且无硬预算→`set_planner` 写回绑定→进入执行路径（与固定执行同等成本地位）；blocked/刷新失败/硬预算保持诚实阻塞并带候选级原因。
- 预览/保存策略/重放三个 HTTP 接口。

**anytool 官方工具接入（参考 codex-host 的 profile 化方法）——受管执行器 4→7：**
- **通用 ACP 会话引擎**（`native_executor/acp.rs`）：从 DeepSeek transport 泛化（帧循环/生命周期/权限/取消），方言差异收敛为 `AcpDialect` trait；**上游子模块零修改**，适配全在 Wonderland 侧。
- 新接入 **mimo**（@mimo-ai/cli 0.1.15）、**kimi-code**（@moonshot-ai/kimi-code 2.0.2）、**minimax-code**（0.5.2）——方言细节全部按上游源码逐项核对并 fail-closed。
- 协议评估结论：ACP 是 anytool 工具收敛点；ZCode 走自有 app-server 协议；opencode Go 版无结构化入口；Grok Build 已确认有 stdio ACP 入口，但尚未接入 Wonderland。

### 当前能力边界（诚实声明）

`auto_dispatch_ready=false` 仍是正确状态：Codex 可回读 API key/ChatGPT 账号渠道与计划档位；Claude、ACP 与 Kimi CLI 已增加工具/模型配置身份记录，但各自账号渠道与真实服务端落点尚未全部实测。账号实付、额度、限速、重置周期仍未核验。无预算 Automatic 派工代码路径已就绪，等账号登录后即可真实验证。硬预算（`budget_usd`）阻塞保持到 H04 闭环。

### 本轮交付（12 个程序/测试文件 + 配套报告）

1. **两个 CLI 的路由策略命令完成**（原「未完成任务」表第 3 行）：Rust CLI 与 npm CLI 均新增 `team routing preview <id> --file <policy.json>`（显式约束在线预览，typed JSON 校验）、`team routing saved <id>`（用团队保存的 routing_policy 预览）、`team routing replay <id>`（重放最新持久化决策，不刷新网络）。三个命令严格透传既有 HTTP 合同，ID 单段 URL 编码，非法参数不发请求。
2. **桌面 Teams 阻塞原因面板完成**（H06 起步项）：Teams 详情页新增「路由决策」区，渲染 `routing_decision`/`binding_applied`/`routing_preview` 事件——决策状态、解释、逐候选状态与拒绝原因、质量分/估算成本、epoch 与身份映射版本证据链；价格面板新增 `dispatch_readiness` 就绪状态与 `missing` 阻塞清单、`automatic_dispatch` 两档状态文案（改为读取后端事实，移除已过时的「Automatic 尚未开放自动价格调度」硬编码文案）。
3. **Grok Build 上游协议初步核对**：源码快照确认 `grok agent stdio` 进入 ACP stdio 服务，另有 `grok agent serve`（HTTP+secret）与 headless 模式。更深核对发现 Wonderland 当前通用 ACP runner 不能直接复用：Grok 要求 `initialize` 后选择 `xai.api_key` 或 `cached_token` 并调用 `authenticate`；身份在 `meta.grokShell` / `meta.agentVersion`，且启动模型通过 `_meta.modelId` 或 ACP 模型配置处理。此前报告记为 `4247f66`，但现存归档的 `SOURCE_REV` 是 `9bb727ccdff0a793ee73bcde4e2e09cbef6b5387`，目录无 `.git`，提交来源尚未独立验证；故只记录为上游协议线索，不能称为已接入或已锁定版本。
4. **ZCode 源码取回待深读**：按固定提交 `872ad96` 取回（`upstream-study/ZCode`）。monorepo 结构确认（`apps/zcode-cli`、`packages/server|rpc|zcode-server-cli`），接入仍按自有 app-server 协议（codex 模式独立 transport）；注意源码仓 `apps/zcode-cli` 版本显示 0.16.9 且标 private，实现前需先核对 npm 发布物 `@zcode/cli` 3.14.0 与该提交的对应关系。
5. **H01 Codex 身份回读第一步**：新增脱敏 `NativeIdentity`，Codex 执行前通过官方 app-server `config/read`、`account/read` 和 `thread/start` 保存配置模型/档位、`api` 或 `subscription` 渠道、ChatGPT 计划档位，以及线程回读的生效模型/提供商/默认档位。账户邮箱不进入身份事件；账户未登录或账号类型不受支持时，在发送提示前失败关闭。
6. **H01 其他原生工具身份回读扩展**：Claude 保存固定 CLI 版本/二进制指纹、已应用模型/档位、first-party provider 边界及成功结果里的模型用量；ACP 通用会话引擎保存工具版本/指纹、会话配置模型、已选择模型，以及模型 ID 明确携带的 provider（DeepSeek、MiMo、MiniMax）；DeepSeek 在协议返回值中另回读 reasoning effort。Kimi Code 的模型 ID 不携带 provider，故 provider 保持 unknown；Python Kimi 保存受限后的本地精确模型配置，但协议不回显实际生效模型。没有由协议确认的模型、档位或计费渠道不作推断，`billing_channel` 与 `account_plan` 在这些适配器中继续为空；真实账户闭环尚未完成。
7. **H04 usage 归档（部分完成）**：规划 child、执行/评审 child 与每次重试结束时，把原生工具 `usage` 事件按 `workflow_id` 归入 `usage_observed` Teams 事件，并标记 phase、node、attempt、app、model、effort。最多保留最近 16 个快照；Claude CLI 与 MiMo harness 报告的 USD 值标成未确认来源，不做快照求和；估算与已确认金额都明确为空。**美元预算仍在派工前阻塞**：usage 事件不等于发票，也不能限制在途消费；预留账本尚未接入派工/结算。
8. **H05 终态任务复制首步**：独立终态 workflow 可通过 `POST /api/v1/workflows/{id}/duplicate` 或 Rust/npm CLI `work duplicate <id>` 复制为新 Draft，并在事务内保存 `duplicated_from` 来源事件。保留任务请求配置，但不复制旧原生 session、输出或错误，不自动启动；活动任务和 Teams-owned child 被拒绝。**H05 原生恢复/分叉尚未完成**，所有适配器 `resume/fork` 仍为 false。

### 本轮报告对账

修正了待办表中已实现的 CLI 命令与桌面面板状态；把 Grok Build 更新为「ACP 入口已确认、Wonderland 适配未开始」，并记录本地源码快照无法证明先前记录提交号的问题。CLI/桌面改动已提交；Codex 与其他原生工具身份字段均通过脱敏事件持久化。新增适配器字段已包含在本轮 Rust 全量回归中。桌面真实窗口检查和依赖登录的 H01 实测仍待完成。

## 二、各文件的说明

主程序 `src/` 共 53 个模块 + 5 个执行器子模块 + 4 个 bin（约 3.7 万行）；逐文件历史职责见 [FILE_CATALOG-2026-09-21](FILE_CATALOG-2026-09-21.md)（按 0.11.0 基线，新增模块以本报告为准）。

**受管执行器（官方工具适配层，全部不修改上游）：**

| 文件 | 说明 |
| --- | --- |
| `native_executor.rs` | 执行器门面：capabilities、绑定校验、共享帧读取/事件/进程树工具；`NativeIdentity`、Codex 配置/账户/线程回读、Python Kimi 受限配置模型与工具指纹 |
| `native_executor/acp.rs` | **通用 ACP 会话引擎**：AcpRunner + AcpDialect trait；JSON-RPC 生命周期、工具状态机、一次性权限、取消；回读工具版本/指纹、已配置/已选择模型、方言明确提供的 provider 与 reasoning effort |
| `native_executor/deepseek.rs` | DeepSeek Harness 方言（隔离 profile、版本/哈希双闸、JSON 数组模型值） |
| `native_executor/claude.rs` | Claude Code 双向 stream-json（严格 2.1.193）；保存应用模型/档位、first-party provider 边界与成功结果的 modelUsage 模型 |
| `native_executor/kimi_code.rs` | Kimi Code·Node 方言（本轮新增）：只认 npm `dist/main.mjs`，与 Python kimi 身份隔离 |
| `native_executor/mimo.rs` | MiMo Code 方言（本轮新增）：平台二进制定位（launcher 包 + 平台包） |
| `native_executor/minimax.rs` | MiniMax Code 方言（本轮新增）：`m:p:m[:v:v]` wire 模型值 |

**本轮程序与测试文件：**

| 文件 | 说明 |
| --- | --- |
| `src/bin/wonderland-cli.rs` | Rust CLI 的 `team routing preview/saved/replay` 与 `work duplicate` 命令；参数/JSON 校验和路由接口映射测试 |
| `packaging/npm/wonderland-cli/bin/wonderland.js` | npm CLI 的路由策略命令与 `work duplicate` HTTP 透传；限制策略文件大小、校验 JSON 对象并透传既有合同 |
| `packaging/npm/wonderland-cli/test/workflows.test.js` | workflow CLI 测试；验证 `work duplicate` 路径编码、POST 空对象和不隐式 start |
| `packaging/npm/wonderland-cli/test/teams-pricing.test.js` | npm CLI 路由策略请求、编码和非法参数不发请求的测试 |
| `src/bin/desktop/teams.rs` | Teams 详情中的路由决策事件、候选拒绝原因、证据链和派工就绪状态面板 |
| `src/workbench_service.rs` | 工作流完成后持久化脱敏 `identity_readback`；为终态独立任务提供 duplicate API，拒绝 Teams child |
| `src/workflow.rs` | SQLite 工作流状态机和事件账本；把终态任务原子复制为全新草稿，不复用 session/output |
| `tests/claude_native/protocol.rs` | Claude 离线协议夹具；核对成功结果的模型回读和身份字段脱敏边界 |
| `docs/PROJECT-TASK-REPORT-2026-09-23.md` | 本总报告；同步当前实现、验证边界与剩余工作 |

**证据与路由层（H02/H03 数据合同）：**

| 文件 | 说明 |
| --- | --- |
| `model_identity.rs` | 身份注册表：内嵌种子加载/校验、精确元组解析、显式 unknown |
| `assets/model-identity/v1.json` | 种子 v1.1：每条记录附双证据 |
| `model_intelligence.rs` | LiveBench 快照（固定 SHA 来源、防篡改、失败保留历史） |
| `pricing.rs` | 五源价格（有界解析、乘数不变量、epoch 绑定、缓存 fail-closed） |
| `account_billing.rs` | 计费渠道合同 + dispatch blockers |
| `routing.rs` | routing-v2 决策内核（身份消费、质量/成本/预算、确定性排序、证据链） |

**团队与任务（H03 执行）：**

| 文件 | 说明 |
| --- | --- |
| `team_service.rs` | Teams HTTP 合同 + Automatic 启动路径（决策→`set_planner` 写回→执行）；将 Planner、节点执行/评审/重试 child 的工具 usage 按 workflow 归档为带尝试归属、未确认金额分类的事件 |
| `team_store.rs` | 团队/节点/attempt 权威存储 + 微美元账本 + `set_planner` |
| `team_workspace.rs` / `team_checks.rs` | 独立 Git 工作区与主机验收 |
| `workflow.rs` / `workbench_service.rs` | 单任务 SQLite 状态机（v2）、执行控制与原生身份回读持久化 |

**其余模块**：API Agent（`agent/api/provider/anthropic/responses/router/tools/`）、会话（`session/session_index/memory/`）、安全（`permissions/sandbox*/mcp*/process_tree/`）、桌面与终端（`desktop_*/bin/desktop/`）、诊断（`app_diagnostics/subscription/cliproxy/`）、npm CLI（`packaging/npm/wonderland-cli/`）、Windows 打包（`packaging/windows/`）、上游参考（`anytool/` 16 子模块，只读）。

## 三、需要做的测试

**本轮自动化验证（Rust stable 1.98.1）：**

| 命令 | 结果 |
| --- | --- |
| `rustup run stable cargo fmt --check` | 通过 |
| `rustup run stable cargo test --bin wonderland-cli team_routing_routes_match_http_contract_and_validate_policy_file` | 1 项通过 |
| `rustup run stable cargo test --bin wonderland-desktop` | 18 项通过 |
| `rustup run stable cargo test --lib codex_identity` | 1 项通过 |
| `rustup run stable cargo test --lib identity_readback_is_persisted_as_a_redacted_event` | 1 项通过 |
| `rustup run stable cargo test --lib team_service::tests::reported_usage_costs_are_not_marked_as_confirmed` | 1 项通过 |
| `rustup run stable cargo test --lib team_service::tests::child_usage_is_attributed_to_a_team_phase_and_attempt` | 1 项通过 |
| `rustup run stable cargo test --lib workflow::tests::terminal_task_duplicates_as_a_fresh_draft_without_reusing_session_or_output` | 1 项通过 |
| `rustup run stable cargo test --lib workflow::tests::active_task_cannot_be_duplicated_as_a_retry` | 1 项通过 |
| `rustup run stable cargo test --lib workbench_service::tests::duplicate_rejects_team_owned_children_and_creates_standalone_draft` | 1 项通过 |
| `rustup run stable cargo run --quiet --bin wonderland-cli -- work duplicate --help` | 通过；已检查 CLI 命令帮助，服务端到端请求仍待做 |
| `rustup run stable cargo test` | H05 增量全量回归：库 449 项通过、7 项按外部条件忽略；Rust CLI 4 项、桌面 18 项、协议集成 7 项通过；文档测试 0 项。覆盖 ACP/Claude 身份断言、usage 归属/金额分类及终态副本语义 |
| `npm test`（`packaging/npm/wonderland-cli`） | 16 项通过（新增 duplicate 透传测试） |

最终 Rust 全量回归通过；H04 首轮全量执行时 `desktop_bridge::tests::version_probe_times_out_hung_program` 在 3 秒限时下波动失败，单项复跑及随后 H04/H05 全量回归通过，属于并行负载敏感的既有时序测试。仍有执行器模块中既有未使用导入/变量及死代码警告。此前历史基线为库 437 + Rust CLI 3 + 桌面 18 + 协议集成 7 + npm 14 项通过；desktop_bridge×3/desktop_terminal×1 在未改动基线即可复现（本机 PowerShell CLIXML 污染与真实 kimi CLI 行为），不属于本轮新增失败。

**仍需人工确认：**在桌面应用打开 Teams 详情，分别检查无路由事件、有 `routing_decision`、有 `binding_applied`，以及 `dispatch_readiness.missing` 有值/为空时的布局与长解释换行。自动化测试覆盖逻辑，尚不能替代该视觉检查。

**真实证据（已有，历史）**：Kimi 订阅 Teams 实测、SQLite v1→v2 迁移实测、五源在线刷新（经代理）、真实官方文档解析冒烟（fixture 存 `F:\everyAI\all\pricing-fixtures-20260922\`）。

**待做（需要你登录账号，按序执行）：**
1. `npm i -g @mimo-ai/cli@0.1.15 @moonshot-ai/kimi-code@2.0.2 minimax-code@0.5.2`，各跑真实 ACP 握手（验证身份/banner 假设，回填固定指纹）。
2. 各工具真实登录（kimi/mimo/minimax 官方流程）+ 真实推理、取消、失败注入各一次；脱敏 fixture 归档 `tests/`。
3. Automatic 无预算路径：独立示例 Git 项目跑一次跨厂商团队（≥2 厂商不同节点），主机测试通过、源 checkout 不变，证据归档（H01 第 4 步）。
4. 真实账号下的 LiveBench 在线刷新 + 路由预览端到端。
5. Codex：在 API key 与 ChatGPT 账号各执行一次脱敏闭环，核对 `identity_readback` 中的账户渠道、计划档位、生效模型/提供商/档位，再确认邮件等账户个人信息未写入事件；Claude/Kimi/DeepSeek/MiMo/MiniMax 对照各自字段确认未知项仍为空、不误报渠道。

## 四、还未完成的任务与完成思路

| 任务 | 现状 | 完成思路 |
| --- | --- | --- |
| **H01 真实账号闭环** | Codex 回读账号渠道/计划和线程模型；Claude、ACP 与 Python Kimi 的工具指纹和可证实模型设置代码已完成。账号渠道/计划仅 Codex 有显式回读；其余字段遵守未知保留规则。真实账户测试未完成，需登录 | 按上节测试 1–5 执行，归档脱敏事件；仅在工具官方会话回传明确账号计费/额度信息时扩充 `billing_channel`/`account_plan`，否则保持 `None`。根据实测更新工具版本与指纹支持策略 |
| **H04 硬预算闭环** | 已把每个 planner/node-attempt child 的原生 usage 事件按 workflow/phase/attempt 归档；金额有单独未确认分类。预留/终态结算仍未接线，`budget_usd` 继续阻塞 | 下一步建立 attempt 级 `estimated_usd`/`confirmed_usd` 数据合同；估算按 routing 成本与安全系数预留；接入官方确认账单源和进程内中断/配额机制后，才可结算并开放硬预算。凡只能拿到 token 数/CLI 或 harness 自报金额的渠道，明确标记不支持硬上限并只显示软提示；检查 Planner、评审、并行子任务、失败重试、取消及崩溃恢复均有费用归属 |
| **两个 CLI 的路由策略命令** | 实现、自动测试完成；随提交 `03a8ff9` 交付 | 已提供 `preview/saved/replay`，严格透传既有 HTTP 合同；无需额外 CLI 测试 |
| **桌面阻塞原因面板（H06 起步）** | 实现、桌面自动化测试完成；随提交 `03a8ff9` 交付 | 已渲染 `dispatch_readiness.missing` 与路由事件；还需按本报告手工检查事件差异、长解释和空数据情形 |
| **Grok Build 接入** | ACP stdio 已确认；适配未开始，源码提交号有冲突 | 先重新取得并校验上游固定提交/官方发布物，再按 `meta.grokShell` 与 `agentVersion` 校验身份；补非交互 `authenticate`、模型/努力值配置、权限和取消适配；真实 CLI 握手及推理后再开放为受管执行器。通用 ACP runner 目前缺少认证步骤，不能直接套用 |
| **ZCode 接入** | 源码已取回，发布物对应关系待核 | 核对 `@zcode/cli@3.14.0` 与源码提交，再按其 app-server 协议实现独立 transport；不复用 ACP 假设 |
| **H05 恢复/分叉** | 新增终态独立 workflow → 新 Draft 的 duplicate API/CLI（不复制 session/output、不自动启动；团队 child 禁止脱离父团队复制）；所有适配器 `resume/fork=false` | 继续完善终态状态/HTTP/CLI覆盖；注入崩溃、断网、审批等待、进程残留和集成中取消。后续按适配器持久化 session ID、工具版本/配置、workspace revision 并实测可恢复性；分叉历史与 Teams attempt 重试单独定合同，未验证前保持 false |
| **H07 插件/定时/远程/PR/网站、H08 沙箱/MCP、H10 研究实验** | 未开始 | 按 [ROADMAP](ROADMAP-2026-09-23.md) 既有分解与验收门槛执行 |

**执行顺序建议**：完成桌面视觉检查 → 你登录账号做 H01 实测（含 Codex API key/ChatGPT 两渠道及其他已接入工具）→ H04 预算闭环（只有兑现成本上限后才解除阻塞）→ 校验 Grok/ZCode 上游版本并适配 → H05/H07/H08 → H10 实验出结论。
