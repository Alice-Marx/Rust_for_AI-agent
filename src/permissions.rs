use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// 权限模式，对应 Claude Code 的 permissionMode。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    Plan,
    AcceptEdits,
    BypassPermissions,
    DontAsk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Ask,
    Deny,
}

/// 规则来源：用户级 / 项目级 / 本地（不入库）/ CLI 参数 / 会话内动态授予。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleSource {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    CliArg,
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// 工具名，如 "Bash"、"FileEdit"；"*" 匹配所有工具。
    pub tool_name: String,
    /// 内容级匹配模式，如 Bash 命令前缀 "git status" / "git *"；None 表示整工具规则。
    pub rule_content: Option<String>,
    pub action: RuleAction,
    pub source: RuleSource,
}

impl PermissionRule {
    pub fn new(
        tool_name: impl Into<String>,
        rule_content: Option<String>,
        action: RuleAction,
        source: RuleSource,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            rule_content,
            action,
            source,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
    Ask,
}

impl PermissionDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, PermissionDecision::Allow)
    }
}

/// 一次工具调用的权限评估输入。Bash 复合命令应在外层拆分后逐段评估。
#[derive(Debug, Clone)]
pub struct PermissionInput<'a> {
    pub tool_name: String,
    pub rule_content: Option<String>,
    pub is_read_only: bool,
    pub is_destructive: bool,
    pub target_paths: Vec<PathBuf>,
    pub mode: PermissionMode,
    pub rules: &'a [PermissionRule],
}

/// 纯函数权限评估管线，首命中胜出。
pub fn evaluate(input: &PermissionInput) -> PermissionDecision {
    // 1. 整工具 deny
    for rule in input.rules {
        if rule.action == RuleAction::Deny
            && rule.rule_content.is_none()
            && tool_matches(&rule.tool_name, &input.tool_name)
        {
            return PermissionDecision::Deny {
                reason: format!("Tool '{}' is denied by a permission rule", input.tool_name),
            };
        }
    }

    // 2. 内容级 deny
    if let Some(content) = input.rule_content.as_deref() {
        for rule in input.rules {
            if rule.action == RuleAction::Deny
                && tool_matches(&rule.tool_name, &input.tool_name)
                && rule
                    .rule_content
                    .as_deref()
                    .is_some_and(|pat| content_matches(pat, content))
            {
                return PermissionDecision::Deny {
                    reason: format!(
                        "Tool '{}' with content '{}' is denied by a permission rule",
                        input.tool_name, content
                    ),
                };
            }
        }
    }

    // 3. 安全检查：禁止写入 .git/ 内部与 .claude/ 目录，任何模式都不可绕过
    if !input.is_read_only && input.target_paths.iter().any(|p| is_protected_path(p)) {
        return PermissionDecision::Deny {
            reason: "Writes into .git/ internals or the .claude/ directory are never allowed"
                .to_string(),
        };
    }

    // 4. 内容级 ask
    if let Some(content) = input.rule_content.as_deref() {
        for rule in input.rules {
            if rule.action == RuleAction::Ask
                && tool_matches(&rule.tool_name, &input.tool_name)
                && rule
                    .rule_content
                    .as_deref()
                    .is_some_and(|pat| content_matches(pat, content))
            {
                return PermissionDecision::Ask;
            }
        }
    }

    // 5. 整工具 ask
    for rule in input.rules {
        if rule.action == RuleAction::Ask
            && rule.rule_content.is_none()
            && tool_matches(&rule.tool_name, &input.tool_name)
        {
            return PermissionDecision::Ask;
        }
    }

    // 6. BypassPermissions 放行（安全检查已在上方执行）
    if input.mode == PermissionMode::BypassPermissions {
        return PermissionDecision::Allow;
    }

    // 7. Plan 模式：只允许读与写 plan 文件
    if input.mode == PermissionMode::Plan && !input.is_read_only {
        return PermissionDecision::Deny {
            reason: "Plan mode is active: only reads and writes to the plan file are allowed"
                .to_string(),
        };
    }

    // 8. AcceptEdits：非破坏性的文件读写工具直接放行
    if input.mode == PermissionMode::AcceptEdits
        && !input.is_read_only
        && !input.is_destructive
        && is_file_tool(&input.tool_name)
    {
        return PermissionDecision::Allow;
    }

    // 9. allow 规则
    for rule in input.rules {
        if rule.action != RuleAction::Allow || !tool_matches(&rule.tool_name, &input.tool_name) {
            continue;
        }
        match rule.rule_content.as_deref() {
            None => return PermissionDecision::Allow,
            Some(pat) => {
                if let Some(content) = input.rule_content.as_deref() {
                    if content_matches(pat, content) {
                        return PermissionDecision::Allow;
                    }
                }
            }
        }
    }

    // 10. DontAsk（headless）：不允许向用户询问
    if input.mode == PermissionMode::DontAsk {
        return PermissionDecision::Deny {
            reason: "Headless mode (DontAsk) cannot prompt the user for permission".to_string(),
        };
    }

    // 11. 兜底：询问用户
    PermissionDecision::Ask
}

/// 工具名匹配：精确或 "*"。
fn tool_matches(rule_tool: &str, tool_name: &str) -> bool {
    rule_tool == "*" || rule_tool == tool_name
}

/// 内容匹配：后缀 `*` 表示前缀匹配（"git *" 匹配 "git status"），否则精确匹配。
fn content_matches(pattern: &str, content: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        let prefix = prefix.trim_end();
        content == prefix
            || (content.starts_with(prefix)
                && content[prefix.len()..].starts_with(char::is_whitespace))
    } else {
        content == pattern
    }
}

/// 文件读写类工具（供 AcceptEdits 模式判断）。
pub fn is_file_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "FileRead" | "FileWrite" | "FileEdit" | "Glob" | "Grep" | "ApplyPatch" | "NotebookEdit"
    )
}

/// 路径是否位于受保护目录（.git 内部 / .claude / .wonderland）。
fn is_protected_path(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str();
        name == ".git" || name == ".claude" || name == ".wonderland"
    })
}

/// 解析 "Bash(git *)" / "FileWrite" 形式的规则字符串，返回 (工具名, 内容模式)。
pub fn parse_rule_string(s: &str) -> Option<(String, Option<String>)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(open) = s.find('(') {
        if s.ends_with(')') && open < s.len() - 1 {
            let tool = s[..open].trim();
            let content = s[open + 1..s.len() - 1].trim();
            if tool.is_empty() {
                return None;
            }
            let content = if content.is_empty() {
                None
            } else {
                Some(content.to_string())
            };
            return Some((tool.to_string(), content));
        }
        return None;
    }
    Some((s.to_string(), None))
}

/// 从 `<cwd>/.claude/settings.json` 与 `<cwd>/.wonderland/settings.json`
/// （旧版 `.rust-ai-agent/settings.json` 兼容保留）的
/// `permissions.allow/ask/deny` 数组加载规则；文件缺失或解析失败则忽略该文件。
pub fn load_rules(cwd: &Path) -> Vec<PermissionRule> {
    let mut rules = Vec::new();
    let files = [
        cwd.join(".claude").join("settings.json"),
        cwd.join(".wonderland").join("settings.json"),
        cwd.join(".rust-ai-agent").join("settings.json"),
    ];
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(permissions) = value.get("permissions") else {
            continue;
        };
        for (key, action) in [
            ("allow", RuleAction::Allow),
            ("ask", RuleAction::Ask),
            ("deny", RuleAction::Deny),
        ] {
            let Some(list) = permissions.get(key).and_then(|v| v.as_array()) else {
                continue;
            };
            for item in list {
                let Some(raw) = item.as_str() else { continue };
                if let Some((tool_name, rule_content)) = parse_rule_string(raw) {
                    rules.push(PermissionRule::new(
                        tool_name,
                        rule_content,
                        action,
                        RuleSource::ProjectSettings,
                    ));
                }
            }
        }
    }
    rules
}

/// 展示给用户的授权请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionPrompt {
    pub tool_name: String,
    pub description: String,
    pub rule_content: Option<String>,
}

/// 决策为 Ask 时的用户询问通道；返回 true 表示用户批准。
#[async_trait]
pub trait PermissionHandler: Send + Sync {
    async fn ask(&self, prompt: &PermissionPrompt) -> bool;
}

/// headless 场景使用：一律拒绝。
pub struct DenyAllHandler;

#[async_trait]
impl PermissionHandler for DenyAllHandler {
    async fn ask(&self, _prompt: &PermissionPrompt) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(tool: &str, content: Option<&str>, action: RuleAction) -> PermissionRule {
        PermissionRule::new(
            tool,
            content.map(str::to_string),
            action,
            RuleSource::Session,
        )
    }

    fn input<'a>(
        tool_name: &str,
        rule_content: Option<&str>,
        mode: PermissionMode,
        rules: &'a [PermissionRule],
    ) -> PermissionInput<'a> {
        PermissionInput {
            tool_name: tool_name.to_string(),
            rule_content: rule_content.map(str::to_string),
            is_read_only: false,
            is_destructive: false,
            target_paths: vec![],
            mode,
            rules,
        }
    }

    #[test]
    fn whole_tool_deny_beats_allow() {
        let rules = vec![
            rule("Bash", None, RuleAction::Allow),
            rule("Bash", None, RuleAction::Deny),
        ];
        let decision = evaluate(&input(
            "Bash",
            Some("git status"),
            PermissionMode::Default,
            &rules,
        ));
        assert!(matches!(decision, PermissionDecision::Deny { .. }));
    }

    #[test]
    fn content_deny_matches_prefix_pattern() {
        let rules = vec![rule("Bash", Some("rm *"), RuleAction::Deny)];
        let decision = evaluate(&input(
            "Bash",
            Some("rm -rf /tmp/x"),
            PermissionMode::Default,
            &rules,
        ));
        assert!(matches!(decision, PermissionDecision::Deny { .. }));

        let rules = vec![rule("Bash", Some("rm *"), RuleAction::Deny)];
        let decision = evaluate(&input(
            "Bash",
            Some("git status"),
            PermissionMode::Default,
            &rules,
        ));
        assert_eq!(decision, PermissionDecision::Ask);
    }

    #[test]
    fn safety_check_denies_git_write_even_in_bypass() {
        let rules = vec![rule("*", None, RuleAction::Allow)];
        for target in [".git/HEAD", "repo/.git/hooks/x", ".claude/settings.json"] {
            let mut inp = input("FileWrite", None, PermissionMode::BypassPermissions, &rules);
            inp.target_paths = vec![PathBuf::from(target)];
            let decision = evaluate(&inp);
            assert!(
                matches!(decision, PermissionDecision::Deny { .. }),
                "expected deny for {target}"
            );
        }
    }

    #[test]
    fn safety_check_allows_read_of_git() {
        let mut inp = input("FileRead", None, PermissionMode::Default, &[]);
        inp.is_read_only = true;
        inp.target_paths = vec![PathBuf::from(".git/HEAD")];
        assert_eq!(evaluate(&inp), PermissionDecision::Ask);
    }

    #[test]
    fn ask_rules_return_ask() {
        let rules = vec![rule("Bash", Some("git *"), RuleAction::Ask)];
        let decision = evaluate(&input(
            "Bash",
            Some("git push"),
            PermissionMode::Default,
            &rules,
        ));
        assert_eq!(decision, PermissionDecision::Ask);

        let rules = vec![rule("FileWrite", None, RuleAction::Ask)];
        let decision = evaluate(&input("FileWrite", None, PermissionMode::Default, &rules));
        assert_eq!(decision, PermissionDecision::Ask);
    }

    #[test]
    fn bypass_allows_when_no_rule_hits() {
        let decision = evaluate(&input(
            "Bash",
            Some("rm x"),
            PermissionMode::BypassPermissions,
            &[],
        ));
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[test]
    fn plan_mode_denies_write_but_allows_read() {
        let decision = evaluate(&input("FileWrite", None, PermissionMode::Plan, &[]));
        match decision {
            PermissionDecision::Deny { reason } => assert!(reason.contains("Plan mode")),
            other => panic!("expected deny, got {other:?}"),
        }

        let mut read = input("FileRead", None, PermissionMode::Plan, &[]);
        read.is_read_only = true;
        assert_eq!(evaluate(&read), PermissionDecision::Ask);
    }

    #[test]
    fn accept_edits_allows_file_tools_only() {
        let decision = evaluate(&input("FileEdit", None, PermissionMode::AcceptEdits, &[]));
        assert_eq!(decision, PermissionDecision::Allow);

        let decision = evaluate(&input("Bash", Some("ls"), PermissionMode::AcceptEdits, &[]));
        assert_eq!(decision, PermissionDecision::Ask);

        // 破坏性操作不放行
        let mut destructive = input("FileWrite", None, PermissionMode::AcceptEdits, &[]);
        destructive.is_destructive = true;
        assert_eq!(evaluate(&destructive), PermissionDecision::Ask);
    }

    #[test]
    fn allow_rules_whole_tool_and_content() {
        let rules = vec![rule("FileRead", None, RuleAction::Allow)];
        let decision = evaluate(&input("FileRead", None, PermissionMode::Default, &rules));
        assert_eq!(decision, PermissionDecision::Allow);

        let rules = vec![rule("Bash", Some("git *"), RuleAction::Allow)];
        let decision = evaluate(&input(
            "Bash",
            Some("git status"),
            PermissionMode::Default,
            &rules,
        ));
        assert_eq!(decision, PermissionDecision::Allow);
        let decision = evaluate(&input(
            "Bash",
            Some("rm x"),
            PermissionMode::Default,
            &rules,
        ));
        assert_eq!(decision, PermissionDecision::Ask);
    }

    #[test]
    fn dont_ask_mode_denies_instead_of_asking() {
        let decision = evaluate(&input("Bash", Some("ls"), PermissionMode::DontAsk, &[]));
        match decision {
            PermissionDecision::Deny { reason } => assert!(reason.contains("Headless")),
            other => panic!("expected deny, got {other:?}"),
        }

        // 已有 allow 规则时仍可放行
        let rules = vec![rule("Bash", None, RuleAction::Allow)];
        let decision = evaluate(&input("Bash", Some("ls"), PermissionMode::DontAsk, &rules));
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[test]
    fn fallback_is_ask() {
        assert_eq!(
            evaluate(&input("Bash", Some("ls"), PermissionMode::Default, &[])),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn wildcard_rule_matches_any_tool() {
        let rules = vec![rule("*", None, RuleAction::Deny)];
        let decision = evaluate(&input("FileWrite", None, PermissionMode::Default, &rules));
        assert!(matches!(decision, PermissionDecision::Deny { .. }));
    }

    #[test]
    fn parse_rule_string_variants() {
        assert_eq!(
            parse_rule_string("Bash(git *)"),
            Some(("Bash".to_string(), Some("git *".to_string())))
        );
        assert_eq!(
            parse_rule_string("FileWrite"),
            Some(("FileWrite".to_string(), None))
        );
        assert_eq!(
            parse_rule_string("Bash()"),
            Some(("Bash".to_string(), None))
        );
        assert_eq!(parse_rule_string("  "), None);
        assert_eq!(parse_rule_string("(git *)"), None);
    }

    #[test]
    fn content_matches_semantics() {
        assert!(content_matches("git *", "git status"));
        assert!(content_matches("git *", "git"));
        assert!(!content_matches("git *", "gitx"));
        assert!(!content_matches("git *", "a git status"));
        assert!(content_matches("git status", "git status"));
        assert!(!content_matches("git status", "git status --short"));
    }

    #[test]
    fn load_rules_from_settings_files() {
        let dir = tempfile::tempdir().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{
                "permissions": {
                    "allow": ["Bash(git *)", "FileRead"],
                    "ask": ["FileWrite"],
                    "deny": ["Bash(rm *)"]
                }
            }"#,
        )
        .unwrap();
        let rust_dir = dir.path().join(".rust-ai-agent");
        std::fs::create_dir_all(&rust_dir).unwrap();
        std::fs::write(rust_dir.join("settings.json"), "{ not valid json").unwrap();

        let rules = load_rules(dir.path());
        assert_eq!(rules.len(), 4);
        assert!(rules.iter().any(|r| r.tool_name == "Bash"
            && r.rule_content.as_deref() == Some("git *")
            && r.action == RuleAction::Allow));
        assert!(rules
            .iter()
            .any(|r| r.tool_name == "FileWrite" && r.action == RuleAction::Ask));
        assert!(rules.iter().any(|r| r.tool_name == "Bash"
            && r.rule_content.as_deref() == Some("rm *")
            && r.action == RuleAction::Deny));

        let empty = tempfile::tempdir().unwrap();
        assert!(load_rules(empty.path()).is_empty());
    }

    #[tokio::test]
    async fn deny_all_handler_always_refuses() {
        let handler = DenyAllHandler;
        let prompt = PermissionPrompt {
            tool_name: "Bash".to_string(),
            description: "run ls".to_string(),
            rule_content: Some("ls".to_string()),
        };
        assert!(!handler.ask(&prompt).await);
    }
}
