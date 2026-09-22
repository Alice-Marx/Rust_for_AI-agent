# 工作报告：H02 账号计费语义合同

日期：2026-09-23
基线：`5061ef7`（`handoff-development`，含五源价格解析）
改动：新增 `src/account_billing.rs`；`src/pricing.rs` 的 `status()`/`quote()` 接入；`src/lib.rs` 注册模块

## 本轮交付

对应开发者交接指南 H02 第 3 项「明确定义 API 实付和订阅剩余额度、限速/重置周期及机会成本」。本轮交付的是**语义合同与结构化 unknown**，不是数值集成：

1. **`ChannelContract`**：每个 (app_id, billing_channel) 组合的完整语义——
   - `api` 渠道：`ApiMeteredUsd`，官方 USD/token 列价适用，但列价 ≠ 实付（账号折扣、信用额度、税费不在官方定价文档内）；
   - `subscription` 渠道：`SubscriptionQuota`，提供商自有额度单位、周期性重置，`token_list_price_applies=false`，机会成本说明中明文禁止把订阅费除以任何 token 数折算单价。
2. **目录覆盖**：codex、kimi-cli、claude 各有 api+subscription 两个渠道；deepseek 只有 api（dsh 适配不宣称订阅 OAuth，官方发行是充值式 API）。共 7 条合同。
3. **账号级字段显式 unknown**：实付、剩余额度、限速、重置周期四类字段全部 `unknown` + 非空原因（无提供商计费/额度 API 被集成）。将来集成官方来源时新增 `verified` 变体并携带来源与时间。
4. **`dispatch_blockers()`**：把「Automatic 为什么还不能派工」从散落字符串变成结构化清单（28 条，每渠道 4 条），经 `/api/v1/pricing` 状态的 `billing_channels`（完整合同）与 `dispatch_readiness`（ready=false + missing 清单 + 范围声明）暴露。UI/CLI 与路由从此共用同一事实源。
5. **单一事实源**：`quote()` 对非 api 渠道的阻塞原因改由 `account_billing::non_api_block_reason()` 生成（订阅渠道说明额度单位语义；未知渠道点名），不再各自维护文案。routing.rs 的候选排除文案保持不变，其测试仅断言包含 "subscription"，兼容。

## 验证结果

- `account_billing` 套件 5 项：目录覆盖与不变量（受管应用必有 api 渠道、订阅渠道永不承载 USD/token 语义、所有账号级字段显式非空 unknown）、blockers 结构、非 api 原因单一来源、JSON 字段稳定。
- `pricing` 套件 10 项全过（含扩展断言：status 含 7 条 `billing_channels`、`dispatch_readiness.ready=false` 且 missing 非空、订阅报价阻塞原因含 "provider quota units"）。
- 全目标回归（含 `ui-snapshots`）：423 库 + 3 Rust CLI + 18 桌面 + 7 协议通过；4 个失败仍是已记录的 desktop_bridge/desktop_terminal 本机环境问题（基线复现过）。npm 14/14。`cargo fmt --all --check` 通过。

## 边界与未完成

1. **没有集成任何真实计费/额度端点**：四类账号级字段全部 unknown 是诚实状态，不是缺陷；解锁需要各提供商可核验的账单或额度 API（多数目前不存在官方机器可读形式）。
2. `dispatch_readiness` 只覆盖计费维度；模型身份与榜单覆盖由 `/api/v1/intelligence` 报告，两个清单共同构成 H03 解锁前提。
3. `FieldAttestation` 目前只有 unknown 形态；将来加 `verified` 时必须携带来源 URL、抓取时间与散列，与价格快照的 provenance 标准一致。
4. CLIProxyAPI 的订阅代理属于 API 对话路径的代理计费，不在本合同渠道枚举内（`quote()` 对其按未知渠道阻塞，语义不变）。
