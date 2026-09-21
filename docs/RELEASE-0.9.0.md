# Wonderland 0.9.0 · 协作执行预览版

Teams 把一个开发目标展开为具有依赖关系的任务。后端持有任务、原生工具进程和证据；关闭某个界面不会丢失执行记录。此次保持原工具源码不变，调用官方 Codex app-server 与 Python Kimi Wire。

![Teams 协作详情展示样例](images/desktop-teams.png)

## 运行方式

桌面顶部打开 Teams，选择有提交记录且干净的 Git 仓库，填写目标、精确模型 ID 和验证命令。程序及参数分别填写，例如程序 `node`，参数 `["--test"]`。验证命令是用户选定的主机程序，会以当前用户权限执行；不能把未知项目视为已隔离的沙箱。

- **固定官方执行器**：规划、执行和评审节点使用同一工具、模型与推理档位。计划留空时由该官方工具只读检查项目并返回 JSON DAG；主机校验依赖、范围和模型绑定后开始。
- **逐节点指定模型**：提供节点 JSON，为每个节点指定允许列表内的 `executor`。可在 Codex 和 Kimi 原生执行器之间分工；不会自动更换失败节点的模型。此模式不作价格权重比较。
- **Automatic 条件检查**：每次启动都重新联网获取两个 LiveBench 仓库版本和官方价格。模型/推理变体与 benchmark 的精确映射、原生账号的计费渠道和订阅额度未验证，因此当前会保留证据并显示受阻，不会伪装成智能调度成功。

后端不替用户自动同意原工具权限。打开节点对应的工作流，确认具体的写入或命令请求，或通过 `work approve` / `work answer` 回应。取消协作会先停止所有子进程，再记录终态。重启把未完成的协作标记为中断，不重放可能已执行的操作。

## Git 与验收

起点必须是干净的仓库根目录，具有 HEAD；当前严格执行拒绝符号链接、子模块及未跟踪源文件。每次运行有集成工作树，每个尝试有独立工作树；完成的补丁按序合入集成工作树。写入范围是项目相对的文件/目录前缀，不能写 `.`、`..`、`.git` 或 glob。冲突保留现场并报错。

主机验证实际改动是否在声明范围内。已跟踪测试文件不允许被自动任务改写；需要修改既有测试的工作请使用人工审查流程。此保护是文件规则和 Git 审计，不是操作系统安全边界，也不能证明测试已覆盖全部需求。

官方进程退出只代表执行完成。节点经过范围和补丁验证后可以解锁依赖；父任务仍需全部用户验证命令通过，且验证后的 Git revision 与测试起点一致，才标记成功。验证输出、退出码、超时、已测试提交与补丁散列均保留。模型报告不能代替这些检查。

只读评审节点还需要返回结构化 `accepted/findings/summary`，有阻断发现时节点失败；不能以“有输出”代替认可。父任务验收不会替失败历史尝试补发成功记录。取消、超时或中途无法启动的验收命令也保留对应结果。

最终结果保存在 Teams 详情的集成工作区；不会自动覆盖原始分支、推送或创建 PR。失败与中断工作区保留供审查。工作树可能共享 Git 管理数据；它们不隔离有意越界的程序。验收命令同样使用当前用户权限。

## CLI

```powershell
wonderland-cli team create --file team.json
wonderland-cli team start <id>
wonderland-cli team get <id>
wonderland-cli team events <id> --after 0
wonderland-cli team cancel <id>
wonderland-cli pricing refresh
wonderland-cli pricing quote --app codex --model <exact-model-id> --billing api
```

Rust 和 npm CLI 接口相同。npm 包连接正在运行的 Rust 服务，不内置后端。创建与启动是分开的操作。JSON 示例见 [npm 客户端说明](../packaging/npm/wonderland-cli/README.md)。

## 价格与预算

OpenAI 和 Kimi 官方文档中能够精确解析的 API 报价保留模型 ID、上下文档位、缓存费项、条件、来源散列及获取时间。Anthropic 的展示名称到精确 ID 的映射，以及 DeepSeek 当前高峰/低峰与节假日条件未完全验证，保留来源并显示 blocked。未知费项不是零。

报价最多展示为一小时有效；重启的缓存只供展示。每次分配权重仍必须重新在线刷新，不能因为缓存年轻就跳过。价格 API 不保证原生 CLI 账号使用同一计费方式。订阅不被当作免费、无限 API token。

预算存储以整数 microUSD 原子预留，未知实际消费不释放已预留金额。当前无法核实官方原生账号的消费上限，所以带 USD 上限的 Teams 会在任何模型调用前受阻；不声称已经提供硬预算保障。

## 当前边界

Codex 与 Python Kimi 具有受管原生协议。Claude Code、Kimi Node、DeepSeek Harness、MiniMax、MiMo、ZCode 等仍可使用应用/终端入口，但尚未全部接入 Teams 自动控制。使用同一官方工具不代表已证明与其独立客户端效率相等。需要继续补齐账号实测、不同任务的质量/成本对照、模型身份映射与计费核验。

插件市场、定时任务、远程主机、PR 与网站专用面板也未在本版完成。

新增 [CodexHost 技术参考记录](CODEX_HOST_REFERENCE.md)，记录其能力接口、原生会话、权限/问题、独立额度模型和分版本 DeepSeek 适配的参考方向；没有把它的全部适配器直接导入本版。
