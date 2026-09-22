# 工作报告：路由决策消费模型身份注册表（H03 第一步）

日期：2026-09-23
基线：`e151314`（`handoff-development`，含五源价格解析与账号计费合同）
改动：`src/routing.rs`、`src/model_identity.rs`、`src/team_service.rs`

## 本轮交付

对应 H03 中「routing-epoch consumption of the mapping」——路由决策正式消费 H02 的模型身份注册表：

1. **榜单行不再由调用方自行声明**。`routing::decide` 新增第四个参数 `&ModelIdentityRegistry`；每个候选先经 `resolve(app_id, model, reasoning_effort, benchmark_snapshot)` 解析身份：
   - `attested` → 用 attestation 的 LiveBench 条目作为评分行；若调用方声明的 `benchmark_model` 与之不一致，拒绝该候选（防止声明错误的行）；
   - `unknown`（未映射、推理变体未映射、核验不在榜、attestation 条目不在当前快照）→ 拒绝该候选并在 reasons 中携带注册表给出的原因。
2. **决策升级为 `routing-v2`**：`RoutingDecision` 新增 `identity_mapping_version` 字段（`#[serde(default)]` 兼容旧持久化事件的反序列化），与 `benchmark_snapshot_id:price_snapshot_id` epoch 一起构成完整证据链。旧 `routing_preview` 事件按原样重放，不受影响。
3. **单一事实源顺带统一**：候选的订阅渠道排除文案改由 `account_billing::non_api_block_reason` 生成。
4. `model_identity.rs` 新增 `mapping_version()` 访问器与仅测试可见的 `from_json` 构造器（供下游模块用合成记录测试身份消费行为）；`team_service.rs` 的预览端点加载内嵌注册表并传入。

## 验证结果

- routing 套件 9 项全过，新增 4 项：未映射模型只阻塞该候选、核验不在榜的模型绝不继承分数、声明榜单行与 attestation 不一致被拒、attestation 条目过期（不在当前快照）阻塞该候选。
- 既有测试（缺类别分数只阻塞单候选、订阅不继承 API 报价、歧义价格阶梯、预算校验）在 v2 语义下全部保持通过；另断言决策携带 `policy_version="routing-v2"` 与 `identity_mapping_version`。
- 全目标回归（含 `ui-snapshots`）：426 库 + 3 Rust CLI + 18 桌面 + 7 协议通过；5 个失败仍是已基线复现的 desktop_bridge/desktop_terminal 本机环境问题。`cargo fmt --all --check` 通过。

## 边界与未完成

1. **Automatic 仍未正式派工**：本片只把身份证据接入决策；账号/订阅身份核验（计费合同全部 unknown）、决策后原子预算预占、真实跨厂商任务证据都未开启，`auto_dispatch_ready` 保持 false。
2. v2 决策下，注册表 v1 种子只 attestation 了两个 Kimi 模型；其余任何候选（包括 OpenAI/Claude/DeepSeek）都会因身份未 attestation 被拒——这是设计行为，扩充种子需逐条附证据（LiveBench 行存在性 + 官方定价文档核对）。
3. `RoutingPolicy::into_request` 仍以 `binding.model` 作为声明榜单行；对 attestation 条目名与模型名不同的记录，需后续在 Team 创建时校验声明（当前在决策时校验并拒绝）。
