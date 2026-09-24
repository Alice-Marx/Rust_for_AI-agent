# 工作报告：一键安装 + 跨厂商分工实验（2026-09-24）

## 一、完成的工作

**1. 新功能：一键安装大模型官方工具**（用户需求："新增可以一键安装大模型工具的功能"）
- 后端 `POST /api/v1/apps/{id}/install` 与 `GET /api/v1/apps/{id}/install/status`：安装命令**固定在服务端注册表**，客户端只传应用名，杜绝任意命令注入；安装输出保留有界尾部（400 行）；npm 操作全局串行；成功后自动跑版本探测并以探测结果（而非安装器自称）确认。
- 版本锁定：kimi-code@2.0.2、dsh@0.1.6-alpha.2、claude-code@2.1.193、minimax-code@0.5.2、mimo@0.1.15——全部与受管适配器核对过的版本一致；codex 跟随上游；kimi-cli 走 `uv tool install`；grok/zcode/wonderland 明确不支持并给出可执行指引（fail-closed）。
- 桌面「应用」页：未安装且支持一键安装的工具显示「一键安装（命令明文）」按钮；安装中显示指示与提示；失败显示原因并可重试；完成后显示探测版本与安装输出折叠区；安装状态 1.5 秒轮询。
- **服务启动时自动把用户真实 npm 全局目录注入 PATH**（`npm config get prefix`），修复"npm 前缀搬到非 C 盘后装了也探测不到"的普适问题。

**2. 真实跨厂商分工实验成功**（Kimi × DeepSeek）
- 通过一键安装装好 kimi-code 2.0.2 与 dsh 0.1.6-alpha.2；Assigned 策略团队：Kimi 实现 strings.js、DeepSeek 实现 math.js，DeepSeek 的沙箱权限请求经人工批准，独立验收 4/4 通过，团队 succeeded。全程 8 轮迭代修复（详见实验记录第 6 节）。
- 顺带打通：受管 dsh 执行器现在自动复用「工作区设置」里配置的 DeepSeek API Key（环境变量缺失时从服务连接凭据转发，仅注入子进程、不落日志）。

**3. 实验过程中修复的执行器真实缺陷**（全部带回归测试）
- Windows 上裸 `Command::new("npm")` 无法执行 npm.cmd → 共享 `npm_root_global()`（PowerShell shim），kimi/minimax/mimo 统一；
- dsh 的 npm 11 嵌套安装布局被适配器拒绝（三个内容哈希与固定常量一致，纯布局差异）→ 接受嵌套/平坦两种布局，混合仍拒绝；
- ACP sessionId 强制 UUID 而 kimi 返回 `session_<uuid>` → 放宽为非空无控制字符；
- `\\?\` verbatim 路径传给 node 导致 CLI 秒退且 stderr 被丢 → 共享 `node_path()` 归一化（deepseek 原逻辑提升为公共）；
- kimi 模型值需 `kimi-code/` 前缀全名 → 确定性前缀 + 初始/设置后分级校验；
- `session_info_update`、`pending` 工具起始态、中间态误判终态 → 方言与通用框架三态化。

## 二、各文件的说明

| 文件 | 变更 |
| --- | --- |
| `src/app_installer.rs` | **新增**。安装注册表（InstallRecipe）、任务状态机、有界输出尾部、`ensure_npm_prefix_on_path`；5 项离线测试 |
| `src/lib.rs` | 注册 app_installer 模块 |
| `src/workbench_service.rs` | 新增 install / install/status 两条路由与处理器 |
| `src/desktop_bridge.rs` | `normalize_launch` 提为 pub(crate)；新增共享 `npm_root_global()` |
| `src/native_executor.rs` | 新增共享 `node_path()`（verbatim 归一化）；`tool_update_terminal` 改三态语义；ACP sessionId 校验放宽 |
| `src/native_executor/kimi_code.rs` | npm 定位改走共享 helper；`model_value` 前缀化；`verify_initial_config_options` 覆盖（初始只验"目录中有该模型"）；`tool_start_status` 接受 pending；passthrough 增加 `session_info_update`；`node_path` 归一化；测试与 fixture 更新为真实服务器形态 |
| `src/native_executor/minimax.rs`、`mimo.rs` | npm 定位改走共享 helper；minimax 接入 `node_path` |
| `src/native_executor/deepseek.rs` | 接受嵌套/平坦安装布局（混合拒绝）；`deepseek_api_key()` 凭据转发；本地 node_path 移除改用共享 |
| `src/main.rs` | 服务启动时调用 `ensure_npm_prefix_on_path` |
| `src/bin/desktop/studio.rs` | 应用页一键安装按钮/状态/输出展示；安装轮询；AppInstall 回信；ui-snapshots fixture 补安装状态样例 |
| `src/bin/desktop/ui.rs` | 截图保存过滤辅助视口（图标 96x96 事件） |
| `docs/EXPERIMENT-2026-09-24-CROSS-VENDOR-TEAM.md` | **新增**。图文实验记录（步骤、命令、真实输出、8 轮修复表、重做路径） |

## 三、需要做的测试

- 已做：471 项库测试 + 6 项 CLI + 19 项桌面全绿（app_installer 5 项新增：注册表完整性、版本锁定、fail-closed、未知 ID 拒绝、输出有界）。
- 已做（真实环境）：kimi-code 与 dsh 一键安装成功并探测确认；grok/未知 ID 拒绝；dsh 网络失败保留原始输出且可重试；跨厂商团队全流程成功。
- 待做（用户侧）：在**你正在用的安装版**上升级到含本功能的版本后，用桌面按钮装一次工具（验证 UI 链路）；用你自己的账号跑一次第 7 节最短路径实验。
- 待观察：npm allow-scripts 警告（postinstall 被拦）在两家工具上均不影响功能（平台二进制随包分发，已实测 `--version` 与真实推理）；如未来某工具依赖 postinstall 再评估。

## 四、还未完成的任务与完成思路

- **0.11.2 打包发布**：当前改动只在源码与本地验证实例上。思路：版本号 0.11.1→0.11.2，跑现有打包脚本出安装器/便携 ZIP/SHA256SUMS，替换 GitHub Release 资产（流程同 0.11.1，见 Windows 打包报告）。
- **用户安装版升级**：你机器上 D 盘的安装版仍是旧二进制；新安装包做好后覆盖安装即可（数据目录不变）。
- **Automatic 策略真实账号验证**：本次验证的是 Assigned（显式分工）；Automatic 的路由条件核验清单仍在总项目报告第三节，待你按清单补齐后测。
- **其余厂商工具的一键安装实测**：claude/minimax/mimo/codex 的安装命令已在注册表并对齐适配器锁定版本，但本机未装、未实测；安装失败时的 npm 原始输出会完整保留，便于排查。
- **截图**：应用页一键安装按钮的界面截图因 egui 视口截图机制在当前会话只产出 96x96 辅助视口而暂缺；ui-snapshots fixture 已备好安装状态样例，待窗口截图问题排查后补进用户指南。
