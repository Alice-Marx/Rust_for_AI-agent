# 改进检查与后续工作完整规划（详细版）

日期：2026-09-23。基线：`handoff-development` @ `afb1208`（0.11.0 `24dbf54` + 19 个阶段提交）。
配套：[总项目任务报告](PROJECT-TASK-REPORT-2026-09-23.md)（现状与能力边界）、[ROADMAP](ROADMAP-2026-09-23.md)（H01–H10 验收门槛）、[ANYTOOL-ADAPTER-ASSESSMENT](ANYTOOL-ADAPTER-ASSESSMENT-2026-09-23.md)（上游协议评估）。
总原则不变：**fail-closed，未知即阻塞，不伪造完成；anytool 上游源码零修改，适配全部落在 Wonderland 侧。**

---

## 第一部分　核查方法与证据来源

本规划的全部结论以以下证据为准，可逐条复核：

1. **Wonderland 源码**：`src/native_executor/acp.rs`（通用 ACP 引擎）、`src/native_executor/{deepseek,claude,kimi_code,mimo,minimax}.rs`（五个方言先例）、`src/bin/wonderland-cli.rs`、`packaging/npm/wonderland-cli/`、`src/bin/desktop/teams.rs`。
2. **上游只读研究副本**（仓库外 `F:\everyAI\all\upstream-study\`，零修改）：
   - `grok-build/`：经 codeload 按子模块固定提交 `4247f661689354b831191f11eeeac8424993fe3d` 下载的不可变归档；归档内 `SOURCE_REV=9bb727ccdff0a793ee73bcde4e2e09cbef6b5387`（上游 README 自述：SOURCE_REV 记录 monorepo 提交，与仓库提交是两个命名空间，**不构成冲突**）。
   - `ZCode/`：按固定提交 `872ad960de7ec172591f7e1952f7849229f94521` 下载。
3. **回归基线**（总报告记录）：库 449 项通过、7 项按外部条件忽略；Rust CLI 4、桌面 18、协议集成 7、npm 16；本机已知环境例外按前文如实描述。

---

## 第二部分　需要改进的地方（按优先级，附证据）

### P0-1　通用 ACP 引擎缺 authenticate 步骤（Grok 接入的唯一硬阻塞）

**现状**：`AcpRunner::initialize`（`src/native_executor/acp.rs:181`）流程为 `initialize → session/new → session/set_config_option`，无认证调用。`AcpDialect` trait（`acp.rs:24-66`）没有认证钩子。

**上游事实**（`xai-grok-shell/src/agent/mvp_agent/acp_agent.rs`）：

- `initialize` 结果 `meta` 携带 `grokShell: true`、`agentVersion`、`agentId`、`defaultAuthMethodId`、`modelState`（`acp_agent.rs:567` 起）——身份校验点明确；
- agent 实现 `authenticate`（同文件 597 行起）；BYOK 不变量要求 `xai.api_key` 必须是 `authMethods` 首项（同文件 473 行断言）；会话类方法为 `cached_token`；`AuthenticateRequest._meta` 支持 `headless`、`reauth`、`use_oauth`、`force_interactive`——受管场景只允许 `headless: true` 的非交互路径，任何要求交互（浏览器 OIDC）的情形必须失败关闭；
- 模型与档位经 `session/set_config_option`：`agent/handlers/config_option.rs` 确认 `CONFIG_ID_MODEL`（值经 `SetSessionModelRequest` 生效）与 `CONFIG_ID_REASONING_EFFORT` 两个 config id——**与 Wonderland 现有引擎的 set_config_option 机制完全同形，无需新增模型选择通道**；
- 权限结果语义存在 `allow_once` / `reject_once`（`session/telemetry/permission.rs`），受管任务只选一次性选项，永不选 allow-always 类；
- `sessionUpdate` 全集（上游发送侧统计）：`AgentMessageChunk`、`AgentThoughtChunk`、`UserMessageChunk`、`ToolCall`、`ToolCallUpdate`、`ToolCallDeltaChunk`、`CurrentModeUpdate`、`AvailableCommandsUpdate`、`Plan`（Acp 类 47 处）、以及 `x.ai/*` 扩展（RetryState、GoalUpdated、SubagentSpawned/Finished、TurnCompleted、RewindMarker、HookAnnotation、CompactionCheckpoint、TaskCompleted、SessionSummaryGenerated 等）——方言需逐项划分「透传为工具活动 / 进输出 / 忽略」；
- agent 实现 `session/load`、`session/resume`、`session/list`、`session/close`——H05 原生恢复的真实协议支点。

### P1-1　npm CLI README 未同步新命令

`packaging/npm/wonderland-cli/README.md` 无 `team routing preview/saved/replay` 与 `work duplicate` 条目（仅 `package.json` 版本 0.11.0，README 无相关内容，已 grep 确认）。用户只能依赖 `--help`。

### P1-2　总报告 Grok SOURCE_REV 条目应更正

总报告记「提交来源尚未独立验证」。本轮已核实：子模块 pin 是 GitHub 仓库提交，归档 `SOURCE_REV` 是 monorepo 同步点，两者经同一不可变 SHA 的 codeload 归档闭环。建议将总报告该条改为「已核对，非冲突」。

### P1-3　研究副本缺溯源清单

`upstream-study/` 两个归档无来源记录文件。应补 `upstream-study/PROVENANCE.md`（目录、上游仓库、固定提交、下载 URL、取得时间、用途），与 `tools/Get-UpstreamSource.ps1` 的 `.wonderland-source.json` 约定对齐。

### P2-1　环境敏感测试污染回归信号

以下测试在本机与改动无关地反复失败： `desktop_bridge::tests::version_probe_times_out_hung_program`（3 秒时限，并行负载敏感）、`kimi_node_launch_rejects_python_command_even_before_detection`（依赖 PATH 上真实 kimi）、`desktop_bridge` 两个 PowerShell CLIXML 相关断言、`desktop_terminal::tests::native_pty_shell_accepts_input_unicode_cwd_and_exits`（PTY 时序）。
改进：检测环境前提不满足时显式 skip 并打印原因（而非失败），或集中到独立 feature 闸门；目标是让全量回归恢复「红即回归」语义。

### P2-2　编译警告未清零

native executor 各模块存在未使用导入/变量与死代码警告（连续多轮报告提及）。建议单开一个小提交清零或按项 `#[allow]` 注明理由。

### P2-3　桌面路由面板缺人工视觉验收

四种情形需人工过一遍：无路由事件 / 有 `routing_decision`（含长 explanation 换行）/ 有 `binding_applied` / `dispatch_readiness.missing` 为空与多项（>8 条折叠）；检查键盘导航、中文、缩放、小窗口。

### P3　观察项（暂不动）

- ZCode 源码仓 `apps/zcode-cli` 标 `private`、版本 0.16.9，与 npm 发布物 `@zcode/cli@3.14.0` 的对应关系**未证实**；未证实前不写 ZCode 适配代码。
- `auto_dispatch_ready=false`、硬预算 `budget_usd` 阻塞均为正确的诚实状态，非缺陷。

---

## 第三部分　后续工作详细规划

### G1　Grok Build 受管适配（纯代码，立即可做；真实握手并入 H01）

**目标产物**：`grok` 成为第 8 个受管执行器（`resume/fork=false`），上游零修改。

**步骤**：

1. **引擎扩展**（`src/native_executor/acp.rs`）：
   - `AcpDialect` 增加可选方法 `fn authenticate(&self, init: &Value) -> Result<Option<Value>>`：输入 initialize 结果（可读 `authMethods` 与 `meta.defaultAuthMethodId`），返回 authenticate 参数或 `Ok(None)`（不认证，现有三个方言默认实现不变）；
   - `AcpRunner::initialize` 在 `verify_initialize` 后、`session/new` 前执行认证；认证被协议拒绝或无可用方法时中止并给出方言标签化错误。
2. **新增 `src/native_executor/grok.rs`（`GrokDialect`）**，逐项 fail-closed：
   - 安装探测：官方 `grok` 二进制（`x.ai/cli` 安装脚本）；`--version` banner + 二进制 SHA-256 记入 Identity 事件；固定指纹常量待真实安装回填（诚实边界，同 MiMo 先例）；环境变量 `WONDERLAND_GROK_BIN` 允许直指二进制；
   - `verify_initialize`：要求 `meta.grokShell == true`，记录 `meta.agentVersion` 为工具版本；缺失即拒绝；
   - 认证：仅接受 initialize 实际返回的 `authMethods` 中的方法；优先 `xai.api_key`（前提 `XAI_API_KEY` 环境变量存在），否则 `cached_token`（无缓存令牌时上游会拒绝，如实上抛）；`_meta.headless: true`；`force_interactive/reauth/use_oauth` 一律不置；
   - 模型/档位：`model_config_id = "model"`、`effort_config_id = Some("reasoning_effort")`，模型值 = 裸模型 id（如 `grok-4`）；以 set_config_option 响应回读校验生效值，回读不符即失败；
   - 权限：仅选 `allow_once`/`reject_once` 语义的选项；选项集合不符时拒绝授权（权限永不静默通过）；
   - `passthrough_updates` 与 `stop_status`：按第二部分 P0-1 的 sessionUpdate 清单逐项核对后锁定；`x.ai/*` 扩展默认归为「工具活动透传，不进输出」；
   - usage：核对 Grok usage 载荷字段后映射既有 usage 事件；无 USD 字段则金额保持空，不估算；
   - provider 身份：Grok 为第一方 xAI 工具，模型 id 前缀可证provider 时按 DeepSeek 先例填写，否则保持 unknown。
3. **注册**（`src/native_executor.rs` 门面 + `app_diagnostics`/`vendors` 既有登记表）：capabilities 标 `resume/fork=false`；路由侧不新增身份种子（Grok 无 LiveBench attestation，Automatic 不可用；指定执行不受影响）。
4. **测试**（全部离线 fixture，不依赖真实安装）：
   - 伪造帧驱动全生命周期：initialize（带 grokShell meta + authMethods）→ authenticate → session/new → set_config_option → prompt → 流式更新 → 取消；
   - 负路径：无 authMethods / 仅交互方法 / authenticate 协议拒绝 / initialize meta 缺 grokShell / set_config_option 回读不符 / 权限选项无一次性语义；
   - `cargo fmt --check`、库与协议全量回归。
5. **文档**：新增 `docs/WORK-REPORT-2026-09-23-GROK-DIALECT.md`；更新 ANYTOOL-ADAPTER-ASSESSMENT Grok 行（「需先确认 server 暴露方式」→「已确认 `grok agent stdio`，适配完成/待真实握手」）、总报告、DOCUMENT-GUIDE。

**验收**：离线 fixture 全绿；真实 `grok agent stdio` 握手与登录并入 H01 账号实测后才宣称真实模型执行。

### G2　ZCode 受管适配（G1 完成之后）

1. **发布物核对**（不写代码）：`npm view @zcode/cli@3.14.0` 取 dist tarball SHA 与文件清单，对照源码提交 `872ad96`（`packages/zcode-server-cli`、`apps/zcode-cli` 构建产物）；对应不上则记录「发布物来源未证实」并保持终端手动路径；
2. **协议深读**（已确认入口存在：`packages/server/src/entry-stdio.ts`、`entry-http.ts`，stdio app-server）：逐帧核对握手、会话、权限、取消、usage 的 JSON 合同（`packages/rpc/src/protocol.ts`）；
3. **实现**：按 codex 适配模式写独立 transport（自有协议，**不复用 ACP 假设**），锁定 3.14.0 双闸（npm 版本 + banner/指纹）；
4. 测试与文档同 G1 第 4–5 步。

### G3　小改进批（可与 G1 同批或紧随）

| 项 | 文件 | 内容 |
| --- | --- | --- |
| README 同步 | `packaging/npm/wonderland-cli/README.md` | 补 `team routing preview/saved/replay`、`work duplicate` 用法与 JSON 合同示例 |
| 溯源清单 | `F:\everyAI\all\upstream-study\PROVENANCE.md`（仓库外） | 两归档的仓库/提交/URL/取得时间 |
| 总报告更正 | `docs/PROJECT-TASK-REPORT-2026-09-23.md` | Grok SOURCE_REV 条目改为已核对结论 |
| 测试闸门 | `src/desktop_bridge.rs`、`src/desktop_terminal.rs` 相关测试 | 环境前提不满足时显式 skip 打印原因 |
| 警告清零 | `src/native_executor*` | 清零或按项 `#[allow]` 注明 |

### G4　关键路径（等用户登录账号）

1. **H01 实测**（按总报告「待做 1–5」）：三 ACP 工具真实握手回填指纹 → 各工具登录 + 真实推理/取消/失败注入 → Codex API key 与 ChatGPT 双渠道身份回读核对 → 跨厂商团队实测（≥2 厂商、主机验收、源 checkout 不变）→ 归档脱敏 fixture；
2. **H04 预算闭环**：attempt 级 `estimated_usd`/`confirmed_usd` 数据合同 → 派工前预留（routing 估算 × 显式安全系数，`team_store` 微美元账本）→ 终态幂等结算/释放 → 重启扫描悬挂预留对账 → 只对能证明兑现 USD 上限的渠道解除 `budget_usd` 阻塞；只能拿到 token 数或 CLI/harness 自报金额的渠道永久标记「不支持硬预算」，UI 仅软提示；
3. **H05 恢复/分叉**：逐适配器持久化原生 session id + 工具版本/指纹 + 模型/档位配置 + workspace revision；恢复探测四条件（会话存在、版本兼容、目录一致、审批可重建）全过才置 `resume/fork=true`；Grok 的 `session/resume` 为现成候选；分叉历史与 Teams attempt 重试各自独立合同。

### G5　并行池

- **H07**（插件/定时/远程/PR/网站）：按 [DESKTOP_PRODUCT_PLAN](DESKTOP_PRODUCT_PLAN.md) 合同先立独立模块再接 `workbench_service`；
- **H08**（沙箱/MCP）：真实隔离与授权合同，OAuth token 不进日志，不互拷各工具 MCP 配置；
- **H10**（实验）：H03 完成后启动，独立样例库四策略对比，无证据不写效率结论；
- **anytool 剩余候选**（Kiro/KiroCrew/Hermes/pi/anomalyco）：按 H09 八步标准逐个补评估，方法同本次 Grok 核对。

### 执行顺序总表

```text
立即可做（纯代码）：G1 Grok 适配 → G3 小改进批 → G2 ZCode（先发布物核对）
等账号（用户）：   G4 = H01 → H04 → H05
并行池：          G5 = H07 / H08 / H10 / 剩余候选评估
每项完成：        写增量工作报告 → 登记 DOCUMENT-GUIDE → 更新总报告与 ROADMAP 对应行
```
