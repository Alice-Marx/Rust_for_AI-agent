# 总项目任务报告

日期：2026-09-23
仓库：[Alice-Marx/Rust_for_AI-agent](https://github.com/Alice-Marx/Rust_for_AI-agent)，main = `d8c3461`
版本基线：0.11.0（`24dbf54`）+ 本阶段 13 个提交
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
- 协议评估结论：ACP 是 anytool 工具收敛点；ZCode 走自有 app-server 协议；opencode Go 版无结构化入口；grok-build 需先确认 server 暴露方式。

### 当前能力边界（诚实声明）

`auto_dispatch_ready=false` 仍是正确状态：全部渠道的账号级计费字段 unknown、无真实账号实测。无预算 Automatic 派工代码路径已就绪，等账号登录后即可真实验证。硬预算（`budget_usd`）阻塞保持到 H04 闭环。

## 二、各文件的说明

主程序 `src/` 共 53 个模块 + 5 个执行器子模块 + 4 个 bin（约 3.7 万行）；逐文件历史职责见 [FILE_CATALOG-2026-09-21](FILE_CATALOG-2026-09-21.md)（按 0.11.0 基线，新增模块以本报告为准）。

**受管执行器（官方工具适配层，全部不修改上游）：**

| 文件 | 说明 |
| --- | --- |
| `native_executor.rs` | 执行器门面：capabilities、绑定校验（模型归属/档位）、dispatch、共享帧读取/事件/进程树工具 |
| `native_executor/acp.rs` | **通用 ACP 会话引擎**（本轮新增）：AcpRunner + AcpDialect trait；JSON-RPC 循环、initialize/session/prompt、工具状态机、一次性权限（read-only/失联永不授权）、取消 |
| `native_executor/deepseek.rs` | DeepSeek Harness 方言（隔离 profile、版本/哈希双闸、JSON 数组模型值） |
| `native_executor/claude.rs` | Claude Code 双向 stream-json（严格 2.1.193） |
| `native_executor/kimi_code.rs` | Kimi Code·Node 方言（本轮新增）：只认 npm `dist/main.mjs`，与 Python kimi 身份隔离 |
| `native_executor/mimo.rs` | MiMo Code 方言（本轮新增）：平台二进制定位（launcher 包 + 平台包） |
| `native_executor/minimax.rs` | MiniMax Code 方言（本轮新增）：`m:p:m[:v:v]` wire 模型值 |

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
| `team_service.rs` | Teams HTTP 合同 + Automatic 启动路径（决策→`set_planner` 写回→执行） |
| `team_store.rs` | 团队/节点/attempt 权威存储 + 微美元账本 + `set_planner` |
| `team_workspace.rs` / `team_checks.rs` | 独立 Git 工作区与主机验收 |
| `workflow.rs` / `workbench_service.rs` | 单任务 SQLite 状态机（v2）与执行控制 |

**其余模块**：API Agent（`agent/api/provider/anthropic/responses/router/tools/`）、会话（`session/session_index/memory/`）、安全（`permissions/sandbox*/mcp*/process_tree/`）、桌面与终端（`desktop_*/bin/desktop/`）、诊断（`app_diagnostics/subscription/cliproxy/`）、npm CLI（`packaging/npm/wonderland-cli/`）、Windows 打包（`packaging/windows/`）、上游参考（`anytool/` 16 子模块，只读）。

## 三、需要做的测试

**自动回归（本机当前全过）**：库 437 + Rust CLI 3 + 桌面 18 + 协议集成 7 + npm 14；7 项外部条件测试按设计忽略；`cargo fmt --check` 干净。已知例外：desktop_bridge×3/desktop_terminal×1 在未改动基线即可复现（本机 PowerShell CLIXML 污染与真实 kimi CLI 行为），非回归。

**真实证据（已有，历史）**：Kimi 订阅 Teams 实测、SQLite v1→v2 迁移实测、五源在线刷新（经代理）、真实官方文档解析冒烟（fixture 存 `F:\everyAI\all\pricing-fixtures-20260922\`）。

**待做（需要你登录账号，按序执行）：**
1. `npm i -g @mimo-ai/cli@0.1.15 @moonshot-ai/kimi-code@2.0.2 minimax-code@0.5.2`，各跑真实 ACP 握手（验证身份/banner 假设，回填固定指纹）。
2. 各工具真实登录（kimi/mimo/minimax 官方流程）+ 真实推理、取消、失败注入各一次；脱敏 fixture 归档 `tests/`。
3. Automatic 无预算路径：独立示例 Git 项目跑一次跨厂商团队（≥2 厂商不同节点），主机测试通过、源 checkout 不变，证据归档（H01 第 4 步）。
4. 真实账号下的 LiveBench 在线刷新 + 路由预览端到端。

## 四、还未完成的任务与完成思路

| 任务 | 现状 | 完成思路 |
| --- | --- | --- |
| **H01 真实账号闭环** | 等你登录 | 按上节测试 1–3 执行；Codex 补生效配置回读，Claude/DeepSeek 补真实推理与失败注入 |
| **H04 硬预算闭环** | 账本已有、未接线 | 派工前按估算成本×安全系数走 `team_store` 微美元预留，终态结算/释放，重启对账；适配器 usage→attempt 账目（mimo 的 `reported_cost_usd` 标为非确认值）；闭环后解除 `budget_usd` 阻塞 |
| **两个 CLI 的路由策略命令** | 仅 HTTP | Rust/npm CLI 加 `team routing save/preview/replay`（透传三接口）+ 双端测试 |
| **桌面阻塞原因面板（H06 起步）** | 无 UI | Teams 界面渲染 `dispatch_readiness.missing` 与 `routing_decision`/`binding_applied` 事件（「为什么阻塞/为什么选它」） |
| **grok-build 接入** | 评估完成 | 查 `xai-grok-agent` 有无 headless/serve 模式；有则 GrokDialect，无则记为终端手动 |
| **ZCode 接入** | 评估完成 | 自有 app-server 协议，按 codex 模式独立 transport，锁 3.14.0 |
| **H05 恢复/分叉** | resume/fork=false | 先立数据合同（原生 session id/工具版本/revision/可恢复性检查），逐适配器实测后才置 true |
| **H07 插件/定时/远程/PR/网站、H08 沙箱/MCP、H10 研究实验** | 未开始 | 按 [ROADMAP](ROADMAP-2026-09-23.md) 既有分解与验收门槛执行 |

**执行顺序建议**：CLI 路由命令 + 桌面面板（纯代码，立即可做）→ 你登录账号做 H01 实测 → H04 闭环（解锁硬预算）→ grok/ZCode 适配 → H05/H07/H08 并行池 → H10 实验出结论。
