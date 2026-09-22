# 2026-09-22 最终交接包

这份文件是交给下一位开发者的唯一入口。它按“可交付源码、候选改动、官方交付物、无关文件”分类现有工作区；**不要整体复制或合并 `F:\harness\Codex` 根目录**。

## 1. 应交付什么

交付下列两个目录即可：

| 优先级 | 路径 | 角色 | 当前状态 |
| --- | --- | --- | --- |
| 必须 | `F:\harness\Codex\Rust_for_AI-agent-0.11.0\Rust_for_AI-agent` | 本轮唯一可交付源码副本 | `master` 干净，HEAD `b2c4640` |
| 可选 | `F:\harness\Codex\deliveries\Wonderland-0.11.0-Delivery-20260922-Final` | 0.11.0 官方安装包、源码基线、验证资料与校验和 | 只读交付参考，含 `FILE-MANIFEST.csv` 与 `SHA256SUMS.txt` |

源码副本共有 6,770 个非 `target` 文件。它在本地以交接归档初始化 Git 历史，已配置官方 remote `https://github.com/Alice-Marx/Rust_for_AI-agent.git`；由于当前主机无法稳定连通 Git Smart HTTP，不能把“远端分支已同步”当作事实，也没有推送本轮提交。

## 2. 接手顺序

1. 先读本文件，再读 [开发者交接指南](DEVELOPER_HANDOFF.md)、[项目进度](PROJECT_PROGRESS-2026-09-21.md)、[逐文件职责](FILE_CATALOG-2026-09-21.md)。
2. 在可交付源码副本运行 `git status --short --branch`，应为空；再运行 `git log --oneline -4`，应看到本节列出的四个提交。
3. 先运行格式与测试，再决定是否继续 H03/H04。不要因路由预览存在而解除 Automatic 或 USD 预算阻塞。
4. 单独审查第 5 节的候选改动。它们不是本轮已验证交付的一部分，不得通过复制目录或 `git add .` 直接混入。

推荐命令：

```powershell
Set-Location F:\harness\Codex\Rust_for_AI-agent-0.11.0\Rust_for_AI-agent
$env:RUSTUP_HOME = 'D:\Jianwei_Li\rust\rustup'
$env:CARGO_HOME = 'D:\Jianwei_Li\rust\cargo'
cargo fmt --all --check
cargo test --locked --all-targets
cargo test --locked --all-targets --features ui-snapshots
npm test --prefix packaging/npm/wonderland-cli
```

盘符仅适用于本机；其他开发者应使用自己的正常 Rust 安装，不要复制这些环境变量。

## 3. 本轮源码提交

| 提交 | 内容 | 接手影响 |
| --- | --- | --- |
| `72e66ae` | 从 0.11.0 交接归档导入源码 | 本地导入基线；对应官方来源提交 `24dbf549450c78681db231d9b5e6870f67202b41` |
| `3f18f1b` | `routing-v1` 可解释、证据绑定的候选预览 | 新增 `src/routing.rs` 与 POST 预览接口；不执行模型 |
| `15fab6d` | Team 路由约束持久化和决策重放 | 保存 `routing_policy`；增加 saved preview 和 replay API |
| `b2c4640` | GitHub 源码归档回退 | 增加 Git-first/codeload fallback 脚本和报告 |

已实现的路由 API：

- `POST /api/v1/teams/{id}/routing/preview`：显式约束的在线预览。
- `POST /api/v1/teams/{id}/routing/preview/saved`：用 Team 保存的 `routing_policy` 再次在线预览。
- `GET /api/v1/teams/{id}/routing/preview`：重放最新持久化决策；不刷新网络，不授予派工权限。

普通与 `ui-snapshots` Rust 全目标回归均通过：411 个库测试、3 个 Rust CLI、18 个桌面、7 个协议测试；7 个需要外部条件的测试按设计忽略。npm CLI 14 项测试通过。详细依据见 [路由工作报告](WORK-REPORT-2026-09-22.md)。

## 4. 源码获取与 Git 历史

本机对 `github.com:443` 的 Git Smart HTTP 连接超时，但官方 `codeload.github.com` 可达。使用 [Get-UpstreamSource.ps1](../tools/Get-UpstreamSource.ps1) 时会先尝试正常 Git 浅克隆；失败时按不可变 40 位提交 SHA 下载官方归档，并写入 `.wonderland-source.json`。

归档回退可用于构建和审阅，但没有 Git 历史，不能用于 rebase、push 或历史敏感操作。网络恢复后应获得真实 clone；若要把本轮已提交改动移植到真实 `24dbf54` 基线，可先在同一台机器添加可交付副本为本地 remote，再依次 cherry-pick：

```powershell
git remote add codex-handoff F:/harness/Codex/Rust_for_AI-agent-0.11.0/Rust_for_AI-agent
git fetch codex-handoff master
git cherry-pick 3f18f1b 15fab6d b2c4640
```

执行上述命令前，目标工作树必须干净，且确实基于 `24dbf549450c78681db231d9b5e6870f67202b41`。不要 cherry-pick 导入基线提交 `72e66ae`。

## 5. 已隔离、待人工审查的候选改动

以下目录均被保留，**不在可交付源码副本中**。它们可能包含有价值工作，但尚未通过本轮验证，也没有被合并。

| 路径 | Git 状态 | 候选文件 | 建议处理 |
| --- | --- | --- | --- |
| `F:\harness\Codex\Wonderland` | `handoff-development`，基线 HEAD `24dbf54`，4 项未提交改动 | `src/lib.rs`、`src/model_intelligence.rs`、`src/model_identity.rs`、`assets/model-identity/v1.json` | 单独 review `model_identity` 注册表实现、资产格式和测试，再决定是否提交 |
| `F:\harness\Codex\Rust_for_AI-agent` | 本地 `master`，5 组未跟踪文件 | `src/bin/agent-cli.rs`、`src/bin/agent-desktop.rs`、`packaging/npm/agent-cli/`、`packaging/windows/Start-RustAIAgent.ps1`、`packaging/windows/rust-ai-agent.iss` | 视为旧 CLI/桌面/Windows 打包候选，逐文件 diff 与现有 Wonderland CLI 合同核对后再导入 |

`Wonderland` 的已跟踪 diff 仅把 `model_identity` 暴露到 intelligence 状态中，但新增模块约 17.9 KB、JSON 资产约 2.1 KB；这不是“可忽略的一行改动”。`Rust_for_AI-agent` 的未跟踪文件时间早于本轮开发，来源和测试状态未知。

## 6. 不应交付或合并的根目录内容

| 路径/类型 | 处理 |
| --- | --- |
| `F:\harness\Codex\organized` | 独立的 Gitee `claudecode` 整理仓库，不属于 Wonderland 主项目 |
| `F:\harness\Codex\node_modules`、`create_paper.js`、`create_zhuxi.js` | 根目录工具/依赖，不属于主项目交付 |
| `F:\harness\Codex\github-source-24dbf54-verify.zip` | 约 6.75 MB 的中断下载残片；不是有效源码归档，不要交付或解压 |
| 根目录的 `Wonderland-0.11.0-Delivery-20260922*` 文件/目录 | 交付过程中的重复副本或压缩件；交付时只采用 `deliveries\Wonderland-0.11.0-Delivery-20260922-Final` 这一份完整目录 |
| `F:\harness\Codex\deliveries.7z` | 全量交付压缩备份；仅在需要恢复时使用，并先核对其附带校验和 |

## 7. 未完成工作与边界

1. Automatic 仍不能正式按质量/成本选择并派工；现有路由仅做证据绑定预览。
2. `budget_usd` 仍不是原生官方工具的硬 USD 上限；账户身份、实际计费、在途请求、崩溃恢复和结算尚未闭环。
3. 没有真实跨厂商成功任务证据；订阅/代理计费仍应保持 unknown/blocked。
4. 路由约束目前可通过 HTTP Team JSON 保存，桌面和两个 CLI 尚无专用编辑界面。
5. 恢复/分叉、插件、定时任务、远程主机、PR 和网站面板仍未实现。完整优先级与验收门槛见 [开发者交接指南](DEVELOPER_HANDOFF.md) 的 H01–H10。

## 8. 交付前检查

- [ ] 只复制第 1 节列出的源码与可选官方交付目录。
- [ ] 在交付副本确认 `git status --short --branch` 为空。
- [ ] 附带本文件、[开发者交接指南](DEVELOPER_HANDOFF.md)、[逐文件职责](FILE_CATALOG-2026-09-21.md)、[官方交付清单](../../../deliveries/Wonderland-0.11.0-Delivery-20260922-Final/FILE-MANIFEST.csv)。
- [ ] 明确告知接手人第 5 节两个目录为未验证候选，不得自动合并。
- [ ] 不交付第 6 节的无关目录、根目录依赖或中断下载残片。

本文件完成的是可核对、可恢复的交接整理；它不声称已经完成尚未验证的功能，也不覆盖用户原有文件。
