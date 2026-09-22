# 阶段工作报告：H02 数据合同完成与 H03 决策侧落地

日期：2026-09-23
分支：`handoff-development`（基线 `24dbf54` = 0.11.0 官方交付基线）
范围：2026-09-22 至 2026-09-23 两天的连续增量，共 9 个提交

## 一、本阶段目标

按 [开发者交接指南](DEVELOPER_HANDOFF.md) 的依赖顺序（H01/H02 → H03/H04），完成 H02「实时榜单、精确报价、账号额度的共同数据合同」的全部四项，并把 H03「真正开启 Automatic 调度」中不依赖真实账号的决策侧全部落地。**不解除 Automatic 的正式阻塞**——那是设计约束，不是未完成项。

## 二、提交清单

| 提交 | 内容 | 归属 |
| --- | --- | --- |
| `85a0cd2` | 模型身份注册表 `src/model_identity.rs` + 种子 v1.0（H02 第 1 项） | 用户完成 |
| `9bc8fe5`/`595209c` | 路由预览、路由策略持久化与决策重放（H03 已有基础） | 此前工作 |
| `ebe75d0`/`9eb174f` | 上游源码归档回退、最终交接文档 | 此前工作 |
| `af15bcb` | Anthropic 与 DeepSeek 官方价格源解析（H02 第 2 项） | 本阶段 |
| `5061ef7` | 官方定价文档 fixture 持久化记录 | 本阶段 |
| `e151314` | 账号计费语义合同 `src/account_billing.rs`（H02 第 3 项） | 本阶段 |
| `febe3ef` | routing-v2：路由决策消费身份注册表（H03 决策侧） | 本阶段 |
| `77612c8` | 身份种子 v1.1：报价模型 × 榜单行系统核验 | 本阶段 |

## 三、交付详情

### 1. 五个官方价格源全部 verified（H02 第 2 项）

`src/pricing.rs` 从两个 blocked 源扩展为五个 verified 源：

- **新增第五源 `anthropic-models`**（官方 models overview）：同一刷新 epoch 内把 Anthropic 定价表的显示名 join 到 attestation 的精确 API model ID/alias。join 不到不发报价；attestation 与官方命名规则冲突整源阻塞；退役行不出第一方报价；缓存乘数（1.25x/2x/0.1x/0.025x）逐行交叉校验。
- **DeepSeek 峰谷解析**：每模型 `off_peak`/`peak` 两档，条件携带逐字官方 UTC 窗口；「谷价为峰价一半」逐格校验；中国节假日历显式 unknown；退役别名按官方脚注以 flash 价出报价。
- `parser_version` 升至 2，旧缓存 fail-closed 失效。
- **真实证据**：在线端到端刷新（经代理）五源全部 verified、59 条报价、快照持久化并重开复验通过；五份官方原文与 SHA-256 存于 `F:\everyAI\all\pricing-fixtures-20260922\` 供复验。

### 2. 账号计费语义合同（H02 第 3 项）

`src/account_billing.rs` 定义 7 条渠道合同（codex/kimi-cli/claude 各 api+subscription，deepseek 仅 api）：

- api = USD/token 列价且列价≠实付；订阅 = 提供商额度单位，明文禁止订阅费÷token 折算；
- 实付/剩余额度/限速/重置周期四类账号级字段全部显式 unknown+原因（无提供商计费 API 集成，诚实状态）；
- `dispatch_blockers()`（28 条）经 `/api/v1/pricing` 的 `billing_channels`+`dispatch_readiness` 暴露，UI/CLI/路由共用一个事实源。

### 3. routing-v2：决策消费身份注册表（H03 决策侧）

榜单行不再由调用方声明。每个候选经注册表对精确 (app_id, model, reasoning_effort) 做 attestation：attested 才评分、声明行不一致拒绝、未映射/不在榜/条目过期拒绝。决策携带 `identity_mapping_version` 与 benchmark:price epoch 构成完整证据链；旧事件兼容重放。

### 4. 身份种子 v1.1

59 条报价模型 × LiveBench 2026-06-25 的 59 个榜单行系统核验：

- OpenAI 37 个、Anthropic 14 个报价模型**零 byte-identical 行**（档位变体行的 effort 映射无官方来源，不映射）；
- 新增 `(deepseek, deepseek-v4-pro)` attestation（官方定价精确名 + 榜单同名行 23 项分数）；
- 新增 `deepseek-flash` not_listed；退役别名不映射的依据写入 evidence。

## 四、验证证据汇总

- 单元/集成：pricing 10、routing 9、model_identity 10、account_billing 5、model 相关 52，全部通过。
- 全目标回归（含 `ui-snapshots`）：428 库 + 3 Rust CLI + 18 桌面 + 7 协议；4 个失败为 desktop_bridge/desktop_terminal 的本机环境问题（已在未改动基线用 `git stash` 复现，见各工作报告）。
- npm 14/14；`cargo fmt --all --check` 通过。
- 真实网络证据：五源在线刷新 verified（59 条报价）；真实官方文档解析冒烟（OpenAI=37 Kimi=4 Anthropic=14 DeepSeek=4）；LiveBench 2026-06-25 表格经官方 raw 源核验。

## 五、当前能力与边界

**现在能做的**：Automatic 团队的路由预览（`/api/v1/teams/{id}/routing/preview` 及 `/saved`、GET 重放）在真实在线证据上给出可解释、可复核的候选决策——身份、榜单行、精确报价、峰谷条件、预算过滤全部绑定不可变 epoch。

**仍然阻塞的（设计如此）**：`auto_dispatch_ready=false`。原因的结构化清单见 `/api/v1/pricing` → `dispatch_readiness.missing`：全部渠道的实付/额度/限速/重置周期 unknown（需要真实账号与提供商计费来源），以及 H01 的真实执行器闭环。**解除阻塞的路径见 [后续路线图](ROADMAP-2026-09-23.md)，不得绕过检查伪装完成。**
