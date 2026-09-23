# 工作报告：H04 原生 usage 观察归档

日期：2026-09-23。对应 [总项目任务报告](PROJECT-TASK-REPORT-2026-09-23.md) 的 H04 增量。

## 已完成

- Teams 在 Planner 结束，以及节点执行、评审或重试 child workflow 进入验证/终止时，读取其持久化原生 `usage` 事件并写成 `usage_observed` 团队事件。
- 事件保留 child `workflow_id`、phase、node、attempt、应用、模型、推理档位、事件序号和最近最多 16 条 usage 快照；大快照会受单条字节上限约束，团队事件总大小也有上限。
- 将 CLI 自报 `total_cost_usd` 与 harness 自报 `reported_cost_usd` 标成未确认观察值。未知金额、估算金额和 provider 确认金额不互相推导，事件中的 `estimated_usd` 与 `confirmed_usd` 明确为空；快照不累加成账单。
- **H04 仍未完成**：预算预留/结算尚未连接到派工和 attempt 生命周期，usage 与官方发票不等价，也没有可证明的在途 USD 上限。因此 `budget_usd` 继续在派工前阻塞。

## 文件说明

| 文件 | 说明 |
| --- | --- |
| `src/team_service.rs` | child 生命周期结束时归档 usage，标记 phase/node/attempt，并对工具报告金额进行未确认分类；包含分类与归属测试。 |
| `docs/PROJECT-TASK-REPORT-2026-09-23.md` | 更新总项目交付、文件职责、测试结果与 H04 未完成边界。 |
| `docs/DEVELOPER_HANDOFF.md` | 将已交付的观察归档与后续预算账本工作同步到 H04。 |
| `docs/DOCUMENT-GUIDE-2026-09-23.md` | 登记本增量工作报告。 |

## 验证与后续测试

- `rustup run stable cargo fmt --check`：通过。
- `git diff --check`：通过。
- `cargo test --lib team_service::tests::reported_usage_costs_are_not_marked_as_confirmed`：通过。
- `cargo test --lib team_service::tests::child_usage_is_attributed_to_a_team_phase_and_attempt`：通过。
- `cargo test`：最终全量回归通过（库 446 项通过、7 项忽略；CLI 4、桌面 18、协议集成 7 项通过；文档测试 0 项）。首轮全量回归有一个 3 秒时限测试波动；该项单测复跑及随后的全量回归均通过。
- 后续需补充集成测试：Planner 与执行/评审/重试的归属、usage 分页超过 1000 个事件、保留快照截断、超大载荷限制、取消/失败终态，以及并发 child 写入。
- 硬预算验收还需账本并发预留、重复结算防护、崩溃重启对账、provider 确认数据源、进程内停止/在途超额策略测试；只有可验证地限制 USD 消费后，才能移除 `budget_usd` 阻塞。

## 未完成任务与实施思路

1. 建立 attempt 级费用合同，将 `estimated_usd`、provider `confirmed_usd` 与订阅额度原生单位分开存储，并定义缓存、评审、重试和并行在途请求的归属。
2. 派工前按可追溯价格和明确安全系数预留；成功、失败、取消时幂等结算或释放；启动恢复时扫描并核对悬挂预留。
3. 只接受 provider 可核验的账单/额度作为确认成本。CLI/harness 自报和 token 用量继续作为观察或估算，不冒充账单。
4. 为每种受管工具确认可执行的轮数/输出/进程停止限制，并测量取消后的最坏在途消耗。没有硬 USD 保证的渠道继续只提供软提示且不开放硬预算。

