# 工作报告：H02 价格源解析——Anthropic 与 DeepSeek

日期：2026-09-22
基线：`9eb174f`（`handoff-development`，含 H02 model identity 注册表与路由预览工作）
改动：仅 `src/pricing.rs`

## 本轮交付

1. **新增第五个官方源 `anthropic-models`**（`platform.claude.com/docs/en/models/overview.md`）。同一刷新 epoch 内先解码它，再用它把 Anthropic 定价表的显示名 join 到经过官方 attestation 的精确 API model ID 与 alias。join 不到的定价行不产报价（fail-closed）；attestation 与官方文档化命名规则冲突时整个源阻塞。这解除了此前 "display names are not yet joined to a separately verified exact API model ID" 的阻塞原因。
2. **`parse_anthropic`**：有界解析定价表。逐行校验 5m/1h 缓存写入价（1.25x/2x）与缓存命中价（0.1x，脚注模型 0.025x）相对基准输入价的乘数不变量，任何漂移按 schema 变化阻塞。`[retired, except on ...]` 注记的行不产生第一方报价；`[limited availability]` 作为条件保留。4.6 代及以后未 attestation 的显示名按官方文档化的 dateless 命名规则派生 ID 并在条件中说明；4.6 代之前按键文档化的 alias 形式报价并声明指针语义。attestation 的模型同时带上下文窗口。
3. **`parse_deepseek`**：有界解析 HTML 定价表。每模型产出 `off_peak`/`peak` 两档，条件携带逐字的官方峰时窗口（周一至周五 01:00–04:00 与 06:00–10:00 UTC，剔除中国法定节假日）；未知项明确中国节假日日历不从该页解码、档位判定归派工方。逐格校验"谷价为峰价一半"的官方声明。官方脚注 (1) 声明的退役别名（`deepseek-v4-flash`、`deepseek-v4-flash-vision-exp`）按 flash 价格单独出报价并注明依据。这解除了此前 "peak/off-peak rates … retired aliases are not mapped" 的阻塞原因。
4. **`decode_source` 重构为 `decode_all`**：五个源在同一函数内按 epoch 顺序解码，`refresh` 与缓存重放 `load_snapshot` 走同一路径，保证缓存校验与在线解码一致。`parser_version` 升到 2；旧 v1 缓存快照按 provenance/版本检查失效（fail-closed），需在线刷新后才有新报价。

## 验证结果

- 单元测试：新增 2 项（Anthropic join/派生/退役/漂移负例；DeepSeek 峰谷/别名/漂移负例），pricing 套件 10 项全部通过。
- 真实文档冒烟（`WONDERLAND_PRICING_FIXTURE_DIR` 指向 2026-09-22 经本机代理下载的官方原文）：OpenAI=37、Kimi=4、**Anthropic=14**（13 个在售显示名，Haiku 4.5 另有 alias 条目）、**DeepSeek=4**（2 个模型 + 2 个退役别名）。
- 真实在线端到端（`HTTPS_PROXY` 指向本机代理运行被忽略的 live smoke 测试）：五个源全部 `verified`，一个 epoch 共 59 条报价，快照持久化并重开复验通过。本机直连 `platform.claude.com` 会被 Anthropic 区域屏蔽（307 到 app-unavailable-in-region），因此本机验证经代理完成；生产代码不变，未屏蔽网络下直接生效。
- 全目标回归（含 `ui-snapshots`）：417 库测试 + 3 Rust CLI + 18 桌面 + 7 协议通过；`desktop_bridge`×4 / `desktop_terminal`×1 的失败在未改动基线上同样出现（本机 PowerShell CLIXML 污染与真实 kimi CLI 行为，见 H02 前一提交说明），与本轮无关。npm 14 项通过。`cargo fmt --all --check` 通过。

## 边界与未完成

1. `auto_dispatch_ready` 仍为 false：订阅剩余额度、限速与重置周期、API 实付的账号级语义仍未定义（H02 第 3 项），本轮只交付官方**列价**解析。
2. Anthropic 报价的 ID 派生路径（非 attestation 行）要求派工前对 API 复核精确 ID；attestation 行没有这一负担。
3. DeepSeek 中国节假日历无官方机器可读来源，保持未知并写入条件；请求时档位分类是派工方（H03）的职责。
4. 本机对 `platform.claude.com` 的区域屏蔽是网络环境问题；受影响环境需经自身可用网络路径完成在线刷新。
