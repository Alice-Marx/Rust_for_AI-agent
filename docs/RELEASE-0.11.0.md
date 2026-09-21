# Wonderland 0.11.0 · 配置持久化与开发交接预览版

Work/Chat 以前没有保存推理档位，创建草稿、复制任务和重启后执行容易缺少同一项配置。本版让桌面、Rust CLI、npm CLI 和 Teams 子任务统一保存 `reasoning_effort`，执行时只读取已保存的工具、模型和档位。

## 本版变化

- Work/Chat 创建界面展示所连服务报告的推理能力；复制任务保留档位，切换应用同时清空不兼容模型/档位。
- SQLite 工作流数据库事务迁移到 v2；旧记录没有指定档位时保持 `null`，不猜测填入 `medium` 等值。
- 创建与启动均校验绑定；启动 API 拒绝临时覆盖模型/档位。Teams 子任务保存同一配置，并核对团队归属。
- Rust/npm CLI 的 `work create` 支持 `--reasoning`。Kimi 未暴露受管档位，不接受虚构的通用 `high`。
- 整理 main 与开发分支、固定上游引用，补齐项目进度、逐文件职责、开发交接和下一阶段验收要求。

界面展示“请求档位”。Codex 当前传入 effort 但没有完整回读有效值，不能据此保证每个模型已经采用相同推理强度。Claude/DeepSeek 按各自原生语义验证；`off`、`none`、`minimal` 不互相翻译。

## 安装与升级

GitHub 分发 `Wonderland-Setup-0.11.0-x64.exe`、`Wonderland-0.11.0-windows-x64.zip` 和 `SHA256SUMS.txt`。Windows 安装包包含 Rust 后端、桌面、原生 CLI 与 CLIProxyAPI 7.3.7；各厂商官方工具需要另外安装。安装器未签名。

```powershell
npm install -g rust-ai-wonderland-cli@next
wonderland-cli --version
wonderland-cli health
```

npm 包需要 Node.js 18+，仅是服务客户端。本版单任务 effort 功能必须配合 0.11.0 后端；`latest` 保持 0.7.0，使用预览版请指定 `next` 或精确版本 `0.11.0`。

**升级前先停止服务，备份完整数据目录。** 安装启动器默认数据目录为 `%LOCALAPPDATA%/WonderlandData`，自定义部署以 `AGENT_DATA_DIR` 为准。完整备份包含工作流/团队数据库、SQLite 附属文件、会话、快照及配置，并应保持私密。正常停止服务后复制完整目录；不要仅复制一个正在写入的 `.sqlite` 文件。

首次启动会把工作流数据库从 v1 迁移至 v2。0.10.0 拒绝读取 v2；需要回退时先停止新版，再将升级前的完整备份恢复到另一个数据目录并启动旧版，不直接覆盖当前数据。应用没有自动降级数据库，也不会自动迁移回旧格式。

## CLI 示例

以下仅创建草稿，不调用模型。路径与模型须替换为本机实际可用配置。

Rust CLI：

```powershell
wonderland-cli --cwd E:/projects/demo --model claude-sonnet-4-6 --reasoning high work create "检查错误处理" --app claude --chat
```

npm CLI：

```powershell
wonderland-cli --cwd E:/projects/demo --model deepseek-chat --reasoning off work create deepseek "检查错误处理"
```

`work start <id>` 使用已保存配置；修改配置应创建新任务。命令同名时检查 PATH，避免把 npm 客户端、Rust 客户端和后端 `wonderland.exe` 混淆。

## 保留的能力边界

受管工具仍为 Codex、Kimi Python、Claude Code、DeepSeek Harness；原生恢复与分叉未实现。Automatic 在线刷新榜单/价格后仍受阻，设置 USD 上限也会阻止原生团队执行。未新增自动成本优化、订阅额度读取、插件、定时、远程控制、PR 或网站专用面板。

Kimi 的真实订阅协作记录继续有效；本次发布验证不新增 Claude/DeepSeek 云端推理或双厂商协作结论，也不证明成本节省或与官方客户端同等效率。

验证证据见 [VALIDATION-0.11.0.md](VALIDATION-0.11.0.md)。详细工程状态和接手入口见 [开发文档索引](README.md)。
