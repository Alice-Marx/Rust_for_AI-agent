//! Explainable, snapshot-bound model routing.
//!
//! This module only makes a routing decision from already verified evidence. It
//! deliberately does not refresh network sources, inspect credentials, or start
//! an official tool. Callers must keep the returned decision with the execution
//! record before dispatching a native workflow.

use crate::{
    model_intelligence::Snapshot,
    native_executor,
    pricing::{ModelQuote, PriceSnapshot},
    team_store::ExecutorBinding,
};
use anyhow::{ensure, Context, Result};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const ROUTING_POLICY_VERSION: &str = "routing-v1";
const MAX_TOKEN_ESTIMATE: u64 = 100_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoutingCandidate {
    /// The exact model ID that the native tool will receive.
    pub binding: ExecutorBinding,
    /// The exact LiveBench row to use. This is explicit so aliases and family
    /// names can never silently inherit a score.
    pub benchmark_model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoutingRequest {
    pub candidates: Vec<RoutingCandidate>,
    /// Published LiveBench category names, not the website's global average.
    pub required_categories: Vec<String>,
    /// Optional category weights. Missing weights use equal weighting.
    #[serde(default)]
    pub category_weights: BTreeMap<String, f64>,
    #[serde(default)]
    pub minimum_quality: Option<f64>,
    #[serde(default = "default_billing_channel")]
    pub billing_channel: String,
    #[serde(default)]
    pub estimated_input_tokens: Option<u64>,
    #[serde(default)]
    pub estimated_output_tokens: Option<u64>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
}

/// Persistable routing constraints for a Team. Candidate bindings are always
/// read from the Team record so a preview cannot evaluate models that the Team
/// did not declare when it was created.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoutingPolicy {
    pub required_categories: Vec<String>,
    #[serde(default)]
    pub category_weights: BTreeMap<String, f64>,
    #[serde(default)]
    pub minimum_quality: Option<f64>,
    #[serde(default = "default_billing_channel")]
    pub billing_channel: String,
    #[serde(default)]
    pub estimated_input_tokens: Option<u64>,
    #[serde(default)]
    pub estimated_output_tokens: Option<u64>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
}

impl RoutingPolicy {
    pub fn into_request(self, candidates: &[ExecutorBinding]) -> RoutingRequest {
        RoutingRequest {
            candidates: candidates
                .iter()
                .cloned()
                .map(|binding| RoutingCandidate {
                    benchmark_model: binding.model.clone(),
                    binding,
                })
                .collect(),
            required_categories: self.required_categories,
            category_weights: self.category_weights,
            minimum_quality: self.minimum_quality,
            billing_channel: self.billing_channel,
            estimated_input_tokens: self.estimated_input_tokens,
            estimated_output_tokens: self.estimated_output_tokens,
            budget_usd: self.budget_usd,
        }
    }
}

/// HTTP-facing name retained for the explicit preview endpoint.
pub type RoutingPreviewRequest = RoutingPolicy;

fn default_billing_channel() -> String {
    "api".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Eligible,
    Rejected,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CandidateDecision {
    pub candidate: RoutingCandidate,
    pub status: CandidateStatus,
    pub quality_score: Option<f64>,
    pub estimated_cost_usd: Option<f64>,
    pub quote_model: Option<String>,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoutingDecision {
    pub policy_version: String,
    pub status: String,
    pub epoch_id: String,
    pub benchmark_snapshot_id: String,
    pub price_snapshot_id: String,
    pub required_categories: Vec<String>,
    pub category_weights: BTreeMap<String, f64>,
    pub billing_channel: String,
    pub estimated_input_tokens: Option<u64>,
    pub estimated_output_tokens: Option<u64>,
    pub budget_usd: Option<f64>,
    pub candidates: Vec<CandidateDecision>,
    pub selected: Option<RoutingCandidate>,
    pub selected_quality_score: Option<f64>,
    pub selected_estimated_cost_usd: Option<f64>,
    pub explanation: String,
}

/// Decide from one immutable benchmark snapshot and one immutable price
/// snapshot. The function is deterministic for the same inputs.
pub fn decide(
    request: &RoutingRequest,
    benchmark: &Snapshot,
    pricing: &PriceSnapshot,
) -> Result<RoutingDecision> {
    validate_request(request)?;
    ensure!(
        !benchmark.id.trim().is_empty(),
        "benchmark snapshot ID is empty"
    );
    ensure!(!pricing.id.trim().is_empty(), "price snapshot ID is empty");
    DateTime::parse_from_rfc3339(&benchmark.checked_at)
        .context("benchmark snapshot timestamp is invalid")?;
    DateTime::parse_from_rfc3339(&pricing.checked_at)
        .context("price snapshot timestamp is invalid")?;

    let weights = normalized_weights(request)?;
    let mut decisions = request
        .candidates
        .iter()
        .map(|candidate| evaluate_candidate(request, candidate, &weights, benchmark, pricing))
        .collect::<Result<Vec<_>>>()?;

    decisions.sort_by(|left, right| candidate_key(left).cmp(&candidate_key(right)));
    let mut eligible = decisions
        .iter()
        .filter(|candidate| candidate.status == CandidateStatus::Eligible)
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| compare_candidates(left, right));
    let selected = eligible
        .first()
        .map(|candidate| candidate.candidate.clone());
    let selected_quality_score = eligible
        .first()
        .and_then(|candidate| candidate.quality_score);
    let selected_estimated_cost_usd = eligible
        .first()
        .and_then(|candidate| candidate.estimated_cost_usd);
    let epoch_id = format!("{}:{}", benchmark.id, pricing.id);
    let explanation = match selected.as_ref() {
        Some(candidate) => format!(
            "selected exact {} / {} from {} eligible candidate(s); quality is the primary objective and known cost breaks ties",
            candidate.binding.app_id,
            candidate.binding.model,
            eligible.len()
        ),
        None => "no candidate has a verified exact native-tool, benchmark, price and budget contract".into(),
    };

    Ok(RoutingDecision {
        policy_version: ROUTING_POLICY_VERSION.into(),
        status: if selected.is_some() {
            "selected".into()
        } else {
            "blocked".into()
        },
        epoch_id,
        benchmark_snapshot_id: benchmark.id.clone(),
        price_snapshot_id: pricing.id.clone(),
        required_categories: request.required_categories.clone(),
        category_weights: weights,
        billing_channel: request.billing_channel.clone(),
        estimated_input_tokens: request.estimated_input_tokens,
        estimated_output_tokens: request.estimated_output_tokens,
        budget_usd: request.budget_usd,
        candidates: decisions,
        selected,
        selected_quality_score,
        selected_estimated_cost_usd,
        explanation,
    })
}

fn validate_request(request: &RoutingRequest) -> Result<()> {
    ensure!(
        !request.candidates.is_empty(),
        "routing requires at least one candidate"
    );
    ensure!(
        !request.required_categories.is_empty(),
        "routing requires at least one explicit LiveBench category"
    );
    let mut categories = BTreeSet::new();
    for category in &request.required_categories {
        ensure!(
            !category.trim().is_empty() && category == category.trim(),
            "invalid required LiveBench category"
        );
        ensure!(
            categories.insert(category),
            "duplicate required LiveBench category"
        );
    }
    ensure!(
        request
            .category_weights
            .keys()
            .all(|key| categories.contains(key)),
        "category weights contain a category that is not required"
    );
    if let Some(minimum) = request.minimum_quality {
        ensure!(
            minimum.is_finite() && (0.0..=100.0).contains(&minimum),
            "minimum quality must be between 0 and 100"
        );
    }
    ensure!(
        !request.billing_channel.trim().is_empty()
            && request.billing_channel == request.billing_channel.trim()
            && request.billing_channel.len() <= 64,
        "invalid billing channel"
    );
    for estimate in [
        request.estimated_input_tokens,
        request.estimated_output_tokens,
    ] {
        if let Some(tokens) = estimate {
            ensure!(tokens <= MAX_TOKEN_ESTIMATE, "token estimate is too large");
        }
    }
    if let Some(budget) = request.budget_usd {
        ensure!(
            budget.is_finite() && (0.000001..=100_000.0).contains(&budget),
            "budget must be between one microUSD and 100000 USD"
        );
        ensure!(
            request.estimated_input_tokens.is_some() && request.estimated_output_tokens.is_some(),
            "a budget requires both input and output token estimates"
        );
    }
    let mut seen = BTreeSet::new();
    for candidate in &request.candidates {
        ensure!(
            !candidate.benchmark_model.trim().is_empty()
                && candidate.benchmark_model == candidate.benchmark_model.trim(),
            "candidate benchmark model must be an exact non-empty ID"
        );
        let key = serde_json::to_string(candidate)?;
        ensure!(seen.insert(key), "duplicate routing candidate");
    }
    Ok(())
}

fn normalized_weights(request: &RoutingRequest) -> Result<BTreeMap<String, f64>> {
    let mut weights = BTreeMap::new();
    if request.category_weights.is_empty() {
        let equal = 1.0 / request.required_categories.len() as f64;
        for category in &request.required_categories {
            weights.insert(category.clone(), equal);
        }
        return Ok(weights);
    }
    let mut total = 0.0;
    for category in &request.required_categories {
        let weight = *request.category_weights.get(category).unwrap_or(&0.0);
        ensure!(
            weight.is_finite() && weight >= 0.0,
            "category weights must be finite and non-negative"
        );
        total += weight;
    }
    ensure!(
        total.is_finite() && total > 0.0,
        "category weights must sum to a positive value"
    );
    for category in &request.required_categories {
        weights.insert(
            category.clone(),
            request
                .category_weights
                .get(category)
                .copied()
                .unwrap_or(0.0)
                / total,
        );
    }
    Ok(weights)
}

fn evaluate_candidate(
    request: &RoutingRequest,
    candidate: &RoutingCandidate,
    weights: &BTreeMap<String, f64>,
    benchmark: &Snapshot,
    pricing: &PriceSnapshot,
) -> Result<CandidateDecision> {
    let mut reasons = Vec::new();
    let mut quality = None;
    let mut quote_model = None;
    let mut estimated_cost = None;

    if let Err(error) = native_executor::validate_binding(
        &candidate.binding.app_id,
        &candidate.binding.model,
        candidate.binding.reasoning_effort.as_deref(),
        false,
    ) {
        reasons.push(format!("native binding rejected: {error:#}"));
    }
    if request.billing_channel != "api" {
        reasons.push("only direct API billing has a verified token-price contract; subscription and proxy usage remain unknown".into());
    }

    match benchmark
        .models
        .iter()
        .find(|model| model.model == candidate.benchmark_model)
    {
        Some(model) => match score_model(model, benchmark, weights) {
            Ok(score) => quality = Some(score),
            Err(error) => reasons.push(format!("benchmark evidence rejected: {error:#}")),
        },
        None => reasons.push(format!(
            "exact benchmark model '{}' is not present; aliases and family scores are not substituted",
            candidate.benchmark_model
        )),
    }

    match pricing.quotes.iter().find(|quote| {
        quote.model == candidate.binding.model
            && quote.billing_channel == request.billing_channel
            && quote
                .app_ids
                .iter()
                .any(|app| app == &candidate.binding.app_id)
    }) {
        Some(quote) => {
            quote_model = Some(quote.model.clone());
            match estimate_cost(request, quote) {
                Ok(cost) => estimated_cost = cost,
                Err(error) => reasons.push(format!("price evidence rejected: {error:#}")),
            }
        }
        None => reasons.push(format!(
            "no exact verified price quote for app='{}', model='{}', billing_channel='{}'",
            candidate.binding.app_id, candidate.binding.model, request.billing_channel
        )),
    }

    if let (Some(minimum), Some(score)) = (request.minimum_quality, quality) {
        if score < minimum {
            reasons.push(format!(
                "quality score {score:.3} is below required minimum {minimum:.3}"
            ));
        }
    }
    if let (Some(budget), Some(cost)) = (request.budget_usd, estimated_cost) {
        if cost > budget {
            reasons.push(format!(
                "estimated cost {cost:.6} USD exceeds budget {budget:.6} USD"
            ));
        }
    }

    Ok(CandidateDecision {
        candidate: candidate.clone(),
        status: if reasons.is_empty() {
            CandidateStatus::Eligible
        } else {
            CandidateStatus::Rejected
        },
        quality_score: quality,
        estimated_cost_usd: estimated_cost,
        quote_model,
        reasons,
    })
}

fn score_model(
    model: &crate::model_intelligence::ModelScore,
    benchmark: &Snapshot,
    weights: &BTreeMap<String, f64>,
) -> Result<f64> {
    let mut score = 0.0;
    for (category, weight) in weights {
        let tasks = benchmark.category_tasks.get(category).with_context(|| {
            format!("category '{category}' is absent from this benchmark snapshot")
        })?;
        ensure!(
            !tasks.is_empty(),
            "benchmark category '{category}' has no tasks"
        );
        let mut category_total = 0.0;
        for task in tasks {
            let task_score = model.scores.get(task).with_context(|| {
                format!("model is missing benchmark task '{task}' in category '{category}'")
            })?;
            ensure!(
                task_score.is_finite() && (0.0..=100.0).contains(task_score),
                "benchmark score is outside 0..=100"
            );
            category_total += task_score;
        }
        score += weight * category_total / tasks.len() as f64;
    }
    ensure!(
        score.is_finite() && (0.0..=100.0).contains(&score),
        "computed quality score is invalid"
    );
    Ok(score)
}

fn estimate_cost(request: &RoutingRequest, quote: &ModelQuote) -> Result<Option<f64>> {
    ensure!(
        quote.currency == "USD",
        "only USD quotes can be compared by this policy"
    );
    ensure!(
        quote.unit == "per_1_000_000_tokens",
        "quote unit is not supported by this policy"
    );
    ensure!(
        quote.unknowns.is_empty(),
        "quote contains unresolved pricing conditions"
    );
    ensure!(
        quote.tiers.len() == 1,
        "quote has multiple context tiers; an exact context tier is required"
    );
    let tier = &quote.tiers[0];
    ensure!(
        tier.unknowns.is_empty(),
        "selected price tier contains unresolved conditions"
    );
    match (
        request.estimated_input_tokens,
        request.estimated_output_tokens,
    ) {
        (Some(input), Some(output)) => Ok(Some(
            (input as f64 * tier.input + output as f64 * tier.output) / 1_000_000.0,
        )),
        _ => Ok(None),
    }
}

fn candidate_key(candidate: &CandidateDecision) -> (String, String, String) {
    (
        candidate.candidate.binding.app_id.clone(),
        candidate.candidate.binding.model.clone(),
        candidate.candidate.benchmark_model.clone(),
    )
}

fn compare_candidates(left: &CandidateDecision, right: &CandidateDecision) -> std::cmp::Ordering {
    right
        .quality_score
        .unwrap_or(0.0)
        .total_cmp(&left.quality_score.unwrap_or(0.0))
        .then_with(
            || match (left.estimated_cost_usd, right.estimated_cost_usd) {
                (Some(left), Some(right)) => left.total_cmp(&right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        )
        .then_with(|| candidate_key(left).cmp(&candidate_key(right)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model_intelligence::ModelScore, pricing::PriceTier};
    use std::collections::BTreeMap;

    fn benchmark() -> Snapshot {
        Snapshot {
            parser_version: 1,
            id: "bench-1".into(),
            release: "2026-06-25".into(),
            checked_at: "2026-09-22T00:00:00Z".into(),
            new_livebench_sha: "a".repeat(40),
            livebench_sha: "b".repeat(40),
            table_url: "https://example.invalid/table".into(),
            categories_url: "https://example.invalid/categories".into(),
            constants_url: "https://example.invalid/constants".into(),
            table_sha256: "c".repeat(64),
            categories_sha256: "d".repeat(64),
            constants_sha256: "e".repeat(64),
            models: vec![
                ModelScore {
                    model: "gpt-4o".into(),
                    scores: BTreeMap::from([("coding".into(), 92.0)]),
                    missing_tasks: vec![],
                },
                ModelScore {
                    model: "kimi-for-coding".into(),
                    scores: BTreeMap::from([("coding".into(), 84.0)]),
                    missing_tasks: vec![],
                },
            ],
            categories: vec!["Coding".into()],
            category_tasks: BTreeMap::from([("Coding".into(), vec!["coding".into()])]),
            raw_table: String::new(),
            raw_categories: serde_json::json!({}),
            raw_categories_json: "{}".into(),
            raw_constants: String::new(),
        }
    }

    fn quote(model: &str, app_id: &str, input: f64, output: f64) -> ModelQuote {
        ModelQuote {
            provider: if app_id == "codex" { "openai" } else { "kimi" }.into(),
            app_ids: vec![app_id.into()],
            model: model.into(),
            billing_channel: "api".into(),
            billing_scope: "first_party_direct_api_list_price".into(),
            currency: "USD".into(),
            unit: "per_1_000_000_tokens".into(),
            tiers: vec![PriceTier {
                name: "standard".into(),
                input,
                cached_input: None,
                cache_write: None,
                cache_write_5m: None,
                cache_write_1h: None,
                output,
                context_window_tokens: Some(128_000),
                conditions: vec![],
                unknowns: vec![],
            }],
            source_url: "https://example.invalid/pricing".into(),
            source_sha256: "f".repeat(64),
            checked_at: "2026-09-22T00:00:00Z".into(),
            conditions: vec![],
            unknowns: vec![],
        }
    }

    fn prices() -> PriceSnapshot {
        PriceSnapshot {
            parser_version: 1,
            id: "price-1".into(),
            checked_at: "2026-09-22T00:00:00Z".into(),
            sources: vec![],
            quotes: vec![
                quote("gpt-4o", "codex", 1.0, 2.0),
                quote("kimi-for-coding", "kimi-cli", 0.5, 1.0),
            ],
        }
    }

    fn candidate(app_id: &str, model: &str) -> RoutingCandidate {
        RoutingCandidate {
            binding: ExecutorBinding {
                app_id: app_id.into(),
                model: model.into(),
                reasoning_effort: None,
            },
            benchmark_model: model.into(),
        }
    }

    fn request() -> RoutingRequest {
        RoutingRequest {
            candidates: vec![
                candidate("codex", "gpt-4o"),
                candidate("kimi-cli", "kimi-for-coding"),
            ],
            required_categories: vec!["Coding".into()],
            category_weights: BTreeMap::new(),
            minimum_quality: Some(80.0),
            billing_channel: "api".into(),
            estimated_input_tokens: Some(100_000),
            estimated_output_tokens: Some(10_000),
            budget_usd: Some(0.2),
        }
    }

    #[test]
    fn chooses_highest_quality_and_keeps_evidence_ids() {
        let decision = decide(&request(), &benchmark(), &prices()).unwrap();
        assert_eq!(decision.status, "selected");
        assert_eq!(decision.selected.unwrap().binding.app_id, "codex");
        assert_eq!(decision.epoch_id, "bench-1:price-1");
        assert_eq!(decision.selected_quality_score, Some(92.0));
        assert_eq!(decision.candidates.len(), 2);
    }

    #[test]
    fn missing_category_score_blocks_only_that_candidate() {
        let mut snapshot = benchmark();
        snapshot.models[0].scores.clear();
        let decision = decide(&request(), &snapshot, &prices()).unwrap();
        assert_eq!(decision.status, "selected");
        let codex = decision
            .candidates
            .iter()
            .find(|candidate| candidate.candidate.binding.app_id == "codex")
            .unwrap();
        assert_eq!(codex.status, CandidateStatus::Rejected);
        assert!(codex
            .reasons
            .iter()
            .any(|reason| reason.contains("missing benchmark task")));
        assert_eq!(decision.selected.unwrap().binding.app_id, "kimi-cli");
    }

    #[test]
    fn subscription_pricing_never_inherits_api_quotes() {
        let mut request = request();
        request.billing_channel = "subscription".into();
        let decision = decide(&request, &benchmark(), &prices()).unwrap();
        assert_eq!(decision.status, "blocked");
        assert!(decision.selected.is_none());
        assert!(decision.candidates.iter().all(|candidate| candidate
            .reasons
            .iter()
            .any(|reason| reason.contains("subscription"))));
    }

    #[test]
    fn malformed_budget_is_rejected_before_selection() {
        let mut request = request();
        request.budget_usd = Some(1.0);
        request.estimated_output_tokens = None;
        let error = decide(&request, &benchmark(), &prices()).unwrap_err();
        assert!(error.to_string().contains("both input and output"));
    }

    #[test]
    fn ambiguous_price_tiers_do_not_become_a_fake_estimate() {
        let mut pricing = prices();
        let tier = pricing.quotes[0].tiers[0].clone();
        pricing.quotes[0].tiers.push(tier);
        let decision = decide(&request(), &benchmark(), &pricing).unwrap();
        let codex = decision
            .candidates
            .iter()
            .find(|candidate| candidate.candidate.binding.app_id == "codex")
            .unwrap();
        assert_eq!(codex.status, CandidateStatus::Rejected);
        assert!(codex
            .reasons
            .iter()
            .any(|reason| reason.contains("multiple context tiers")));
        assert_eq!(decision.selected.unwrap().binding.app_id, "kimi-cli");
    }
}
