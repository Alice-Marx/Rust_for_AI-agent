# 任务报告：H04 预算闭环第一片——预留接线、保守结算与对账

日期：2026-09-23
基线：main `7130209`（0.11.1 npm 发布后）

## 一、完成的工作

把 `team_store` 里已存在但**从未接线**的微美元预留账本接入真实派工路径，并建立对账闸门：

1. **渠道硬预算能力声明**：`account_billing` 的 `ChannelContract` 新增 `hard_budget_capable: bool` 与 `hard_budget_blocker`——没有提供商确认账单源、无法在 USD 边界强制中断在途请求，故今天**全部渠道为 false**。预算团队（`budget_usd`）启动时（Automatic 与固定两条路径）改为按 planner 渠道合同生成结构化阻塞原因，替代原先的硬编码句子。
2. **估算→预留接线**：Automatic 团队的路由决策估算（`selected_estimated_cost_usd`）× 显式安全系数 1.25（`ROUTING_RESERVATION_MARGIN`，写入 `binding_applied` 事件）后，按当前计划节点数**平摊**传给每个 `begin_attempt`——预留真实发生。平摊假设显式注释（逐节点 token 细分不宣称）；fixed/assigned 团队无决策估算，维持无预留（与现状一致）。
3. **终态保守结算**：节点 attempt 成功/失败终态处调用新便捷方法 `settle_attempt_reservation`——actual 保持 `None`（charged 回落为预留额），直到存在提供商确认账单；取消/未启动场景由对账兜底。
4. **对账闸门**：新增 `team_store::reconcile_reservations`——把团队内所有遗留 `Reserved` 预留关闭：attempt 已终态的保守结算（按预留额），未启动的释放。三个挂点：每次运行收尾（成功/失败/取消均执行，原因入事件）、Blocked 团队重跑前（旧 hold 不带入新一轮）、未来服务重启扫描可直接复用。**预留从此不会泄漏，也不会双重计费。**

**边界（诚实声明）**：`budget_usd` 团队**仍被阻塞**——所有渠道 `hard_budget_capable=false`。本片交付的是完整的预留/结算/对账机械，解锁条件是任一渠道获得提供商确认账单源并证明可强制中断。

## 二、各文件的说明

| 文件 | 说明 |
| --- | --- |
| `src/account_billing.rs` | `ChannelContract` 新增 `hard_budget_capable`/`hard_budget_blocker`（含共享原因常量）；新增「无渠道可在无确认来源时宣称硬预算」测试；JSON 字段清单更新 |
| `src/team_store.rs` | 新增 `reconcile_reservations`（终态结算/未启动释放，幂等）与 `settle_attempt_reservation`（按 attempt 定位预留并保守结算）；新增对账单测（含重试预算验证与幂等验证） |
| `src/team_service.rs` | 预算阻塞两条路径改为渠道合同驱动的原因；`ROUTING_RESERVATION_MARGIN=1.25`；决策估算平摊传入 `begin_attempt`（`binding_applied` 记录系数与说明）；attempt 成功/失败结算挂点；运行收尾与重跑前的对账挂点 |

## 三、需要做的测试

**已做（全过，零失败）**：
- 新增单测：对账关闭遗留 hold（终态保守结算 500µ$、未启动释放、释放后节点标 Interrupted 可在预算内重试、幂等）；无渠道宣称硬预算。
- 全目标回归（含 ui-snapshots）：库 456 + Rust CLI 4 + 桌面 18 + 协议 7，**0 失败**、8 项按设计忽略；npm 16/16；`cargo fmt --check` 通过。

**待做**：
1. 集成级：用真实 Automatic 团队跑一轮（需账号），检查 `budget_reconciled`/`budget_settled` 事件序列与 charged 累计。
2. 解锁验证（未来）：某渠道接入确认账单源后，把该渠道 `hard_budget_capable=true` 并补「预算内放行/超预算阻塞/在途超额」三场景测试——这是解除 `budget_usd` 阻塞的验收门。

## 四、还未完成的任务与完成思路

| 任务 | 思路 |
| --- | --- |
| **H04 第二片：确认账单源** | 候选：OpenAI usage API / Anthropic usage 导出 / 各家 invoice 端点。任一渠道拿到「提供商确认金额 + 可中断执行」证据后置 `hard_budget_capable=true`，补三场景测试，解除该渠道预算阻塞；其余渠道保持阻塞并在 UI 显示软提示 |
| **usage→结算精度** | 目前结算 actual=None（按预留额）。确认账单源接入后，把 `usage_observed` 的确认金额写入 `settle_reservation(Some(actual))`；harness 自报值（mimo/Claude CLI 的 reported USD）保持「未确认」分类不参与硬预算 |
| **服务重启扫描** | `reconcile_reservations` 已可复用；在服务启动恢复流程里对Interrupted 团队批量调用即可（挂 `workbench_service` 启动路径，下一片） |
| **H01 账号实测 / Grok 真实闭环 / ZCode 核对** | 按总报告第三节待办清单执行（需登录） |
| **H05/H07/H08/H10** | 按 [ROADMAP](ROADMAP-2026-09-23.md) 分解 |
