//! 模型能力档案：把「模型 id」映射到上下文窗口、输出上限、推理参数风格、
//! 提示缓存策略与编辑工具偏好。
//!
//! 参考来源：
//! - Claude Code / Codex 都按模型族切换提示词与工具面（Codex 对 GPT 系优先
//!   `apply_patch`，Claude Code 对 Anthropic 模型优先精确字符串替换）。
//! - Codex 的 `model_reasoning_effort` 与 Anthropic 的 `thinking.budget_tokens`
//!   是两套不同的推理开关，需要按模型族分别下发。
//! - Anthropic 的提示缓存需要在请求里显式打 `cache_control` 断点，OpenAI 兼容
//!   端点则自动缓存，请求里不需要额外字段。

/// 推理参数风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStyle {
    /// 不支持推理开关，忽略相关配置。
    None,
    /// OpenAI 兼容的 `reasoning_effort`（low / medium / high）。
    Effort,
    /// Anthropic 的 `thinking: {type: "enabled", budget_tokens}`。
    ThinkingBudget,
}

/// 提示缓存策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStyle {
    /// 端点自动缓存，请求里不需要额外字段。
    Automatic,
    /// 需要在 system / tools / 消息上显式打 `cache_control` 断点。
    Explicit,
}

/// 编辑类工具的偏好，用于生成模型专属的工具使用指引。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPreference {
    /// 精确字符串替换（FileEdit）。
    StringReplace,
    /// V4A 多文件补丁（ApplyPatch）。
    Patch,
}

/// 模型能力档案。
#[derive(Debug, Clone, Copy)]
pub struct ModelProfile {
    /// 档案名（用于日志与 `AGENT_MODEL_PROFILE` 强制指定）。
    pub name: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub reasoning: ReasoningStyle,
    pub cache: CacheStyle,
    pub edit_preference: EditPreference,
}

const DEFAULT_PROFILE: ModelProfile = ModelProfile {
    name: "generic",
    context_window: 200_000,
    max_output_tokens: 8_192,
    reasoning: ReasoningStyle::None,
    cache: CacheStyle::Automatic,
    edit_preference: EditPreference::StringReplace,
};

const CLAUDE_PROFILE: ModelProfile = ModelProfile {
    name: "anthropic-claude",
    context_window: 200_000,
    max_output_tokens: 32_000,
    reasoning: ReasoningStyle::ThinkingBudget,
    cache: CacheStyle::Explicit,
    edit_preference: EditPreference::StringReplace,
};

const OPENAI_REASONING_PROFILE: ModelProfile = ModelProfile {
    name: "openai-reasoning",
    context_window: 272_000,
    max_output_tokens: 32_000,
    reasoning: ReasoningStyle::Effort,
    cache: CacheStyle::Automatic,
    edit_preference: EditPreference::Patch,
};

const OPENAI_CHAT_PROFILE: ModelProfile = ModelProfile {
    name: "openai-chat",
    context_window: 128_000,
    max_output_tokens: 16_384,
    reasoning: ReasoningStyle::None,
    cache: CacheStyle::Automatic,
    edit_preference: EditPreference::StringReplace,
};

const QWEN_PROFILE: ModelProfile = ModelProfile {
    name: "qwen",
    context_window: 262_144,
    max_output_tokens: 16_384,
    reasoning: ReasoningStyle::None,
    cache: CacheStyle::Automatic,
    edit_preference: EditPreference::StringReplace,
};

/// 内置档案表：按顺序做「模型 id 小写包含」匹配，先命中先返回，
/// 越具体的条目越靠前。
pub const BUILTIN_PROFILES: &[(&str, ModelProfile)] = &[
    ("claude", CLAUDE_PROFILE),
    ("anthropic", CLAUDE_PROFILE),
    ("codex", OPENAI_REASONING_PROFILE),
    ("gpt-5", OPENAI_REASONING_PROFILE),
    (
        "o3",
        ModelProfile {
            context_window: 200_000,
            ..OPENAI_REASONING_PROFILE
        },
    ),
    (
        "o4",
        ModelProfile {
            context_window: 200_000,
            ..OPENAI_REASONING_PROFILE
        },
    ),
    (
        "gpt-4.1",
        ModelProfile {
            context_window: 1_047_576,
            ..OPENAI_CHAT_PROFILE
        },
    ),
    (
        "gpt-4o",
        ModelProfile {
            max_output_tokens: 16_384,
            ..OPENAI_CHAT_PROFILE
        },
    ),
    (
        "deepseek-reasoner",
        ModelProfile {
            name: "deepseek-reasoner",
            context_window: 131_072,
            max_output_tokens: 65_536,
            ..DEFAULT_PROFILE
        },
    ),
    (
        "deepseek",
        ModelProfile {
            name: "deepseek-chat",
            context_window: 131_072,
            ..DEFAULT_PROFILE
        },
    ),
    (
        "kimi",
        ModelProfile {
            name: "kimi",
            context_window: 262_144,
            max_output_tokens: 16_384,
            ..DEFAULT_PROFILE
        },
    ),
    (
        "moonshot",
        ModelProfile {
            name: "kimi",
            context_window: 262_144,
            max_output_tokens: 16_384,
            ..DEFAULT_PROFILE
        },
    ),
    ("qwen", QWEN_PROFILE),
    (
        "gemini",
        ModelProfile {
            name: "gemini",
            context_window: 1_048_576,
            max_output_tokens: 65_536,
            ..DEFAULT_PROFILE
        },
    ),
    (
        "glm",
        ModelProfile {
            name: "glm",
            context_window: 204_800,
            max_output_tokens: 16_384,
            ..DEFAULT_PROFILE
        },
    ),
    (
        "grok",
        ModelProfile {
            name: "grok",
            context_window: 262_144,
            max_output_tokens: 16_384,
            reasoning: ReasoningStyle::Effort,
            ..DEFAULT_PROFILE
        },
    ),
];

impl ModelProfile {
    /// 按模型 id 解析档案。
    pub fn resolve(model: &str) -> Self {
        Self::resolve_with_override(model, None)
    }

    /// `force` 非空时按键名（匹配关键字或档案名）强制指定，未知键回退到模型 id 匹配。
    pub fn resolve_with_override(model: &str, force: Option<&str>) -> Self {
        if let Some(force) = force.map(str::trim).filter(|value| !value.is_empty()) {
            let needle = force.to_ascii_lowercase();
            if let Some((_, profile)) = BUILTIN_PROFILES
                .iter()
                .find(|(key, profile)| *key == needle || profile.name == needle)
            {
                return *profile;
            }
            tracing::warn!(profile = %force, "unknown model profile override, falling back to model id");
        }
        let needle = model.trim().to_ascii_lowercase();
        if !needle.is_empty() {
            if let Some((_, profile)) = BUILTIN_PROFILES
                .iter()
                .find(|(key, _)| needle.contains(key))
            {
                return *profile;
            }
        }
        DEFAULT_PROFILE
    }

    /// 结合 `AGENT_MODEL_PROFILE` / `AGENT_CONTEXT_WINDOW` / `AGENT_MAX_OUTPUT_TOKENS`
    /// 得到最终生效的档案。
    pub fn from_env(model: &str) -> Self {
        let override_name = std::env::var("AGENT_MODEL_PROFILE").ok();
        let mut profile = Self::resolve_with_override(model, override_name.as_deref());
        if let Some(window) = env_u64("AGENT_CONTEXT_WINDOW") {
            profile.context_window = window;
        }
        if let Some(max_output) = env_u64("AGENT_MAX_OUTPUT_TOKENS") {
            profile.max_output_tokens = max_output.min(u64::from(u32::MAX)) as u32;
        }
        profile
    }

    pub fn supports_reasoning(&self) -> bool {
        self.reasoning != ReasoningStyle::None
    }

    /// 模型专属的工具使用指引，插在系统提示词的静态段（保护 prompt cache 前缀）。
    pub fn tool_guidance(&self, reasoning_effort: Option<&str>) -> String {
        let mut section = String::from("\n## Tool Preferences\n\n");
        match self.edit_preference {
            EditPreference::Patch => {
                section.push_str(
                    "- This model handles V4A patches best: prefer the `ApplyPatch` tool for multi-file or multi-hunk edits.\n\
                     - Use `FileEdit` only for a single exact-string replacement in a file you have already read.\n\
                     - Use `Bash` for commands, not for editing files.\n",
                );
            }
            EditPreference::StringReplace => {
                section.push_str(
                    "- Read a file before editing it; `FileWrite` / `FileEdit` reject writes to files you have not read in this session.\n\
                     - Prefer `FileEdit` with an exact unique string for surgical changes; use `ApplyPatch` when the user asks for a patch.\n\
                     - Keep edits minimal and do not reformat unrelated code.\n",
                );
            }
        }
        match self.reasoning {
            ReasoningStyle::Effort => {
                if let Some(effort) = reasoning_effort {
                    section.push_str(&format!(
                        "- Reasoning effort for this run is `{effort}`; scale how much you deliberate accordingly.\n"
                    ));
                }
            }
            ReasoningStyle::ThinkingBudget => {
                section.push_str(
                    "- Extended thinking is enabled: plan inside the thinking block, then act with tools.\n",
                );
            }
            ReasoningStyle::None => {}
        }
        section
    }
}

/// 本次请求实际下发的推理档位：
/// - 档案不支持推理 → `None`；
/// - OpenAI 风格推理模型未显式配置时默认 `medium`（与 Codex 默认档位一致）；
/// - Anthropic 的 thinking 会产生额外计费，因此只在显式配置时开启。
pub fn reasoning_effort_for(profile: &ModelProfile, configured: Option<&str>) -> Option<String> {
    let configured = configured.map(str::trim).filter(|value| !value.is_empty());
    match profile.reasoning {
        ReasoningStyle::None => None,
        ReasoningStyle::Effort => Some(configured.unwrap_or("medium").to_string()),
        ReasoningStyle::ThinkingBudget => configured.map(str::to_string),
    }
}

/// `AGENT_REASONING_EFFORT`：low / medium / high。`none` / `off` 表示关闭。
pub fn configured_reasoning_effort() -> Option<String> {
    let value = std::env::var("AGENT_REASONING_EFFORT").ok()?;
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value == "none" || value == "off" {
        return None;
    }
    Some(value)
}

/// Anthropic `thinking.budget_tokens`：显式配置优先，其次按 effort 档位换算。
pub fn thinking_budget_tokens(effort: Option<&str>) -> u32 {
    if let Some(budget) = env_u64("AGENT_THINKING_BUDGET") {
        return budget.min(u64::from(u32::MAX)) as u32;
    }
    match effort.map(str::to_ascii_lowercase).as_deref() {
        Some("low") | Some("minimal") => 2_048,
        Some("high") | Some("max") => 16_000,
        _ => 8_000,
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_model_families() {
        let claude = ModelProfile::resolve("claude-sonnet-4-5-20250929");
        assert_eq!(claude.name, "anthropic-claude");
        assert_eq!(claude.reasoning, ReasoningStyle::ThinkingBudget);
        assert_eq!(claude.cache, CacheStyle::Explicit);

        let gpt5 = ModelProfile::resolve("gpt-5.4-codex");
        assert_eq!(gpt5.name, "openai-reasoning");
        assert_eq!(gpt5.reasoning, ReasoningStyle::Effort);
        assert_eq!(gpt5.edit_preference, EditPreference::Patch);
        assert_eq!(gpt5.context_window, 272_000);

        assert_eq!(
            ModelProfile::resolve("kimi-k2-0905-preview").context_window,
            262_144
        );
        assert_eq!(ModelProfile::resolve("deepseek-chat").name, "deepseek-chat");
        assert_eq!(
            ModelProfile::resolve("deepseek-reasoner").max_output_tokens,
            65_536
        );
        assert_eq!(
            ModelProfile::resolve("claude-opus-4-1").cache,
            CacheStyle::Explicit
        );
    }

    #[test]
    fn unknown_model_falls_back_to_generic_profile() {
        let profile = ModelProfile::resolve("some-local-model");
        assert_eq!(profile.name, "generic");
        assert_eq!(profile.context_window, 200_000);
        assert_eq!(profile.edit_preference, EditPreference::StringReplace);
        assert!(!profile.supports_reasoning());
    }

    #[test]
    fn empty_model_id_is_generic() {
        assert_eq!(ModelProfile::resolve("   ").name, "generic");
    }

    #[test]
    fn forced_profile_wins_over_model_id() {
        assert_eq!(
            ModelProfile::resolve_with_override("gpt-5.4", Some("anthropic-claude")).name,
            "anthropic-claude"
        );
        assert_eq!(
            ModelProfile::resolve_with_override("gpt-5.4", Some("CLAUDE")).name,
            "anthropic-claude"
        );
        // 未知覆盖名回退到模型 id 匹配。
        assert_eq!(
            ModelProfile::resolve_with_override("gpt-5.4", Some("nope")).name,
            "openai-reasoning"
        );
        assert_eq!(
            ModelProfile::resolve_with_override("gpt-5.4", Some("  ")).name,
            "openai-reasoning"
        );
    }

    #[test]
    fn reasoning_effort_is_gated_by_profile() {
        let claude = ModelProfile::resolve("claude-sonnet-4-5");
        // thinking 需要显式开启。
        assert_eq!(reasoning_effort_for(&claude, None), None);
        assert_eq!(
            reasoning_effort_for(&claude, Some("high")),
            Some("high".to_string())
        );

        let gpt5 = ModelProfile::resolve("gpt-5.4");
        // OpenAI 推理模型默认 medium。
        assert_eq!(
            reasoning_effort_for(&gpt5, None),
            Some("medium".to_string())
        );

        let deepseek = ModelProfile::resolve("deepseek-chat");
        assert_eq!(reasoning_effort_for(&deepseek, Some("high")), None);
        assert_eq!(reasoning_effort_for(&deepseek, Some("  ")), None);
    }

    #[test]
    fn tool_guidance_matches_edit_preference() {
        let patch = ModelProfile::resolve("gpt-5.4").tool_guidance(Some("high"));
        assert!(patch.contains("ApplyPatch"));
        assert!(patch.contains("`high`"));

        let edit = ModelProfile::resolve("claude-sonnet-4-5").tool_guidance(None);
        assert!(edit.contains("FileEdit"));
        assert!(edit.contains("Extended thinking"));

        // 不支持推理开关的档案不注入档位说明。
        let plain = ModelProfile::resolve("deepseek-chat").tool_guidance(Some("high"));
        assert!(!plain.contains("Reasoning effort"));
    }

    #[test]
    fn thinking_budget_scales_with_effort() {
        assert_eq!(thinking_budget_tokens(Some("low")), 2_048);
        assert_eq!(thinking_budget_tokens(Some("high")), 16_000);
        assert_eq!(thinking_budget_tokens(Some("medium")), 8_000);
        assert_eq!(thinking_budget_tokens(None), 8_000);
    }
}
