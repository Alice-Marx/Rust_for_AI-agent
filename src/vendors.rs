//! OpenAI-compatible 厂商预设。
//!
//! `AGENT_PROVIDER=<别名>` 直接指向各家原生端点，不必手写 base URL
//! （仍可用 `*_BASE_URL` 覆盖，用于自建网关或本机代理）。
//! 这些厂商的 chat.completions 兼容层是官方推荐的通用接入方式，
//! 也是 Codex / Kimi CLI 等参考实现里「一套协议适配多模型」的常规做法。

/// 单个厂商的连接参数约定。
#[derive(Debug, Clone, Copy)]
pub struct VendorPreset {
    /// 可用的 `AGENT_PROVIDER` 别名。
    pub names: &'static [&'static str],
    pub default_base_url: &'static str,
    /// API key 环境变量（按顺序取第一个非空值）。
    pub key_vars: &'static [&'static str],
    /// base URL 覆盖环境变量。
    pub base_vars: &'static [&'static str],
    /// 模型名环境变量。
    pub model_vars: &'static [&'static str],
    pub default_model: &'static str,
    /// 是否要求 API key（本地推理服务允许为空）。
    pub requires_key: bool,
}

pub const VENDOR_PRESETS: &[VendorPreset] = &[
    VendorPreset {
        names: &["deepseek"],
        default_base_url: "https://api.deepseek.com/v1",
        key_vars: &["DEEPSEEK_API_KEY"],
        base_vars: &["DEEPSEEK_BASE_URL"],
        model_vars: &["DEEPSEEK_MODEL"],
        default_model: "deepseek-chat",
        requires_key: true,
    },
    VendorPreset {
        names: &["kimi", "moonshot"],
        default_base_url: "https://api.moonshot.cn/v1",
        key_vars: &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
        base_vars: &["MOONSHOT_BASE_URL", "KIMI_BASE_URL"],
        model_vars: &["MOONSHOT_MODEL", "KIMI_MODEL"],
        default_model: "kimi-k2-0905-preview",
        requires_key: true,
    },
    VendorPreset {
        names: &["qwen", "dashscope"],
        default_base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        key_vars: &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
        base_vars: &["DASHSCOPE_BASE_URL", "QWEN_BASE_URL"],
        model_vars: &["QWEN_MODEL"],
        default_model: "qwen3-coder-plus",
        requires_key: true,
    },
    VendorPreset {
        names: &["glm", "zhipu"],
        default_base_url: "https://open.bigmodel.cn/api/paas/v4",
        key_vars: &["ZHIPU_API_KEY", "GLM_API_KEY"],
        base_vars: &["ZHIPU_BASE_URL", "GLM_BASE_URL"],
        model_vars: &["GLM_MODEL"],
        default_model: "glm-4.6",
        requires_key: true,
    },
    VendorPreset {
        names: &["grok", "xai"],
        default_base_url: "https://api.x.ai/v1",
        key_vars: &["XAI_API_KEY"],
        base_vars: &["XAI_BASE_URL"],
        model_vars: &["XAI_MODEL"],
        default_model: "grok-4",
        requires_key: true,
    },
    VendorPreset {
        names: &["gemini", "google"],
        default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        base_vars: &["GEMINI_BASE_URL"],
        model_vars: &["GEMINI_MODEL"],
        default_model: "gemini-2.5-pro",
        requires_key: true,
    },
    VendorPreset {
        names: &["openrouter"],
        default_base_url: "https://openrouter.ai/api/v1",
        key_vars: &["OPENROUTER_API_KEY"],
        base_vars: &["OPENROUTER_BASE_URL"],
        model_vars: &["OPENROUTER_MODEL"],
        default_model: "deepseek/deepseek-chat-v3.1",
        requires_key: true,
    },
    VendorPreset {
        names: &["ollama", "local"],
        default_base_url: "http://127.0.0.1:11434/v1",
        key_vars: &["OLLAMA_API_KEY"],
        base_vars: &["OLLAMA_BASE_URL"],
        model_vars: &["OLLAMA_MODEL"],
        default_model: "qwen3-coder",
        requires_key: false,
    },
];

/// 按 `AGENT_PROVIDER` 别名查找厂商预设。
pub fn vendor_preset(alias: &str) -> Option<&'static VendorPreset> {
    let alias = alias.trim().to_ascii_lowercase();
    VENDOR_PRESETS
        .iter()
        .find(|preset| preset.names.iter().any(|name| *name == alias))
}

/// 解析厂商连接参数：`(base_url, api_key, model)`。
/// `env` 以闭包注入，便于无网络、无进程环境变量的单测。
pub fn resolve_vendor(
    preset: &VendorPreset,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<(String, Option<String>, String)> {
    let lookup = |names: &[&str]| -> Option<String> {
        names
            .iter()
            .filter_map(|name| env(name))
            .map(|value| value.trim().to_string())
            .find(|value| !value.is_empty())
    };
    let base_url = lookup(preset.base_vars).unwrap_or_else(|| preset.default_base_url.to_string());
    let api_key = lookup(preset.key_vars);
    if preset.requires_key && api_key.is_none() {
        anyhow::bail!(
            "{} is required when AGENT_PROVIDER={}",
            preset.key_vars.join(" or "),
            preset.names[0]
        );
    }
    let model = lookup(preset.model_vars).unwrap_or_else(|| preset.default_model.to_string());
    Ok((base_url, api_key, model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn aliases_resolve_to_presets() {
        assert_eq!(
            vendor_preset("deepseek").unwrap().default_model,
            "deepseek-chat"
        );
        assert_eq!(vendor_preset(" KIMI ").unwrap().names[0], "kimi");
        assert_eq!(vendor_preset("moonshot").unwrap().names[0], "kimi");
        assert!(vendor_preset("anthropic").is_none());
        assert!(vendor_preset("nope").is_none());
    }

    #[test]
    fn preset_defaults_are_used_without_env() {
        let preset = vendor_preset("deepseek").unwrap();
        let (base_url, api_key, model) =
            resolve_vendor(preset, &env_from(&[("DEEPSEEK_API_KEY", "sk-test")])).unwrap();
        assert_eq!(base_url, "https://api.deepseek.com/v1");
        assert_eq!(api_key.as_deref(), Some("sk-test"));
        assert_eq!(model, "deepseek-chat");
    }

    #[test]
    fn env_overrides_base_url_and_model() {
        let preset = vendor_preset("kimi").unwrap();
        let (base_url, _, model) = resolve_vendor(
            preset,
            &env_from(&[
                ("KIMI_API_KEY", "sk-kimi"),
                ("KIMI_BASE_URL", "http://127.0.0.1:8317/v1"),
                ("KIMI_MODEL", "kimi-k2-thinking"),
            ]),
        )
        .unwrap();
        assert_eq!(base_url, "http://127.0.0.1:8317/v1");
        assert_eq!(model, "kimi-k2-thinking");
    }

    #[test]
    fn primary_key_var_wins_and_blanks_are_skipped() {
        let preset = vendor_preset("kimi").unwrap();
        let (_, api_key, _) = resolve_vendor(
            preset,
            &env_from(&[("MOONSHOT_API_KEY", "  "), ("KIMI_API_KEY", "sk-kimi")]),
        )
        .unwrap();
        assert_eq!(api_key.as_deref(), Some("sk-kimi"));
    }

    #[test]
    fn missing_key_is_an_error_when_required() {
        let preset = vendor_preset("qwen").unwrap();
        let error = resolve_vendor(preset, &env_from(&[])).unwrap_err();
        assert!(error.to_string().contains("DASHSCOPE_API_KEY"));
    }

    #[test]
    fn local_presets_do_not_require_key() {
        let preset = vendor_preset("ollama").unwrap();
        let (base_url, api_key, model) = resolve_vendor(preset, &env_from(&[])).unwrap();
        assert_eq!(base_url, "http://127.0.0.1:11434/v1");
        assert!(api_key.is_none());
        assert_eq!(model, "qwen3-coder");
    }

    #[test]
    fn every_preset_has_unique_aliases() {
        let mut seen = Vec::new();
        for preset in VENDOR_PRESETS {
            assert!(!preset.names.is_empty());
            for name in preset.names {
                assert!(!seen.contains(name), "duplicate alias {name}");
                seen.push(*name);
            }
            assert!(preset.default_base_url.starts_with("http"));
        }
    }
}
