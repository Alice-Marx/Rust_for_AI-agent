//! Source-backed, direct-API list prices. These are not subscription prices,
//! provider availability guarantees, or an identity bridge for benchmark variants.
//! Only bounded, recognized document schemas are decoded; changed schemas fail
//! closed for that provider. Every refresh fetches all five official sources:
//! Anthropic quotes additionally require the same epoch's official model
//! overview to join display names to attested exact API model IDs.

use anyhow::{ensure, Context, Result};
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::RwLock,
    time::Duration,
};
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_SOURCE: usize = 4 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 48 * 1024 * 1024;
const MAX_AGE_SECONDS: i64 = 60 * 60;
const OPENAI_URL: &str = "https://developers.openai.com/api/docs/pricing.md";
const KIMI_URL: &str = "https://platform.moonshot.ai/docs/pricing/chat.md";
const ANTHROPIC_URL: &str = "https://platform.claude.com/docs/en/about-claude/pricing.md";
const ANTHROPIC_MODELS_URL: &str = "https://platform.claude.com/docs/en/models/overview.md";
const DEEPSEEK_URL: &str = "https://api-docs.deepseek.com/quick_start/pricing";
const SOURCE_SPECS: [(&str, &str); 5] = [
    ("openai", OPENAI_URL),
    ("kimi", KIMI_URL),
    // The identity join source must decode before the pricing table.
    ("anthropic-models", ANTHROPIC_MODELS_URL),
    ("anthropic", ANTHROPIC_URL),
    ("deepseek", DEEPSEEK_URL),
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PriceTier {
    pub name: String,
    pub input: f64,
    pub cached_input: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub output: f64,
    pub context_window_tokens: Option<u64>,
    pub conditions: Vec<String>,
    pub unknowns: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelQuote {
    pub provider: String,
    pub app_ids: Vec<String>,
    /// Exact model spelling published by the official source; no fuzzy aliases.
    pub model: String,
    pub billing_channel: String,
    pub billing_scope: String,
    pub currency: String,
    pub unit: String,
    pub tiers: Vec<PriceTier>,
    pub source_url: String,
    pub source_sha256: String,
    pub checked_at: String,
    pub conditions: Vec<String>,
    pub unknowns: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PriceSource {
    pub provider: String,
    pub requested_url: String,
    pub final_url: Option<String>,
    pub checked_at: String,
    /// verified, blocked (fetched but unsupported semantics), or failed.
    pub status: String,
    pub reason: Option<String>,
    pub sha256: Option<String>,
    pub raw: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PriceSnapshot {
    pub parser_version: u32,
    pub id: String,
    pub checked_at: String,
    pub sources: Vec<PriceSource>,
    pub quotes: Vec<ModelQuote>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QuoteResult {
    /// verified, cached, or blocked. Cached quotes are for display only. A
    /// verified quote still carries conditions and must belong to the current
    /// scheduling round's mandatory refresh epoch.
    pub status: String,
    pub reason: Option<String>,
    pub snapshot_id: Option<String>,
    pub checked_at: Option<String>,
    pub quote: Option<ModelQuote>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Failure {
    checked_at: String,
    error: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredStatus {
    latest_id: Option<String>,
    latest_sha256: Option<String>,
    latest_failure: Option<Failure>,
}
struct State {
    stored: StoredStatus,
    latest: Option<PriceSnapshot>,
    /// Never persisted: reading the cache cannot claim a network check occurred.
    live_snapshot_id: Option<String>,
}

pub struct PriceService {
    directory: PathBuf,
    client: reqwest::Client,
    refreshing: Mutex<()>,
    state: RwLock<State>,
}

impl PriceService {
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let directory = data_dir.into().join("pricing");
        fs::create_dir_all(directory.join("snapshots"))?;
        let status_path = directory.join("status.json");
        let mut stored: StoredStatus = match bounded_read(&status_path, 64 * 1024) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("pricing status is corrupt")?,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                StoredStatus::default()
            }
            Err(error) => return Err(error),
        };
        let latest = match stored.latest_id.as_deref() {
            Some(id) => match load_snapshot(&directory, id, stored.latest_sha256.as_deref()) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    stored.latest_failure = Some(Failure {
                        checked_at: now(),
                        error: bounded_error(&format!(
                            "cached price snapshot failed validation: {error:#}"
                        )),
                    });
                    None
                }
            },
            None => None,
        };
        let client = reqwest::Client::builder()
            .user_agent("Wonderland-OfficialPricing/1")
            .default_headers(
                [(
                    reqwest::header::CACHE_CONTROL,
                    reqwest::header::HeaderValue::from_static("no-cache"),
                )]
                .into_iter()
                .collect(),
            )
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 {
                    return attempt.error("pricing redirect limit exceeded");
                }
                if !allowed_url(attempt.url()) {
                    return attempt.error("pricing redirect left official HTTPS sources");
                }
                attempt.follow()
            }))
            .build()?;
        Ok(Self {
            directory,
            client,
            refreshing: Mutex::new(()),
            state: RwLock::new(State {
                stored,
                latest,
                live_snapshot_id: None,
            }),
        })
    }

    pub async fn refresh(&self) -> Result<PriceSnapshot> {
        let _lock = self.refreshing.lock().await;
        let checked_at = now();
        let results = futures_util::future::join_all(
            SOURCE_SPECS
                .into_iter()
                .map(|(provider, url)| self.fetch_source(provider, url, &checked_at)),
        )
        .await;
        let mut snapshot = PriceSnapshot {
            parser_version: 2,
            id: Uuid::new_v4().to_string(),
            checked_at: checked_at.clone(),
            sources: Vec::new(),
            quotes: Vec::new(),
        };
        let (sources, quotes) = decode_all(results);
        snapshot.sources = sources;
        snapshot.quotes = quotes;
        if snapshot.quotes.is_empty() {
            let error = anyhow::anyhow!(
                "No official pricing source passed verification: {}",
                snapshot
                    .sources
                    .iter()
                    .map(|source| format!(
                        "{}: {}",
                        source.provider,
                        source.reason.as_deref().unwrap_or("unknown")
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            // Retain this failed epoch's exact fetched sources for diagnosis;
            // never promote it as the latest verified price snapshot.
            let _ = self.write_snapshot(&snapshot);
            self.record_failure(&checked_at, &error);
            return Err(error);
        }
        if let Err(error) = self.persist(&snapshot) {
            self.record_failure(&checked_at, &error);
            return Err(error);
        }
        Ok(snapshot)
    }

    pub fn latest_snapshot(&self) -> Option<PriceSnapshot> {
        self.state
            .read()
            .ok()
            .and_then(|state| state.latest.clone())
    }

    pub fn status(&self) -> Value {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let blockers = crate::account_billing::dispatch_blockers();
        json!({
            "latest_snapshot":state.latest.as_ref().map(|snapshot| json!({"id":snapshot.id,"checked_at":snapshot.checked_at,"quote_count":snapshot.quotes.len(),"sources":snapshot.sources.iter().map(|source|json!({"provider":source.provider,"status":source.status,"reason":source.reason,"url":source.final_url,"sha256":source.sha256,"checked_at":source.checked_at})).collect::<Vec<_>>()})),
            "latest_failure":state.stored.latest_failure,
            "cache_is_stale":state.stored.latest_failure.is_some() || state.latest.as_ref().is_none_or(|snapshot|!fresh(&snapshot.checked_at)),
            "max_quote_age_seconds":MAX_AGE_SECONDS,
            "online_verified_in_this_process":state.live_snapshot_id.is_some() && state.stored.latest_failure.is_none(),
            "refresh_required_each_dispatch_round":true,
            "subscription_pricing":"unknown; subscription quota is not zero-cost API tokens",
            "billing_channels":crate::account_billing::catalog_json(),
            "dispatch_readiness":{
                "ready":blockers.is_empty(),
                "missing":blockers,
                "scope":"hard-budget dispatch only; budget-less automatic dispatch runs on verified routing decisions with the same cost posture as fixed execution"
            },
            "automatic_dispatch":{
                "no_budget":"enabled: an automatic team starts when a routing-v2 decision selects a candidate from fully attested identity, benchmark and quote evidence",
                "hard_budget":"blocked until native billing settlement closes (H04); the decision is still recorded"
            },
            "auto_dispatch_ready":false,
            "note":"Cached snapshots are for display. Every dispatch round must refresh online and bind quotes to that snapshot ID. Exact direct-API routing also requires a verified endpoint, billing channel, model identity, benchmark variant and matching tier conditions. Claude quotes are keyed by exact API model IDs joined from the same epoch's official model overview; DeepSeek quotes carry peak/off-peak UTC window conditions."
        })
    }

    pub fn quote(&self, app_id: &str, model: &str, billing_channel: &str) -> QuoteResult {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut result = QuoteResult {
            status: "blocked".into(),
            reason: None,
            snapshot_id: state.stored.latest_id.clone(),
            checked_at: state
                .latest
                .as_ref()
                .map(|snapshot| snapshot.checked_at.clone()),
            quote: None,
        };
        let block = if billing_channel != "api" {
            Some(crate::account_billing::non_api_block_reason(
                billing_channel,
            ))
        } else if state.stored.latest_failure.is_some() {
            Some("The latest pricing refresh or cache verification failed; refresh online before cost comparison".to_owned())
        } else if let Some(snapshot) = state.latest.as_ref() {
            if !fresh(&snapshot.checked_at) {
                Some(
                    "The price snapshot is expired; refresh online before cost comparison"
                        .to_owned(),
                )
            } else if let Some(quote) = snapshot.quotes.iter().find(|quote| {
                quote.model == model
                    && quote.billing_channel == billing_channel
                    && quote.app_ids.iter().any(|app| app == app_id)
            }) {
                result.quote = Some(quote.clone());
                if state.live_snapshot_id.as_deref() == Some(snapshot.id.as_str()) {
                    result.status = "verified".into();
                    None
                } else {
                    result.status = "cached".into();
                    Some("Display-only cached quote: this process has not verified the source online; refresh for the current dispatch round".into())
                }
            } else {
                Some("No verified exact app/model/billing-channel quote; benchmark suffixes, aliases and model families are never substituted".to_owned())
            }
        } else {
            Some("No verified official price snapshot is available".to_owned())
        };
        result.reason = block;
        result
    }

    /// A scheduler must call `refresh` at the start of each dispatch round, then
    /// pass that snapshot's ID here. Another refresh cannot silently substitute
    /// a different epoch in an already reviewed cost decision.
    pub fn quote_for_epoch(
        &self,
        app_id: &str,
        model: &str,
        billing_channel: &str,
        snapshot_id: &str,
    ) -> QuoteResult {
        let mut quote = self.quote(app_id, model, billing_channel);
        if quote.status != "verified" || quote.snapshot_id.as_deref() != Some(snapshot_id) {
            quote.status = "blocked".into();
            quote.reason = Some("Quote does not belong to the verified online snapshot for this dispatch round; refresh and bind an exact epoch".into());
            quote.quote = None;
        }
        quote
    }

    async fn fetch_source(&self, provider: &str, url: &str, checked_at: &str) -> PriceSource {
        let mut source = PriceSource {
            provider: provider.into(),
            requested_url: url.into(),
            final_url: None,
            checked_at: checked_at.into(),
            status: "failed".into(),
            reason: None,
            sha256: None,
            raw: None,
        };
        let result = async {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .context("official pricing request failed")?;
            ensure!(
                response.status() == reqwest::StatusCode::OK,
                "official pricing returned HTTP {}",
                response.status()
            );
            ensure!(
                allowed_provider_url(provider, response.url()),
                "official pricing changed provider origin"
            );
            source.final_url = Some(response.url().to_string());
            ensure!(
                response
                    .content_length()
                    .is_none_or(|length| length <= MAX_SOURCE as u64),
                "official pricing body too large"
            );
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                ensure!(
                    body.len().saturating_add(chunk.len()) <= MAX_SOURCE,
                    "official pricing body too large"
                );
                body.extend_from_slice(&chunk);
            }
            ensure!(!body.is_empty(), "official pricing body empty");
            source.sha256 = Some(hash(&body));
            source.raw = Some(String::from_utf8(body).context("official pricing is not UTF-8")?);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => source.status = "fetched".into(),
            Err(error) => source.reason = Some(bounded_error(&format!("{error:#}"))),
        }
        source
    }

    fn write_snapshot(&self, snapshot: &PriceSnapshot) -> Result<Vec<u8>> {
        ensure!(
            Uuid::parse_str(&snapshot.id)?.to_string() == snapshot.id,
            "invalid snapshot id"
        );
        let bytes = serde_json::to_vec(snapshot)?;
        ensure!(bytes.len() <= MAX_SNAPSHOT, "pricing snapshot too large");
        publish(
            &self
                .directory
                .join("snapshots")
                .join(format!("{}.json", snapshot.id)),
            &bytes,
            true,
        )?;
        Ok(bytes)
    }
    fn persist(&self, snapshot: &PriceSnapshot) -> Result<()> {
        let bytes = self.write_snapshot(snapshot)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("pricing state lock poisoned"))?;
        let stored = StoredStatus {
            latest_id: Some(snapshot.id.clone()),
            latest_sha256: Some(hash(&bytes)),
            latest_failure: None,
        };
        publish(
            &self.directory.join("status.json"),
            &serde_json::to_vec_pretty(&stored)?,
            false,
        )?;
        state.stored = stored;
        state.latest = Some(snapshot.clone());
        state.live_snapshot_id = Some(snapshot.id.clone());
        Ok(())
    }
    fn record_failure(&self, checked_at: &str, error: &anyhow::Error) {
        if let Ok(mut state) = self.state.write() {
            state.stored.latest_failure = Some(Failure {
                checked_at: checked_at.into(),
                error: bounded_error(&format!("{error:#}")),
            });
            if let Ok(bytes) = serde_json::to_vec(&state.stored) {
                if let Err(error) = publish(&self.directory.join("status.json"), &bytes, false) {
                    tracing::warn!(%error,"Unable to persist pricing failure");
                }
            }
        }
    }
}

/// Identity of one Claude model as attested by the official model overview in
/// the same refresh epoch. The map key is the exact display name shared with
/// the pricing table; nothing is matched by prefix or fuzzy similarity.
#[derive(Clone, Debug, PartialEq)]
struct AnthropicIdentity {
    api_id: String,
    alias: Option<String>,
    context_window_tokens: Option<u64>,
}

fn decode_all(sources: Vec<PriceSource>) -> (Vec<PriceSource>, Vec<ModelQuote>) {
    let mut identities: Option<BTreeMap<String, AnthropicIdentity>> = None;
    let mut join_error: Option<String> = None;
    let mut decoded_sources = Vec::with_capacity(sources.len());
    let mut quotes = Vec::new();
    for mut source in sources {
        let result = match source.provider.as_str() {
            "openai" => source
                .raw
                .as_deref()
                .map(parse_openai)
                .unwrap_or_else(|| Err(anyhow::anyhow!("fetched source has no body"))),
            "kimi" => source
                .raw
                .as_deref()
                .map(parse_kimi)
                .unwrap_or_else(|| Err(anyhow::anyhow!("fetched source has no body"))),
            "anthropic-models" => match source.raw.as_deref().map(parse_anthropic_models) {
                Some(Ok(map)) => {
                    identities = Some(map);
                    Ok(Vec::new())
                }
                Some(Err(error)) => {
                    join_error = Some(bounded_error(&format!("{error:#}")));
                    Err(error)
                }
                None => Err(anyhow::anyhow!("fetched source has no body")),
            },
            "anthropic" => match (&identities, source.raw.as_deref()) {
                (Some(map), Some(raw)) => parse_anthropic(raw, map),
                (None, Some(_)) => Err(anyhow::anyhow!(
                    "the official Anthropic model overview did not verify in this epoch ({}); display names cannot be joined to attested exact API model IDs, so no Claude quote is inferred",
                    join_error.as_deref().unwrap_or("not fetched")
                )),
                (_, None) => Err(anyhow::anyhow!("fetched source has no body")),
            },
            "deepseek" => source
                .raw
                .as_deref()
                .map(parse_deepseek)
                .unwrap_or_else(|| Err(anyhow::anyhow!("fetched source has no body"))),
            _ => Err(anyhow::anyhow!("unsupported official pricing source")),
        };
        match result {
            Ok(mut provider_quotes) => {
                for quote in &mut provider_quotes {
                    quote.source_url = source
                        .final_url
                        .clone()
                        .unwrap_or_else(|| source.requested_url.clone());
                    quote.source_sha256 = source.sha256.clone().unwrap_or_default();
                    quote.checked_at = source.checked_at.clone();
                }
                source.status = "verified".into();
                source.reason = None;
                quotes.extend(provider_quotes);
            }
            Err(error) => {
                source.status = "blocked".into();
                source.reason = Some(bounded_error(&format!("{error:#}")));
            }
        }
        decoded_sources.push(source);
    }
    (decoded_sources, quotes)
}

fn new_quote(provider: &str, model: &str, tiers: Vec<PriceTier>) -> ModelQuote {
    ModelQuote {provider:provider.into(),app_ids:match provider {"openai"=>vec!["codex".into()],"kimi"=>vec!["kimi-cli".into(),"kimi-code".into()],"anthropic"=>vec!["claude".into()],"deepseek"=>vec!["deepseek".into()],_=>Vec::new()},model:model.into(),billing_channel:"api".into(),billing_scope:"first_party_direct_api_list_price".into(),currency:"USD".into(),unit:"per_1_000_000_tokens".into(),tiers,source_url:String::new(),source_sha256:String::new(),checked_at:String::new(),conditions:vec!["Direct provider API only; verify the actual endpoint and account billing channel before use".into(),"Public token list prices; taxes, account discounts, credits and non-token tool fees are excluded".into()],unknowns:vec!["Account access, negotiated rates and provider rate limits are not verified".into()]}
}

fn parse_openai(raw: &str) -> Result<Vec<ModelQuote>> {
    ensure!(
        raw.len() <= MAX_SOURCE && raw.contains("Prices per 1M tokens."),
        "OpenAI pricing unit/schema changed"
    );
    let marker = "### Standard pricing data";
    ensure!(
        raw.matches(marker).count() == 1,
        "OpenAI standard pricing section missing or ambiguous"
    );
    let section = raw.split_once(marker).unwrap().1;
    let rows = section
        .lines()
        .skip_while(|line| !line.trim().starts_with('|'))
        .take_while(|line| line.trim().starts_with('|'))
        .map(markdown_cells)
        .collect::<Result<Vec<_>>>()?;
    let expected = [
        "Model",
        "Short context input",
        "Short context cached input",
        "Short context cache writes",
        "Short context output",
        "Long context input",
        "Long context cached input",
        "Long context cache writes",
        "Long context output",
    ];
    ensure!(
        rows.len() > 2 && rows.len() <= 502 && rows[0] == expected,
        "OpenAI standard pricing table columns changed"
    );
    ensure!(
        rows[1].len() == 9
            && rows[1].iter().all(
                |cell| !cell.is_empty() && cell.bytes().all(|byte| matches!(byte, b'-' | b':'))
            ),
        "OpenAI pricing table delimiter changed"
    );
    let decorated = Regex::new(r"^([a-z0-9][a-z0-9.-]*) \(<([1-9][0-9]*)K context length\)$")?;
    let mut seen = BTreeSet::new();
    let mut quotes = Vec::new();
    for row in &rows[2..] {
        ensure!(row.len() == 9, "OpenAI pricing row has unexpected columns");
        let (model, threshold) = if let Some(capture) = decorated.captures(&row[0]) {
            (
                capture[1].to_owned(),
                Some(
                    capture[2]
                        .parse::<u64>()?
                        .checked_mul(1000)
                        .context("context threshold overflow")?,
                ),
            )
        } else {
            validate_model(&row[0])?;
            (row[0].clone(), None)
        };
        ensure!(seen.insert(model.clone()), "duplicate OpenAI model price");
        let short = read_openai_tier(&row[1..5], "standard_short_context", threshold)?;
        let long = if row[5..9].iter().all(|cell| cell == "-") {
            None
        } else {
            Some(read_openai_tier(
                &row[5..9],
                "standard_long_context",
                threshold,
            )?)
        };
        let mut tiers = vec![short];
        if let Some(long) = long {
            tiers.push(long)
        }
        let mut quote = new_quote("openai", &model, tiers);
        quote.conditions.push("Standard synchronous text pricing only; batch, flex, fast/priority, regional processing, media and fine-tuning prices are separate".into());
        quote.unknowns.push("For models with short/long prices, confirm the exact context boundary and cache-write TTL against the model documentation before estimating a request".into());
        quotes.push(quote);
    }
    ensure!(!quotes.is_empty(), "OpenAI standard price table empty");
    Ok(quotes)
}

fn markdown_cells(line: &str) -> Result<Vec<String>> {
    let line = line.trim();
    ensure!(
        line.starts_with('|') && line.ends_with('|'),
        "malformed pricing markdown row"
    );
    Ok(line[1..line.len() - 1]
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect())
}
fn read_openai_tier(cells: &[String], name: &str, threshold: Option<u64>) -> Result<PriceTier> {
    let input = required_money(&cells[0])?;
    let cached_input = optional_money(&cells[1])?;
    let cache_write = optional_money(&cells[2])?;
    let output = required_money(&cells[3])?;
    let mut unknowns = Vec::new();
    if threshold.is_none() {
        unknowns.push("Context boundary is not stated in this row; short/long classification needs separate model documentation".into());
    }
    if cached_input.is_none() {
        unknowns.push("Cached-input price is not published in this row; not assumed free".into());
    }
    if cache_write.is_some() {
        unknowns.push("Cache-write duration/applicability is not decoded from this table".into());
    }
    Ok(PriceTier {name:name.into(),input,cached_input,cache_write,cache_write_5m:None,cache_write_1h:None,output,context_window_tokens:None,conditions:vec![threshold.map(|value|format!("Source row labels the short-context tier as <{value} tokens; equality and long-context applicability require model documentation")).unwrap_or_else(||"Select the documented short/long context tier explicitly".into())],unknowns})
}

fn parse_kimi(raw: &str) -> Result<Vec<ModelQuote>> {
    ensure!(
        raw.len() <= MAX_SOURCE
            && raw.contains("Here, 1M = 1,000,000.")
            && raw.contains("Prices exclude applicable taxes"),
        "Kimi pricing unit/tax schema changed"
    );
    ensure!(
        raw.contains("If no TTL is specified, the 5min tier applies by default.")
            && raw.contains("with no additional cache write charge"),
        "Kimi cache billing semantics changed"
    );
    let tables = Regex::new(r"(?s)<DocTable\s+columns=\{\[(.*?)\]\}\s+rows=\{\[(.*?)\]\}\s*/>")?;
    let column = Regex::new(r#"\{\s*title:\s*"([^"]+)"\s*,\s*width:\s*"[0-9]+%"\s*\}\s*,?"#)?;
    let money_jsx = Regex::new(r#"<>\{\s*"\$"\s*\}([0-9]+(?:\.[0-9]{1,6})?)</>"#)?;
    let mut quotes = Vec::new();
    let mut seen = BTreeSet::new();
    let mut table_count = 0;
    let mut table_kinds = BTreeSet::new();
    for capture in tables.captures_iter(raw) {
        table_count += 1;
        let columns = column
            .captures_iter(&capture[1])
            .map(|item| item[1].to_owned())
            .collect::<Vec<_>>();
        ensure!(
            column.replace_all(&capture[1], "").trim().is_empty(),
            "Kimi pricing column schema changed"
        );
        let k3 = [
            "Model",
            "Unit",
            "Cache Write Price (TTL 5min)",
            "Cache Write Price (TTL 1h)",
            "Cached Input Price",
            "Input Price",
            "Output Price",
            "Context Window",
        ];
        let k2 = [
            "Model",
            "Unit",
            "Input Price (Cache Hit)",
            "Input Price (Cache Miss)",
            "Output Price",
            "Context Window",
        ];
        let with_writes = if columns == k3 {
            true
        } else {
            ensure!(columns == k2, "Kimi pricing columns changed");
            false
        };
        ensure!(
            table_kinds.insert(with_writes),
            "Kimi pricing table kind duplicated"
        );
        let normalized = money_jsx.replace_all(&capture[2], |caps: &regex::Captures| {
            format!("\"${}\"", &caps[1])
        });
        let rows: Vec<Vec<String>> =
            serde_json::from_str(&format!("[{}]", normalized.trim().trim_end_matches(',')))
                .context(
                    "Kimi pricing rows are not supported literal data; no JavaScript is executed",
                )?;
        ensure!(
            !rows.is_empty() && rows.len() <= 500,
            "Kimi pricing row count invalid"
        );
        for row in rows {
            ensure!(
                row.len() == columns.len() && row[1] == "1M tokens",
                "Kimi pricing row schema/unit changed"
            );
            validate_model(&row[0])?;
            ensure!(seen.insert(row[0].clone()), "duplicate Kimi model price");
            let (cached, input, output, window) = if with_writes {
                (4, 5, 6, 7)
            } else {
                (2, 3, 4, 5)
            };
            let context = Regex::new(r"^([1-9][0-9]{0,2}(?:,[0-9]{3})*) tokens$")?;
            let context = context
                .captures(&row[window])
                .context("Kimi context window format changed")?[1]
                .replace(',', "")
                .parse::<u64>()?;
            ensure!(
                (1024..=100_000_000).contains(&context),
                "Kimi context window out of range"
            );
            let tier = PriceTier {
                name: "standard".into(),
                input: required_money(&row[input])?,
                cached_input: Some(required_money(&row[cached])?),
                cache_write: None,
                cache_write_5m: if with_writes {
                    Some(required_money(&row[2])?)
                } else {
                    None
                },
                cache_write_1h: if with_writes {
                    Some(required_money(&row[3])?)
                } else {
                    None
                },
                output: required_money(&row[output])?,
                context_window_tokens: Some(context),
                conditions: if with_writes {
                    vec!["Cache writes are charged by TTL; default TTL is 5min. Cache hits refresh TTL without an additional cache-write charge".into()]
                } else {
                    vec!["Cache-hit and cache-miss input are distinct token categories".into()]
                },
                unknowns: if with_writes {
                    vec![]
                } else {
                    vec!["A separate cache-write price/TTL is not published for this table; not assumed free".into()]
                },
            };
            let mut quote = new_quote("kimi", &row[0], vec![tier]);
            quote.conditions.push("Moonshot/Kimi international API USD list prices only; Kimi Code subscription quotas and China-region CNY billing are separate".into());
            quotes.push(quote);
        }
    }
    ensure!(
        table_count == 2 && raw.matches("<DocTable").count() == 2,
        "Kimi pricing table structure changed"
    );
    ensure!(!quotes.is_empty(), "Kimi pricing tables empty");
    Ok(quotes)
}

/// Strips markdown link syntax (`[label](url)`) and inline code backticks from
/// a table cell, keeping the visible label only.
fn cell_text(cell: &str) -> String {
    let link = Regex::new(r"\[([^\]]+)\]\((?:[^)]+)\)").unwrap();
    let code = Regex::new("`([^`]*)`").unwrap();
    let linked = link.replace_all(cell.trim(), |caps: &regex::Captures| caps[1].to_owned());
    code.replace_all(&linked, |caps: &regex::Captures| caps[1].to_owned())
        .trim()
        .to_owned()
}

fn parse_anthropic_models(raw: &str) -> Result<BTreeMap<String, AnthropicIdentity>> {
    ensure!(
        raw.len() <= MAX_SOURCE,
        "Anthropic model overview too large"
    );
    let display = Regex::new(r"^Claude [A-Z][a-z]+ [0-9]+(?:\.[0-9]+)?$")?;
    // Group consecutive markdown-table lines; only a group whose header starts
    // with the literal Feature column is the capability table.
    let mut groups: Vec<Vec<Vec<String>>> = Vec::new();
    for line in raw
        .lines()
        .filter(|line| line.trim_start().starts_with('|'))
    {
        let row = markdown_cells(line)?;
        match groups.last_mut() {
            Some(group) => group.push(row),
            None => groups.push(vec![row]),
        }
    }
    let mut tables = groups.into_iter().filter(|group| {
        group
            .first()
            .is_some_and(|row| row.first().is_some_and(|cell| cell_text(cell) == "Feature"))
    });
    let table = tables
        .next()
        .context("Anthropic model overview capability table missing")?;
    ensure!(
        tables.next().is_none(),
        "Anthropic model overview has an ambiguous capability table"
    );
    let header = table
        .first()
        .context("capability table header row missing")?;
    let mut names = Vec::new();
    for cell in &header[1..] {
        let name = cell_text(cell);
        ensure!(
            display.is_match(&name),
            "Anthropic model overview display name is not a bounded identifier: {name}"
        );
        ensure!(
            names.iter().all(|seen| seen != &name),
            "duplicate Anthropic display name"
        );
        names.push(name);
    }
    ensure!(
        names.len() >= 2,
        "Anthropic model overview table has too few models"
    );
    let mut id_row = None;
    let mut alias_row = None;
    let mut context_row = None;
    for row in &table[1..] {
        let label = row.first().map(|cell| cell_text(cell)).unwrap_or_default();
        match label.as_str() {
            "Claude API ID" => id_row = Some(row),
            "Claude API alias" => alias_row = Some(row),
            "Context window" => context_row = Some(row),
            _ => {}
        }
    }
    let context_row = context_row.context("Anthropic model overview context-window row missing")?;
    let id_row = id_row.context("Anthropic model overview Claude API ID row missing")?;
    let alias_row = alias_row.context("Anthropic model overview Claude API alias row missing")?;
    let window = Regex::new(r"^([0-9]+(?:\.[0-9]+)?)([KM]) tokens$")?;
    let mut identities = BTreeMap::new();
    for (index, name) in names.iter().enumerate() {
        let id = cell_text(id_row.get(index + 1).context("API ID row is short")?);
        validate_model(&id)?;
        let alias = match alias_row.get(index + 1) {
            Some(cell) if !cell_text(cell).is_empty() => {
                let alias = cell_text(cell);
                validate_model(&alias)?;
                Some(alias)
            }
            _ => None,
        };
        let context = cell_text(context_row.get(index + 1).context("context row is short")?);
        let capture = window
            .captures(&context)
            .context("Anthropic context-window format changed")?;
        let multiplier = if &capture[2] == "K" { 1_000 } else { 1_000_000 };
        let tokens = (capture[1].parse::<f64>()? * multiplier as f64) as u64;
        ensure!(
            (1024..=100_000_000).contains(&tokens),
            "Anthropic context window out of range"
        );
        identities.insert(
            name.clone(),
            AnthropicIdentity {
                api_id: id,
                alias,
                context_window_tokens: Some(tokens),
            },
        );
    }
    ensure!(!identities.is_empty(), "Anthropic model overview empty");
    Ok(identities)
}

fn parse_anthropic(
    raw: &str,
    identities: &BTreeMap<String, AnthropicIdentity>,
) -> Result<Vec<ModelQuote>> {
    ensure!(
        raw.len() <= MAX_SOURCE && raw.contains("## Model pricing"),
        "Anthropic pricing structure changed"
    );
    for anchor in [
        "1.25x base input price",
        "2x base input price",
        "0.1x base input price",
        "0.025x on Claude Fable 5.1 and Claude Mythos 5.1",
    ] {
        ensure!(
            raw.contains(anchor),
            "Anthropic cache multiplier anchors changed: {anchor}"
        );
    }
    let section = raw
        .split_once("## Model pricing")
        .unwrap()
        .1
        .split_once("## Cloud platform pricing")
        .context("Anthropic pricing section is not bounded")?
        .0;
    let table = section
        .lines()
        .filter(|line| line.trim_start().starts_with('|'))
        .map(markdown_cells)
        .collect::<Result<Vec<_>>>()?;
    let expected = [
        "Model",
        "Base input tokens",
        "5m cache writes",
        "1h cache writes",
        "Cache hits and refreshes",
        "Output tokens",
    ];
    ensure!(
        table.len() > 2
            && table.len() <= 62
            && table[0] == expected
            && table[1].len() == 6
            && table[1].iter().all(|cell| {
                !cell.is_empty() && cell.bytes().all(|byte| matches!(byte, b'-' | b':'))
            }),
        "Anthropic pricing table columns changed"
    );
    let display = Regex::new(
        r"^(Claude [A-Z][a-z]+ [0-9]+(?:\.[0-9]+)?)(?:\s+\(\[([^\]]+)\]\((?:https?://[^)]+)\)\))?$",
    )?;
    let mut quotes = Vec::new();
    let mut seen = BTreeSet::new();
    let mut skipped_retired = 0;
    for row in &table[2..] {
        ensure!(
            row.len() == 6,
            "Anthropic pricing row has unexpected columns"
        );
        let capture = display
            .captures(&row[0])
            .context("Anthropic pricing model cell is not a bounded display name")?;
        let name = capture[1].to_owned();
        let annotation = capture.get(2).map(|value| value.as_str().to_owned());
        if let Some(annotation) = &annotation {
            ensure!(
                annotation.starts_with("retired, except on ")
                    || annotation == "limited availability",
                "Anthropic pricing row carries an unrecognized availability annotation"
            );
        }
        if annotation
            .as_deref()
            .is_some_and(|value| value.starts_with("retired"))
        {
            // First-party API no longer serves this model; cloud-platform-only
            // pricing is a different billing channel and is never inferred here.
            skipped_retired += 1;
            continue;
        }
        let mut values = Vec::with_capacity(5);
        for (index, cell) in row[1..].iter().enumerate() {
            let (money, footnote) = match cell.strip_suffix("<sup>1</sup>") {
                Some(rest) => (rest, true),
                None => (cell.as_str(), false),
            };
            let money = money
                .strip_suffix(" / MTok")
                .context("Anthropic price cell is not per-MTok notation")?;
            ensure!(
                index == 3 || !footnote,
                "Anthropic cache-hit footnote marker appears outside the cache-hit column"
            );
            values.push((required_money(money)?, footnote));
        }
        let [(input, _), (write_5m, _), (write_1h, _), (hits, footnote), (output, _)] =
            values.try_into().unwrap();
        let hit_multiplier = if footnote { 0.025 } else { 0.1 };
        for (actual, expected_multiple) in
            [(write_5m, 1.25), (write_1h, 2.0), (hits, hit_multiplier)]
        {
            ensure!(
                (actual - input * expected_multiple).abs() <= 1e-6,
                "Anthropic cache price no longer matches the documented multiplier for {name}"
            );
        }
        let (family, major, minor) = {
            let mut parts = name.rsplitn(2, ' ');
            let version = parts.next().unwrap();
            let family = parts.next().unwrap().strip_prefix("Claude ").unwrap();
            let mut numbers = version.split('.');
            let major = numbers
                .next()
                .unwrap()
                .parse::<u32>()
                .context("Anthropic model major version is not numeric")?;
            let minor = numbers
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(0);
            (family.to_ascii_lowercase(), major, minor)
        };
        let dateless = format!("claude-{family}-{major}-{}", minor)
            .trim_end_matches("-0")
            .to_owned();
        let dateless_generation = major > 4 || (major == 4 && minor >= 6);
        let mut emitted = Vec::new();
        if let Some(identity) = identities.get(&name) {
            if dateless_generation {
                ensure!(
                    identity.api_id == dateless,
                    "attested Anthropic API ID disagrees with the documented dateless scheme for {name}"
                );
            } else {
                ensure!(
                    identity.alias.as_deref() == Some(dateless.as_str()),
                    "attested Anthropic alias disagrees with the documented pre-4.6 alias scheme for {name}"
                );
            }
            emitted.push((
                identity.api_id.clone(),
                identity.context_window_tokens,
                None,
            ));
            if let Some(alias) = identity.alias.as_deref() {
                if alias != identity.api_id {
                    emitted.push((
                        alias.to_owned(),
                        identity.context_window_tokens,
                        Some(format!(
                            "Keyed by the official pre-4.6 convenience alias that resolves to the dated snapshot {api_id}; the alias is a pointer, not a pinned model ID",
                            api_id = identity.api_id
                        )),
                    ));
                }
            }
        } else if dateless_generation {
            emitted.push((
                dateless.clone(),
                None,
                Some(format!(
                    "Model ID follows Anthropic's documented dateless naming scheme (claude-{{name}}-{{major}}[-{{minor}}]) for the 4.6 generation and later; it was not separately attested in this epoch's model overview, so verify the exact ID against the API before dispatch"
                )),
            ));
        } else {
            emitted.push((
                dateless.clone(),
                None,
                Some(
                    "Keyed by the documented pre-4.6 alias format claude-{name}-{major}-{minor}, which resolves to the most recent dated snapshot; the exact dated snapshot ID is not attested in this epoch's sources"
                        .to_owned(),
                ),
            ));
        }
        for (model, context, derivation) in emitted {
            ensure!(
                seen.insert(model.clone()),
                "duplicate Anthropic model price"
            );
            let mut quote_conditions = Vec::new();
            if annotation.as_deref() == Some("limited availability") {
                quote_conditions.push(
                    "Officially listed as limited availability; account access to this model is not verified"
                        .to_owned(),
                );
            }
            if let Some(derivation) = derivation {
                quote_conditions.push(derivation);
            }
            let mut conditions = vec![
                "5m/1h cache writes follow the official prompt-caching multipliers (1.25x/2x of base input); cache hits read at the documented hit multiplier".to_owned(),
                "Batch API (50% discount), fast-mode premiums, and data-residency multipliers are separate pricing paths".to_owned(),
                "First-party Claude API USD list prices; Amazon Bedrock, Google Cloud, Microsoft Foundry, and Claude Platform on AWS bill through their own channels and IDs".to_owned(),
            ];
            if footnote {
                conditions.push(
                    "Cache hits for this model are priced at 0.025x base input per the official pricing footnote".to_owned(),
                );
            }
            let tier = PriceTier {
                name: "standard".into(),
                input,
                cached_input: Some(hits),
                cache_write: None,
                cache_write_5m: Some(write_5m),
                cache_write_1h: Some(write_1h),
                output,
                context_window_tokens: context,
                conditions,
                unknowns: if context.is_some() {
                    Vec::new()
                } else {
                    vec![
                        "Context window is not attested for this model in this epoch's sources"
                            .to_owned(),
                    ]
                },
            };
            let mut quote = new_quote("anthropic", &model, vec![tier]);
            quote.conditions.extend(quote_conditions);
            quotes.push(quote);
        }
    }
    ensure!(
        !quotes.is_empty(),
        "Anthropic pricing produced no first-party quotes (retired rows skipped: {skipped_retired})"
    );
    Ok(quotes)
}

fn parse_deepseek(raw: &str) -> Result<Vec<ModelQuote>> {
    ensure!(raw.len() <= MAX_SOURCE, "DeepSeek pricing page too large");
    for anchor in [
        "Off-peak rates are half of the peak rates.",
        "Peak hours are 01:00 - 04:00 and 06:00 - 10:00 UTC, Monday through Friday, excluding Chinese public holidays.",
        "All other hours are off-peak, including weekends and Chinese public holidays in full.",
    ] {
        ensure!(raw.contains(anchor), "DeepSeek peak/off-peak semantics changed: {anchor}");
    }
    ensure!(
        raw.matches("<table").count() == 1,
        "DeepSeek pricing table structure changed"
    );
    let table = raw
        .split_once("<table")
        .unwrap()
        .1
        .split_once("</table>")
        .context("DeepSeek pricing table is unterminated")?
        .0;
    let row_pattern = Regex::new(r"(?s)<tr>(.*?)</tr>")?;
    let cell_pattern = Regex::new(r"(?s)<td[^>]*>(.*?)</td>")?;
    let tag = Regex::new(r"<[^>]+>")?;
    let rows: Vec<Vec<(String, String)>> = row_pattern
        .captures_iter(table)
        .map(|row| {
            cell_pattern
                .captures_iter(&row[1])
                .map(|cell| {
                    let raw_cell = cell[1].replace("<br>", " ");
                    let marker = raw_cell.contains("<sup>(1)</sup>");
                    let text = tag
                        .replace_all(raw_cell.replace("<sup>(1)</sup>", "").as_str(), " ")
                        .trim()
                        .to_owned();
                    (text, if marker { "1" } else { "" }.to_owned())
                })
                .collect()
        })
        .collect();
    fn row_text(row: &[(String, String)]) -> Vec<String> {
        row.iter().map(|cell| cell.0.clone()).collect()
    }
    let model_row = rows
        .iter()
        .find(|row| row.first().is_some_and(|cell| cell.0 == "MODEL"))
        .context("DeepSeek MODEL row missing")?;
    let models: Vec<(String, bool)> = model_row[1..]
        .iter()
        .map(|cell| {
            validate_model(&cell.0)?;
            Ok((cell.0.clone(), !cell.1.is_empty()))
        })
        .collect::<Result<_>>()?;
    ensure!(
        models.len() >= 2 && models.len() <= 6,
        "DeepSeek model column count is not bounded"
    );
    ensure!(
        models.iter().any(|(_, marked)| *marked),
        "DeepSeek footnote marker for the canonical model name is missing"
    );
    let mut context_tokens = None;
    let mut hit = Vec::new();
    let mut miss = Vec::new();
    let mut output = Vec::new();
    let mut label = None;
    for row in &rows {
        let cells = row_text(row);
        if cells.first().is_some_and(|cell| *cell == "CONTEXT LENGTH") {
            let value = cells.get(1).context("DeepSeek context length is missing")?;
            let tokens = match value.trim() {
                "1M" => 1_000_000,
                "128K" => 128_000,
                "64K" => 64_000,
                other => other
                    .trim_end_matches('K')
                    .parse::<u64>()
                    .ok()
                    .map(|thousands| thousands * 1_000)
                    .context("DeepSeek context length format changed")?,
            };
            ensure!(
                (1024..=100_000_000).contains(&tokens),
                "DeepSeek context window out of range"
            );
            context_tokens = Some(tokens);
            continue;
        }
        // A pricing row either introduces a category (carrying the off-peak
        // values in the same row) or continues it with the peak values; both
        // shapes are recognized by scanning the row's cells.
        if let Some(kind) = cells.iter().find_map(|cell| match cell.as_str() {
            "1M INPUT TOKENS (CACHE HIT)" => Some("hit"),
            "1M INPUT TOKENS (CACHE MISS)" => Some("miss"),
            "1M OUTPUT TOKENS" => Some("output"),
            _ => None,
        }) {
            label = Some(kind.to_owned());
        }
        let Some(kind) = &label else { continue };
        let tier = if cells.iter().any(|cell| cell == "OFF-PEAK") {
            "off_peak"
        } else if cells.iter().any(|cell| cell == "PEAK") {
            "peak"
        } else {
            continue;
        };
        let values = cells[cells.len() - models.len()..]
            .iter()
            .map(|cell| required_money(cell))
            .collect::<Result<Vec<f64>>>()?;
        ensure!(
            values.len() == models.len(),
            "DeepSeek price row width mismatch"
        );
        let bucket = match kind.as_str() {
            "hit" => &mut hit,
            "miss" => &mut miss,
            "output" => &mut output,
            _ => unreachable!(),
        };
        bucket.push((tier.to_owned(), values));
    }
    for (name, bucket) in [
        ("cache-hit", &hit),
        ("cache-miss", &miss),
        ("output", &output),
    ] {
        ensure!(
            bucket.len() == 2 && bucket[0].0 == "off_peak" && bucket[1].0 == "peak",
            "DeepSeek {name} peak/off-peak rows are missing or out of order"
        );
        for model in 0..models.len() {
            let (off, peak) = (bucket[0].1[model], bucket[1].1[model]);
            ensure!(
                (off - peak / 2.0).abs() <= 1e-9,
                "DeepSeek off-peak price is no longer half of the peak price"
            );
        }
    }
    let canonical = models.iter().position(|(_, marked)| *marked).unwrap_or(0);
    let footnote = Regex::new(
        r#"(?s)\(1\) Use <code>([a-z0-9.-]+)</code> as the model name\. The legacy names .*?are still accepted, but the corresponding models have been retired, their requests are served by the DeepSeek-[A-Za-z0-9.-]+ model and billed at the Flash price\."#,
    )?
    .captures(raw);
    let mut legacy_names = Vec::new();
    if let Some(footnote) = &footnote {
        ensure!(
            &footnote[1] == models[canonical].0,
            "DeepSeek footnote canonical name does not match the marked model column"
        );
        let sentence = footnote.get(0).unwrap().as_str();
        let code = Regex::new(r"<code>([a-z0-9.-]+)</code>")?;
        for capture in code.captures_iter(
            sentence
                .split_once("The legacy names ")
                .and_then(|(_, rest)| rest.split_once(" are still accepted"))
                .context("DeepSeek legacy-name footnote sentence changed")?
                .0,
        ) {
            let name = capture[1].to_owned();
            validate_model(&name)?;
            ensure!(
                models.iter().all(|(model, _)| model != &name),
                "DeepSeek legacy footnote names a currently published model"
            );
            legacy_names.push(name);
        }
    }
    let window = "Peak hours are 01:00 - 04:00 and 06:00 - 10:00 UTC, Monday through Friday, excluding Chinese public holidays; all other hours are off-peak, including weekends and Chinese public holidays in full".to_owned();
    let mut quotes = Vec::new();
    for (index, (model, _)) in models.iter().enumerate() {
        let tier = |tier_name: &str, bucket_index: usize| -> PriceTier {
            PriceTier {
                name: tier_name.into(),
                input: miss[bucket_index].1[index],
                cached_input: Some(hit[bucket_index].1[index]),
                cache_write: None,
                cache_write_5m: None,
                cache_write_1h: None,
                output: output[bucket_index].1[index],
                context_window_tokens: context_tokens,
                conditions: vec![
                    window.clone(),
                    if tier_name == "off_peak" {
                        "Off-peak tier applies outside the documented peak windows; off-peak rates are half of the peak rates (official footnote 2)".to_owned()
                    } else {
                        "Peak tier applies during the documented weekday UTC windows (official footnote 2)".to_owned()
                    },
                ],
                unknowns: vec![
                    "The Chinese public holiday calendar is not decoded from this page; a weekday peak-window request that falls on a Chinese public holiday is billed at off-peak rates".to_owned(),
                    "The dispatching side must classify the request time against the UTC peak windows; this quote does not resolve which tier applies".to_owned(),
                ],
            }
        };
        let mut quote = new_quote(
            "deepseek",
            model,
            vec![tier("off_peak", 0), tier("peak", 1)],
        );
        quote.conditions.push(
            "Expense equals tokens multiplied by price, deducted from the topped-up or granted balance; DeepSeek may adjust prices (official deduction rules)".to_owned(),
        );
        quotes.push(quote);
    }
    for legacy in &legacy_names {
        let mut quote = new_quote(
            "deepseek",
            legacy,
            vec![
                PriceTier {
                    name: "off_peak".into(),
                    ..{
                        let quote = &quotes[canonical];
                        quote.tiers[0].clone()
                    }
                },
                PriceTier {
                    name: "peak".into(),
                    ..quotes[canonical].tiers[1].clone()
                },
            ],
        );
        quote.conditions.push(format!(
            "Retired legacy name: requests are still accepted, served by the retired model's successor, and billed at the {} price per official footnote (1)",
            models[canonical].0
        ));
        quotes.push(quote);
    }
    ensure!(!quotes.is_empty(), "DeepSeek pricing produced no quotes");
    Ok(quotes)
}

fn required_money(raw: &str) -> Result<f64> {
    let digits = raw
        .strip_prefix('$')
        .context("price is not explicitly USD dollar notation")?;
    ensure!(
        digits.len() <= 20
            && !digits.is_empty()
            && digits
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
            && digits.matches('.').count() <= 1
            && !digits.starts_with('.')
            && !digits.ends_with('.'),
        "price decimal format changed"
    );
    let value = digits.parse::<f64>()?;
    ensure!(
        value.is_finite() && value > 0.0 && value <= 1_000_000.0,
        "price outside supported positive range; zero is never inferred"
    );
    Ok(value)
}
fn optional_money(raw: &str) -> Result<Option<f64>> {
    if raw == "-" {
        Ok(None)
    } else {
        required_money(raw).map(Some)
    }
}
fn validate_model(model: &str) -> Result<()> {
    ensure!(
        !model.is_empty()
            && model.len() <= 200
            && model.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'.')),
        "price model is not an exact supported API identifier"
    );
    Ok(())
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn fresh(checked_at: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(checked_at)
        .map(|date| {
            let age = Utc::now().signed_duration_since(date).num_seconds();
            (0..=MAX_AGE_SECONDS).contains(&age)
        })
        .unwrap_or(false)
}
fn bounded_error(error: &str) -> String {
    let mut end = error.len().min(4096);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].into()
}
fn allowed_url(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "developers.openai.com"
                    | "platform.openai.com"
                    | "platform.moonshot.ai"
                    | "platform.kimi.ai"
                    | "platform.claude.com"
                    | "api-docs.deepseek.com"
            )
        )
}
fn allowed_provider_url(provider: &str, url: &url::Url) -> bool {
    allowed_url(url)
        && match provider {
            "openai" => matches!(
                url.host_str(),
                Some("developers.openai.com" | "platform.openai.com")
            ),
            "kimi" => matches!(
                url.host_str(),
                Some("platform.moonshot.ai" | "platform.kimi.ai")
            ),
            "anthropic" | "anthropic-models" => url.host_str() == Some("platform.claude.com"),
            "deepseek" => url.host_str() == Some("api-docs.deepseek.com"),
            _ => false,
        }
}
fn bounded_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "pricing file too large"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "pricing file too large");
    Ok(bytes)
}

fn load_snapshot(directory: &Path, id: &str, expected: Option<&str>) -> Result<PriceSnapshot> {
    ensure!(
        Uuid::parse_str(id)?.to_string() == id,
        "invalid cached pricing snapshot id"
    );
    let bytes = bounded_read(
        &directory.join("snapshots").join(format!("{id}.json")),
        MAX_SNAPSHOT,
    )?;
    ensure!(
        expected == Some(hash(&bytes).as_str()),
        "cached pricing snapshot hash mismatch"
    );
    let snapshot: PriceSnapshot = serde_json::from_slice(&bytes)?;
    ensure!(
        snapshot.id == id
            && snapshot.parser_version == 2
            && snapshot.sources.len() == SOURCE_SPECS.len(),
        "cached pricing snapshot metadata invalid"
    );
    chrono::DateTime::parse_from_rfc3339(&snapshot.checked_at)?;
    for (source, (provider, url)) in snapshot.sources.iter().zip(SOURCE_SPECS) {
        ensure!(
            source.provider == provider
                && source.requested_url == url
                && source.checked_at == snapshot.checked_at,
            "cached price source provenance mismatch"
        );
        if let Some(raw) = source.raw.as_deref() {
            ensure!(
                raw.len() <= MAX_SOURCE
                    && source.sha256.as_deref() == Some(hash(raw.as_bytes()).as_str()),
                "cached price source hash mismatch"
            );
            ensure!(
                allowed_provider_url(
                    provider,
                    &url::Url::parse(
                        source
                            .final_url
                            .as_deref()
                            .context("cached source URL missing")?
                    )?,
                ),
                "cached price origin invalid"
            );
        } else {
            ensure!(
                source.status == "failed" && source.reason.is_some(),
                "cached price source is missing raw data"
            );
        }
    }
    let (decoded, quotes) = decode_all(snapshot.sources.clone());
    ensure!(
        decoded == snapshot.sources,
        "cached price source status mismatches parser"
    );
    ensure!(
        quotes == snapshot.quotes && !quotes.is_empty(),
        "cached quotes do not match official raw sources"
    );
    Ok(snapshot)
}

fn publish(path: &Path, bytes: &[u8], immutable: bool) -> Result<()> {
    let parent = path.parent().context("pricing path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if immutable {
            fs::hard_link(&temporary, path)?;
        } else {
            replace(&temporary, path)?;
        }
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}
#[cfg(not(windows))]
fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Small schema fixtures deliberately use arbitrary prices. Production
    // quotes are always parsed from a freshly downloaded official document.
    const OPENAI: &str = "Prices per 1M tokens.\n### Standard pricing data\n\n| Model | Short context input | Short context cached input | Short context cache writes | Short context output | Long context input | Long context cached input | Long context cache writes | Long context output |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n| gpt-test | $1.25 | $0.125 | - | $10.00 | - | - | - | - |\n| gpt-test-long (<272K context length) | $2.00 | $0.20 | $2.50 | $12.00 | $4.00 | $0.40 | $5.00 | $18.00 |\n\n### Batch pricing data\n| gpt-test | $0.1 |\n";
    const KIMI: &str = r#"
Here, 1M = 1,000,000. Prices exclude applicable taxes.
If no TTL is specified, the 5min tier applies by default. Cache hits refresh the cache with no additional cache write charge.
<DocTable
 columns={[
 { title: "Model", width: "12%" },
 { title: "Unit", width: "10%" },
 { title: "Cache Write Price (TTL 5min)", width: "13%" },
 { title: "Cache Write Price (TTL 1h)", width: "13%" },
 { title: "Cached Input Price", width: "13%" },
 { title: "Input Price", width: "13%" },
 { title: "Output Price", width: "10%" },
 { title: "Context Window", width: "16%" },
 ]}
 rows={[
 ["kimi-test", "1M tokens", <>{"$"}3.00</>, <>{"$"}6.00</>, <>{"$"}0.30</>, <>{"$"}3.00</>, <>{"$"}15.00</>, "1,048,576 tokens"],
 ]}
/>
<DocTable
 columns={[
 { title: "Model", width: "24%" },
 { title: "Unit", width: "12%" },
 { title: "Input Price (Cache Hit)", width: "16%" },
 { title: "Input Price (Cache Miss)", width: "16%" },
 { title: "Output Price", width: "14%" },
 { title: "Context Window", width: "18%" },
 ]}
 rows={[
 ["kimi-code-test", "1M tokens", <>{"$"}0.19</>, <>{"$"}0.95</>, <>{"$"}4.00</>, "262,144 tokens"],
 ]}
/>
"#;

    const ANTHROPIC_MODELS: &str = r#"
## Current models

| Feature | Claude Fable 5.1 | Claude Opus 5 | Claude Haiku 4.5 |
| :--- | :--- | :--- | :--- |
| Description | For demanding reasoning | For complex agentic coding | The fastest model |
| [Pricing](https://platform.claude.com/docs/en/about-claude/pricing) | $10 / input MTok, $50 / output MTok | $5 / input MTok, $25 / output MTok | $1 / input MTok, $5 / output MTok |
| Claude API ID | `claude-fable-5-1` | `claude-opus-5` | `claude-haiku-4-5-20251001` |
| [Context window](https://platform.claude.com/docs/en/build-with-claude/context-windows) | 1M tokens | 1M tokens | 200K tokens |
| Claude API alias | `claude-fable-5-1` | `claude-opus-5` | `claude-haiku-4-5` |

* **Claude API ID:** Every Claude model ID is a pinned snapshot, including the dateless IDs used from the 4.6 generation on.
* **Claude API alias:** For models before the 4.6 generation, the alias is a convenience pointer that resolves to the dated ID.
"#;
    const ANTHROPIC: &str = r#"
## Model pricing

| Model | Base input tokens | 5m cache writes | 1h cache writes | Cache hits and refreshes | Output tokens |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Claude Fable 5.1 | $10 / MTok | $12.50 / MTok | $20 / MTok | $0.25 / MTok<sup>1</sup> | $50 / MTok |
| Claude Opus 5 | $5 / MTok | $6.25 / MTok | $10 / MTok | $0.50 / MTok | $25 / MTok |
| Claude Opus 4.6 | $5 / MTok | $6.25 / MTok | $10 / MTok | $0.50 / MTok | $25 / MTok |
| Claude Sonnet 4.5 | $3 / MTok | $3.75 / MTok | $6 / MTok | $0.30 / MTok | $15 / MTok |
| Claude Mythos 5.1 ([limited availability](https://anthropic.com/glasswing)) | $10 / MTok | $12.50 / MTok | $20 / MTok | $0.25 / MTok<sup>1</sup> | $50 / MTok |
| Claude Opus 4.1 ([retired, except on Bedrock and Google Cloud](https://platform.claude.com/docs/en/about-claude/model-deprecations)) | $15 / MTok | $18.75 / MTok | $30 / MTok | $1.50 / MTok | $75 / MTok |
| Claude Haiku 4.5 | $1 / MTok | $1.25 / MTok | $2 / MTok | $0.10 / MTok | $5 / MTok |

*<sup>1 Cache hits and refreshes on Claude Fable 5.1 and Claude Mythos 5.1 are priced at 0.025x the base input price. All other models use the standard 0.1x multiplier.</sup>*

## Cloud platform pricing

Cloud platforms bill separately.

### Prompt caching

Prompt caching uses the following pricing multipliers relative to base input token rates:

| Cache operation | Multiplier | Duration |
| --- | --- | --- |
| 5-minute cache write | 1.25x base input price | Cache valid for 5 minutes |
| 1-hour cache write | 2x base input price | Cache valid for 1 hour |
| Cache read (hit) | 0.1x base input price (0.025x on Claude Fable 5.1 and Claude Mythos 5.1) | Same duration as the preceding write |
"#;
    const DEEPSEEK: &str = r#"<html><body><div><b><table style="text-align:center"><tr><td colspan="3" style="text-align:center">MODEL</td><td>deepseek-flash<sup>(1)</sup></td><td>deepseek-v4-pro</td></tr><tr><td colspan="3">BASE URL (OpenAI Format)</td><td colspan="2"><a href="https://api.deepseek.com">https://api.deepseek.com</a></td></tr><tr><td colspan="3">CONTEXT LENGTH</td><td colspan="2">1M</td></tr><tr><td rowspan="7">FEATURES</td><td colspan="2"><a href="/guides/tool_calls">Tool Calls</a></td><td>✓</td><td>✓</td></tr><tr><td rowspan="6">PRICING<sup>(2)</sup></td><td rowspan="2">1M INPUT TOKENS<br>(CACHE HIT)</td><td>OFF-PEAK</td><td>$0.003</td><td>$0.022</td></tr><tr><td>PEAK</td><td>$0.006</td><td>$0.044</td></tr><tr><td rowspan="2">1M INPUT TOKENS<br>(CACHE MISS)</td><td>OFF-PEAK</td><td>$0.15</td><td>$0.66</td></tr><tr><td>PEAK</td><td>$0.3</td><td>$1.32</td></tr><tr><td rowspan="2">1M OUTPUT TOKENS</td><td>OFF-PEAK</td><td>$0.6</td><td>$1.98</td></tr><tr><td>PEAK</td><td>$1.2</td><td>$3.96</td></tr><tr><td colspan="3">Concurrency Limit<sup>(3)</sup></td><td>2500</td><td>500</td></tr></table></b></div>
<div style="font-size:14px"><p>(1) Use <code>deepseek-flash</code> as the model name. The legacy names <code>deepseek-v4-flash</code> and <code>deepseek-v4-flash-vision-exp</code> are still accepted, but the corresponding models have been retired, their requests are served by the DeepSeek-V4.1-Flash model and billed at the Flash price.</p><p>(2) Off-peak rates are half of the peak rates. Peak hours are 01:00 - 04:00 and 06:00 - 10:00 UTC, Monday through Friday, excluding Chinese public holidays. All other hours are off-peak, including weekends and Chinese public holidays in full.</p></div>
</body></html>"#;

    fn fixture() -> PriceSnapshot {
        let checked_at = now();
        let mut sources = Vec::new();
        for (provider, url) in SOURCE_SPECS {
            let raw = match provider {
                "openai" => OPENAI,
                "kimi" => KIMI,
                "anthropic" => ANTHROPIC,
                "anthropic-models" => ANTHROPIC_MODELS,
                "deepseek" => DEEPSEEK,
                _ => unreachable!("fixture covers every source spec"),
            }
            .to_owned();
            sources.push(PriceSource {
                provider: provider.into(),
                requested_url: url.into(),
                final_url: Some(url.into()),
                checked_at: checked_at.clone(),
                status: "fetched".into(),
                reason: None,
                sha256: Some(hash(raw.as_bytes())),
                raw: Some(raw),
            });
        }
        let (sources, quotes) = decode_all(sources);
        PriceSnapshot {
            parser_version: 2,
            id: Uuid::new_v4().to_string(),
            checked_at,
            sources,
            quotes,
        }
    }

    #[test]
    fn openai_preserves_tiers_cache_unknowns_and_exact_model() {
        let quotes = parse_openai(OPENAI).unwrap();
        assert_eq!(quotes.len(), 2);
        assert_eq!(quotes[0].model, "gpt-test");
        assert_eq!(quotes[0].tiers[0].input, 1.25);
        assert_eq!(quotes[0].tiers[0].cache_write, None);
        assert_eq!(quotes[1].model, "gpt-test-long");
        assert_eq!(quotes[1].tiers.len(), 2);
        assert_eq!(quotes[1].tiers[1].input, 4.0);
        assert!(quotes[1].tiers[0].conditions[0].contains("272000"));
        assert!(parse_openai(&OPENAI.replace("Short context input", "Input")).is_err());
        assert!(parse_openai(&OPENAI.replace("$1.25", "NaN")).is_err());
        assert!(parse_openai(&OPENAI.replace("$1.25", "$0.00")).is_err());
        assert!(
            parse_openai(&OPENAI.replace("gpt-test-long (<272K context length)", "gpt-test"))
                .is_err()
        );
    }

    #[test]
    fn kimi_parses_literals_without_executing_mdx() {
        let quotes = parse_kimi(KIMI).unwrap();
        assert_eq!(quotes.len(), 2);
        let k3 = &quotes[0].tiers[0];
        assert_eq!(k3.cached_input, Some(0.30));
        assert_eq!(k3.cache_write_1h, Some(6.0));
        assert_eq!(k3.context_window_tokens, Some(1_048_576));
        let k2 = &quotes[1].tiers[0];
        assert_eq!(k2.cache_write_1h, None);
        assert_eq!(k2.input, 0.95);
        assert_eq!(k2.output, 4.0);
        assert!(parse_kimi(&KIMI.replace("Input Price (Cache Miss)", "Input Price")).is_err());
        assert!(parse_kimi(&KIMI.replace("<>{\"$\"}0.95</>", "computePrice()")).is_err());
        assert!(parse_kimi(&KIMI.replace("\"1M tokens\"", "\"1000 tokens\"")).is_err());
        assert!(parse_kimi(&KIMI.replace("kimi-code-test", "kimi-test")).is_err());
        assert!(parse_kimi(&KIMI.replace("$\"}0.95", "$\"}-1")).is_err());
    }

    #[test]
    fn anthropic_joins_attested_ids_and_derives_documented_schemes() {
        let identities = parse_anthropic_models(ANTHROPIC_MODELS).unwrap();
        let quotes = parse_anthropic(ANTHROPIC, &identities).unwrap();
        let find = |model: &str| {
            quotes
                .iter()
                .find(|quote| quote.model == model)
                .unwrap_or_else(|| panic!("missing quote for {model}"))
        };
        // Attested in the same epoch's model overview.
        assert_eq!(find("claude-fable-5-1").tiers[0].input, 10.0);
        assert_eq!(
            find("claude-fable-5-1").tiers[0].context_window_tokens,
            Some(1_000_000)
        );
        assert_eq!(
            find("claude-fable-5-1").tiers[0].cached_input,
            Some(0.25),
            "footnoted 0.025x cache-hit multiplier"
        );
        assert_eq!(find("claude-haiku-4-5-20251001").tiers[0].input, 1.0);
        let alias = find("claude-haiku-4-5");
        assert!(alias.conditions.iter().any(|condition| condition.contains(
            "convenience alias that resolves to the dated snapshot claude-haiku-4-5-20251001"
        )));
        // Derived from the documented dateless scheme (4.6 generation and later).
        let derived = find("claude-opus-4-6");
        assert!(derived
            .conditions
            .iter()
            .any(|condition| condition.contains("documented dateless naming scheme")));
        assert!(derived.tiers[0].context_window_tokens.is_none());
        assert!(derived.tiers[0]
            .unknowns
            .iter()
            .any(|unknown| unknown.contains("Context window is not attested")));
        // Pre-4.6 rows without attestation stay keyed by the documented alias.
        let pointer = find("claude-sonnet-4-5");
        assert!(pointer
            .conditions
            .iter()
            .any(|condition| condition.contains("documented pre-4.6 alias format")));
        // Limited availability is carried as a condition, never dropped.
        assert!(find("claude-mythos-5-1")
            .conditions
            .iter()
            .any(|condition| condition.contains("limited availability")));
        // Retired first-party models produce no quote.
        assert!(quotes
            .iter()
            .all(|quote| !quote.model.starts_with("claude-opus-4-1")));
        assert_eq!(quotes.len(), 7);
        assert_eq!(find("claude-opus-5").app_ids, vec!["claude".to_owned()]);
        // Schema drift fails closed.
        assert!(parse_anthropic(
            &ANTHROPIC.replace("5m cache writes", "Cache writes"),
            &identities
        )
        .is_err());
        assert!(parse_anthropic(
            &ANTHROPIC.replace("$12.50 / MTok", "$13.00 / MTok"),
            &identities
        )
        .is_err());
        assert!(parse_anthropic(
            &ANTHROPIC.replace("1.25x base input price", "1.2x base input"),
            &identities
        )
        .is_err());
        // An attested ID that disagrees with the documented dateless scheme
        // blocks the epoch instead of quoting a mismatched identity.
        let tampered = parse_anthropic_models(
            &ANTHROPIC_MODELS.replace("`claude-opus-5`", "`claude-opus-5-alt`"),
        )
        .unwrap();
        assert!(parse_anthropic(ANTHROPIC, &tampered).is_err());
        // A join source that stops attesting the current table blocks the epoch.
        assert!(parse_anthropic_models("no table here").is_err());
        let (sources, quotes) = decode_all(vec![
            PriceSource {
                provider: "anthropic-models".into(),
                requested_url: ANTHROPIC_MODELS_URL.into(),
                final_url: Some(ANTHROPIC_MODELS_URL.into()),
                checked_at: now(),
                status: "fetched".into(),
                reason: None,
                sha256: Some(hash(b"x")),
                raw: Some("not an overview".into()),
            },
            PriceSource {
                provider: "anthropic".into(),
                requested_url: ANTHROPIC_URL.into(),
                final_url: Some(ANTHROPIC_URL.into()),
                checked_at: now(),
                status: "fetched".into(),
                reason: None,
                sha256: Some(hash(b"y")),
                raw: Some(ANTHROPIC.into()),
            },
        ]);
        assert!(sources.iter().all(|source| source.status == "blocked"));
        assert!(quotes.is_empty());
    }

    #[test]
    fn deepseek_peak_offpeak_tiers_and_retired_alias_billing() {
        let quotes = parse_deepseek(DEEPSEEK).unwrap();
        let find = |model: &str| {
            quotes
                .iter()
                .find(|quote| quote.model == model)
                .unwrap_or_else(|| panic!("missing quote for {model}"))
        };
        let flash = find("deepseek-flash");
        assert_eq!(flash.tiers.len(), 2);
        assert_eq!(flash.tiers[0].name, "off_peak");
        assert_eq!(flash.tiers[0].input, 0.15);
        assert_eq!(flash.tiers[0].cached_input, Some(0.003));
        assert_eq!(flash.tiers[0].output, 0.6);
        assert_eq!(flash.tiers[1].name, "peak");
        assert_eq!(flash.tiers[1].input, 0.3);
        assert_eq!(
            flash.tiers[0].context_window_tokens,
            Some(1_000_000),
            "context length row is joined"
        );
        assert!(flash.tiers[0]
            .conditions
            .iter()
            .any(|condition| condition.starts_with("Peak hours are 01:00 - 04:00")));
        assert!(flash.tiers[0]
            .unknowns
            .iter()
            .any(|unknown| unknown.contains("Chinese public holiday calendar is not decoded")));
        assert_eq!(flash.app_ids, vec!["deepseek".to_owned()]);
        let pro = find("deepseek-v4-pro");
        assert_eq!(pro.tiers[1].output, 3.96);
        // Retired legacy names are billed at the flash price per the official footnote.
        let legacy = find("deepseek-v4-flash-vision-exp");
        assert_eq!(legacy.tiers[0].input, flash.tiers[0].input);
        assert!(legacy.conditions.iter().any(|condition| condition
            .contains("billed at the deepseek-flash price per official footnote (1)")));
        assert_eq!(quotes.len(), 4);
        // Schema and semantics drift fail closed.
        assert!(parse_deepseek(&DEEPSEEK.replace("OFF-PEAK", "DISCOUNT")).is_err());
        assert!(parse_deepseek(&DEEPSEEK.replace("$0.15</td>", "$0.16</td>"),).is_err());
        assert!(parse_deepseek(&DEEPSEEK.replace(
            "Peak hours are 01:00 - 04:00 and 06:00 - 10:00 UTC",
            "Peak hours are 09:00 - 10:00 UTC"
        ))
        .is_err());
        assert!(parse_deepseek(&DEEPSEEK.replace("</table>", "</tablex>")).is_err());
        assert!(parse_deepseek(
            &DEEPSEEK.replace("deepseek-v4-flash-vision-exp", "deepseek-v4-pro")
        )
        .is_err());
    }

    #[test]
    fn quotes_require_exact_app_model_and_api_billing() {
        let directory = tempfile::tempdir().unwrap();
        let service = PriceService::open(directory.path()).unwrap();
        service.persist(&fixture()).unwrap();
        assert_eq!(
            service.quote("kimi-cli", "kimi-test", "api").status,
            "verified"
        );
        assert_eq!(
            service.quote("kimi-code", "kimi-test", "api").status,
            "verified"
        );
        assert_eq!(service.quote("codex", "gpt-test", "api").status, "verified");
        for (app, model, channel) in [
            ("kimi-cli", "kimi-test", "subscription"),
            ("kimi-cli", "kimi-test-thinking", "api"),
            ("codex", "kimi-test", "api"),
            ("local-codex", "gpt-test", "api"),
            ("claude", "claude-test", "api"),
            ("kimi-cli", "kimi-test", "unknown"),
        ] {
            let result = service.quote(app, model, channel);
            assert_eq!(result.status, "blocked");
            assert!(result.quote.is_none());
        }
        assert_eq!(service.status()["auto_dispatch_ready"], false);
        let status = service.status();
        let channels = status["billing_channels"].as_array().unwrap();
        assert_eq!(channels.len(), 9);
        assert!(channels
            .iter()
            .any(|entry| entry["app_id"] == "deepseek" && entry["billing_channel"] == "api"));
        assert!(!channels.iter().any(
            |entry| entry["app_id"] == "deepseek" && entry["billing_channel"] == "subscription"
        ));
        assert_eq!(status["dispatch_readiness"]["ready"], false);
        assert!(!status["dispatch_readiness"]["missing"]
            .as_array()
            .unwrap()
            .is_empty());
        let subscription = service.quote("kimi-cli", "kimi-test", "subscription");
        assert_eq!(subscription.status, "blocked");
        assert!(subscription
            .reason
            .as_deref()
            .unwrap()
            .contains("provider quota units"));
    }

    #[test]
    fn restored_cache_is_display_only_and_quotes_bind_to_online_epoch() {
        let directory = tempfile::tempdir().unwrap();
        let service = PriceService::open(directory.path()).unwrap();
        let snapshot = fixture();
        service.persist(&snapshot).unwrap();
        assert_eq!(
            service
                .quote_for_epoch("codex", "gpt-test", "api", &snapshot.id)
                .status,
            "verified"
        );
        assert_eq!(
            service
                .quote_for_epoch("codex", "gpt-test", "api", "another-round")
                .status,
            "blocked"
        );
        let reopened = PriceService::open(directory.path()).unwrap();
        let cached = reopened.quote("codex", "gpt-test", "api");
        assert_eq!(cached.status, "cached");
        assert!(cached.quote.is_some());
        assert_eq!(reopened.status()["online_verified_in_this_process"], false);
        assert_eq!(
            reopened
                .quote_for_epoch("codex", "gpt-test", "api", &snapshot.id)
                .status,
            "blocked"
        );
    }

    #[test]
    fn failed_epoch_and_expired_prices_never_become_fresh_quotes() {
        let directory = tempfile::tempdir().unwrap();
        let service = PriceService::open(directory.path()).unwrap();
        let snapshot = fixture();
        service.persist(&snapshot).unwrap();
        service.record_failure(&now(), &anyhow::anyhow!("offline"));
        assert_eq!(
            service.latest_snapshot().unwrap().checked_at,
            snapshot.checked_at
        );
        assert_eq!(service.quote("codex", "gpt-test", "api").status, "blocked");
        let reopened = PriceService::open(directory.path()).unwrap();
        assert_eq!(reopened.quote("codex", "gpt-test", "api").status, "blocked");
        assert!(!fresh("2000-01-01T00:00:00Z"));
        assert!(!fresh("2999-01-01T00:00:00Z"));
    }

    #[test]
    fn snapshots_are_immutable_and_failed_status_write_preserves_epoch() {
        let directory = tempfile::tempdir().unwrap();
        let service = PriceService::open(directory.path()).unwrap();
        let first = fixture();
        service.persist(&first).unwrap();
        assert!(service.persist(&first).is_err());
        let second = fixture();
        service.persist(&second).unwrap();
        assert!(directory
            .path()
            .join("pricing/snapshots")
            .join(format!("{}.json", first.id))
            .exists());
        let reopened = PriceService::open(directory.path()).unwrap();
        assert_eq!(reopened.latest_snapshot(), Some(second.clone()));
        let status = directory.path().join("pricing/status.json");
        fs::remove_file(&status).unwrap();
        fs::create_dir(&status).unwrap();
        assert!(service.persist(&fixture()).is_err());
        assert_eq!(service.latest_snapshot().unwrap().id, second.id);
    }

    #[test]
    fn cache_tampering_and_path_traversal_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let service = PriceService::open(directory.path()).unwrap();
        let mut snapshot = fixture();
        service.persist(&snapshot).unwrap();
        snapshot.quotes[0].tiers[0].input = 0.0;
        fs::write(
            directory
                .path()
                .join("pricing/snapshots")
                .join(format!("{}.json", snapshot.id)),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let reopened = PriceService::open(directory.path()).unwrap();
        assert!(reopened.latest_snapshot().is_none());
        assert_eq!(reopened.quote("codex", "gpt-test", "api").status, "blocked");
        assert!(load_snapshot(directory.path(), "../../outside", None).is_err());
    }

    #[test]
    fn redirects_cannot_transfer_sources_between_providers() {
        assert!(allowed_provider_url(
            "kimi",
            &url::Url::parse("https://platform.kimi.ai/docs/pricing/chat.md").unwrap()
        ));
        for value in [
            "http://developers.openai.com/api/docs/pricing.md",
            "https://evil.example/pricing",
            "https://developers.openai.com:444/pricing",
            "https://user@developers.openai.com/pricing",
        ] {
            assert!(!allowed_url(&url::Url::parse(value).unwrap()));
        }
        assert!(!allowed_provider_url(
            "openai",
            &url::Url::parse(KIMI_URL).unwrap()
        ));
    }

    #[tokio::test]
    #[ignore = "explicit online official-pricing smoke test"]
    async fn live_official_prices_refresh_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let service = PriceService::open(directory.path()).unwrap();
        let snapshot = service.refresh().await.unwrap();
        for provider in ["openai", "kimi"] {
            assert!(
                snapshot
                    .sources
                    .iter()
                    .any(|source| source.provider == provider && source.status == "verified"),
                "{provider} did not pass verification: {:?}",
                snapshot
                    .sources
                    .iter()
                    .map(|source| (&source.provider, &source.reason))
                    .collect::<Vec<_>>()
            );
        }
        let reopened = PriceService::open(directory.path()).unwrap();
        assert_eq!(reopened.latest_snapshot(), Some(snapshot.clone()));
        println!(
            "official pricing epoch={} quotes={} sources={}",
            snapshot.id,
            snapshot.quotes.len(),
            snapshot
                .sources
                .iter()
                .map(|source| format!("{}:{}", source.provider, source.status))
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    #[test]
    #[ignore = "explicit downloaded official-source parser smoke test"]
    fn downloaded_official_sources_parse() {
        let folder = std::env::var_os("WONDERLAND_PRICING_FIXTURE_DIR")
            .expect("set fixture folder containing pricing-openai.md, pricing-kimi.md, pricing-anthropic.md, models-anthropic.md and pricing-deepseek.html");
        let folder = PathBuf::from(folder);
        let openai =
            parse_openai(&fs::read_to_string(folder.join("pricing-openai.md")).unwrap()).unwrap();
        let kimi =
            parse_kimi(&fs::read_to_string(folder.join("pricing-kimi.md")).unwrap()).unwrap();
        let anthropic_models_raw = fs::read_to_string(folder.join("models-anthropic.md")).unwrap();
        let anthropic_pricing_raw =
            fs::read_to_string(folder.join("pricing-anthropic.md")).unwrap();
        let identities = parse_anthropic_models(&anthropic_models_raw)
            .expect("real Anthropic model overview must parse");
        let anthropic = parse_anthropic(&anthropic_pricing_raw, &identities)
            .expect("real Anthropic pricing must parse after the identity join");
        let deepseek =
            parse_deepseek(&fs::read_to_string(folder.join("pricing-deepseek.html")).unwrap())
                .expect("real DeepSeek pricing must parse");
        println!(
            "actual official documents parsed: OpenAI={} Kimi={} Anthropic={} DeepSeek={}",
            openai.len(),
            kimi.len(),
            anthropic.len(),
            deepseek.len()
        );
        assert!(!openai.is_empty());
        assert!(!kimi.is_empty());
        assert!(!anthropic.is_empty());
        assert!(!deepseek.is_empty());
    }
}
#[cfg(windows)]
fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
