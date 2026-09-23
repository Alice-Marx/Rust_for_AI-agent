//! 账号级计费语义合同（开发者交接指南 H02 第 3 项）。
//!
//! 这个模块定义每个受管应用在各个计费渠道上的**语义边界**，而不是数值：
//! - `api` 渠道按提供商官方 USD/token 列价计量，但列价不等于实付；
//! - `subscription` 渠道使用提供商自己的额度单位并周期性重置，
//!   **永远不把订阅费除以任何 token 数折算成单价**；
//! - 账号级字段（实付、剩余额度、限速、重置周期）在集成可核验的
//!   官方来源之前一律显式 `unknown`，并给出原因。
//!
//! 这些合同通过 `/api/v1/pricing` 状态暴露给 UI 和 CLI；Automatic 派工的
//! 解锁条件也从这里推导（`dispatch_blockers`），不靠散落的字符串。

use serde::Serialize;

pub const CHANNEL_API: &str = "api";
pub const CHANNEL_SUBSCRIPTION: &str = "subscription";
const UNKNOWN: &str = "unknown";

/// 计费渠道的计量方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// 按提供商直连 API 的 USD/token 列价计量；实付另行核算。
    ApiMeteredUsd,
    /// 提供商自有额度单位（次数/加权单位/窗口额度），周期性重置；
    /// 不与 USD/token 语义互相换算。
    SubscriptionQuota,
}

/// 一个账号级字段的核验状态。当前所有字段都是 `unknown`；将来集成
/// 官方计费/额度来源时，新增 `verified` 变体并携带来源与时间。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FieldAttestation {
    pub status: &'static str,
    pub reason: &'static str,
}

const fn unknown(reason: &'static str) -> FieldAttestation {
    FieldAttestation {
        status: UNKNOWN,
        reason,
    }
}

/// 一个 (app_id, billing_channel) 组合的完整计费语义。
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ChannelContract {
    pub app_id: &'static str,
    pub billing_channel: &'static str,
    pub kind: ChannelKind,
    /// 该渠道价格的计量单位标识。
    pub pricing_unit: &'static str,
    /// 官方 token 列价合同是否适用于该渠道。仅 `api` 为 true。
    pub token_list_price_applies: bool,
    /// 实付金额（账号折扣、信用额度、税后）。
    pub actual_spend: FieldAttestation,
    /// 剩余额度：API 渠道指预付余额/信用，订阅渠道指周期额度。
    pub remaining_quota: FieldAttestation,
    /// 账号级限速。
    pub rate_limits: FieldAttestation,
    /// 额度/账单的重置周期。
    pub reset_cycle: FieldAttestation,
    /// 该渠道的机会成本说明。
    pub opportunity_cost: &'static str,
}

const API_SPEND: FieldAttestation = unknown(
    "no provider billing/usage API is integrated; official list prices exclude account credits, negotiated rates, and taxes, so the actually billed amount is unknown",
);
const API_BALANCE: FieldAttestation = unknown(
    "prepaid balance and credit state are account-private and are not exposed by any integrated official source",
);
const API_RATE: FieldAttestation = unknown(
    "per-account rate limits depend on the provider tier and are not published in a machine-readable form integrated here",
);
const API_RESET: FieldAttestation = unknown(
    "metered API spend has no attested billing or quota reset period in the integrated sources",
);
const SUB_SPEND: FieldAttestation = unknown(
    "subscription plans bill per period, not per token; no invoice line is mapped to a request",
);
const SUB_QUOTA: FieldAttestation = unknown(
    "plan usage windows are enforced server-side; no official machine-readable quota endpoint is integrated",
);
const SUB_RATE: FieldAttestation = unknown(
    "subscription throughput limits and throttling rules are provider-defined and not attested",
);
const SUB_RESET: FieldAttestation = unknown(
    "quota window boundaries (for example rolling usage windows) are provider-defined, variable, and not attested",
);
const SUB_OPPORTUNITY: &str = "requests consume periodically-resetting plan quota; a marginal token price must never be derived by dividing the subscription fee by any token count";

const fn api_channel(app_id: &'static str) -> ChannelContract {
    ChannelContract {
        app_id,
        billing_channel: CHANNEL_API,
        kind: ChannelKind::ApiMeteredUsd,
        pricing_unit: "usd_per_million_tokens",
        token_list_price_applies: true,
        actual_spend: API_SPEND,
        remaining_quota: API_BALANCE,
        rate_limits: API_RATE,
        reset_cycle: API_RESET,
        opportunity_cost:
            "metered USD per token at the official list price; no subscription quota is consumed",
    }
}

const fn subscription_channel(app_id: &'static str) -> ChannelContract {
    ChannelContract {
        app_id,
        billing_channel: CHANNEL_SUBSCRIPTION,
        kind: ChannelKind::SubscriptionQuota,
        pricing_unit: "provider_quota_units",
        token_list_price_applies: false,
        actual_spend: SUB_SPEND,
        remaining_quota: SUB_QUOTA,
        rate_limits: SUB_RATE,
        reset_cycle: SUB_RESET,
        opportunity_cost: SUB_OPPORTUNITY,
    }
}

/// 受管应用与其可用计费渠道的静态合同。DeepSeek 官方发行只有充值式
/// API（dsh 适配不宣称订阅 OAuth），因此没有订阅渠道。
const CATALOG: [ChannelContract; 13] = [
    api_channel("codex"),
    subscription_channel("codex"),
    api_channel("kimi-cli"),
    subscription_channel("kimi-cli"),
    api_channel("claude"),
    subscription_channel("claude"),
    api_channel("deepseek"),
    // MiMo Code is a multi-provider harness: BYOK API keys (api channel) plus
    // Xiaomi-managed subscription accounts; both attestation sets are absent.
    api_channel("mimo"),
    subscription_channel("mimo"),
    api_channel("kimi-code"),
    subscription_channel("kimi-code"),
    api_channel("minimax"),
    subscription_channel("minimax"),
];

pub fn catalog() -> &'static [ChannelContract] {
    &CATALOG
}

pub fn channel(app_id: &str, billing_channel: &str) -> Option<&'static ChannelContract> {
    CATALOG
        .iter()
        .find(|contract| contract.app_id == app_id && contract.billing_channel == billing_channel)
}

/// 非 `api` 渠道请求报价时的统一阻塞原因（单一事实源）。
pub fn non_api_block_reason(billing_channel: &str) -> String {
    match billing_channel {
        CHANNEL_SUBSCRIPTION => format!(
            "Only direct API billing has a verified token-price contract; the subscription channel is metered in provider quota units ({SUB_QUOTA_REASON})"
        ),
        other => format!(
            "Only direct API billing has a verified token-price contract; the '{other}' billing channel has no verified token-price contract"
        ),
    }
}
const SUB_QUOTA_REASON: &str =
    "plan quota is unknown and must not be priced by dividing the subscription fee by tokens";

/// Automatic 派工在计费维度的解锁清单。每一条都对应一个必须从
/// `unknown` 变成可核验状态的账号级字段；清单为空前 `auto_dispatch_ready`
/// 保持 false。
pub fn dispatch_blockers() -> Vec<String> {
    let mut blockers = Vec::new();
    for contract in CATALOG {
        for (field, attestation) in [
            ("actual_spend", contract.actual_spend),
            ("remaining_quota", contract.remaining_quota),
            ("rate_limits", contract.rate_limits),
            ("reset_cycle", contract.reset_cycle),
        ] {
            if attestation.status == UNKNOWN {
                blockers.push(format!(
                    "billing:{}/{}/{}: {}",
                    contract.app_id, contract.billing_channel, field, attestation.reason
                ));
            }
        }
    }
    blockers
}

/// 校验目录自身的不变量；供测试与将来的目录扩展使用。
pub fn validate_invariants() -> Result<(), String> {
    let managed = ["codex", "kimi-cli", "claude", "deepseek"];
    for app in managed {
        if !CATALOG
            .iter()
            .any(|contract| contract.app_id == app && contract.billing_channel == CHANNEL_API)
        {
            return Err(format!(
                "managed app {app} is missing its api billing channel"
            ));
        }
    }
    for contract in CATALOG {
        match contract.kind {
            ChannelKind::ApiMeteredUsd => {
                if !contract.token_list_price_applies
                    || contract.pricing_unit != "usd_per_million_tokens"
                {
                    return Err(format!(
                        "api channel {}/{} must carry the official USD token list-price semantics",
                        contract.app_id, contract.billing_channel
                    ));
                }
            }
            ChannelKind::SubscriptionQuota => {
                if contract.token_list_price_applies
                    || contract.pricing_unit == "usd_per_million_tokens"
                {
                    return Err(format!(
                        "subscription channel {}/{} must never inherit USD-per-token semantics",
                        contract.app_id, contract.billing_channel
                    ));
                }
            }
        }
        for (field, attestation) in [
            ("actual_spend", contract.actual_spend),
            ("remaining_quota", contract.remaining_quota),
            ("rate_limits", contract.rate_limits),
            ("reset_cycle", contract.reset_cycle),
        ] {
            if attestation.status != UNKNOWN || attestation.reason.trim().is_empty() {
                return Err(format!(
                    "field {field} on {}/{} must be an explicit non-empty unknown until an official source is integrated",
                    contract.app_id, contract.billing_channel
                ));
            }
        }
    }
    Ok(())
}

/// 目录的 JSON 形态（嵌入 `/api/v1/pricing` 状态）。
pub fn catalog_json() -> serde_json::Value {
    serde_json::json!(CATALOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_managed_apps_and_holds_invariants() {
        validate_invariants().unwrap();
        // 订阅渠道只出现在有官方订阅登录的适配器上。
        let subscription_apps: Vec<_> = CATALOG
            .iter()
            .filter(|contract| contract.kind == ChannelKind::SubscriptionQuota)
            .map(|contract| contract.app_id)
            .collect();
        assert_eq!(
            subscription_apps,
            [
                "codex",
                "kimi-cli",
                "claude",
                "mimo",
                "kimi-code",
                "minimax"
            ]
        );
        assert!(channel("deepseek", CHANNEL_SUBSCRIPTION).is_none());
        assert!(channel("codex", CHANNEL_API).is_some());
        assert!(channel("unknown-app", CHANNEL_API).is_none());
    }

    #[test]
    fn subscription_channels_never_carry_usd_per_token_semantics() {
        for contract in CATALOG {
            if contract.kind == ChannelKind::SubscriptionQuota {
                assert!(!contract.token_list_price_applies);
                assert_eq!(contract.pricing_unit, "provider_quota_units");
                assert!(contract.opportunity_cost.contains("never be derived"));
            } else {
                assert!(contract.token_list_price_applies);
                assert_eq!(contract.pricing_unit, "usd_per_million_tokens");
            }
        }
    }

    #[test]
    fn dispatch_blockers_are_explicit_until_sources_exist() {
        let blockers = dispatch_blockers();
        assert_eq!(blockers.len(), CATALOG.len() * 4);
        assert!(blockers
            .iter()
            .all(|blocker| blocker.starts_with("billing:")));
        assert!(blockers
            .iter()
            .any(|blocker| blocker.contains("remaining_quota")
                && blocker.contains("kimi-cli/subscription")));
    }

    #[test]
    fn non_api_reasons_stay_single_sourced() {
        let subscription = non_api_block_reason(CHANNEL_SUBSCRIPTION);
        assert!(subscription.contains("subscription"));
        assert!(subscription.contains("provider quota units"));
        let other = non_api_block_reason("proxy");
        assert!(other.contains("'proxy'"));
    }

    #[test]
    fn catalog_json_keeps_its_fields() {
        let value = catalog_json();
        let entries = value.as_array().unwrap();
        assert_eq!(entries.len(), CATALOG.len());
        for entry in entries {
            for field in [
                "app_id",
                "billing_channel",
                "kind",
                "pricing_unit",
                "token_list_price_applies",
                "actual_spend",
                "remaining_quota",
                "rate_limits",
                "reset_cycle",
                "opportunity_cost",
            ] {
                assert!(entry.get(field).is_some(), "missing field {field}");
            }
        }
    }
}
