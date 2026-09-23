# 工作报告：H05 终态任务复制加固

日期：2026-09-23
范围：在既有“终态独立任务复制为全新 Draft”合同上补齐状态边界、请求边界、路径编码和桌面实际操作。关联总览见 [总项目任务报告](PROJECT-TASK-REPORT-2026-09-23.md)。

## 一、已完成的工作

1. **终态边界改为由状态机统一判断。** `WorkflowStatus::is_terminal()` 现在作为复制许可的唯一条件。`Succeeded`、`Failed`、`Cancelled` 与 `Interrupted` 可以复制；`Draft`、`Running`、`WaitingInput`、`Verifying` 与 `Blocked` 均会被拒绝，避免把仍在等待、验证或受阻的任务误当作可重试任务。
2. **复制 API 限定为空对象请求。** `POST /api/v1/workflows/{id}/duplicate` 接受无 body 或 JSON `{}`；带字段的对象、数组和其他 JSON 形状均返回错误。复制始终使用来源任务已保存的配置，调用方不能借此覆盖标题、模型、目录或权限设置。
3. **Rust CLI 的 workflow 路径逐段编码。** `work get`、`duplicate`、`events`、`start`、`cancel`、`approve`、`answer` 与 `accept` 统一经过单段 URL 编码，并拒绝空 ID、`.` 和 `..`。任务 ID 中的 `/`、空格和查询符号不会改变请求路径。
4. **桌面端执行真实的复制操作。** Studio 详情页只对终态任务显示“复制为新草稿”；点击后向 duplicate API 发送 `{}`，而不是把旧任务字段填回编辑器。按钮会遵守正在提交时的禁用状态。
5. **保留原有审计语义。** 成功复制在 SQLite 事务内创建独立 Draft 和 `duplicated_from` 事件；来源原生 session、输出和错误不进入新任务，且新任务不会自动 start。

## 二、各文件的说明

| 文件 | 作用 |
| --- | --- |
| `src/workflow.rs` | 以 `is_terminal()` 约束复制来源状态；测试每个允许终态和每个拒绝的非终态。 |
| `src/workbench_service.rs` | 对 duplicate HTTP body 做空对象校验，防止请求覆盖来源任务配置。 |
| `src/bin/wonderland-cli.rs` | 集中实现 workflow ID 的单段 URL 编码和非法段拒绝，供全部 workflow 子命令使用。 |
| `src/bin/desktop/studio.rs` | 将终态任务的复制按钮接到真实 duplicate API；增加桌面端路径编码单测。 |
| `docs/WORK-REPORT-2026-09-23-H05-HARDENING.md` | 本次加固的完成项、验证状态和后续验收方法。 |

## 三、已执行验证

| 命令或场景 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过。 |
| `cargo test --locked --all-targets --features ui-snapshots` | 通过：库 466 项通过、8 项按外部条件忽略；Rust CLI 6/6、桌面 20/20、协议集成 7/7。覆盖四个允许终态、五个拒绝终态、空对象 body、Rust/桌面路径编码。 |
| `npm test --prefix packaging/npm/wonderland-cli` | 16/16 通过。 |
| 离线 HTTP 端到端复测 | `AGENT_PROVIDER=offline` 下创建 `codex/gpt-test` Draft、取消、再执行 `{}` duplicate；生成独立 Draft 和 `created`/`duplicated_from` 事件。带覆盖字段的 duplicate 返回 HTTP 409。没有登录或模型调用。 |

## 四、尚未完成的任务与完成方式

| 任务 | 完成方式 |
| --- | --- |
| H05 原生会话恢复与分叉 | 每个适配器先持久化可验证的 session ID、工具版本和二进制指纹、实际模型/推理配置与 workspace revision；仅在工具协议明确支持且离线 fixture 验证通过时再开放 `resume` 或 `fork`。目前能力继续为 `false`。 |
| 故障恢复验收 | 注入崩溃、断网、等待审批和残留进程，检查旧任务只进入可解释的终态，用户可在检查副作用后显式复制，不发生隐式恢复或重跑。 |
| Teams 失败节点重试 | 另建由父团队控制的 attempt 数据合同和审计事件，不能借用 standalone workflow 的 duplicate 路径绕过团队预算、依赖和验收规则。 |
| 真实账号验证 | 安装包交付后由账号持有人在各官方工具中完成登录、取消和失败恢复试验；该步骤用于核对真实原生会话能力，不是本次复制 API 的前置条件。 |
