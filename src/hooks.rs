use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// 借鉴 Claude Code 的 Hooks 机制：settings.json 里按事件配置命令 hook，
/// 工具调用前后 / 会话开始结束由引擎执行，hook 可以批准或拦截操作。
///
/// 配置格式（`.claude/settings.json` 或 `.wonderland/settings.json`）：
///
/// ```json
/// {
///   "hooks": {
///     "PreToolUse": [
///       {
///         "matcher": "Bash|FileWrite",
///         "hooks": [
///           {"type": "command", "command": "python check.py", "timeout": 30}
///         ]
///       }
///     ]
///   }
/// }
/// ```
///
/// hook 进程从 stdin 收到 `{event, session_id, tool_name, tool_input}` JSON。
/// 约定：退出码 2 表示拦截（stderr 作为拒绝理由）；退出码 0 时 stdout 的
/// JSON `{"decision": "block"|"approve", "reason": "...", "additionalContext":
/// "..."}`（或 `permissionDecision: deny|allow`）决定行为；其他退出码忽略。
pub const HOOK_EVENTS: &[&str] = &["PreToolUse", "PostToolUse", "SessionStart", "Stop"];

const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// 单个命令 hook 定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookCommand {
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    /// 秒；缺省 60。
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECS
}

/// 一个 matcher + 若干命令 hook。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookMatcher {
    /// 空 或 "*" 匹配所有工具；否则按 `|` 拆分精确匹配工具名。
    #[serde(default)]
    pub matcher: String,
    pub hooks: Vec<HookCommand>,
}

/// 从 settings JSON 的 `hooks` 字段解析事件表。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookConfig {
    #[serde(flatten)]
    events: HashMap<String, Vec<HookMatcher>>,
}

impl HookConfig {
    pub fn is_empty(&self) -> bool {
        self.events.values().all(|list| list.is_empty())
    }

    pub fn event_names(&self) -> Vec<String> {
        self.events
            .keys()
            .filter(|event| self.events.get(*event).is_some_and(|list| !list.is_empty()))
            .cloned()
            .collect()
    }

    fn matchers_for(&self, event: &str) -> &[HookMatcher] {
        self.events
            .get(event)
            .map(|list| list.as_slice())
            .unwrap_or(&[])
    }

    /// 该事件配置的 hook 命令总数（诊断用）。
    pub fn command_count(&self, event: &str) -> usize {
        self.matchers_for(event)
            .iter()
            .map(|matcher| matcher.hooks.len())
            .sum()
    }
}

/// 从项目目录的 settings 文件加载 hook 配置（.wonderland 与 .claude 兼容目录）。
pub fn load_hook_config(cwd: &Path) -> HookConfig {
    let files = [
        cwd.join(".wonderland").join("settings.json"),
        cwd.join(".claude").join("settings.json"),
    ];
    let mut config = HookConfig::default();
    for file in files {
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            tracing::warn!(file = %file.display(), "settings.json is not valid JSON; hooks ignored");
            continue;
        };
        let Some(hooks_value) = value.get("hooks") else {
            continue;
        };
        match serde_json::from_value::<HookConfig>(hooks_value.clone()) {
            Ok(parsed) => {
                for (event, matchers) in parsed.events {
                    config.events.entry(event).or_default().extend(matchers);
                }
            }
            Err(error) => {
                tracing::warn!(file = %file.display(), error = %error, "invalid hooks config ignored")
            }
        }
    }
    config
}

/// hook 引擎给出的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// 未表达意见或 hook 全部通过。
    None,
    /// 明确批准（退出码 0 且 decision=approve / permissionDecision=allow）。
    Approve,
    /// 拦截（退出码 2 或 decision=block / permissionDecision=deny）。
    Block { reason: Option<String> },
}

#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub decision: HookDecision,
    /// hook 注入的附加上下文，追加到工具结果中反馈给模型。
    pub additional_context: Option<String>,
}

impl HookOutcome {
    fn none() -> Self {
        Self {
            decision: HookDecision::None,
            additional_context: None,
        }
    }
}

/// hook 进程的 stdin 输入。
#[derive(Serialize)]
struct HookInput<'a> {
    event: &'a str,
    session_id: &'a str,
    tool_name: &'a str,
    tool_input: &'a serde_json::Value,
    tool_output: Option<&'a str>,
}

/// 执行某事件下匹配 `tool_name` 的全部命令 hook；顺序执行，任何 Block 短路。
/// `tool_name` 对非工具事件传 ""；`tool_output` 供 PostToolUse 传入结果文本。
pub async fn run_hooks(
    config: &HookConfig,
    event: &str,
    session_id: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_output: Option<&str>,
    working_dir: &Path,
) -> HookOutcome {
    let matchers = config.matchers_for(event);
    if matchers.is_empty() {
        return HookOutcome::none();
    }

    let payload = serde_json::to_string(&HookInput {
        event,
        session_id,
        tool_name,
        tool_input,
        tool_output,
    })
    .unwrap_or_default();

    for matcher in matchers {
        if !matcher_matches(&matcher.matcher, tool_name) {
            continue;
        }
        for hook in &matcher.hooks {
            if hook.kind != "command" {
                tracing::warn!(event, kind = %hook.kind, "unsupported hook type ignored");
                continue;
            }
            let outcome = run_command_hook(hook, &payload, working_dir).await;
            match outcome.decision {
                HookDecision::Block { .. } => return outcome,
                HookDecision::Approve => return outcome,
                HookDecision::None => {
                    if outcome.additional_context.is_some() {
                        return outcome;
                    }
                }
            }
        }
    }
    HookOutcome::none()
}

fn matcher_matches(matcher: &str, tool_name: &str) -> bool {
    let matcher = matcher.trim();
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    if tool_name.is_empty() {
        // 非工具事件（SessionStart / Stop）：无 matcher 语义，直接放行。
        return true;
    }
    matcher
        .split('|')
        .any(|candidate| candidate.trim() == tool_name)
}

async fn run_command_hook(hook: &HookCommand, payload: &str, working_dir: &Path) -> HookOutcome {
    let (shell, args) = crate::tools::shell::default_shell();
    let mut child = match Command::new(&shell)
        .args(&args)
        .arg(&hook.command)
        .current_dir(working_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(command = %hook.command, error = %error, "failed to spawn hook");
            return HookOutcome::none();
        }
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        let _ = stdin.shutdown().await;
        drop(stdin);
    }
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let wait = async {
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let (out_task, err_task) = tokio::join!(
            async {
                if let Some(pipe) = stdout_pipe.as_mut() {
                    let _ = pipe.read_to_end(&mut stdout_buf).await;
                }
            },
            async {
                if let Some(pipe) = stderr_pipe.as_mut() {
                    let _ = pipe.read_to_end(&mut stderr_buf).await;
                }
            }
        );
        let _ = (out_task, err_task);
        let status = child.wait().await;
        (
            status,
            String::from_utf8_lossy(&stdout_buf).into_owned(),
            String::from_utf8_lossy(&stderr_buf).into_owned(),
        )
    };

    let (status, stdout, stderr) =
        match tokio::time::timeout(Duration::from_secs(hook.timeout.max(1)), wait).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(command = %hook.command, "hook timed out; treated as non-blocking");
                return HookOutcome::none();
            }
        };

    let exit_code = status.map(|s| s.code()).unwrap_or(None);
    // Claude Code 语义：退出码 2 = 拦截，stderr 是给模型的理由。
    if exit_code == Some(2) {
        let reason = if stderr.trim().is_empty() {
            format!("blocked by hook: {}", hook.command)
        } else {
            format!("blocked by hook: {}", stderr.trim())
        };
        return HookOutcome {
            decision: HookDecision::Block {
                reason: Some(reason),
            },
            additional_context: None,
        };
    }
    if exit_code != Some(0) {
        // 其他非零退出码按非阻塞处理，仅记录。
        tracing::warn!(command = %hook.command, exit_code, "hook exited non-zero; ignored");
        return HookOutcome::none();
    }

    parse_hook_stdout(&stdout, &hook.command)
}

fn parse_hook_stdout(stdout: &str, command: &str) -> HookOutcome {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return HookOutcome::none();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // stdout 不是 JSON：按纯文本附加上下文处理（提示性 hook 的常见形态）。
        return HookOutcome {
            decision: HookDecision::None,
            additional_context: Some(format!("[hook {}] {}", command, trimmed)),
        };
    };

    let decision_word = value
        .get("decision")
        .or_else(|| value.get("permissionDecision"))
        .and_then(|v| v.as_str())
        .map(str::to_ascii_lowercase);
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let additional = value
        .get("additionalContext")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let decision = match decision_word.as_deref() {
        Some("block") | Some("deny") => HookDecision::Block {
            reason: Some(reason.unwrap_or_else(|| format!("blocked by hook: {command}"))),
        },
        Some("approve") | Some("allow") => HookDecision::Approve,
        _ => HookDecision::None,
    };
    HookOutcome {
        decision,
        additional_context: additional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_settings(dir: &Path, json: &str) {
        let claude = dir.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.json"), json).unwrap();
    }

    #[test]
    fn parses_hook_config() {
        let raw = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash|FileWrite",
                    "hooks": [{"type": "command", "command": "echo hi", "timeout": 5}]
                }],
                "Stop": [{
                    "hooks": [{"type": "command", "command": "notify.sh"}]
                }]
            }
        });
        let config: HookConfig = serde_json::from_value(raw["hooks"].clone()).unwrap();
        assert_eq!(config.command_count("PreToolUse"), 1);
        assert_eq!(config.command_count("Stop"), 1);
        assert_eq!(config.command_count("PostToolUse"), 0);
        let stop_hooks = &config.matchers_for("Stop")[0].hooks[0];
        assert_eq!(stop_hooks.timeout, 60, "default timeout applied");
    }

    #[test]
    fn loads_from_project_settings() {
        let dir = tempfile::tempdir().unwrap();
        write_settings(
            dir.path(),
            r#"{"permissions":{"allow":[]},"hooks":{"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"x"}]}]}}"#,
        );
        let config = load_hook_config(dir.path());
        assert_eq!(config.command_count("PreToolUse"), 1);

        let empty = load_hook_config(Path::new(dir.path().join("nowhere").to_str().unwrap()));
        assert!(empty.is_empty());
    }

    #[test]
    fn matcher_semantics() {
        assert!(matcher_matches("", "Bash"));
        assert!(matcher_matches("*", "Bash"));
        assert!(matcher_matches("Bash|FileWrite", "Bash"));
        assert!(!matcher_matches("Bash|FileWrite", "Glob"));
        // 非工具事件一律匹配。
        assert!(matcher_matches("Bash", ""));
    }

    #[tokio::test]
    async fn exit_code_two_blocks_with_stderr_reason() {
        let dir = tempfile::tempdir().unwrap();
        let config: HookConfig = serde_json::from_value(serde_json::json!({
            "PreToolUse": [{"matcher": "Bash", "hooks": [{
                "type": "command", "command": "cat > /dev/null; echo hook-says-no >&2; exit 2", "timeout": 30
            }]}]
        }))
        .unwrap();

        let outcome = run_hooks(
            &config,
            "PreToolUse",
            "s1",
            "Bash",
            &serde_json::json!({"command": "ls"}),
            None,
            dir.path(),
        )
        .await;
        match outcome.decision {
            HookDecision::Block { reason } => {
                assert!(reason.unwrap_or_default().contains("hook-says-no"))
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn json_decision_block_and_additional_context() {
        let dir = tempfile::tempdir().unwrap();
        let config: HookConfig = serde_json::from_value(serde_json::json!({
            "PreToolUse": [{"matcher": "Bash", "hooks": [{
                "type": "command",
                "command": "cat > /dev/null; echo '{\"decision\":\"block\",\"reason\":\"forbidden command\"}'",
                "timeout": 30
            }]}]
        }))
        .unwrap();
        let outcome = run_hooks(
            &config,
            "PreToolUse",
            "s1",
            "Bash",
            &serde_json::json!({"command": "ls"}),
            None,
            dir.path(),
        )
        .await;
        match outcome.decision {
            HookDecision::Block { reason } => {
                assert!(reason.unwrap_or_default().contains("forbidden command"))
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdout_text_becomes_additional_context() {
        let dir = tempfile::tempdir().unwrap();
        let config: HookConfig = serde_json::from_value(serde_json::json!({
            "SessionStart": [{"matcher": "", "hooks": [{
                "type": "command", "command": "echo remember-the-conventions", "timeout": 30
            }]}]
        }))
        .unwrap();
        let outcome = run_hooks(
            &config,
            "SessionStart",
            "s1",
            "",
            &serde_json::json!({}),
            None,
            dir.path(),
        )
        .await;
        assert_eq!(outcome.decision, HookDecision::None);
        assert!(outcome
            .additional_context
            .unwrap_or_default()
            .contains("remember-the-conventions"));
    }

    #[tokio::test]
    async fn non_matching_tool_skips_hook() {
        let dir = tempfile::tempdir().unwrap();
        let config: HookConfig = serde_json::from_value(serde_json::json!({
            "PreToolUse": [{"matcher": "FileWrite", "hooks": [{
                "type": "command", "command": "exit 2", "timeout": 30
            }]}]
        }))
        .unwrap();
        let outcome = run_hooks(
            &config,
            "PreToolUse",
            "s1",
            "Glob",
            &serde_json::json!({}),
            None,
            dir.path(),
        )
        .await;
        assert_eq!(outcome.decision, HookDecision::None);
    }

    #[tokio::test]
    async fn timeout_is_non_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let sleep = if cfg!(windows) {
            "ping -n 3 127.0.0.1 >nul"
        } else {
            "sleep 2"
        };
        let config: HookConfig = serde_json::from_value(serde_json::json!({
            "PreToolUse": [{"matcher": "", "hooks": [{
                "type": "command", "command": sleep, "timeout": 1
            }]}]
        }))
        .unwrap();
        let outcome = run_hooks(
            &config,
            "PreToolUse",
            "s1",
            "Glob",
            &serde_json::json!({}),
            None,
            dir.path(),
        )
        .await;
        assert_eq!(outcome.decision, HookDecision::None);
    }
}
