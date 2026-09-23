# 工作报告：H06 Teams 路由与阻塞信息视觉回归

日期：2026-09-23
范围：为 Teams 详情页的路由决策、绑定结果与派工阻塞信息建立确定性离线快照和绘制验证。关联总览见 [总项目任务报告](PROJECT-TASK-REPORT-2026-09-23.md)。

## 一、已完成的工作

1. **补齐复杂路由数据的离线 fixture。** `prepare_snapshot` 构造了 10 条较长的 `dispatch_readiness.missing` 阻塞项，以及过期、失败和 Automatic 派工相关的价格状态，覆盖长文本与多状态同时出现的布局。
2. **覆盖两类关键审计事件。** fixture 含 `routing_preview` 与 `binding_applied`，显示候选、解释、绑定结果与路由依据，避免仅在后端有事件时才发现详情页没有绘制。
3. **建立固定尺寸的 egui 绘制检查。** Teams 详情以 `1280×820` 渲染，路由事件卡以 `940×620` 渲染；测试断言两次绘制均产生 shapes，并同时检查 fixture 中的长阻塞文本和两个事件类型。
4. **完成静态视觉检查。** 已查看生成的 `target/ui-audit/studio-teams-routing.png`：截图可见 Teams 路由卡、`binding_applied`、`routing_preview`、候选状态以及长解释/阻塞内容，没有发现被裁切为空白的绘制结果。

## 二、各文件的说明

| 文件 | 作用 |
| --- | --- |
| `src/bin/desktop/teams.rs` | 增加用于快照的复杂 Teams 记录、路由事件和阻塞项，并在 feature-gated 测试中渲染详情页及事件卡。 |
| `target/ui-audit/studio-teams-routing.png` | 本次静态视觉检查使用的本地生成截图；这是构建产物，不作为源代码发行文件。 |
| `docs/WORK-REPORT-2026-09-23-H06-ROUTING-VISUAL.md` | 记录 H06 的离线视觉证据、局限和后续验收。 |

## 三、已执行验证与边界

| 验证 | 结果 |
| --- | --- |
| `cargo test --locked --bin wonderland-desktop routing_snapshot_fixture_contains_blockers_and_renders_routing_events --features ui-snapshots` | 通过，1/1。验证 10 条阻塞项、长文本、`routing_preview`、`binding_applied` 和两种固定尺寸 egui 绘制。 |
| `cargo test --locked --all-targets --features ui-snapshots` | 通过：库 466 项通过、8 项按外部条件忽略；Rust CLI 6/6、桌面 20/20、协议集成 7/7。 |
| 静态截图检查 | 已人工查看 `F:\everyAI\all\Wonderland\target\ui-audit\studio-teams-routing.png`，确认路由卡和长解释可见。 |
| 离线后端检查 | 用 `AGENT_PROVIDER=offline` 启动本地服务；`/health` 显示 provider 为 offline，`/api/v1/pricing` 显示 `ready=false` 且有 52 项缺失字段。没有登录账号或发起模型调用。 |

原生 Computer Use 自动化在本环境不可用：`cua.getState()` 没有可操作应用，随后 node REPL 请求返回 fetch 错误。因此本次没有声称完成真实 Windows 窗口的点击、缩放或账户态测试；截图是 egui 离线渲染产物，不能替代原生桌面交互验收。

## 四、尚未完成的任务与完成方式

| 任务 | 完成方式 |
| --- | --- |
| 原生桌面窗口验收 | 在最终 Windows 安装包中打开 Teams 详情，分别检查空事件、单个路由事件、多候选拒绝和长 `missing` 清单；在常见 DPI、窄窗口与中英文系统字体下确认滚动和换行。 |
| 实际后端数据联调 | 使用保留的脱敏 Teams 记录导入或离线 mock 服务，核对事件时间、候选字段缺失、价格刷新失败和绑定失败在 UI 中都有可读降级文案。 |
| H01 真实账号路径 | 由账号持有人登录官方工具后跑一次受管 Teams，检查真实 `routing_preview`、`binding_applied` 与账户阻塞信息。这需要真实账号，留待安装包交付后执行。 |
| 可访问性与回归 | 加入键盘焦点、屏幕阅读器文本和更多窗口尺度的桌面测试；每次调整路由卡布局时重新生成并审阅静态截图。 |
