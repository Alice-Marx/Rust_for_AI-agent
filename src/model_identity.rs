//! Versioned identity mapping between an exact model + reasoning configuration
//! and a LiveBench leaderboard entry.
//!
//! This module answers one narrow question: which exact LiveBench leaderboard
//! row, if any, is attested to correspond to an exact (app, model, reasoning
//! effort) tuple. It is not a ranking, a quality score, or a routing decision.
//! `Unknown` is a normal result, not an error: every model without a reviewed
//! record resolves to unknown and stays out of automatic dispatch candidates.
//!
//! The mapping data is embedded into the binary at compile time from
//! `assets/model-identity/v1.json`, so an installed binary always carries it
//! and it cannot be tampered with at runtime. Upgrading the mapping requires a
//! code change with per-record evidence attached, reviewed like any other code.

use std::collections::BTreeSet;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAPPING_JSON: &str = include_str!("../assets/model-identity/v1.json");
const SUPPORTED_MAJOR_VERSION: u64 = 1;
const MAX_RECORDS: usize = 4_096;
const MAX_MODEL_BYTES: usize = 200;
const MAX_ENTRY_BYTES: usize = 512;
const MAX_EVIDENCE_BYTES: usize = 1_024;
const MAX_EVIDENCE_ITEMS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMapping {
    mapping_version: String,
    created_at: String,
    records: Vec<RawRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecord {
    app_id: String,
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
    status: String,
    #[serde(default)]
    livebench_entry: Option<String>,
    #[serde(default)]
    evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordStatus {
    Attested,
    NotListed,
}

#[derive(Clone, Debug)]
struct IdentityRecord {
    app_id: String,
    model: String,
    reasoning_effort: Option<String>,
    status: RecordStatus,
    livebench_entry: Option<String>,
    // Evidence is validated on load (non-empty, bounded) and lives in the
    // embedded JSON for review; resolutions never surface it.
}

/// The result kind of an identity lookup. `Unknown` carries no negative
/// signal about the model itself; it only means no reviewed mapping applies.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MappingOutcome {
    Attested,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resolution {
    pub outcome: MappingOutcome,
    pub livebench_entry: Option<String>,
    pub reason: Option<String>,
    pub mapping_version: String,
}

pub struct ModelIdentityRegistry {
    mapping_version: String,
    created_at: String,
    records: Vec<IdentityRecord>,
}

impl ModelIdentityRegistry {
    /// Parse and fully validate the embedded mapping data. Any validation
    /// failure is an error; there is no lenient fallback for identity data.
    pub fn load() -> Result<Self> {
        let raw: RawMapping =
            serde_json::from_str(MAPPING_JSON).context("模型身份映射数据不是有效 JSON")?;
        validate_mapping(raw)
    }

    /// The mapping version that attests every resolution this registry makes;
    /// consumers persist it with their decisions as an evidence trail.
    pub fn mapping_version(&self) -> &str {
        &self.mapping_version
    }

    /// Test-only constructor from arbitrary JSON so downstream modules can
    /// exercise their identity-consumption logic against synthetic records.
    #[cfg(test)]
    pub(crate) fn from_json(json: &str) -> Result<Self> {
        let raw: RawMapping = serde_json::from_str(json).context("测试映射数据不是有效 JSON")?;
        validate_mapping(raw)
    }

    /// Resolve an exact (app, model, reasoning effort) tuple against a LiveBench
    /// snapshot. Aliases, prefixes and fuzzy matches are never substituted.
    pub fn resolve(
        &self,
        app_id: &str,
        model: &str,
        reasoning_effort: Option<&str>,
        snapshot: &crate::model_intelligence::Snapshot,
    ) -> Resolution {
        let unknown = |reason: String| Resolution {
            outcome: MappingOutcome::Unknown,
            livebench_entry: None,
            reason: Some(reason),
            mapping_version: self.mapping_version.clone(),
        };
        let candidates = self
            .records
            .iter()
            .filter(|record| record.app_id == app_id && record.model == model)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return unknown(format!(
                "该精确应用/模型没有身份映射记录 ({app_id}/{model})；从不使用别名、前缀或模糊匹配替代"
            ));
        }
        let Some(record) = candidates
            .iter()
            .find(|record| record.reasoning_effort.as_deref() == reasoning_effort)
        else {
            return unknown(format!(
                "模型 {model} 的推理变体 {effort} 未映射；记录的推理档位与请求不一致",
                effort = reasoning_effort.unwrap_or("<none>")
            ));
        };
        if record.status == RecordStatus::NotListed {
            return unknown(format!(
                "模型 {model} 经核验不在 LiveBench 榜单中，无基准证据"
            ));
        }
        let entry = record
            .livebench_entry
            .clone()
            .unwrap_or_else(|| unreachable!("attested records always carry an entry"));
        if !snapshot.models.iter().any(|score| score.model == entry) {
            return unknown(format!(
                "映射条目 {entry} 在当前快照 {} 中不存在，映射需要复核",
                snapshot.release
            ));
        }
        Resolution {
            outcome: MappingOutcome::Attested,
            livebench_entry: Some(entry),
            reason: None,
            mapping_version: self.mapping_version.clone(),
        }
    }

    /// Registry summary for status reporting; shape mirrors the
    /// model_intelligence status JSON style.
    pub fn summary(&self) -> Value {
        let attested = self
            .records
            .iter()
            .filter(|record| record.status == RecordStatus::Attested)
            .count();
        json!({
            "status": "loaded",
            "mapping_version": self.mapping_version,
            "created_at": self.created_at,
            "records_total": self.records.len(),
            "attested": attested,
            "not_listed": self.records.len() - attested,
            "note": "未覆盖的模型解析为 unknown，不进入自动派工候选；映射升级必须修改代码并附证据",
        })
    }
}

fn validate_mapping(raw: RawMapping) -> Result<ModelIdentityRegistry> {
    let mut components = raw.mapping_version.split('.');
    let major = components
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .context("mapping_version 主版本缺失或无效")?;
    ensure!(
        major == SUPPORTED_MAJOR_VERSION,
        "mapping_version 主版本不兼容：{}（仅支持 {SUPPORTED_MAJOR_VERSION}.x）",
        raw.mapping_version
    );
    ensure!(
        components.all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
        "mapping_version 格式无效：{}",
        raw.mapping_version
    );
    chrono::NaiveDate::parse_from_str(&raw.created_at, "%Y-%m-%d")
        .context("created_at 必须是 YYYY-MM-DD 日期")?;
    ensure!(
        !raw.records.is_empty() && raw.records.len() <= MAX_RECORDS,
        "模型身份映射记录数量无效"
    );
    let mut keys = BTreeSet::new();
    let mut records = Vec::with_capacity(raw.records.len());
    for record in &raw.records {
        records.push(validate_record(record)?);
        ensure!(
            keys.insert((
                record.app_id.clone(),
                record.model.clone(),
                record.reasoning_effort.clone()
            )),
            "模型身份映射记录重复：{}/{}/{:?}",
            record.app_id,
            record.model,
            record.reasoning_effort
        );
    }
    Ok(ModelIdentityRegistry {
        mapping_version: raw.mapping_version,
        created_at: raw.created_at,
        records,
    })
}

fn validate_record(record: &RawRecord) -> Result<IdentityRecord> {
    ensure!(
        crate::native_executor::supports_native(&record.app_id),
        "模型身份映射的 app_id 不是受管适配器：{}",
        record.app_id
    );
    validate_nonempty_bounded(&record.model, "模型 ID", MAX_MODEL_BYTES)?;
    if let Some(effort) = &record.reasoning_effort {
        ensure!(!effort.is_empty(), "reasoning_effort 不能为空字符串");
        ensure!(
            !effort.chars().any(char::is_whitespace),
            "reasoning_effort 不能包含空白字符"
        );
    }
    if let Some(entry) = &record.livebench_entry {
        validate_nonempty_bounded(entry, "LiveBench 榜单条目", MAX_ENTRY_BYTES)?;
    }
    ensure!(
        !record.evidence.is_empty() && record.evidence.len() <= MAX_EVIDENCE_ITEMS,
        "每条映射记录必须附非空证据列表"
    );
    for item in &record.evidence {
        validate_nonempty_bounded(item, "证据条目", MAX_EVIDENCE_BYTES)?;
    }
    let status = match record.status.as_str() {
        "attested" => {
            ensure!(
                record.livebench_entry.is_some(),
                "attested 记录必须给出精确的 LiveBench 榜单条目"
            );
            RecordStatus::Attested
        }
        "not_listed" => {
            ensure!(
                record.livebench_entry.is_none(),
                "not_listed 记录的 livebench_entry 必须为 null"
            );
            RecordStatus::NotListed
        }
        other => anyhow::bail!("未知的映射状态：{other}（仅允许 attested/not_listed）"),
    };
    Ok(IdentityRecord {
        app_id: record.app_id.clone(),
        model: record.model.clone(),
        reasoning_effort: record.reasoning_effort.clone(),
        status,
        livebench_entry: record.livebench_entry.clone(),
    })
}

fn validate_nonempty_bounded(value: &str, label: &str, max: usize) -> Result<()> {
    ensure!(!value.is_empty(), "{label}不能为空");
    ensure!(value.len() <= max, "{label}超过大小上限");
    ensure!(value.trim() == value, "{label}包含首尾空白");
    ensure!(!value.chars().any(char::is_control), "{label}包含控制字符");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_intelligence::{ModelScore, Snapshot};
    use std::collections::BTreeMap;

    fn snapshot_with(entries: &[&str]) -> Snapshot {
        Snapshot {
            parser_version: 1,
            id: "synthetic".into(),
            release: "2026-06-25".into(),
            checked_at: "2026-09-22T00:00:00Z".into(),
            new_livebench_sha: "a".repeat(40),
            livebench_sha: "b".repeat(40),
            table_url: "https://example.invalid/table.csv".into(),
            categories_url: "https://example.invalid/categories.json".into(),
            constants_url: "https://example.invalid/constants.js".into(),
            table_sha256: "c".repeat(64),
            categories_sha256: "d".repeat(64),
            constants_sha256: "e".repeat(64),
            models: entries
                .iter()
                .map(|name| ModelScore {
                    model: (*name).to_owned(),
                    scores: BTreeMap::from([("Coding".to_owned(), 90.0)]),
                    missing_tasks: Vec::new(),
                })
                .collect(),
            categories: vec!["Coding".into()],
            category_tasks: BTreeMap::from([("Coding".to_owned(), vec!["code_generation".into()])]),
            raw_table: String::new(),
            raw_categories: json!({}),
            raw_categories_json: String::new(),
            raw_constants: String::new(),
        }
    }

    fn full_snapshot() -> Snapshot {
        snapshot_with(&["kimi-k2.7-code", "kimi-k3"])
    }

    fn validate_value(value: Value) -> Result<ModelIdentityRegistry> {
        validate_mapping(serde_json::from_value(value).context("测试映射数据结构无效")?)
    }

    fn embedded_value() -> Value {
        serde_json::from_str(MAPPING_JSON).unwrap()
    }

    #[test]
    fn embedded_registry_loads_and_seed_records_validate() {
        let registry = ModelIdentityRegistry::load().unwrap();
        assert_eq!(registry.mapping_version, "1.0.0");
        assert_eq!(registry.records.len(), 4);
        let summary = registry.summary();
        assert_eq!(summary["status"], "loaded");
        assert_eq!(summary["records_total"], 4);
        assert_eq!(summary["attested"], 2);
        assert_eq!(summary["not_listed"], 2);
        assert_eq!(summary["mapping_version"], "1.0.0");
    }

    #[test]
    fn exact_app_model_and_effort_tuple_attests() {
        let registry = ModelIdentityRegistry::load().unwrap();
        let snapshot = full_snapshot();
        let resolution = registry.resolve("kimi-cli", "kimi-k2.7-code", None, &snapshot);
        assert_eq!(resolution.outcome, MappingOutcome::Attested);
        assert_eq!(
            resolution.livebench_entry.as_deref(),
            Some("kimi-k2.7-code")
        );
        assert_eq!(resolution.reason, None);
        assert_eq!(resolution.mapping_version, "1.0.0");
        let resolution = registry.resolve("kimi-cli", "kimi-k3", None, &snapshot);
        assert_eq!(resolution.outcome, MappingOutcome::Attested);
        assert_eq!(resolution.livebench_entry.as_deref(), Some("kimi-k3"));
    }

    #[test]
    fn prefixes_aliases_and_foreign_apps_are_never_substituted() {
        let registry = ModelIdentityRegistry::load().unwrap();
        let snapshot = full_snapshot();
        for (app_id, model) in [
            ("kimi-cli", "kimi-k2.7"),
            ("kimi-cli", "kimi-k2"),
            ("codex", "kimi-k2.7-code"),
            ("codex", "gpt-5.2-2025-12-11"),
        ] {
            let resolution = registry.resolve(app_id, model, None, &snapshot);
            assert_eq!(
                resolution.outcome,
                MappingOutcome::Unknown,
                "{app_id}/{model}"
            );
            assert_eq!(resolution.livebench_entry, None);
            assert!(resolution
                .reason
                .as_deref()
                .unwrap()
                .contains("没有身份映射记录"));
        }
    }

    #[test]
    fn unmapped_reasoning_variant_is_unknown() {
        let registry = ModelIdentityRegistry::load().unwrap();
        let snapshot = full_snapshot();
        let resolution = registry.resolve("kimi-cli", "kimi-k2.7-code", Some("high"), &snapshot);
        assert_eq!(resolution.outcome, MappingOutcome::Unknown);
        assert!(resolution.reason.as_deref().unwrap().contains("推理变体"));
    }

    #[test]
    fn verified_unlisted_models_resolve_unknown_with_benchmark_reason() {
        let registry = ModelIdentityRegistry::load().unwrap();
        let snapshot = full_snapshot();
        for model in ["kimi-k2.6", "kimi-k2.7-code-highspeed"] {
            let resolution = registry.resolve("kimi-cli", model, None, &snapshot);
            assert_eq!(resolution.outcome, MappingOutcome::Unknown, "{model}");
            assert!(resolution
                .reason
                .as_deref()
                .unwrap()
                .contains("不在 LiveBench 榜单"));
        }
    }

    #[test]
    fn attested_entry_missing_from_snapshot_requires_review() {
        let registry = ModelIdentityRegistry::load().unwrap();
        let snapshot = snapshot_with(&["kimi-k2.7-code"]);
        let resolution = registry.resolve("kimi-cli", "kimi-k3", None, &snapshot);
        assert_eq!(resolution.outcome, MappingOutcome::Unknown);
        let reason = resolution.reason.unwrap();
        assert!(reason.contains("kimi-k3"));
        assert!(reason.contains("2026-06-25"));
        assert!(reason.contains("需要复核"));
    }

    #[test]
    fn validator_rejects_duplicate_keys_and_unknown_apps() {
        let mut duplicate = embedded_value();
        let first = duplicate["records"][0].clone();
        duplicate["records"].as_array_mut().unwrap().push(first);
        assert!(validate_value(duplicate).is_err());

        let mut unknown_app = embedded_value();
        unknown_app["records"][0]["app_id"] = json!("mimo");
        assert!(validate_value(unknown_app).is_err());
    }

    #[test]
    fn validator_enforces_status_and_entry_consistency() {
        let mut attested_without_entry = embedded_value();
        attested_without_entry["records"][0]["livebench_entry"] = Value::Null;
        assert!(validate_value(attested_without_entry).is_err());

        let mut attested_without_evidence = embedded_value();
        attested_without_evidence["records"][0]["evidence"] = json!([]);
        assert!(validate_value(attested_without_evidence).is_err());

        let mut not_listed_with_entry = embedded_value();
        not_listed_with_entry["records"][2]["livebench_entry"] = json!("kimi-k2.7-code");
        assert!(validate_value(not_listed_with_entry).is_err());

        let mut bad_status = embedded_value();
        bad_status["records"][0]["status"] = json!("verified");
        assert!(validate_value(bad_status).is_err());
    }

    #[test]
    fn validator_rejects_incompatible_major_version_and_malformed_fields() {
        let mut bad_version = embedded_value();
        bad_version["mapping_version"] = json!("2.0.0");
        assert!(validate_value(bad_version).is_err());
        let mut bad_version = embedded_value();
        bad_version["mapping_version"] = json!("1.x");
        assert!(validate_value(bad_version).is_err());

        let mut padded_model = embedded_value();
        padded_model["records"][0]["model"] = json!(" kimi-k2.7-code");
        assert!(validate_value(padded_model).is_err());

        let mut blank_effort = embedded_value();
        blank_effort["records"][0]["reasoning_effort"] = json!("high effort");
        assert!(validate_value(blank_effort).is_err());

        let mut control_entry = embedded_value();
        control_entry["records"][0]["livebench_entry"] = json!("kimi-k2.7-code\n");
        assert!(validate_value(control_entry).is_err());
    }
}
