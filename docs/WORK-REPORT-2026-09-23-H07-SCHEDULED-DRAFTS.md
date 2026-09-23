# H07 一次性定时草稿工作报告

日期：2026-09-23
状态：已完成本地一站式定时草稿纵切；不包含周期计划、远程执行、插件、GitHub 写入或网站发布。

## 完成的工作

实现了可持久化的一次性定时草稿。计划到期或服务重启补触发时，只会创建新的 `Draft`，不会启动官方 CLI、提交模型请求或复用任何旧会话。

- `run_at` 只接受带偏移量的 RFC 3339 时间，并规范化为 UTC 微秒精度；创建时必须在未来。
- 计划、触发记录、工作流、`created` 事件和 `scheduled_from` 事件全部位于 `workflows.sqlite` 的同一个 `Immediate` SQLite 事务内。崩溃或写入失败时不会留下“已触发但没有草稿”的状态。
- `schedule_triggers` 以 `(schedule_id, scheduled_for)` 为主键记录一次终态尝试。成功会写入新 Draft ID；失效工作目录或失效受管绑定会写入 `failed` 审计记录且不自动重试。
- 工作台取得单一所有者锁后同步处理已到期计划，随后以弱引用定时轮询。停机期间过期的计划会在下次启动补触发一次。
- HTTP 接口提供 `GET/POST /api/v1/schedules`、`GET/DELETE /api/v1/schedules/{id}`；Rust CLI 提供 `wonderland-cli schedule list|create|get|cancel`。模板是严格的 `WorkflowCreate` JSON，路径 ID 使用单段百分号编码。

## 文件说明

| 文件 | 作用 |
| --- | --- |
| `src/schedule_store.rs` | 定时计划/触发的序列化类型、状态枚举、RFC 3339 与模板大小校验；不再单独打开 SQLite 数据库。 |
| `src/workflow.rs` | 数据库 v3 迁移；计划 CRUD、到期查询、取消、原子物化和审计行读取；新增事务回滚与失效目录测试。 |
| `src/workbench_service.rs` | 将服务侧的工具绑定、聊天只读化与工作目录规范化统一为一个流程；启动补触发、轮询和 HTTP 路由。 |
| `src/bin/wonderland-cli.rs` | `schedule` 子命令、JSON 模板文件读取、DELETE 请求和路径编码测试。 |

## 已执行测试

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过。 |
| `cargo test --locked --lib schedule` | 5/5 通过：单次 Draft、失效目录失败审计、事务回滚、取消、启动补触发。 |
| `cargo test --locked --all-targets --features ui-snapshots` | 466 项库测试通过，8 项按外部条件忽略；Rust CLI 6/6、桌面 20/20、协议集成 7/7 通过。 |
| `npm test --prefix packaging/npm/wonderland-cli` | 16/16 通过。 |
| 离线 HTTP 冒烟 | `AGENT_PROVIDER=offline` 下创建、取消和复制任务；带覆盖字段的 duplicate 返回 HTTP 409。服务停止后重启，已到期计划变为 `fired`，只生成一个 `draft`，`native_session_id=null`，并有 `scheduled_from`（`started_automatically=false`、`model_dispatched=false`）事件。 |

该冒烟使用独立的 `target/offline-smoke-h07-20260923` 数据目录，没有访问真实账号、OAuth、官方 CLI 或模型服务。

## 剩余任务与推进方式

1. **周期与时区规则**：新增明确的 recurrence 数据合同、DST 策略、下一次触发计算和每个计划时段的唯一触发键；先用可复现时间源测试跨夏令时和停机补偿。
2. **桌面计划管理页**：在现有工作流 UI 中加入创建、列表、取消、失败原因和已生成草稿链接；小窗口和中文长错误需要截图与人工操作验证。
3. **通知与外部集成**：计划状态变化可先落本地审计，再为通知、远程主机、插件、PR 和网站发布分别定义授权、目标确认、重试与撤销合同。外部写入不得从计划触发中隐式执行。
4. **数据库升级操作**：`workflows.sqlite` 已从 v2 升为 v3。用户升级前应备份完整数据目录；旧二进制会拒绝读取新版数据库，不能通过手工改 `user_version` 回退。
