//! Source-backed, direct-API list prices. These are not subscription prices,
//! provider availability guarantees, or an identity bridge for benchmark variants.
//! Only bounded, recognized document schemas are decoded; changed schemas fail
//! closed for that provider. Every refresh fetches all four official sources.

use anyhow::{ensure, Context, Result};
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
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
const DEEPSEEK_URL: &str = "https://api-docs.deepseek.com/quick_start/pricing";
const SOURCE_SPECS: [(&str, &str); 4] = [
    ("openai", OPENAI_URL),
    ("kimi", KIMI_URL),
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
            parser_version: 1,
            id: Uuid::new_v4().to_string(),
            checked_at: checked_at.clone(),
            sources: Vec::new(),
            quotes: Vec::new(),
        };
        for source in results {
            let (source, quotes) = decode_source(source);
            snapshot.sources.push(source);
            snapshot.quotes.extend(quotes);
        }
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
        json!({
            "latest_snapshot":state.latest.as_ref().map(|snapshot| json!({"id":snapshot.id,"checked_at":snapshot.checked_at,"quote_count":snapshot.quotes.len(),"sources":snapshot.sources.iter().map(|source|json!({"provider":source.provider,"status":source.status,"reason":source.reason,"url":source.final_url,"sha256":source.sha256,"checked_at":source.checked_at})).collect::<Vec<_>>()})),
            "latest_failure":state.stored.latest_failure,
            "cache_is_stale":state.stored.latest_failure.is_some() || state.latest.as_ref().is_none_or(|snapshot|!fresh(&snapshot.checked_at)),
            "max_quote_age_seconds":MAX_AGE_SECONDS,
            "online_verified_in_this_process":state.live_snapshot_id.is_some() && state.stored.latest_failure.is_none(),
            "refresh_required_each_dispatch_round":true,
            "subscription_pricing":"unknown; subscription quota is not zero-cost API tokens",
            "auto_dispatch_ready":false,
            "note":"Cached snapshots are for display. Every dispatch round must refresh online and bind quotes to that snapshot ID. Exact direct-API routing also requires a verified endpoint, billing channel, model identity, benchmark variant and matching tier conditions."
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
            Some("Only direct API billing is verified; subscriptions, proxies, cloud resellers, credits, and unknown billing channels cannot inherit API token prices".to_owned())
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

fn decode_source(mut source: PriceSource) -> (PriceSource, Vec<ModelQuote>) {
    let Some(raw) = source.raw.as_deref() else {
        return (source, Vec::new());
    };
    let result=match source.provider.as_str() {
        "openai"=>parse_openai(raw),
        "kimi"=>parse_kimi(raw),
        "anthropic"=>Err(anyhow::anyhow!("Official prices were fetched, but display names are not yet joined to a separately verified exact API model ID and context/residency tier; no Claude quote is inferred")),
        "deepseek"=>Err(anyhow::anyhow!("Official prices were fetched, but peak/off-peak rates depend on UTC windows and Chinese public holidays; calendar/tier semantics are not verified, and retired aliases are not mapped")),
        _=>Err(anyhow::anyhow!("unsupported official pricing source")),
    };
    match result {
        Ok(mut quotes) => {
            for quote in &mut quotes {
                quote.source_url = source
                    .final_url
                    .clone()
                    .unwrap_or_else(|| source.requested_url.clone());
                quote.source_sha256 = source.sha256.clone().unwrap_or_default();
                quote.checked_at = source.checked_at.clone();
            }
            source.status = "verified".into();
            source.reason = None;
            (source, quotes)
        }
        Err(error) => {
            source.status = "blocked".into();
            source.reason = Some(bounded_error(&format!("{error:#}")));
            (source, Vec::new())
        }
    }
}

fn new_quote(provider: &str, model: &str, tiers: Vec<PriceTier>) -> ModelQuote {
    ModelQuote {provider:provider.into(),app_ids:match provider {"openai"=>vec!["codex".into()],"kimi"=>vec!["kimi-cli".into(),"kimi-code".into()],_=>Vec::new()},model:model.into(),billing_channel:"api".into(),billing_scope:"first_party_direct_api_list_price".into(),currency:"USD".into(),unit:"per_1_000_000_tokens".into(),tiers,source_url:String::new(),source_sha256:String::new(),checked_at:String::new(),conditions:vec!["Direct provider API only; verify the actual endpoint and account billing channel before use".into(),"Public token list prices; taxes, account discounts, credits and non-token tool fees are excluded".into()],unknowns:vec!["Account access, negotiated rates and provider rate limits are not verified".into()]}
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
            "anthropic" => url.host_str() == Some("platform.claude.com"),
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
            && snapshot.parser_version == 1
            && snapshot.sources.len() == SOURCE_SPECS.len(),
        "cached pricing snapshot metadata invalid"
    );
    chrono::DateTime::parse_from_rfc3339(&snapshot.checked_at)?;
    let mut reparsed = Vec::new();
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
                    )?
                ),
                "cached price origin invalid"
            );
            let (decoded, quotes) = decode_source(source.clone());
            ensure!(
                decoded == *source,
                "cached price source status mismatches parser"
            );
            reparsed.extend(quotes);
        } else {
            ensure!(
                source.status == "failed" && source.reason.is_some(),
                "cached price source is missing raw data"
            );
        }
    }
    ensure!(
        reparsed == snapshot.quotes && !reparsed.is_empty(),
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

    fn fixture() -> PriceSnapshot {
        let checked_at = now();
        let mut snapshot = PriceSnapshot {
            parser_version: 1,
            id: Uuid::new_v4().to_string(),
            checked_at: checked_at.clone(),
            sources: Vec::new(),
            quotes: Vec::new(),
        };
        for (provider, url) in SOURCE_SPECS {
            let raw = match provider {
                "openai" => OPENAI,
                "kimi" => KIMI,
                _ => "official source with unverified pricing semantics",
            }
            .to_owned();
            let source = PriceSource {
                provider: provider.into(),
                requested_url: url.into(),
                final_url: Some(url.into()),
                checked_at: checked_at.clone(),
                status: "fetched".into(),
                reason: None,
                sha256: Some(hash(raw.as_bytes())),
                raw: Some(raw),
            };
            let (source, quotes) = decode_source(source);
            snapshot.sources.push(source);
            snapshot.quotes.extend(quotes);
        }
        snapshot
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
            .expect("set fixture folder containing pricing-openai.md and pricing-kimi.md");
        let folder = PathBuf::from(folder);
        let openai =
            parse_openai(&fs::read_to_string(folder.join("pricing-openai.md")).unwrap()).unwrap();
        let kimi =
            parse_kimi(&fs::read_to_string(folder.join("pricing-kimi.md")).unwrap()).unwrap();
        println!(
            "actual official documents parsed: OpenAI={} Kimi={}",
            openai.len(),
            kimi.len()
        );
        assert!(!openai.is_empty());
        assert!(!kimi.is_empty());
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
