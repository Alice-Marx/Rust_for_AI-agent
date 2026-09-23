# 工作报告：ACP 框架泛化、MiMo 受管接入与 Automatic 启动路径

日期：2026-09-23
基线：`7bb072a`（`handoff-development`，已合入 main）
改动：`src/native_executor/acp.rs`（新）、`src/native_executor/mimo.rs`（新）、`src/native_executor.rs`、`src/native_executor/deepseek.rs`、`src/desktop_bridge.rs`、`src/team_service.rs`、`src/team_store.rs`、`src/account_billing.rs`、`src/pricing.rs`、`src/model_identity.rs`（测试样本）

## 本轮交付

### 1. 通用 ACP 框架（`acp.rs`）

把 DeepSeek Harness transport 的会话引擎泛化：JSON-RPC 帧循环、initialize/session/prompt 生命周期、流式更新与工具状态机、一次性权限（read-only/失联/无归属永不授权）、取消与优雅关闭。方言差异（握手身份、模型/档位配置值、权限选项形状、停止原因、透传更新）收敛到 `AcpDialect` trait。`deepseek.rs` 重构为 `DeepSeekDialect` + 框架调用，全部既有测试等价迁移（原 Runner 测试迁至 `acp::tests` 以 DeepSeek 方言驱动），行为不变。

### 2. MiMo Code 受管接入（`mimo.rs`，参考 codex-host 的 profile 化方法）

`mimo acp`（@mimo-ai/cli 0.1.15）成为第五个受管执行器（app_id `mimo`）。方言细节按上游源码逐项核对并 fail-closed（见 [anytool 接入评估](ANYTOOL-ADAPTER-ASSESSMENT-2026-09-23.md)）：fork 遗留身份 `OpenCode` 的锁定、`providerID/modelID` 模型值、无 effort 选项、三选项权限只取一次性对、`available_commands_update` 透传、`usage_update` 的 `reported_cost_usd` 标注为非提供商确认值。安装探测：`WONDERLAND_MIMO_BIN`/官方 `MIMOCODE_BIN_PATH`/npm 平台包布局；版本双闸（包版本 + banner）。**真实握手与推理待用户登录账号后执行（H09 步骤 7）**；二进制摘要记录但固定指纹常量待真实安装回填。

### 3. Automatic 团队启动路径（H03 核心）

`execute_team` 的 Automatic 分支从「刷新后一律 Blocked」改为完整决策链：

1. 同轮在线刷新榜单+价格（失败即 Blocked，保留原证据事件）；
2. 加载身份注册表，用保存的 `routing_policy` 构造请求（无保存策略 → Blocked 并指引用预览端点保存）；
3. `routing::decide`（routing-v2：身份 attestation、榜单行、精确报价、预算过滤），完整决策写入 `routing_decision` 事件；
4. 决策 blocked → 团队 Blocked（原因来自候选排除理由，不再是硬编码句子）；
5. **决策 selected 且 `budget_usd` 为 None** → 新增 `team_store::set_planner` 把选中绑定写回为 planner（即所有无显式绑定节点的执行器；事务校验 revision、执行前、候选声明过），`binding_applied` 事件记录质量分/估算成本/epoch/映射版本，然后走既有执行路径；
6. 决策 selected 但带 `budget_usd` → 决策照常记录，执行保持 Blocked（硬预算结算未闭环，H04）。

**成本语义**：无预算自动派工与固定执行处于同等地位（用户账号运行官方工具，Wonderland 不做预算承诺）；每轮的证据链（身份/榜单/报价 epoch + 映射版本）完整持久化，可复核为何选了它。`/api/v1/pricing` 状态新增 `automatic_dispatch` 对象区分两条路径；`dispatch_readiness` 的 scope 收窄为硬预算维度；`auto_dispatch_ready` 保持 false（专指硬预算/账号核验）。

## 验证结果

- `acp::tests` 9 项（迁移的会话/权限/取消/工具状态机测试，DeepSeek 方言驱动）+ `mimo` 3 项（模型 ID 格式、方言锁定、**完整 ACP 会话集成测试**：握手→模型设置→透传更新→usage 带 cost→三选项权限只读自动拒绝→工具完成清理 stale→end_turn）全过。
- `deepseek` 4 项 profile 测试保持；`native_executor` 套件 45 项全过（含 mimo）。`account_billing`/`pricing` 断言随 mimo 渠道（9 条）与状态形态更新后全过。
- 全目标回归（含 `ui-snapshots`）：430 库 + 3 Rust CLI + 18 桌面 + 7 协议；失败仅为已基线复现的 desktop_bridge/desktop_terminal 本机环境问题。npm 14/14。`cargo fmt --all --check` 通过。

## 边界与未完成

1. **mimo 真实账号验证未做**（用户将自行登录执行 H09 步骤 7–8：真实握手、推理、取消/失败注入、脱敏 fixture、固定指纹回填）。在实测前不宣称 mimo 有真实模型执行能力。
2. grok-build / kimi-code / minimax-code 同为 ACP，**按同框架接 dialect 即可**（评估报告给出各自待读细节）；ZCode 的自有协议审计已完成，但没有官方可验证发行物，保持未接入，详见 [ZCode 审计报告](WORK-REPORT-2026-09-23-ZCODE-AUDIT.md)；opencode Go 版暂不接。
3. Automatic 带硬预算（`budget_usd`）仍阻塞到 H04 预算结算闭环；订阅渠道派工仍按计费合同阻塞。
4. Automatic 无预算路径的**真实跨厂商成功实例**待账号验证后按 [ROADMAP](ROADMAP-2026-09-23.md) H01 第 4 步归档。
