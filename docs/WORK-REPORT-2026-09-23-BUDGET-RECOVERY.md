# 工作报告：H04 预算闭环第二片——服务重启预留恢复

日期：2026-09-23
基线：main `8db113f`（H04 预留接线第一片之后）

## 一、完成的工作

1. **当前启动中断的团队原子对账**：`TeamStore::recover_interrupted` 在把运行、等待输入、验证或有活动节点的阻塞团队标记为 `Interrupted` 的同一 SQLite 事务里，将仍为 `Reserved` 的预留按保守金额结算。`recovered_interrupted` 事件现在记录每笔预留的 ID、节点、尝试号和结果，避免服务崩溃后把可解释的已在途金额遗留为 hold。
2. **历史 Interrupted 记录启动扫描**：`TeamService::open` 在已有活动记录恢复之后，扫描已经是 `Interrupted` 且仍带 `Reserved` 预留的历史团队，并调用既有的 `reconcile_reservations`。该路径保留 `budget_reconciled` 事件和 `service startup recovery` 原因；没有遗留 hold 的重启不新增事件。
3. **幂等和账本语义验证**：恢复中的预留保留 `actual_microusd=None`，因此计费继续按原预留额保守计算；第二次恢复或再次启动不会重复结算或追加恢复事件。没有把 CLI/harness 自报金额升级为确认账单，也没有解除 `budget_usd` 的阻塞。

## 二、各文件的说明

| 文件 | 说明 |
| --- | --- |
| `src/team_store.rs` | 把服务重启时的 Reserved hold 在恢复事务中结算，并把清单写入 `recovered_interrupted` 事件；单测覆盖状态、金额、事件内容和幂等恢复。 |
| `src/team_service.rs` | 服务打开时扫描旧的 Interrupted 团队，调用共享对账逻辑；测试覆盖历史遗留 hold 的一次性关闭。 |
| `docs/WORK-REPORT-2026-09-23-BUDGET-RECOVERY.md` | 本增量的完成项、验证证据、剩余边界和后续方法。 |
| `docs/PROJECT-TASK-REPORT-2026-09-23.md` | 总项目报告同步 H04 接线、启动恢复和安装包交付计划。 |
| `docs/ROADMAP-2026-09-23.md`、`docs/DEVELOPER_HANDOFF.md` | 将 H04 的已交付机械与仍受账号/账单证据阻塞的部分分开说明。 |

## 三、需要做的测试

**已做：**

- `cargo fmt --all -- --check`：通过。
- `cargo test --locked --lib team_`：38 项通过、0 失败。覆盖 `recover_interrupted` 的预留状态/事件清单/幂等性，以及服务启动扫描历史 Interrupted 团队。

**仍需真实账号或提供商证据：**

1. 用真实 Automatic 团队中断一次运行，核对 `budget_settled`、`budget_reconciled` 和 `recovered_interrupted` 的事件顺序与账户侧金额。
2. 接入一个提供商确认账单源后，测试预算内放行、超预算阻塞和在途请求超过边界三种情况。

## 四、还未完成的任务与完成思路

| 任务 | 完成思路 |
| --- | --- |
| **H04 确认账单和硬预算** | 只在获得提供商确认金额和可中断在途请求的证据后，为相应渠道设置 `hard_budget_capable=true`；把确认金额传给 `settle_reservation(Some(actual))`，其余渠道继续阻塞。 |
| **H01 真实账号闭环** | 用最终 Windows 安装包分别登录各官方工具，执行脱敏的握手、推理、权限、取消和失败注入，并归档证据。 |
| **H09 ZCode** | 发行物审计已完成：源码包均 private、精确 npm 查询无包、GitHub 无 release；等待官方可验证发行物后，再按实际初始 JSON 握手和二进制 RPC 实现独立 transport。详见 [ZCode 审计报告](WORK-REPORT-2026-09-23-ZCODE-AUDIT.md)。 |
| **安装包** | 在剩余无需账号的代码、离线测试和桌面检查完成后，生成并校验 Windows 安装器和便携包，供用户自行进行真实登录测试。 |
