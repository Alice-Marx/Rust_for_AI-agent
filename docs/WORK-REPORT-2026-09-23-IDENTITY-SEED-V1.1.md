# 工作报告：身份注册表种子 v1.1——报价模型与 LiveBench 行的系统核验

日期：2026-09-23
基线：`febe3ef`（`handoff-development`，含 routing-v2 身份消费）
改动：`assets/model-identity/v1.json`（v1.0.0 → v1.1.0）、`src/model_identity.rs`、`src/model_intelligence.rs`（测试断言）

## 本轮交付

routing-v2 要求榜单行必须经身份注册表 attestation。为扩大可路由候选面，把**全部 59 条已验证报价模型**与 **LiveBench 2026-06-25 的 59 个榜单行**做了系统交集核验（榜单原文 `public/table_2026_06_25.csv` 经本机代理于 2026-09-23 从官方 raw.githubusercontent 获取）：

| 厂商 | 报价模型数 | byte-identical 榜单行 | 处理 |
| --- | --- | --- | --- |
| OpenAI (codex) | 37 | **0**（全部为 `-max`/`-xhigh`/`-high` 等档位变体行；`gpt-5.2-codex` 行存在但该名不在报价清单） | 不映射 |
| Kimi (kimi-cli) | 4 | 2（kimi-k2.7-code、kimi-k3） | 已有 v1.0 记录，保持 |
| Anthropic (claude) | 14 | **0**（全部为「ID+档位后缀」行，如 `claude-opus-5-max-effort`） | 不映射 |
| DeepSeek (deepseek) | 4 | 3（deepseek-v4-pro、deepseek-v4-flash、deepseek-v4-flash-vision-exp） | 新增 1 条 attestation（v4-pro）；两个退役别名不映射 |

种子 v1.1.0 的变化：

1. **新增 `(deepseek, deepseek-v4-pro) → deepseek-v4-pro` attestation**。证据：官方定价页 MODEL 行明示该精确名（2026-09-22 获取）；榜单存在 byte-identical 行且带 23 项类目分数。`deepseek-v4-pro-0813` 是另一行（分数不同），官方 MODEL VERSION 虽称现役版本为 0813，但 API 名对应哪行未独立核验，故证据中明确记录「不映射」。
2. **新增 `(deepseek, deepseek-flash) → not_listed`**。现役 canonical 名在榜单无行；`deepseek-v4.1-flash-max` 行的对应关系未核验。
3. **退役别名不映射的依据写入 evidence**：`deepseek-v4-flash`、`deepseek-v4-flash-vision-exp` 虽有 byte-identical 榜单行，但官方脚注 (1) 声明这两个名字的请求现由 V4.1-Flash 服务——行分数描述的是退役前的模型，绝不映射。

## 验证结果

- `model_identity` 套件 10 项全过：新增 `deepseek_attestations_follow_the_same_exactness_rules`（attestation 生效、版本后缀行不被 API 名选中、flash 为核验不在榜、退役别名即使有同名行也是 unknown）；种子计数断言更新为 6 记录/3 attested/3 not_listed/版本 1.1.0。
- `model_intelligence` 状态断言更新为 1.1.0；model 相关套件 52 项全过。
- 全目标回归（含 `ui-snapshots`）：428 库 + 3 Rust CLI + 18 桌面 + 7 协议通过；4 个失败仍是已基线复现的 desktop_bridge/desktop_terminal 本机环境问题。`cargo fmt --all --check` 通过。

## 边界与未完成

1. **OpenAI/Anthropic 的档位变体行全部未映射**：`-max-effort`/`-xhigh`/`-thinking-auto-high` 等后缀与各 API `reasoning_effort` 参数的官方对应规则没有可核验来源（LiveBench 未发布命名规范文档）。在取得官方映射证据前，这些模型保持 unknown——这符合 H02/H03 验收门槛，不是遗漏。
2. 榜单核验基于 2026-06-25 release；LiveBench 发布新 release 后行集合会变，resolve() 的快照存在性检查会在新快照上自动失效缺失条目。
3. 本轮只扩充了身份种子；Automatic 正式派工仍被计费合同（全部 unknown）阻塞，`auto_dispatch_ready` 保持 false。
