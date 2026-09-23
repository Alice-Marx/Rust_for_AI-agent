# 工作报告：H08 MCP 发现分页、资源模板与名称冲突防护

日期：2026-09-23
范围：完善 MCP 读取型发现流程，使工具、资源、提示词和资源模板的分页行为一致，并在加载阶段阻止公开名称歧义。关联总览见 [总项目任务报告](PROJECT-TASK-REPORT-2026-09-23.md)。

## 一、已完成的工作

1. **抽取共享分页收集器。** `collect_paginated` 统一处理 `nextCursor` 回传、重复 cursor 检测和最多 100 页的上限。服务端反复返回同一 cursor 或持续产生新 cursor 时，客户端会返回错误而不是无限循环。
2. **工具发现使用同一安全边界。** `McpSession::list_tools` 改为使用共享收集器，保留所有工具页，并获得与其他列表操作一致的重复 cursor 和页数保护。
3. **资源、提示词和资源模板支持分页。** `resources/list`、`prompts/list` 与 `resources/templates/list` 都能收集全部页面；新增只读辅助工具 `mcp__<server>__list_resource_templates`，并将 `resourceTemplates` 的 `uriTemplate`、名称或描述渲染成可读文本。
4. **加载前验证公开工具名。** 在注册一个 MCP server 的普通工具和辅助工具之前，检查本 server 内以及先前已接受 server 之间的公开名称。名称替换或 64 字符截断造成的碰撞会使后加载的整个 server 失败关闭，避免模型看到的工具名与实际调用目标不一致。
5. **新增可控会话测试替身。** `ScriptedSession` 记录方法和参数，覆盖多页 resources/prompts/templates、重复与无界 cursor、同 server 及跨 server 名称碰撞。

## 二、各文件的说明

| 文件 | 作用 |
| --- | --- |
| `src/mcp.rs` | MCP 分页收集器、工具/资源/提示词/模板发现、资源模板渲染、公开名称碰撞验证与 `ScriptedSession` 测试。 |
| `docs/WORK-REPORT-2026-09-23-H08-MCP-DISCOVERY.md` | 记录本次离线合同、已运行测试、资源模板兼容边界和后续集成方法。 |

## 三、已执行验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过。 |
| `cargo test --locked --lib mcp::tests` | 通过，13/13。覆盖多页资源、提示词和资源模板，重复 cursor、100 页上限，以及本地/跨 server 的公开名称碰撞。 |
| `cargo test --locked --all-targets --features ui-snapshots` | 通过：库 466 项通过、8 项按外部条件忽略；Rust CLI 6/6、桌面 20/20、协议集成 7/7。 |

这些测试使用脚本化 MCP 会话，不需要 OAuth、外部 MCP 服务或真实账号。

### 资源模板兼容性边界

本增量在 server 声明 `resources` capability 时提供 `list_resource_templates` 辅助工具，并调用 `resources/templates/list`。不同版本或实现的 MCP server 可能声明 `resources` 却没有实现该模板列表方法；当前实现会如实传播该请求的错误，不臆造模板、不把普通资源伪装为模板。与每个目标 server 的互操作性仍需通过实际协议版本和 fixture 验证后才能声明支持。

## 四、尚未完成的任务与完成方式

| 任务 | 完成方式 |
| --- | --- |
| 真实 MCP server 兼容性矩阵 | 选取 stdio、Streamable HTTP 与 SSE server，分别覆盖单页/多页、空列表、模板列表缺失、超时、取消和 malformed JSON；把脱敏响应固化为离线 fixture。 |
| capability 细分 | 根据目标 MCP 协议版本和真实 server 行为，决定是否仅在明确声明模板能力时暴露 `list_resource_templates`；为旧实现保留清楚的错误信息或协议版本闸。 |
| 加载失败可观测性 | 在桌面和 CLI 中展示被拒绝的 server、冲突公开名称及请求方法，避免用户只看到工具缺失；日志不得包含令牌或资源内容。 |
| H08 其余隔离能力 | 按路线图继续完成沙箱、OAuth/断连、远程 transport、资源权限与取消恢复的端到端验收。真实账号或第三方 server 的测试应由账号持有人在安装包环境中执行。 |
