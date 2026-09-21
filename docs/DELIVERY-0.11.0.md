# 0.11.0 发布收尾与项目交付报告

日期：2026-09-21。接手者先阅读 [开发者交接指南](DEVELOPER_HANDOFF.md)，再看 [逐文件职责](FILE_CATALOG-2026-09-21.md)和 [历史完成步骤与后续规划](PROJECT_PROGRESS-2026-09-21.md)。本报告记录这次“把未上传工作收尾并移交”的结果，不把长期计划写成已经交付。

## 1. 本次收尾范围

开始时，功能代码已经通过 PR #1 合入 main，本地和远端同为 `6383c9a`；不存在遗漏未推送的功能提交。尚未上传的是两份详细报告及文档入口，尚未分发的是已经进入源码的 0.11.0 配置持久化改动。

本轮完成的工程准备：

1. 整理并上传项目进度报告与 130 个主项目文件的职责目录。
2. 新增开发者交接指南，包含独立运行、两种 CLI 的真实语法、数据库迁移、官方工具版本、状态/权限合同、排错和 H01–H10 待办。
3. 更新仓库首页、文档索引及 npm 说明，使 0.11.0 的使用方式与限制一致。
4. 构建不带截图功能的 release 程序，生成 Windows 安装器、便携包、npm tarball 和 SHA-256 清单。
5. 验证真实 v1 数据库副本迁移、打包 npm 的请求行为、升级安装及安装后服务/CLI/桌面启动。
6. 最终包纳入完整交接说明，检查二进制、压缩包内容、许可证和校验值。

没有在本次发布中扩大模型功能范围；已发布能力不能被解释成完成了全部产品愿景。

## 2. 源码与分发状态

| 对象 | 状态与位置 |
| --- | --- |
| 功能基线 | `6383c9a01df9612dce66dcd957f45b7634649a23`，已包含 Work/Chat/Teams 的配置持久化 |
| 交接及发布准备提交 | `ef6e89595ed09186c65532e7ea3eaf769e095e1e`，已推送 GitHub main；新增/更新 9 个文档和证据文件 |
| 对应 CI | [35613295180](https://github.com/Alice-Marx/Rust_for_AI-agent/actions/runs/35613295180)，success |
| GitHub 0.11.0 | [v0.11.0 预览版](https://github.com/Alice-Marx/Rust_for_AI-agent/releases/tag/v0.11.0) 已公开发布，2026-09-21 23:16:11 北京时间；四个附件均 uploaded，远端 SHA-256 与本地一致 |
| 发布 tag | `v0.11.0` 精确指向 `ef6e89595ed09186c65532e7ea3eaf769e095e1e`，已从 GitHub git ref API 核对 |
| npm registry | 0.11.0 尚未发布；两次 web auth 会话均返回 404 失效，尚缺账号持有者完成本次二次验证。最后核对 `next=0.10.0`、`latest=0.7.0` |
| 本机安装 | 0.11.0 当前用户安装成功，路径 `%LOCALAPPDATA%/Programs/Wonderland` |

本报告写于产物构建之后，因此不包含在已经构建的 0.11.0 安装包中；最新发布结果由 GitHub main 上本报告提供。包内包含开发者交接指南、历史进度、文件目录和 0.11.0 验证说明。main 随后的交付结果提交仅更新文档，程序源码与发布 tag 相同，不移动既有 tag。

npm registry 的登录仍有效（已通过 `npm whoami`），但登录不替代每次发布的二次验证。没有绕过认证或把旧版本的授权复用到本次发布。接手者应在账号持有者在线时重新执行 `npm publish ./dist/rust-ai-wonderland-cli-0.11.0.tgz --tag next --access public --auth-type=web`，完成新生成页面的验证后，核对 registry 的版本、SHA-1 和 dist-tags，再更新本报告。这里不保留已经失效的授权链接。

CLI 现在可直接安装 GitHub 上同一份已校验 tarball，无须等 npm registry：

```powershell
npm install -g https://github.com/Alice-Marx/Rust_for_AI-agent/releases/download/v0.11.0/rust-ai-wonderland-cli-0.11.0.tgz
wonderland-cli --version
```

该方式使用 GitHub 附件，不表示 npm registry 已发布 0.11.0。后端仍需对应桌面安装包或源码构建。

## 3. 0.11.0 的实际功能变化

以前 Work/Chat 的单任务配置没有持久保存推理档位。本版从创建、复制、存储、重启、服务启动一直到官方 `NativeRequest` 使用同一 `reasoning_effort`；Teams 子任务也采用相同保存和归属校验路径。

桌面从所连接服务读取能力，切换应用清空不相容的选择；Rust/npm CLI 都在创建时提交档位。启动不能临时覆盖模型或档位。旧记录的未指定档位保持 `null`，不伪造默认值；Kimi 当前没有受管推理档位，不能填写通用 `high`。显示的请求档位不自动等于官方程序的有效档位，尤其 Codex 的回读仍待完善。

工作流数据库从 schema v1 升为 v2。首次启动前应停止服务并备份完整数据目录，回退使用升级前备份；0.10.0 不能直接读取 v2。具体操作与风险见 [发布说明](RELEASE-0.11.0.md)和交接指南第 5 节。

## 4. 验证结果与边界

| 验证 | 结果 |
| --- | --- |
| Rust 功能回归及 main CI | 432 项通过；7 项外部条件测试默认忽略。此次功能源码未改变；发布准备提交的 CI 再次通过 |
| npm 回归 | 本轮 14 项通过 |
| release 构建 | 三个 Windows 程序构建成功；CLI 版本 0.11.0 |
| 迁移 | 从真实 v1 数据库只读备份副本，迁移成功；原有 5 个成功任务保持；原数据库 hash 未变 |
| 配置端到端 | release Rust CLI 的 Claude high、打包 npm CLI 的 DeepSeek off、旧客户端 null 均保持；重启保持；非法档位/启动覆盖被拒绝 |
| 安装 | 初次升级和最终交接文档版安装器均返回 0；最终二进制和交接/验证文档与 staging 一致 |
| 安装后启动 | 隔离空数据、offline API provider、临时端口下后端/CLI 健康正常；四个受管执行器能力正确；桌面启动后观察 3 秒存活 |
| 最终文件清单 | ZIP 45 个文件、npm 5 个文件；逐文件与 staging 比较；三个二进制与已测试 release 相同 |
| GitHub CLI 附件复核 | 重新下载 npm tgz 与 SHA256SUMS，散列与本地一致；解包执行客户端报告 0.11.0 |
| 模型调用 | 本轮 0 次；未消耗账号进行新推理测试 |

桌面启动检查不是完整交互体验测试；Windows 安装成功不是 Linux/macOS 安装成功。既有 Kimi 订阅 Teams 真实项目 12 项测试仍见 [0.9.0 记录](VALIDATION-0.9.0.md)；Claude 的合成响应和 DeepSeek 的无密钥握手仍不算真实云端推理。

## 5. 交付物校验值

| 文件 | SHA-256 |
| --- | --- |
| `Wonderland-Setup-0.11.0-x64.exe` | `57d2d2e71dfc1223a255ee61bf14fca3b760170bb0bc0d7ec841e2590fef198b` |
| `Wonderland-0.11.0-windows-x64.zip` | `89a2203683b0a16e85bb09098771bcca2dad1080654ea94fd0add24e2489a70b` |
| `rust-ai-wonderland-cli-0.11.0.tgz` | `b5a3b5e38e7d226b27a5d3c961fd7bce581d314d373c1486dcd203f2828eda04` |

npm tarball 的 SHA-1 为 `2fd9e6ba84634179ead402f330c82dfef55dac73`。安装包未代码签名。二进制散列、迁移摘要与具体测试说明见 [VALIDATION-0.11.0.md](VALIDATION-0.11.0.md)。

## 6. 交给下一位开发者的阅读顺序

| 文档 | 目的 |
| --- | --- |
| [DEVELOPER_HANDOFF.md](DEVELOPER_HANDOFF.md) | 先按独立目录/端口启动，了解接口、状态和迁移；是实际接手入口 |
| [FILE_CATALOG-2026-09-21.md](FILE_CATALOG-2026-09-21.md) | 确定每个文件的职责，避免把早期示例模块当成新协作系统 |
| [PROJECT_PROGRESS-2026-09-21.md](PROJECT_PROGRESS-2026-09-21.md) | 了解九个已完成阶段、需求完成矩阵与后续实施顺序；发布状态为收尾前快照 |
| [MULTI_MODEL_ORCHESTRATION_PLAN.md](MULTI_MODEL_ORCHESTRATION_PLAN.md) | 长期协作、官方工具绑定、榜单/价格、预算及研究实验设计 |
| [DESKTOP_PRODUCT_PLAN.md](DESKTOP_PRODUCT_PLAN.md) | 桌面功能范围、应用中心、插件/定时/远程/PR/网站的产品验收 |
| [CODEX_HOST_REFERENCE.md](CODEX_HOST_REFERENCE.md) | 参考来源、采用的分层与仍缺的能力 |
| [RELEASE-0.11.0.md](RELEASE-0.11.0.md) / [VALIDATION-0.11.0.md](VALIDATION-0.11.0.md) | 本版用户变化、兼容性、构建安装与测试边界 |

## 7. 后续任务与建议分工

以下均为**未完成工作**；具体按文件拆分的步骤和验收见交接指南 H01–H10。

| 顺序/方向 | 主要工作 | 接手文件 | 必须交付的证据 |
| --- | --- | --- | --- |
| 官方执行器 | 精确模型/effort/账号能力、Codex 有效值回读、真实 Claude/DeepSeek/Codex 任务和双厂商协作 | `native_executor.rs`、`native_executor/`、`app_diagnostics.rs` | 工具/模型/版本、实际事件、独立示例和保护测试的验收 |
| 价格与身份 | 价格解析覆盖、模型 ↔ 榜单 ↔ 计费项映射、订阅额度与实际 usage | `pricing.rs`、`model_intelligence.rs`、适配器 | 每条候选证据来源/时间/条件，unknown 不当零成本 |
| 自动派工 | 在线刷新 epoch、候选过滤、质量约束选择、返修/升级和决策理由 | `team_service.rs`、`team_store.rs` | Automatic 真正完成任务；证据缺失仍阻塞 |
| 预算 | 账本预留与实际执行/结算连接，处理并发、取消和在途费用 | `team_store.rs`、`team_service.rs`、usage 合同 | 实际可兑现的上限或明确软限制，不先移除 Blocked |
| 恢复与交付 | 原生 resume/fork、中断、成果审阅/应用、分支/PR | Workflow、Teams、native、Git 工作区模块 | 不重放编辑、不复用审批、被测试 revision 可追溯 |
| 桌面 | 表单化 Teams、统一待处理、应用模型发现、体验打磨；再做插件/定时/远程/PR/网站 | `src/bin/desktop/` 及对应后端 | 真正完整流程、失败/取消/权限状态和实际操作验证 |
| 安全兼容 | Unix 隔离/资源限制、MCP 缺项、更多官方工具版本与语言项目 | sandbox、MCP、process、bridge 模块 | 系统级边界与协议回归，缺能力明确拒绝 |
| 研究 | 官方工具基线、固定/Assigned/Automatic 对照、多任务集、全成本和重复实验 | 新实验数据与 Teams/定价接口 | 可复核统计结果；没有实验不声称等效效率或节省比例 |

## 8. 保留在本机而不公开的内容

用户原有 `RESEARCH_PAPER_FRAMEWORK.md` 保持未跟踪，没有自动公开到 GitHub；它是研究材料，不是本轮编写的程序成果。若团队需要全文，应由项目所有者决定共享范围。

账号配置、OAuth token、正式数据、模型会话、独立测试数据库与本机日志不上传。`dist/` 的二进制通过 Release 附件分发，不加入源码 Git。上游参考检出按原有忽略规则保留；16 个子模块和既有 Claude 快照不被此次交接改写。

接手者应使用自己的账号授权和独立数据环境。仓库内文档与可运行测试足以理解当前实现；不需要读取本机账号文件来进行初始开发。

## 9. 之后每次开发应留下的说明

每个功能交付都记录：需求与最终行为、主要修改文件、API/数据/CLI 兼容性、测试命令与真实结果、官方工具版本与真实/模拟调用区别、已知限制、剩余任务和下一步验收。发布时额外记录源码提交、CI、tag、安装验证、产物 hash 与 npm 标签。

项目尚处于预览阶段。已有官方工具执行、持久任务、隔离集成和独立验收基础；接手重点是完成可证明的模型选择、账号计费和质量/成本实验，再扩展成熟产品能力。
