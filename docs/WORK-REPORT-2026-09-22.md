# 工作报告：Teams 路由约束持久化与决策重放

日期：2026-09-22  
项目版本：0.11.0 交接基线  
工作目录：`F:\harness\Codex\Rust_for_AI-agent-0.11.0\Rust_for_AI-agent`

## 本轮已完成

1. 在 `src/routing.rs` 增加可序列化的 `RoutingPolicy`，把 LiveBench 类别、类别权重、质量门槛、计费渠道、token 估算和预算约束作为可保存 Team 配置。旧 Team JSON 没有该字段时仍默认为 `null`。
2. 在 `TeamCreate` 中持久化可选 `routing_policy`，并限制它只能用于 `Automatic` Team；文件数据库重开测试确认配置不会丢失。
3. 增加 Teams 路由入口：
   - `POST /api/v1/teams/{id}/routing/preview`：提交显式约束，在线刷新 LiveBench 与官方价格后预览。
   - `POST /api/v1/teams/{id}/routing/preview/saved`：读取 Team 保存的约束，再次在线刷新并预览。
   - `GET /api/v1/teams/{id}/routing/preview`：只读读取最新 `routing_preview` 事件，返回 `status=replayed` 和来源事件序号；不刷新网络，也不授予派工权限。
4. 在 `TeamStore` 增加按事件类型读取最新事件的能力，并测试同类事件只重放最新一条。
5. 更新 README、开发交接文档和文档索引，明确三个入口、证据 epoch、事件持久化和当前安全阻塞边界。

## 验证结果

- `cargo fmt --all --check`：通过。
- `cargo test --locked --all-targets`：411 个库测试、3 个 Rust CLI 测试、18 个桌面测试、7 个协议测试通过；7 个需要外部安装/在线条件的测试按项目设计忽略。
- `cargo test --locked --all-targets --features ui-snapshots`：同样通过。
- `npm test --prefix packaging/npm/wonderland-cli`：14 项通过。
- 追加的 TeamStore 定向测试：13 项通过，其中包含保存策略重开和最新路由事件重放。
- 没有调用真实模型，没有使用用户凭据，也没有修改原有 `F:\harness\Codex\Rust_for_AI-agent` 旧工作区。

## 本轮尚未完成

1. `Automatic` 仍不会正式派工；当前路由只做证据绑定预览，未知模型身份、订阅/代理计费、缺失榜单或价格证据仍然阻塞。
2. `budget_usd` 仍不是原生工具的硬 USD 上限；尚未完成提供商实际用量、在途请求、崩溃恢复和结算对账闭环。
3. 真实跨厂商在线成功实例、账号/订阅身份核验、决策后原子预占、重试/升级新决策事件仍未完成。
4. 桌面 Team 创建页和 Rust/npm CLI 尚未提供 `routing_policy` 的专用编辑表单/参数；当前可通过 HTTP Team JSON 保存，后续应补客户端界面和 schema 示例。
5. 插件、定时任务、远程主机、恢复/分叉、PR 与网站面板仍属于交接文件列出的后续范围。

## 交接说明

本机本轮无法通过 GitHub 443 直接 clone，因此使用交接目录中的 `Rust_for_AI-agent-main-24dbf54.zip` 恢复 0.11.0 源码，已在新目录初始化本地 Git，并配置 `origin` 为官方仓库地址；旧工作区保持不动。后续若网络恢复，可再执行远端历史对齐和推送，但本轮没有向 GitHub 写入或创建 PR。
