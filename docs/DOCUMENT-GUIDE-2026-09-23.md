# 文档指南：每份文档是什么、什么时候读

日期：2026-09-23。本指南覆盖 `docs/` 全部文档与根目录关键文件。阅读顺序建议：根 README → 本指南 → STAGE-REPORT → DEVELOPER_HANDOFF，其余按需检索。

## 一、入口与交接（新接手者必读）

| 文档 | 说明 |
| --- | --- |
| [根 README](../README.md) | 项目门面：Wonderland 是什么、0.11.0 版本概要、四个官方执行器、分发渠道（GitHub Releases / npm `next` 频道）。 |
| [docs/README](README.md) | 开发文档与仓库结构总览：代码模块位置表、0.11.0 本轮整理说明、版本特性摘要。 |
| [DOCUMENT-GUIDE（本文档）](DOCUMENT-GUIDE-2026-09-23.md) | 每份文档的用途与阅读时机。 |
| [STAGE-REPORT-2026-09-23](STAGE-REPORT-2026-09-23.md) | 最新阶段完成情况：H02 数据合同四项全部落地、H03 决策侧完成、验证证据与能力边界。**了解「现在做到哪了」先读这份。** |
| [DEVELOPER_HANDOFF](DEVELOPER_HANDOFF.md) | **权威开发交接指南**（50KB）：三条执行路径语义、构建/启动/数据迁移、HTTP 与双 CLI 合同、官方工具账号前提、H01–H10 任务分解与验收门槛、发布流程、故障定位表。任何改动前必读。 |
| [ROADMAP-2026-09-23](ROADMAP-2026-09-23.md) | 未完成工作的详细推进路线：每项的现状、前置依赖、文件级实施步骤与验收标准。 |
| [FINAL_HANDOFF-2026-09-22](FINAL_HANDOFF-2026-09-22.md) | 2026-09-22 工作区交接快照：可交付副本、候选改动隔离清单、源码获取回退命令。历史归档用途；工作区已迁移至 `F:\everyAI\all`，隔离审查结论仍有效。 |

## 二、长期规划（目标蓝图，非完成清单）

| 文档 | 说明 |
| --- | --- |
| [MULTI_MODEL_ORCHESTRATION_PLAN](MULTI_MODEL_ORCHESTRATION_PLAN.md) | 多模型自主协作平台总方案：产品目标、架构分层、多厂商协作与计费的长期设计。 |
| [DESKTOP_PRODUCT_PLAN](DESKTOP_PRODUCT_PLAN.md) | 桌面产品建设方案：统一待处理中心、成果导航、插件/定时/远程/PR/网站面板的产品合同（对应 H06/H07）。 |

## 三、历史进度与逐文件目录（按注明基线阅读）

| 文档 | 说明 |
| --- | --- |
| [PROJECT_PROGRESS-2026-09-21](PROJECT_PROGRESS-2026-09-21.md) | 截至 0.11.0 的建设步骤、每步证据（含 SQLite v1→v2 迁移实测、Kimi 官方订阅 Teams 实测）。 |
| [FILE_CATALOG-2026-09-21](FILE_CATALOG-2026-09-21.md) | 逐文件职责目录：每个源码文件干什么、不属于什么。改代码前查「这个模块的边界」用它。 |

## 四、版本发布与验证记录（历史快照，按对应版本理解）

| 文档 | 说明 |
| --- | --- |
| [RELEASE-0.6.0 … 0.11.0](RELEASE-0.11.0.md)（共 6 份） | 各版本发布说明：新功能、迁移注意（尤其 0.11.0 的 SQLite v2）、已知限制。 |
| [VALIDATION-0.6.0 … 0.11.0](VALIDATION-0.11.0.md)（共 6 份） | 各版本验证记录：测试矩阵、真实模型实测边界（0.9.0 有 Kimi 官方订阅 Teams 实测；0.10.0 有 Claude/DeepSeek 适配验证）。 |
| [DELIVERY-0.11.0](DELIVERY-0.11.0.md) | 0.11.0 发布收尾与交付报告：GitHub Release 与 npm 远端核验结果、产物校验和。 |

## 五、适配器协议参考（改 native_executor 前读）

| 文档 | 说明 |
| --- | --- |
| [CLAUDE_NATIVE_ADAPTER](CLAUDE_NATIVE_ADAPTER.md) | Claude Code 适配协议：严格版本锁定（2.1.193）、双向 stream-json、权限与设置校验边界。 |
| [DEEPSEEK_NATIVE_ADAPTER](DEEPSEEK_NATIVE_ADAPTER.md) | DeepSeek Harness ACP 适配：dsh 0.1.6-alpha.2、隔离 profile、无订阅 OAuth 的声明边界。 |
| [CODEX_HOST_REFERENCE](CODEX_HOST_REFERENCE.md) | CodexHost 技术参考：app-server 协议与身份校验的采用依据。 |

## 六、工作报告序列（按时间倒序，每份对应一个可评审增量）

| 文档 | 说明 |
| --- | --- |
| [WORK-REPORT-2026-09-23-IDENTITY-SEED-V1.1](WORK-REPORT-2026-09-23-IDENTITY-SEED-V1.1.md) | 身份种子 v1.1：59 报价模型 × 59 榜单行核验结论、deepseek-v4-pro attestation 依据。 |
| [WORK-REPORT-2026-09-23-ROUTING-IDENTITY](WORK-REPORT-2026-09-23-ROUTING-IDENTITY.md) | routing-v2：决策消费身份注册表的语义与测试。 |
| [WORK-REPORT-2026-09-23-ACCOUNT-BILLING](WORK-REPORT-2026-09-23-ACCOUNT-BILLING.md) | 账号计费合同：渠道语义、显式 unknown、dispatch blockers。 |
| [WORK-REPORT-2026-09-22-PRICE-PARSERS](WORK-REPORT-2026-09-22-PRICE-PARSERS.md) | 五源价格解析：Anthropic 双源 join、DeepSeek 峰谷、真实在线验证证据与 fixture 位置。 |
| [WORK-REPORT-2026-09-22](WORK-REPORT-2026-09-22.md) | Teams 路由约束持久化与决策重放（routing 预览三接口）。 |
| [WORK-REPORT-2026-09-22-UPSTREAM-ACQUISITION](WORK-REPORT-2026-09-22-UPSTREAM-ACQUISITION.md) | GitHub 源码归档回退脚本（codeload 不可变 SHA 下载）。 |
| [WORK-REPORT-2026-09-22-FINAL-HANDOFF](WORK-REPORT-2026-09-22-FINAL-HANDOFF.md) | 9-22 交接整理的当日总结（并入 FINAL_HANDOFF 阅读）。 |

## 七、根目录其他关键文件

| 文件 | 说明 |
| --- | --- |
| [REFERENCES.md](../REFERENCES.md) | 采用技术与上游项目归属、许可记录。 |
| [CONTRIBUTORS.md](../CONTRIBUTORS.md) | 贡献者记录。 |
| [assets/model-identity/v1.json](../assets/model-identity/v1.json) | 模型身份注册表种子（编译期内嵌）：每条记录附证据，升级须改代码评审。 |
| `docs/validation/`、`docs/images/` | 验证数据与桌面截图素材。 |
| `F:\everyAI\all\pricing-fixtures-20260922\`（仓库外） | 五份官方定价原文 + SHA-256，复验解析器时设为 `WONDERLAND_PRICING_FIXTURE_DIR`。 |

## 八、阅读原则

1. **发布状态以 RELEASE/DELIVERY 为准，进度以 STAGE-REPORT 为准，计划以 ROADMAP 为准**；规划文档（第二节）描述目标而非现状。
2. 历史文档中的测试数字、远端状态是当时快照，不自动等同当前构建。
3. 工作报告按「一个可评审增量一份」追加；新增报告后在本指南登记。
