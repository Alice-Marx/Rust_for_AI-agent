# 工作报告：H05 终态任务复制为新草稿

日期：2026-09-23。对应 [总项目任务报告](PROJECT-TASK-REPORT-2026-09-23.md) 的 H05 首个代码增量。

## 已完成

- 新增 `POST /api/v1/workflows/{id}/duplicate`，Rust 与 npm CLI 均提供 `work duplicate <id>`。来源必须是已终止的独立任务；执行中、等待输入、待验收、草稿和团队拥有的 child 都不能通过该操作重试。
- 复制任务的标题、提示、目录、模式、工具、模型、推理档位、只读设置、时限和验收条件；创建新的 workflow ID，状态为 Draft。
- 旧任务的原生 session ID、输出和错误不复制；新工作流以 `duplicated_from` 事件记录来源和“不复用会话、不复制输出、不自动启动”的事实。开始执行仍须单独调用 start。
- 创建记录与来源事件在一个 SQLite 事务中完成。服务层检查工具绑定，并要求 Teams child 通过父团队管理重试。
- **H05 仍未完成**：所有原生适配器的 `resume/fork` 仍为 false。此增量仅实现可审计的新任务副本，不声称继续原会话或重放失败步骤。

## 文件说明

| 文件 | 说明 |
| --- | --- |
| `src/workflow.rs` | 抽取事务内新建记录逻辑；实现终态复制、来源事件和新草稿状态；覆盖清空旧输出/session 和拒绝活动任务。 |
| `src/workbench_service.rs` | 增加 standalone duplicate 服务规则与 HTTP 路由；拒绝复制团队 child。 |
| `src/bin/wonderland-cli.rs` | 增加 `work duplicate <id>`，只创建草稿，不自动启动。 |
| `packaging/npm/wonderland-cli/bin/wonderland.js` | 增加同合同的 npm `work duplicate <id>` HTTP 透传。 |
| `packaging/npm/wonderland-cli/test/workflows.test.js` | 验证 ID 路径编码、POST 空对象及不隐式 start。 |
| `docs/PROJECT-TASK-REPORT-2026-09-23.md` | 更新 H05 增量、文件说明、回归结果和未完成任务。 |
| `docs/DEVELOPER_HANDOFF.md` | 更新 H05 交接状态与剩余验收。 |
| `docs/ROADMAP-2026-09-23.md` | 反映 H05 现状已有独立任务副本合同，原生 resume/fork 仍未支持。 |
| `docs/DOCUMENT-GUIDE-2026-09-23.md` | 登记本增量工作报告。 |

## 已运行验证

- `rustup run stable cargo fmt --check`：通过。
- `git diff --check`：通过。
- `cargo test --lib workflow::tests::terminal_task_duplicates_as_a_fresh_draft_without_reusing_session_or_output`：通过。
- `cargo test --lib workflow::tests::active_task_cannot_be_duplicated_as_a_retry`：通过。
- `cargo test --lib workbench_service::tests::duplicate_rejects_team_owned_children_and_creates_standalone_draft`：通过。
- `rustup run stable cargo run --quiet --bin wonderland-cli -- work duplicate --help`：通过，CLI 子命令已被解析器公开；尚未对运行中的服务做真实 HTTP/CLI 调用。
- `rustup run stable cargo test`：全量通过，库 449 项通过、7 项按外部条件忽略；Rust CLI 4、桌面 18、协议集成 7 项通过；文档测试 0 项。
- `npm test`（`packaging/npm/wonderland-cli`）：16 项通过；新增请求使用 mock HTTP 服务验证 CLI 透传。
- 构建仍显示若干原有 native executor 未使用导入、变量和死代码警告。

## 仍需测试与完成思路

1. 对 `succeeded`、`cancelled`、`interrupted` 等各终态补充允许性覆盖，并验证 Draft、Blocked、WaitingInput、Verifying 均拒绝复制。
2. 通过运行中 Wonderland 服务检查真实 HTTP 路由与 Rust/npm CLI 返回 Draft，且执行只在用户随后明确调用 `work start` 时开始。npm CLI 的 mock HTTP 请求已通过，Rust CLI help 已验证，服务进程端到端请求仍待做。
3. 做 service 崩溃/重启、审批等待中断、断网和残留子进程注入，验证旧 workflow 只变为 Interrupted、不会自动恢复或自动重跑；确认用户只能在检查工作区与外部副作用后选择“复制为新任务”。
4. 真正恢复原生会话前，逐适配器持久化原生 session ID、工具版本/二进制指纹、模型/推理配置和 workspace revision；在会话存在、工具版本兼容、目录身份一致且审批状态可重建时执行恢复探测。未通过真实协议验收的适配器继续报告 `resume/fork=false`。
5. 分叉历史与 Teams 失败节点重试应各有独立数据合同，不能借用本次新草稿副本冒充会话恢复或团队 attempt 重试。
