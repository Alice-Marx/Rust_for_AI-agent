# Wonderland 逐文件职责目录

对应 [项目进度与实施步骤报告](PROJECT_PROGRESS-2026-09-21.md)。核对基线：`6383c9a01df9612dce66dcd957f45b7634649a23`，2026-09-21。

根目录为 `E:/harness/codex/Rust_for_AI-agent`。下表路径均相对该根目录；文件链接可在仓库内直接跳转。第 1–10 节逐一说明基线的 **130 个主项目维护文件**，其中 **74 个 Rust 文件**，包括 `build.rs` 与 Rust 测试。第 11 节说明另外 16 个子模块及 6,702 个 Claude 快照文件的归属；第 12 节说明运行时和本地参考文件。

这里的“文件存在”不等于“功能完整或真实模型验证通过”。测试多数直接写在相应 `.rs` 文件的 `#[cfg(test)]` 模块中，因此不能只看 `tests/` 的文件数量判断测试覆盖。

## 1. 根目录、构建和仓库管理（11 个）

| 文件 | 作用 | 维护时注意 |
| --- | --- | --- |
| [.github/workflows/ci.yml](../.github/workflows/ci.yml) | GitHub Actions：Windows 全目标 Rust/格式/npm 检查，Linux/macOS 编译和选定模块回归 | Unix 没有运行与 Windows 完全相同的全量套件；不自动证明真实账号调用 |
| [.gitignore](../.gitignore) | 排除构建包、运行数据、日志、环境文件和本地只读参考仓库 | 新增发布/数据路径时同步维护，避免把凭据或大型构建物加入 Git |
| [.gitmodules](../.gitmodules) | 声明 16 个上游工具子模块的路径与仓库 URL | 固定提交由 Git gitlink 记录；修改来源不代表适配器支持了新工具 |
| [CONTRIBUTORS.md](../CONTRIBUTORS.md) | 项目贡献者名单 | 属于项目署名资料 |
| [Cargo.toml](../Cargo.toml) | Rust 包版本、依赖、平台依赖、功能开关和默认运行目标 | 当前 0.11.1；`ui-snapshots` 是开发截图能力，不应无意加入正式包 |
| [Cargo.lock](../Cargo.lock) | 锁定 Rust 依赖解析结果 | 与 `--locked` 配合保持构建可复现，升级依赖需重新验证 |
| [LICENSE](../LICENSE) | Wonderland 自有项目 MIT 许可 | 不替代上游工具、字体等各自许可证 |
| [README.md](../README.md) | 产品介绍、安装/运行、API/CLI、能力与版本入口 | 用户最先看到的说明，应区分源码版与已发布版 |
| [REFERENCES.md](../REFERENCES.md) | 技术参考来源与借鉴内容 | 参考/来源记录不等于源码均已整合或效率已经等同 |
| [anytool/README.md](../anytool/README.md) | 上游目录结构、仓库来源、固定版本获取和 Claude 快照说明 | 上游不参与 Wonderland 主程序编译，子模块需单独初始化 |
| [build.rs](../build.rs) | 构建时为 Windows 二进制嵌入图标和版本资源 | 平台条件编译避免 Unix 尝试使用 Windows 资源工具 |

## 2. 启动、公共契约与 API（7 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [src/lib.rs](../src/lib.rs) | 导出库模块、AgentRuntime 和请求/响应契约，供各 Rust 程序复用 | 是模块入口，不直接启动服务 |
| [src/main.rs](../src/main.rs) | 后端进程入口：加载连接、记忆、会话、索引、沙箱、MCP、工作台，启动 HTTP 并清理退出 | 非本地监听要求服务器令牌；也装配旧示例费用服务 |
| [src/api.rs](../src/api.rs) | Axum 路由和访问检查：Agent SSE、会话、记忆、模型、MCP、审批、代理账号；挂载 Workbench/Teams | 远程 token 与来源检查不等于完整远程控制产品 |
| [src/model.rs](../src/model.rs) | 旧 API Agent 的请求/响应、委派、Todo、记忆和沙箱数据类型 | 与 `workflow.rs` 的受管任务契约不同 |
| [src/observability.rs](../src/observability.rs) | 初始化 tracing 日志与 `RUST_LOG` 过滤 | 尚无完整遥测平台或集中日志检索 |
| [src/bin/wonderland-cli.rs](../src/bin/wonderland-cli.rs) | Rust 命令行客户端：聊天/流式/审批、会话、应用诊断、工作流、Teams、情报和价格 | 连接后端；与 npm 同名 CLI 的实现独立，需维持接口一致 |
| [src/bin/wonderland-desktop.rs](../src/bin/wonderland-desktop.rs) | egui 桌面入口、应用状态、字体/主题和后台请求通信，组合各页面 | 桌面状态不应替代服务端工作流事实状态 |

## 3. API Agent、模型协议与连接（13 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [src/agent.rs](../src/agent.rs) | 自有 API Agent 循环：模型调用、工具、hooks、权限、上下文压缩、会话/记忆、SSE 和基础评估 | 不等于各厂商官方 CLI 的原生 Agent 循环 |
| [src/agent_defs.rs](../src/agent_defs.rs) | 加载 `.wonderland/.claude/agents/*.md`，解析提示/白名单，创建简化 API 子 Agent | 共用 API provider，限制嵌套 Task/TodoWrite；不是 Teams 官方工具派工 |
| [src/provider.rs](../src/provider.rs) | 中立消息/工具/usage/流类型与 ModelProvider；Chat Completions/SSE、离线规则 provider、环境构建和共享订阅端点 | 这里的统一类型并不消除各厂商能力差异 |
| [src/responses.rs](../src/responses.rs) | OpenAI Responses 请求、SSE、工具结果、推理摘要/加密封装、缓存键及 usage | 协议实现不等于 Codex 整体效率验证 |
| [src/anthropic.rs](../src/anthropic.rs) | Anthropic Messages 请求与 SSE，工具、thinking/adaptive thinking、签名和缓存断点 | 是直连 API 协议，不是 `native_executor/claude.rs` 的官方进程适配 |
| [src/router.rs](../src/router.rs) | 按显式配置/模型档案选择 Responses、Messages 或 Chat Completions wire 协议 | **不是** LiveBench、成本、质量选模器 |
| [src/model_profile.rs](../src/model_profile.rs) | 静态模型族能力：上下文/输出限制、协议、推理参数、缓存和工具偏好，生成模型指引 | 不会自动用 LiveBench 更新成任务权重 |
| [src/vendors.rs](../src/vendors.rs) | 厂商 API 预设、别名、默认 URL/模型和环境变量约定 | 预设不证明用户账号可用 |
| [src/connection.rs](../src/connection.rs) | 连接设置读写/校验，构建或替换 provider，提供服务 HTTP 客户端 | 含私密字段，通过 credentials 持久化；原生 CLI 登录是另一条路径 |
| [src/credentials.rs](../src/credentials.rs) | 私密文件存储及临时文件替换；Windows 当前用户 DPAPI，Unix 受限目录/文件权限 | Unix 不是加密存储；不要输出凭据内容 |
| [src/subscription.rs](../src/subscription.rs) | CLIProxyAPI sidecar 定位/获取/校验、配置、本机访问密钥、健康检查和生命周期，封装登录管理 | 复用原版 sidecar，不是 Go 项目全量 Rust 重写 |
| [src/cliproxy.rs](../src/cliproxy.rs) | CLIProxyAPI HTTP 客户端：模型、账号、登录进度、验证及管理，兼容账号返回格式 | 功能端点对接不代表上游所有提供商都已实测 |
| [src/context.rs](../src/context.rs) | 收集平台/工作目录/Git、分层 AGENTS/CLAUDE 和限额 `@path` include，构建系统提示 | API 上下文组装；原生工具仍有自己的提示/上下文机制 |

## 4. 官方工具、工作流与团队（14 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [src/native_executor.rs](../src/native_executor.rs) | NativeCapabilities/Request/Event/Control 合同与绑定校验，Codex app-server、Kimi Wire 实现，分派 Claude/DeepSeek | 无通用 API 静默回退；当前 resume/fork 均关闭；Codex effort 请求值尚无完整有效值回读 |
| [src/native_executor/claude.rs](../src/native_executor/claude.rs) | 官方 Claude stream-json 进程、设置/模型/effort 核验、正文/思考去重、审批/问题、取消和清理 | 固定已验证官方 2.1.193 发行；只有离线/合成响应真实程序验证，没有真实模型推理 |
| [src/native_executor/deepseek.rs](../src/native_executor/deepseek.rs) | 官方 DeepSeek ACP 与临时 profile、发行指纹、会话/模型/effort、权限/工具生命周期和清理 | 官方 0.1.6-alpha.2；有无密钥握手，无真实 prompt；上下文占用不当作计费 token |
| [src/app_diagnostics.rs](../src/app_diagnostics.rs) | 有界并发/超时的 CLI 版本探测、入口识别、文件摘要和能力诊断 | 不读取账号凭据、不发模型请求；hash 不是发行商签名 |
| [src/workflow.rs](../src/workflow.rs) | SQLite 工作流/项目/一次性计划主存储：配置、状态、输出、原生会话 ID、有序事件、计划与触发审计；schema v3 | v2→v3 增加计划表；旧二进制不能降级读取，升级前备份完整数据目录 |
| [src/schedule_store.rs](../src/schedule_store.rs) | 一次性计划类型、状态、RFC 3339 规范化与模板大小校验；SQL 事务由 WorkflowStore 统一拥有 | 仅一站式 Draft；未实现周期、时区/DST、远程或通知 |
| [src/workbench_service.rs](../src/workbench_service.rs) | 独占数据目录的执行服务：建/启/停/验收任务、权限/提问转发、重叠目录保护，提供工作流/项目/应用/一次性计划接口 | 启动补触发只创建 Draft；服务重启标 Interrupted；Teams 子任务禁止独立绕过父级验收 |
| [src/team_store.rs](../src/team_store.rs) | Teams SQLite 事务：DAG、节点、attempt、绑定、事件、revision、原子 claim、验收和 microUSD 预留/结算 | 预算账本存在不代表实际官方账号花费已可约束 |
| [src/team_service.rs](../src/team_service.rs) | 团队规划、依赖/并发调度、固定/Assigned 绑定、重试、只读评审、集成、主机验收和成功判定 | Automatic 刷新证据后仍无条件 Blocked；带 USD 上限也 Blocked |
| [src/team_workspace.rs](../src/team_workspace.rs) | 创建独立 detached worktree、捕获改动、保护测试/写范围、提交/补丁散列与串行集成 | 要求干净 Git 根；成果在 integration 目录，不直接写回原始分支；不是 OS 沙箱 |
| [src/team_checks.rs](../src/team_checks.rs) | 以明确程序和 argv 运行用户声明的验收命令，记录退出码/耗时/有界输出，处理超时取消 | 命令在宿主机执行，测试覆盖决定可证明的质量范围 |
| [src/model_intelligence.rs](../src/model_intelligence.rs) | 在线核验两个 LiveBench main SHA、发现公布版本、解析分类/成绩，保存快照和来源证据 | 自动派工未就绪；刷新失败不把旧时间改成新时间；还存在旧 pricing 占位字段 |
| [src/pricing.rs](../src/pricing.rs) | 抓取四家官方 API 价格、解析已知格式和条件、保存来源/时间/hash/快照，按精确型号与渠道报价 | 覆盖有限；订阅额度、真实账单和未知别名不是 API 标价能证明的 |
| [src/process_tree.rs](../src/process_tree.rs) | Windows Job/Unix 进程组及终端 OS session 的生命周期跟踪与清理 | 防残留进程，不提供文件/网络权限隔离 |
| [src/process_tree_terminal_tests.rs](../src/process_tree_terminal_tests.rs) | PTY shell 和后台作业的真实生命周期回归，检查取消/关闭后无残留 | 被模块引用的测试文件，计入 Rust 测试，不是单独产品功能 |

## 5. 桌面工作区与界面（8 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [src/desktop_bridge.rs](../src/desktop_bridge.rs) | 官方应用目录、来源/checkout 检查、程序发现、配置与跨平台 argv，打开终端/目录/本地构建入口 | 应用有卡片不等于已安装、已登录或已有受管适配 |
| [src/desktop_terminal.rs](../src/desktop_terminal.rs) | PTY/ConPTY、vt100、终端读写线程、输入法/键鼠、滚动选择、尺寸/输出限额和关闭清理 | 以当前用户运行；不在 SandboxRun 隔离内 |
| [src/desktop_workspace.rs](../src/desktop_workspace.rs) | 文件树、文本读写、语言/清单识别、Git 状态与 diff；修订校验和原子保存 | 发现外部改动时拒绝覆盖；不是完整 LSP/调试 IDE |
| [src/bin/desktop/studio.rs](../src/bin/desktop/studio.rs) | Work/Chat 创建、列表/看板、详情/事件/审批、应用与安装诊断、项目页面，连接 Teams；终态复制调用真实 API | 任务由服务拥有；从服务读取能力；看板不能随意拖成成功 |
| [src/bin/desktop/teams.rs](../src/bin/desktop/teams.rs) | Teams 创建/详情、策略/执行器/effort、节点 JSON 和验收命令，展示依赖、attempt、改动/验证和价格；有路由事件/长 blocker snapshot 回归 | JSON 编辑仍较重；Automatic 控件存在不代表自动路由可执行；真实窗口操作另验收 |
| [src/bin/desktop/workbench.rs](../src/bin/desktop/workbench.rs) | 文件树/编辑器、Git diff、CLI 配置扫描、内嵌/外部终端、项目切换，处理未保存编辑和终端关闭 | 本地文件/终端面板与服务型 Studio 页面职责不同 |
| [src/bin/desktop/ui.rs](../src/bin/desktop/ui.rs) | 公共视觉组件、导航/欢迎页、对话/推理/工具内容、Markdown/代码复制、连接与账号设置、开发截图 | 截图 fixture 中展示数据不是真实模型执行记录 |
| [src/bin/desktop/icons.rs](../src/bin/desktop/icons.rs) | egui Painter 绘制的可缩放图标、导航按钮和品牌图形 | 由代码绘制，未依赖 AI 位图生成服务 |

## 6. 权限、MCP、沙箱和会话辅助（17 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [src/permissions.rs](../src/permissions.rs) | 工具 allow/ask/deny、模式和参数/路径匹配，兼容 settings，交互与非交互权限合同 | 属于策略判断，不是 OS 强制隔离 |
| [src/approval.rs](../src/approval.rs) | API Agent SSE 的一次性、有时限审批桥，发送请求、接收应答并撤销过期项 | 与 native 协议审批实现不同 |
| [src/hooks.rs](../src/hooks.rs) | 项目 settings 中 SessionStart/工具前后等命令 hooks、超时和结果解释 | 执行宿主命令，不由 SandboxRun 自动隔离 |
| [src/skills.rs](../src/skills.rs) | SKILL.md 扫描/去重/摘要与显式或关键词激活，兼容 `.claude/skills` | 加载指南不授权脚本执行，不是插件市场 |
| [src/commands.rs](../src/commands.rs) | Markdown 斜杠命令、YAML 元数据、目录命名空间和参数展开 | 是提示模板，不是独立官方 CLI 指令集 |
| [src/mcp.rs](../src/mcp.rs) | MCP 配置和会话，stdio/Streamable HTTP，握手、ID、tools/resources/prompts/resource templates 的有界发现与辅助包装 | 重复/无限 cursor 与公开名称碰撞失败关闭；通知、sampling/elicitation 和真实服务兼容性仍待验收 |
| [src/mcp_sse.rs](../src/mcp_sse.rs) | 旧式 HTTP+SSE：GET 事件 endpoint、同源 POST JSON-RPC、ID 分发和断线处理 | 与新 Streamable HTTP 分开实现 |
| [src/mcp_oauth.rs](../src/mcp_oauth.rs) | OAuth 发现/注册、PKCE S256、localhost 回调、state、刷新/退出和 token 存储 | 需远端服务器实际兼容；不是所有 MCP 服务已验证 |
| [src/sandbox.rs](../src/sandbox.rs) | Python/Node 执行合同、输入/输出/时限/temp 目录和平台分派，缺隔离则拒绝 | Linux bwrap/macOS sandbox-exec；Unix 资源限制未全部落实 |
| [src/sandbox_windows.rs](../src/sandbox_windows.rs) | Windows AppContainer SID/ACL、无网络 capability、受控句柄、挂起加入 Job、内存/进程限制和清理 | 仅对应专用 SandboxRun 路径 |
| [src/session.rs](../src/session.rs) | API 会话 JSON 主存储：消息、usage、Todo、归属、时间和摘要，净化会话 ID | 与新工作流数据库分开 |
| [src/session_index.rs](../src/session_index.rs) | SQLite WAL 会话/消息派生索引、FTS5 trigram、关键词片段和重建 | 不是真正向量语义搜索；JSON 仍是正文来源 |
| [src/memory.rs](../src/memory.rs) | JSON 记忆中的事实/偏好/对话/任务/标签，基于词命中、重要性和时间检索 | 不等于多模型共享证据库或向量数据库 |
| [src/planning.rs](../src/planning.rs) | 旧 Plan/PlanStep/Reflection 契约；标点分句、最多六步等启发式规划及简单反思 | 不是官方 LLM 生成并校验 DAG 的 Teams planner |
| [src/collaboration.rs](../src/collaboration.rs) | AgentWorker 注册与命名委派；内置 Research/Expense 固定话术与测试 Echo | 示例骨架，不是真实多厂商智能协作 |
| [src/evaluation.rs](../src/evaluation.rs) | 持久化评估 JSON，按反思、非空输出、关键词和耗时做基础评分 | 未校准，不能用来证明质量或自动选模收益 |
| [src/expenses.rs](../src/expenses.rs) | 内存费用 CRUD、按月/分类汇总及种子账目示例 | **不是模型 token 账本、订阅额度或预算系统** |

## 7. 自有 API Agent 工具（12 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [src/tools/mod.rs](../src/tools/mod.rs) | Tool/ToolContext/ToolOutput、注册表、内置工具组装、schema 和 MCP 工具替换 | 服务于 API Agent 工具循环 |
| [src/tools/fs.rs](../src/tools/fs.rs) | FileRead/FileWrite/FileEdit：按行读、写文件、精确替换，记录读取修订避免覆盖外部改动 | 权限由调用管线控制，不因文件读写封装就有 OS 沙箱 |
| [src/tools/search.rs](../src/tools/search.rs) | Glob/Grep 路径/正则搜索，gitignore/隐藏文件和输出限额 | 文本/路径搜索，不是语义搜索 |
| [src/tools/shell.rs](../src/tools/shell.rs) | 跨平台 Bash/shell、复合命令权限拆分、前后台执行、超时与长输出落盘 | 实际运行宿主 shell |
| [src/tools/background.rs](../src/tools/background.rs) | Bash 后台作业、输出缓冲、TaskOutput/TaskStop、进程状态与清理 | 没有周期计划/时区/错过触发，不能叫定时任务 |
| [src/tools/patch.rs](../src/tools/patch.rs) | Codex 风格 apply_patch 的增删改/移动、上下文查找和有限空白容错 | 是文件补丁工具，不负责 Git PR |
| [src/tools/notebook.rs](../src/tools/notebook.rs) | 修改 `.ipynb` 单元格的内容、插入、删除及 ID/序号定位 | 不启动 Jupyter 内核执行单元格 |
| [src/tools/webfetch.rs](../src/tools/webfetch.rs) | 限时限量抓取 HTTP(S) 内容，简单 HTML 提取并按域名进入权限检查 | 非浏览器；内网检查是启发式，不是 DNS 级完整出站防护 |
| [src/tools/todo.rs](../src/tools/todo.rs) | TodoWrite 校验会话待办、保存并在后续轮次提供上下文 | 不是 Teams DAG/看板的主任务记录 |
| [src/tools/task.rs](../src/tools/task.rs) | 调用旧 AgentDirectory 的命名 worker，按 agent 名校验权限并返回文本 | 不从这里分配到各厂商官方工具 |
| [src/tools/plan.rs](../src/tools/plan.rs) | EnterPlanMode/ExitPlanMode 临时切换权限模式，限制写入并恢复 | 不负责智能生成 DAG |
| [src/tools/sandbox.rs](../src/tools/sandbox.rs) | 将 SandboxExecutor 暴露为模型工具 SandboxRun | 专门的代码片段隔离入口 |

## 8. 独立测试与协议样本（4 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [tests/protocol_regressions.rs](../tests/protocol_regressions.rs) | 跨模块协议回归：SSE UTF-8 任意切块、thinking/cache 字段、推理回传及异常流等 | 自动测试，不依赖真实账号推理 |
| [tests/claude_native/protocol.rs](../tests/claude_native/protocol.rs) | 在 Claude 模块内加载的协议测试：设置、权限、工具/文本生命周期和官方轨迹回放 | 算入库测试，不应再重复计算为额外集成测试数量 |
| [tests/claude_native/official_2_1_193.jsonl](../tests/claude_native/official_2_1_193.jsonl) | 官方 Claude 2.1.193 配合本地合成服务得到的规范化脱敏轨迹 | 不是 Claude 云端真实回答或质量实测 |
| [tests/claude_native_probe.py](../tests/claude_native_probe.py) | 可选的官方 CLI 离线握手/本地 SSE turn 探针，支持保存规范化轨迹 | 默认无 prompt；不使用真实凭据/远程端点 |

## 9. 打包、CLI 发布和第三方声明（15 个）

| 文件 | 作用 | 边界 |
| --- | --- | --- |
| [packaging/windows/Start-Wonderland.ps1](../packaging/windows/Start-Wonderland.ps1) | 安装后启动器：选择用户数据目录、配置 sidecar/服务 URL、健康探测、隐藏启动后端后打开桌面 | 生命周期仍由 Rust 服务管理，脚本不是调度器 |
| [packaging/windows/build-windows-package.ps1](../packaging/windows/build-windows-package.ps1) | 收集二进制/资源/许可证、获取并校验 sidecar、运行 Inno Setup、生成便携包和校验文件 | 需先完成正确版本的 release 构建；不能把开发二进制当发布验收 |
| [packaging/windows/wonderland.iss](../packaging/windows/wonderland.iss) | Inno Setup 安装器定义：版本、文件、目录、快捷方式和安装行为 | 当前版本字段随源码为 0.11.1，不证明已生成/发布该包 |
| [packaging/npm/wonderland-cli/package.json](../packaging/npm/wonderland-cli/package.json) | npm 名称/版本、Node 要求、两个命令名、打包白名单、测试和公开发布设置 | 包名 `rust-ai-wonderland-cli`，只分发客户端 |
| [packaging/npm/wonderland-cli/README.md](../packaging/npm/wonderland-cli/README.md) | npm 用户的安装、后端依赖、账号/工作流/Teams/情报/价格命令 | Rust 与 npm 某些参数写法不同，以各自说明为准 |
| [packaging/npm/wonderland-cli/LICENSE](../packaging/npm/wonderland-cli/LICENSE) | 随 npm tarball 分发的 MIT 许可 | 不是官方工具的授权文件 |
| [packaging/npm/wonderland-cli/bin/wonderland.js](../packaging/npm/wonderland-cli/bin/wonderland.js) | Node CLI 入口：参数、后端请求、交互聊天/审批和工作流/Teams/诊断命令 | 不内置模型执行后端 |
| [packaging/npm/wonderland-cli/bin/stream.js](../packaging/npm/wonderland-cli/bin/stream.js) | SSE 解码、UTF-8/分块处理、流完成与错误检查的客户端辅助 | 服务端异常不能当完整成功响应 |
| [packaging/npm/wonderland-cli/test/stream.test.js](../packaging/npm/wonderland-cli/test/stream.test.js) | Node SSE 分块、编码、终止与错误测试 | 与 Rust 流测试互补 |
| [packaging/npm/wonderland-cli/test/teams-pricing.test.js](../packaging/npm/wonderland-cli/test/teams-pricing.test.js) | Teams/价格/应用诊断等命令的路由、参数与服务错误传播回归 | 使用测试服务，不证明真实订阅额度/调用 |
| [packaging/npm/wonderland-cli/test/workflows.test.js](../packaging/npm/wonderland-cli/test/workflows.test.js) | Work 创建/操作和参数合同回归，包括推理档位行为 | 需与对应版本后端一起验证 |
| [packaging/third-party/CLIProxyAPI-LICENSE.txt](../packaging/third-party/CLIProxyAPI-LICENSE.txt) | sidecar 上游许可证副本 | 随分发保留 |
| [packaging/third-party/CLIProxyAPI-NOTICE.md](../packaging/third-party/CLIProxyAPI-NOTICE.md) | CLIProxyAPI 来源/版本与使用说明 | 不表示 Wonderland 拥有该上游代码 |
| [packaging/third-party/NotoSansSC-LICENSE.txt](../packaging/third-party/NotoSansSC-LICENSE.txt) | 中文字体许可证 | 随字体分发保留 |
| [packaging/third-party/NotoSansSC-NOTICE.md](../packaging/third-party/NotoSansSC-NOTICE.md) | 字体来源和使用说明 | 说明字体资产归属 |

## 10. 文档、截图与视觉资源（29 个）

| 文件 | 作用 |
| --- | --- |
| [docs/README.md](README.md) | 开发文档索引、仓库整理说明、当前源码/发布差异、推理配置迁移与验证边界 |
| [docs/CLAUDE_NATIVE_ADAPTER.md](CLAUDE_NATIVE_ADAPTER.md) | 官方 Claude 适配版本、协议、设置/权限、回归证据及未验证项 |
| [docs/DEEPSEEK_NATIVE_ADAPTER.md](DEEPSEEK_NATIVE_ADAPTER.md) | DeepSeek ACP、官方包/profile 指纹、模型/effort、权限/流/用量和验证边界 |
| [docs/CODEX_HOST_REFERENCE.md](CODEX_HOST_REFERENCE.md) | CodexHost 参考提交、采用的协议/能力/配置分层及未导入的功能；0.11 持久化关联 |
| [docs/DESKTOP_PRODUCT_PLAN.md](DESKTOP_PRODUCT_PLAN.md) | 长期桌面产品方案：Work/Chat、应用、项目、插件、定时、远程、PR、网站及验收设计；不是完成清单 |
| [docs/MULTI_MODEL_ORCHESTRATION_PLAN.md](MULTI_MODEL_ORCHESTRATION_PLAN.md) | 官方工具约束、任务合同、DAG、动态榜单/价格、预算、质量和研究实验的总方案；并非全部已实现 |
| [docs/RELEASE-0.6.0.md](RELEASE-0.6.0.md) | 0.6 桌面改版、协议/订阅等用户可见发布说明 |
| [docs/RELEASE-0.7.0.md](RELEASE-0.7.0.md) | 0.7 项目工作台、文件/Git/终端和官方 CLI 入口发布说明 |
| [docs/RELEASE-0.8.0.md](RELEASE-0.8.0.md) | 0.8 持久化 Work/Chat、官方执行器与 LiveBench 发布说明 |
| [docs/RELEASE-0.9.0.md](RELEASE-0.9.0.md) | 0.9 Teams、隔离协作、验收与价格来源发布说明 |
| [docs/RELEASE-0.10.0.md](RELEASE-0.10.0.md) | 0.10 Claude/DeepSeek 受管适配与安装诊断发布说明；历史单任务 effort 限制按该版本理解 |
| [docs/VALIDATION-0.6.0.md](VALIDATION-0.6.0.md) | 0.6 回归、Kimi 订阅 API 工具循环和发布物检查证据 |
| [docs/VALIDATION-0.7.0.md](VALIDATION-0.7.0.md) | 0.7 工作台/跨平台、安装、CLI/账号验证及边界 |
| [docs/VALIDATION-0.8.0.md](VALIDATION-0.8.0.md) | 0.8 回归、在线 LiveBench、真实 Kimi 单任务和 Codex 调用限制 |
| [docs/VALIDATION-0.9.0.md](VALIDATION-0.9.0.md) | 真实 Kimi 团队、独立 JS 项目 12 项验收、快照/价格、隔离工作区和发布证据 |
| [docs/VALIDATION-0.10.0.md](VALIDATION-0.10.0.md) | Claude 官方本地合成 turn、DeepSeek 无密钥握手、回归、界面、构建和未验证项 |
| [docs/images/desktop-welcome.png](images/desktop-welcome.png) | 欢迎/首页外观截图 |
| [docs/images/desktop-compact.png](images/desktop-compact.png) | 紧凑窗口布局截图 |
| [docs/images/desktop-cli.png](images/desktop-cli.png) | 官方应用/CLI 入口界面截图 |
| [docs/images/desktop-terminal.png](images/desktop-terminal.png) | 内嵌终端界面截图 |
| [docs/images/desktop-work-new.png](images/desktop-work-new.png) | Work 新建任务界面截图 |
| [docs/images/desktop-work-board.png](images/desktop-work-board.png) | 工作流看板布局截图 |
| [docs/images/desktop-teams-new.png](images/desktop-teams-new.png) | Teams 创建页面截图 |
| [docs/images/desktop-teams.png](images/desktop-teams.png) | Teams 详情/节点布局截图 |
| [assets/fonts/NotoSansSC-Regular.ttf](../assets/fonts/NotoSansSC-Regular.ttf) | 桌面实际嵌入的中文静态字体，保证中文可读 |
| [assets/fonts/NotoSansSC-VF.ttf](../assets/fonts/NotoSansSC-VF.ttf) | 保留的可变字体来源资源，当前桌面未直接 include 此文件 |
| [assets/icons/icon.ico](../assets/icons/icon.ico) | Windows EXE/安装等使用的多尺寸图标资源 |
| [assets/icons/icon.rgba](../assets/icons/icon.rgba) | 桌面窗口使用的原始 RGBA 图标数据 |
| [icon.jpg](../icon.jpg) | 原始项目图标图像来源，供品牌/派生资源使用 |

截图用于展示界面，不是任务成功、实际费用或模型效率的证据。历史 release/validation 文档按各自版本阅读；例如 0.10 不支持单任务 effort，而当前 0.11 源码已补齐，二者不是相互矛盾。

以上分组计数：11 + 7 + 13 + 14 + 8 + 17 + 12 + 4 + 15 + 29 = **130**。

## 11. 上游参考目录为什么另列

这些目录由其上游维护，不能把全部源文件计为 Wonderland 自研或已适配功能。此报告对主程序的 130 个维护文件逐项说明；没有对 6,702 个 Claude 快照文件和子模块内部所有文件做逐项代码审计。它们内部的具体职责应继续查看各上游 README/源码，不能用统一一句“模型适配”冒充逐文件理解。

| 目录 | 来源与作用 | Wonderland 当前集成程度 |
| --- | --- | --- |
| `anytool/ChatGPT/codex` | [openai/codex](https://github.com/openai/codex)，固定提交参考 | 自有 Codex app-server 适配；运行使用实际安装程序 |
| `anytool/DeepSeek/deepseek-harness` | [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) | 自有 DeepSeek ACP 适配；使用已核验 npm 发行 |
| `anytool/kimi/kimi-cli` | [MoonshotAI/kimi-cli](https://github.com/MoonshotAI/kimi-cli) | Python Wire 受管路径，已有真实订阅团队 |
| `anytool/kimi/kimi-code` | [MoonshotAI/kimi-code](https://github.com/MoonshotAI/kimi-code) | 与 Python CLI 区分；目录/手动入口，尚无同等级受管路径 |
| `anytool/minimax/cli` | [MiniMax-AI/cli](https://github.com/MiniMax-AI/cli) | 来源参考/直接入口，未完成受管协作验收 |
| `anytool/minimax/minimax-code` | [MiniMax-AI/minimax-code](https://github.com/MiniMax-AI/minimax-code) | 来源参考/直接入口，未完成受管协作验收 |
| `anytool/mimo/MiMo-Code` | [XiaomiMiMo/MiMo-Code](https://github.com/XiaomiMiMo/MiMo-Code) | 来源参考/直接入口，未完成受管协作验收 |
| `anytool/zai/ZCode` | [zai-org/ZCode](https://github.com/zai-org/ZCode) | 来源参考/直接入口，不保证可嵌入任意原生 GUI |
| `anytool/zai/zcode-plugins` | [zai-org/zcode-plugins](https://github.com/zai-org/zcode-plugins) | 上游插件参考；不表示 Wonderland 插件平台已建成 |
| `anytool/Grok/grok-build` | [xai-org/grok-build](https://github.com/xai-org/grok-build) | 来源收录，未实现对应受管适配 |
| `anytool/pi/pi` | [earendil-works/pi](https://github.com/earendil-works/pi) | 来源收录，未实现对应受管适配 |
| `anytool/opencode/opencode-ai` | [opencode-ai/opencode](https://github.com/opencode-ai/opencode) | 来源收录，与另一个 OpenCode 仓库分开 |
| `anytool/opencode/anomalyco` | [anomalyco/opencode](https://github.com/anomalyco/opencode) | 来源收录，未实现对应受管适配 |
| `anytool/kiro/Kiro` | [kirodotdev/Kiro](https://github.com/kirodotdev/Kiro) | 来源收录，未实现对应受管适配 |
| `anytool/kiro/KiroCrew` | [kirodotdev/KiroCrew](https://github.com/kirodotdev/KiroCrew) | 来源收录，未实现对应受管适配 |
| `anytool/Hermes/hermes-agent` | [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent) | 来源收录，未实现对应受管适配 |
| `anytool/Claude/` | [Gitee Alice-Marx/claudecode](https://gitee.com/Alice-Marx/claudecode)，提交 `a1d261f9f79e4075a94406bbefa06d7b9f61a0e2` 的普通文件快照 | 6,702 个文件含源码、归档、发布物、文档和许可；不是官方受管 Claude 2.1.193 发行身份的证据 |

上表前 16 项是 Git 子模块；Claude 是普通跟踪文件快照。子模块固定 SHA 可用 `git ls-tree HEAD <路径>` 查看；`git submodule update --init --recursive --depth 1` 获取的是主仓库固定版本，不会自动追到最新上游。Claude 快照后续要单独导入更新。

## 12. 本地运行、构建及未提交材料

| 路径/类别 | 作用与处理方式 |
| --- | --- |
| 根目录 `claude-code/`、`codex/`、`kimi-cli/`、`kimi-code/`、`deepseek-harness/` | 历史本地参考检出，已忽略；不要与 `anytool/` 固定版本或已安装官方程序混淆。其中本地 Claude fork 不应标作官方发行 |
| 根目录 `CLIProxyAPI/` | sidecar 上游参考；实际发布使用校验过的发行二进制 |
| 根目录 `dsh-desktop/` | 参考项目/文件/变更/终端产品分层；Wonderland 用 Rust 自行实现，未照搬 DeepSeek 专用桌面 |
| 根目录 `MiMo-Code/`、`minimax-code/`、`cli/`、`qwen-code/` | 保留的本地参考目录，已加入忽略规则，不整体上传 |
| `E:/harness/toolchains/reference-codex-host/` | 主仓库外的 CodexHost 只读参考，采用情况见技术参考文档 |
| `RESEARCH_PAPER_FRAMEWORK.md` | 用户研究构想，当前未跟踪且保持原状；内含目标/算法设想，不是模型运行指令或实验完成证明 |
| `.agent-data/` 或 `AGENT_DATA_DIR` | 后端运行数据根，实际位置由启动方式决定；安装启动器默认使用用户 `LocalAppData/WonderlandData` |
| 数据目录 `sessions/*.json` | API 会话原始记录 |
| 数据目录 `sessions-index.sqlite` | API 会话派生 FTS 索引，可从会话重建 |
| 数据目录 `workflows.sqlite`、`workbench.lock` | 官方任务/项目数据库及独占服务锁 |
| 数据目录的 Teams 数据库、`team-workspaces/` | 团队/节点/attempt/事件/验收与独立工作区；不是发布内容 |
| 数据目录 `memory.json`、`evaluations.json` | 旧 Agent 记忆和规则评估记录 |
| 数据目录 `model-intelligence/`、`pricing/` | 来源快照、当前状态与失败记录；历史展示不能替代新选模时在线核验 |
| 连接/账号配置、OAuth token、代理数据 | 私密运行数据，不写进源码、安装器、npm 包或报告 |
| `dist/` | 安装器、便携包、打包中间目录与校验文件，已忽略 |
| `target/` 或配置的 Cargo target | Rust 构建输出；本机主要位于 `E:/harness/toolchains/wonderland-target` |

本次新写的 [PROJECT_PROGRESS-2026-09-21.md](PROJECT_PROGRESS-2026-09-21.md) 是完整进度/历史步骤/后续计划，本文件是逐文件说明；它们不属于上述提交基线的 130 个文件。`docs/README.md` 增加入口后仍为原有文件。

## 13. 0.11.0 发布收尾新增文件

下列文件晚于本目录的 130 文件基线，单独说明，不改写历史覆盖统计：

- [RELEASE-0.11.0.md](RELEASE-0.11.0.md)：配置持久化、安装/CLI 使用、v1→v2 升级与回退边界。
- [VALIDATION-0.11.0.md](VALIDATION-0.11.0.md)：源提交/CI、release 构建、打包 npm 与真实 v1 副本迁移证据及限制。
- [validation/0.11.0-smoke.json](validation/0.11.0-smoke.json)：不含私人目录、任务正文或凭据的结构化迁移结果。
- [DEVELOPER_HANDOFF.md](DEVELOPER_HANDOFF.md)：新开发者的运行、测试、迁移、API/CLI 合同、模块约束、任务分解与验收指南。
- [DELIVERY-0.11.0.md](DELIVERY-0.11.0.md)：产物构建之后的实际上传结果、散列、渠道阻塞及接手清单；通过 GitHub main 更新，不嵌回已构建安装包。
