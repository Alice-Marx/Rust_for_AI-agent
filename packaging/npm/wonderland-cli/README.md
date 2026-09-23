# rust-ai-wonderland-cli

Wonderland 0.11.0 后端兼容的终端客户端，当前 npm 客户端版本为 0.11.1。Node.js 18+，连接已启动的 Wonderland Rust 后端；后端来自桌面安装包、便携包或源码构建。npm 包不包含后端。预览版本使用 `next` 频道，稳定频道 `latest` 仍为 0.7.0。

新增官方应用任务命令：

```sh
wonderland-cli apps
wonderland-cli apps --probe claude
wonderland-cli --cwd /path/to/project --model kimi-for-coding work create kimi-cli "修复测试"
wonderland-cli work start <id>
wonderland-cli work events <id>
wonderland-cli work duplicate <terminal-id>
wonderland-cli work approve <id> <request-id> allow
wonderland-cli work cancel <id>
wonderland-cli work accept <id> "独立检查变更和测试结果"
wonderland-cli intelligence refresh
```

受管任务支持官方 Codex、Kimi CLI（Python）、Claude Code、DeepSeek Harness、Kimi Code（Node）、MiniMax Code、MiMo Code 和 Grok Build；其他登记工具保留终端入口。Claude 固定官方原生 npm 版 2.1.193，DeepSeek 固定 0.1.6-alpha.2，Kimi Code 固定 2.0.2，MiniMax Code 固定 0.5.2，MiMo 固定 0.1.15，Grok 固定 1.0.38。工具需单独安装，未知版本或身份回读不符会阻止受管执行。Grok 只使用 `XAI_API_KEY` 或官方程序实际广告的缓存令牌认证，不发起交互登录。任务完成执行后进入待验收。API 对话与官方工具任务是独立执行路径，实际可用能力以 `apps` 返回的后端信息为准。

`apps --probe <id>` 只检测注册程序的版本、路径和 SHA-256，不调用模型或读取凭据。程序已安装不代表账号、模型或订阅可用；文件指纹也不是发行商签名认证。

0.11.0 支持在 `work create` 时使用全局 `--reasoning`，保存为任务的 `reasoning_effort`；例如 `wonderland-cli --model claude-sonnet-4-6 --reasoning high work create claude "检查错误处理"`。启动使用已保存配置，不接受临时覆盖；省略档位表示沿用官方默认。Teams 同样在执行器绑定中指定 `reasoning_effort`。这项改动需要配套 0.11.0 后端；0.10.0 仍只支持在 Teams 中指定档位。Kimi 当前没有受管 effort 选项。

后端首次打开旧工作流数据库会事务迁移到 schema v2。升级前停止服务并备份完整 `AGENT_DATA_DIR`；回退 0.10.0 时使用升级前备份，不能用旧程序直接读取 v2 数据库。客户端包本身不保存或迁移这些数据。

团队任务使用 JSON 计划。以下命令在 npm CLI 和 Rust CLI 中相同，`create` 仅创建计划，`start` 明确启动执行：

```sh
wonderland-cli team create --file team.json
wonderland-cli team list
wonderland-cli team get <team-id>
wonderland-cli team start <team-id>
wonderland-cli team events <team-id> --after 0
wonderland-cli team cancel <team-id>
wonderland-cli team routing preview <team-id> --file routing-policy.json
wonderland-cli team routing saved <team-id>
wonderland-cli team routing replay <team-id>
```

`team.json` 示例；将 `cwd` 改为后端机器上的绝对项目路径，并将规划器模型替换为当前账号实际支持的精确 ID：

```json
{
  "title": "修复测试",
  "prompt": "检查失败测试，修复原因并完成独立验证",
  "cwd": "E:/projects/demo",
  "strategy": "fixed",
  "planner": { "app_id": "kimi-cli", "model": "kimi-for-coding" },
  "candidates": [],
  "nodes": [],
  "checks": [
    { "program": "cargo", "args": ["test", "--locked"], "timeout_secs": 300 }
  ],
  "max_parallel": 2,
  "max_duration_secs": 1800,
  "max_attempts": 2,
  "budget_usd": null
}
```

`nodes: []` 由规划器生成计划；也可提交带 `id/objective/dependencies/write_paths/acceptance/executor` 的节点。验收命令使用独立的 `program` 和 `args`，不执行拼接的 shell 命令。固定策略沿用规划器执行器；自动策略需要显式候选列表以及已核验的模型、计费渠道和价格，条件不足时后端会报告阻塞原因。事件的 `seq` 可作为下一次 `--after` 的游标；取消以服务返回的最终状态为准。

路由策略文件是严格的 `RoutingPolicy` JSON 对象。例如：

```json
{
  "required_categories": ["Coding"],
  "category_weights": { "Coding": 1.0 },
  "minimum_quality": 0.75,
  "billing_channel": "api",
  "estimated_input_tokens": 12000,
  "estimated_output_tokens": 4000,
  "budget_usd": 1.5
}
```

`routing preview` 使用文件中的约束在线刷新证据并保存本次决策；`routing saved` 使用 Team 创建时保存的 `routing_policy` 再次在线预览；`routing replay` 只读取最近一次持久化决策，不联网刷新。三者都不会启动 Team，也不会解除 Automatic 的预算与身份闸门。`work duplicate` 只接受已终止的独立任务，返回新的 Draft；它保留任务请求，不复制旧 session、输出或错误，也不自动启动。

官方价格与 LiveBench 数据分别刷新：

```sh
wonderland-cli pricing status
wonderland-cli pricing refresh
wonderland-cli pricing quote --app kimi-cli --model kimi-k2.7-code --billing api
wonderland-cli pricing quote --app codex --model gpt-5.6-sol --billing subscription
```

`status` 和 `quote` 不会自动联网刷新。报价保留输入、缓存读取、缓存写入及输出的 USD/百万 token 单价和适用档位；应用、模型及计费渠道必须精确匹配。OpenAI 标准 API 与 Kimi 国际 API 已有官方来源解析，其余来源或未知档位会说明限制。订阅配额不会换算成零价 API token；`blocked` 是查询结果，不表示可以按零成本执行。每轮自动调度由后端重新核验来源，不能用历史报价或榜单历史成本代替当前计费证据。

```sh
npm install -g rust-ai-wonderland-cli@next
wonderland-cli --version
wonderland-cli health
wonderland-cli login kimi --wait
wonderland-cli models
wonderland-cli --cwd /path/to/project --model kimi-k2.5 --reasoning on chat
wonderland-cli --continue chat
wonderland-cli search "会话关键词"
wonderland-cli profile --model gpt-5.4
wonderland-cli mcp-login remote --wait
wonderland-cli mcp-reload
```

默认 SSE 流式输出；`--no-stream` 获取完整响应。交互式工具权限提示展示完整参数，只有手动输入 y 才批准。工作区内读取默认允许，明确拒绝/询问规则优先。`--mode plan` 只读，`--mode acceptEdits` 允许编辑。

环境变量：`AGENT_SERVER_URL`（默认 http://127.0.0.1:8080）、`WONDERLAND_SERVER_TOKEN`、`AGENT_USER_ID`、`AGENT_SESSION_ID`、`AGENT_CWD`、`AGENT_MODEL`、`AGENT_PERMISSION_MODE`、`AGENT_REASONING_EFFORT`。

提供 `wonderland` 和 `wonderland-cli` 两个命令名；与原生安装版一起使用时优先运行 `wonderland-cli`，避免后端 `wonderland.exe` 的同名冲突。

聊天命令：`/help`、`/exit`、`/health`、`/models`、`/sessions`、`/session`、`/cost`、`/tools`、`/mcp`、`/skills`、`/commands`。支持项目 `.claude/commands`、`.wonderland/commands` 的 Markdown 模板。

[项目与完整文档](https://github.com/Alice-Marx/Rust_for_AI-agent)
