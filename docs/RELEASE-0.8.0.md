# Wonderland 0.8.0 · 工作台预览版

这是多模型协作建设方案的第一阶段实现，不是完整自主协作平台的完成声明。

![Work 看板（展示数据）](images/desktop-work-board.png)

![新建官方工具任务（展示数据）](images/desktop-work-new.png)

## 新增能力

- 桌面 Work、Chat、任务列表/看板、项目与 AI 应用中心；保留文件、Git 变更、原生 PTY 与 API 对话。
- Codex app-server、Kimi CLI Wire 结构化执行，显式模型与工具绑定，按次权限确认、问题回答、取消、超时及进程树清理。
- SQLite/WAL 任务与事件记录。后端持有执行，关闭 UI 不取消受管任务。服务重启将未结束的任务标为中断，保留现场且不自动重放修改。
- 执行结束进入待验收，独立检查结果并填写依据后才能成功。并发受管写任务不能占用重叠项目目录；需要并行修改时请使用独立 worktree。
- 每次调用刷新接口都重新查询两个 LiveBench 仓库，读取公布的最新 RELEASES，按固定提交取得表格与类别映射，存储原始数据、散列和时间。失败不会把旧快照改成新数据。
- 原生与 npm CLI 提供相同的任务操作。

## 官方应用与使用范围

| 应用 | 本版受管任务 | 直接使用 |
|---|---|---|
| Codex | app-server 协议 | 原生终端 |
| Kimi CLI（Python） | Wire 1.x 协议 | 原生终端 |
| Claude Code、Kimi Code（Node）、DeepSeek Harness、MiniMax Code、MiMo Code、ZCode | 尚未实现 | 已登记启动入口，需自行安装/构建对应官方版本 |

受管任务调用本机安装的原版 CLI；Kimi 与 Codex 的登录配置必须在原版工具中可用。CLIProxyAPI 的登录用于旧 API 对话，不能替代原生 CLI 的登录。运行时检查程序指纹、协议、提供商端点和模型身份；程序版本字符串不是密码学意义的发布者签名验证。

Chat 使用只读配置。Codex 的限制由官方 sandbox 设置执行；Kimi 则剔除写入、Shell、子代理等工具并拒绝授权，其工具配置不等于操作系统沙箱。直接交互终端保持原程序权限，Wonderland 不能强制管理终端内用户自行发起的费用与模型切换。

本版关闭已知内嵌选模/子代理路径，拒绝检测到的模型重路由。滚动模型别名仍由提供商解释，不能承诺后台模型权重恒定。手动受管任务费用由原工具账号产生，本版未提供实时账单或预算上限。

## CLI

原生 CLI：

```powershell
wonderland-cli apps
wonderland-cli --cwd E:\demo --model kimi-for-coding work create --app kimi-cli --start "修复测试并报告结果"
wonderland-cli work list
wonderland-cli work events <task-id> --after 0
wonderland-cli work approve <task-id> <request-id> --allow
wonderland-cli work answer <task-id> <request-id> '<answers-json>'
wonderland-cli work cancel <task-id>
wonderland-cli work accept <task-id> "检查变更，node --test 全部通过"
wonderland-cli intelligence --refresh
```

npm CLI：

```powershell
wonderland-cli apps
wonderland-cli --cwd E:\demo --model kimi-for-coding work create kimi-cli "修复测试并报告结果"
wonderland-cli work start <task-id>
wonderland-cli work get <task-id>
wonderland-cli work events <task-id>
wonderland-cli work approve <task-id> <request-id> allow
wonderland-cli work accept <task-id> "检查变更与测试结果"
wonderland-cli intelligence refresh
```

新接口位于 `/api/v1/apps`、`/api/v1/workflows`、`/api/v1/projects`、`/api/v1/intelligence`，复用服务的 Origin 与 Bearer token 验证。事件按 `seq` 游标分页，默认 200 条。项目规范化路径；旧 `/v1` API 对话保持独立。

任务草稿才能启动。失败、中断后的重试通过复制新任务进行，避免静默重放修改和更换原生会话身份。文本输出保留上限 1 MiB，事件单条 256 KiB。文件、工具参数与输出会保存在本机任务数据库中。

## 下一阶段

自动派工需先完成官方渠道价格与订阅额度核验、精确评分映射、持久 DAG、预算预占、隔离工作树及测试验收。随后扩展其它官方工具的受管协议、定时任务、插件/应用连接、远程主机、PR 与预览交付。本版 `auto_dispatch_ready` 固定为 false，未知报价明确为 unverified。
