# 开发文档与仓库结构

当前版本为 [0.11.0](RELEASE-0.11.0.md)，包含 Work/Chat 推理配置持久化和完整开发交接资料。分发与验证状态见 [0.11.0 验证记录](VALIDATION-0.11.0.md)。npm 已发布，预览频道 `next=0.11.0`，稳定频道 `latest` 保持 0.7.0；历史版本文档中的限制按对应版本理解。

**最终发布与远端核验结果见 [0.11.0 交付报告](DELIVERY-0.11.0.md)。** 安装包构建、GitHub 发布与 npm registry 发布已分别核对，包含实际下载与安装检查。

## 代码位置

| 目录/模块 | 职责 |
| --- | --- |
| src/workflow.rs、workbench_service.rs | 单任务 SQLite 状态、配置与执行控制 |
| src/team_*.rs | 团队 DAG、官方执行器绑定、独立工作区和验收 |
| src/native_executor.rs、native_executor/ | 8 个受管执行器门面，以及 Codex/Kimi/Claude/ACP 方言原生协议 |
| src/model_intelligence.rs、pricing.rs、routing.rs | LiveBench、官方价格证据与快照绑定的可解释路由预览 |
| src/bin/desktop/ | Rust egui 工作台、Teams、项目与终端界面 |
| src/bin/wonderland-cli.rs | 原生 Rust 服务客户端 |
| packaging/npm/wonderland-cli/ | npm 服务客户端及测试 |
| packaging/windows/ | 安装包和便携包构建 |
| anytool/ | 固定提交的上游子模块与 Claude 来源快照，详见目录 README |
| 根目录的官方工具检出 | 本地只读参考，不参与主程序构建，也不自动打包 |

主程序的适配器独立维护，上游目录不承担 Wonderland 功能修改。根目录的 MiMo-Code、minimax-code、cli 与 anytool 中记录的版本一致，原文件保留并加入忽略规则；qwen-code 同样保留为本地参考。用户的 RESEARCH_PAPER_FRAMEWORK.md 保持原状，未作为代码提交。不要通过整体 git add 将研究材料、凭据或本地参考仓库混入发布。

## 本轮整理

核对时，GitHub main 的两个上游目录提交（22ad26f、eb3c12b）与开发分支的 0.8–0.10 功能提交（04abea3、513505c、61819b1）分开。已在开发分支合并 main 的目录记录，保留全部功能与上游固定提交；同时修正文档中仍指向旧 npm 包的命令。main 在统一检查通过后接收这些已测试内容。

## 当前源码的新功能

Work/Chat 支持创建时保存推理档位，任务详情显示请求值，复制任务保留选择。重启后启动任务只读取数据库内的配置；更改配置需要创建新任务。Teams 子任务采用同一保存和校验路径。两个 CLI 只在 work create 接受 --reasoning，start API 也拒绝携带配置覆盖。

SQLite 工作流数据库从 v1 事务迁移到 v2；旧记录保持 reasoning_effort=null，原始模型、历史和状态不变。旧 0.10.0 程序会拒绝读取 v2 数据库，因此调试新源码应使用独立 AGENT_DATA_DIR；若要回退，使用升级前的完整数据备份。应用并不自动降级数据库。

推理选项来自所连接服务的能力报告，不是每个模型实际可用档位的保证。Kimi 当前没有这项受管配置，不能给它填入通用的 high。指定 None/省略表示官方默认，off、none、minimal 等原生值不会互相翻译。

## 本轮验证

Windows 全目标回归通过：404 项库测试、3 项 Rust CLI、18 项桌面、7 项协议集成，共 432 项，外部条件测试 7 项默认忽略。npm 14 项测试通过。新增测试覆盖真实 v1 数据库迁移与重开、旧 JSON、非法档位、跨提供商拒绝、Teams 子任务持久化和归属检查，以及服务能力变更。

另从已发布版本的独立测试数据库创建副本，启动新版后端：Rust CLI 创建 Claude high 只读草稿，npm CLI 创建 DeepSeek off 草稿，旧客户端省略档位得到 null。重启后选择保持不变，5 个既有成功任务的模型、状态和输出保持一致；启动时覆盖配置、Kimi 不支持的 high 都被拒绝。测试未调用模型、未更改原数据库。界面在常规与 940×620 窗口检查过。

## 文档入口

- **总项目任务报告：[PROJECT-TASK-REPORT](PROJECT-TASK-REPORT-2026-09-23.md)**——项目级四段汇总（完成的工作/各文件说明/需要的测试/未完成任务与思路）。
- 当前进度明细：[2026-09-23 阶段工作报告](STAGE-REPORT-2026-09-23.md)。H02 数据合同四项全部落地（身份注册表、五源官方报价、账号计费合同、epoch 绑定），H03 决策侧完成（routing-v2 消费身份注册表）；含验证证据与能力边界。
- **每份文档的用途与阅读时机：[文档指南](DOCUMENT-GUIDE-2026-09-23.md)**。
- **未完成工作的详细推进路线：[后续路线图](ROADMAP-2026-09-23.md)**。H01–H10 的现状、前置依赖、文件级步骤与验收标准。
- **交接先读：[开发者交接指南](DEVELOPER_HANDOFF.md)**。面向未参与此前对话的开发者，包含独立启动、构建测试、数据库备份/回退、两个 CLI 的语法区别、状态与权限约束，以及 H01–H10 后续任务及验收要求。
- **本机交付先读：[2026-09-22 最终交接包](FINAL_HANDOFF-2026-09-22.md)**。指定可交付源码副本，隔离两个含未提交候选改动的相邻副本，并提供无网络依赖的提交移植步骤。
- [2026-09-21 项目进度、已完成编写步骤与后续计划](PROJECT_PROGRESS-2026-09-21.md)、[130 个主项目文件职责目录](FILE_CATALOG-2026-09-21.md)：以 main 提交 `6383c9a` 为基线，区分已实现、真实验证和已发布，列明自动调度与产品功能的剩余工作。
- [多模型协作建设方案](MULTI_MODEL_ORCHESTRATION_PLAN.md)、[桌面产品方案](DESKTOP_PRODUCT_PLAN.md)：长期目标与验收设计，不代表全部实现。
- [CodexHost 技术参考](CODEX_HOST_REFERENCE.md)：来源、采用方式与能力边界。
- [Claude 适配](CLAUDE_NATIVE_ADAPTER.md)、[DeepSeek 适配](DEEPSEEK_NATIVE_ADAPTER.md)：支持版本与协议证据。
- [0.11.0 验证](VALIDATION-0.11.0.md)、[0.10.0 官方程序验证](VALIDATION-0.10.0.md)、[0.9.0 Kimi 实测](VALIDATION-0.9.0.md)：不同层次的测试和真实调用记录。
- [2026-09-22 本轮工作报告](WORK-REPORT-2026-09-22.md)：路由约束持久化、在线预览与决策重放的完成项和剩余项。
- [2026-09-22 价格解析工作报告](WORK-REPORT-2026-09-22-PRICE-PARSERS.md)、[2026-09-23 账号计费合同](WORK-REPORT-2026-09-23-ACCOUNT-BILLING.md)、[2026-09-23 路由身份消费](WORK-REPORT-2026-09-23-ROUTING-IDENTITY.md)、[2026-09-23 身份种子 v1.1](WORK-REPORT-2026-09-23-IDENTITY-SEED-V1.1.md)：本轮四个增量的逐项报告。
- [2026-09-23 ACP 框架/MiMo/Automatic 启动](WORK-REPORT-2026-09-23-MIMO-ACP-DISPATCH.md)：ACP 会话引擎泛化、MiMo 受管接入、Automatic 决策→绑定写回→执行路径。
- [anytool 上游工具受管接入评估](ANYTOOL-ADAPTER-ASSESSMENT-2026-09-23.md)：各工具协议入口结论与后续接入路径。
- [2026-09-23 kimi-code/minimax 接入](WORK-REPORT-2026-09-23-KIMI-CODE-MINIMAX.md)：受管执行器扩至 7 个；含待做测试清单与剩余任务思路。
- [2026-09-23 Grok Build ACP 接入](WORK-REPORT-2026-09-23-GROK-DIALECT.md)：受管执行器扩至 8 个；完成非交互认证、模型/档位回读、权限、usage 与离线完整生命周期，真实账号握手待做。
- [2026-09-22 源码获取工作报告](WORK-REPORT-2026-09-22-UPSTREAM-ACQUISITION.md)：Git Smart HTTP 不可达时的官方归档回退、验证结果与未解决网络限制。
- [2026-09-22 最终交接整理报告](WORK-REPORT-2026-09-22-FINAL-HANDOFF.md)：文件分类、完成项、未完成项和交付前检查。

待完成的重点仍包括自动质量/成本选模、可执行预算限制、更多原生适配器、恢复/分叉、插件、定时任务、远程主机及 PR/网站面板。源码目录收录某工具不代表它已支持受管协作。
