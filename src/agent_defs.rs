use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::collaboration::{AgentDirectory, AgentWorker};
use crate::permissions::{PermissionMode, PermissionRule, RuleAction, RuleSource};
use crate::provider::{ChatMessage, ContentBlock, ModelProvider, ModelRequest, StopReason};
use crate::tools::{ReadFileState, ToolContext, ToolRegistry};

/// 文件定义子代理的 frontmatter，对应 Claude Code 的 agents/*.md 格式。
#[derive(Debug, Clone, Deserialize)]
pub struct FileAgentDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 工具白名单（YAML 列表或逗号分隔字符串）；缺省只给只读工具。
    #[serde(default, deserialize_with = "deserialize_tools")]
    pub tools: Option<Vec<String>>,
    /// 子代理最多工具调用轮数；缺省 8。
    #[serde(default)]
    pub max_turns: Option<usize>,
}

/// 兼容两种写法：YAML 列表与逗号分隔字符串（`tools: "FileRead, Grep"`）。
fn deserialize_tools<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_yaml::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(serde_yaml::Value::Sequence(items)) => {
            let mut tools = Vec::new();
            for item in items {
                if let Some(tool) = item.as_str() {
                    tools.push(tool.trim().to_string());
                }
            }
            Ok(Some(tools))
        }
        Some(serde_yaml::Value::String(raw)) => Ok(Some(
            raw.split(',')
                .map(|tool| tool.trim().to_string())
                .filter(|tool| !tool.is_empty())
                .collect(),
        )),
        Some(other) => Err(serde::de::Error::custom(format!(
            "tools must be a list or comma-separated string, got {other:?}"
        ))),
    }
}

/// 从 markdown 文本解析 frontmatter 与正文（正文 = 子代理系统提示词）。
pub fn parse_agent_markdown(raw: &str) -> Result<(FileAgentDefinition, String)> {
    let trimmed = raw.trim_start_matches('\u{feff}');
    let rest = trimmed
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("agent file must start with '---' frontmatter delimiter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("frontmatter is not closed with '---'"))?;
    let frontmatter = &rest[..end];
    let body = rest[end + 4..]
        .strip_prefix('\n')
        .unwrap_or(&rest[end + 4..]);

    let definition: FileAgentDefinition =
        serde_yaml::from_str(frontmatter).context("invalid agent frontmatter")?;
    if definition.name.trim().is_empty() {
        return Err(anyhow::anyhow!("agent frontmatter requires a name"));
    }
    Ok((definition, body.trim().to_string()))
}

/// 从项目目录加载 agents/*.md（.wonderland 与 .claude 兼容目录）。
pub fn load_agent_definitions(cwd: &Path) -> Vec<(FileAgentDefinition, String, PathBuf)> {
    let mut loaded = Vec::new();
    for dir_name in [".wonderland", ".claude"] {
        let agents_dir = cwd.join(dir_name).join("agents");
        let Ok(entries) = std::fs::read_dir(&agents_dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        files.sort();
        for file in files {
            match std::fs::read_to_string(&file)
                .map_err(anyhow::Error::from)
                .and_then(|raw| {
                    parse_agent_markdown(&raw)
                        .map(|(definition, prompt)| (definition, prompt, file.clone()))
                }) {
                Ok(triple) => loaded.push(triple),
                Err(error) => {
                    tracing::warn!(file = %file.display(), error = %error, "agent file ignored")
                }
            }
        }
    }
    loaded
}

/// 默认工具池：只读探索工具（对应 Claude Code 的 Explore 型子代理）。
const DEFAULT_AGENT_TOOLS: &[&str] = &["FileRead", "Glob", "Grep", "WebFetch"];
/// 子代理禁止使用的工具：不允许嵌套委派，也不共享主会话的任务清单。
const FORBIDDEN_AGENT_TOOLS: &[&str] = &["Task", "TodoWrite"];

/// 文件定义的子代理：用共享的 provider 与（过滤后的）工具注册表跑一个
/// 简化版 agentic loop，frontmatter 正文作为其系统提示词。
pub struct FileAgent {
    pub definition: FileAgentDefinition,
    system_prompt: String,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    tool_rules: Vec<PermissionRule>,
    max_turns: usize,
}

impl FileAgent {
    pub fn new(
        definition: FileAgentDefinition,
        system_prompt: String,
        provider: Arc<dyn ModelProvider>,
        full_registry: &ToolRegistry,
    ) -> Self {
        let requested: Vec<String> = definition
            .tools
            .clone()
            .map(|tools| {
                tools
                    .iter()
                    .flat_map(|tool| tool.split(','))
                    .map(|tool| tool.trim().to_string())
                    .filter(|tool| !tool.is_empty())
                    .collect()
            })
            .unwrap_or_else(|| {
                DEFAULT_AGENT_TOOLS
                    .iter()
                    .map(|tool| tool.to_string())
                    .collect()
            });

        // 工具池过滤：frontmatter tools 既是池也是白名单；Task / TodoWrite
        // 禁止出现在子代理中（不允许嵌套委派、不共享主会话清单）。
        let mut filtered = ToolRegistry::new();
        let mut tool_rules = Vec::new();
        for tool in full_registry.iter() {
            let name = tool.name();
            if FORBIDDEN_AGENT_TOOLS.contains(&name) {
                continue;
            }
            if requested.iter().any(|allowed| allowed == name) {
                tool_rules.push(PermissionRule::new(
                    name,
                    None,
                    RuleAction::Allow,
                    RuleSource::Session,
                ));
                filtered.register(tool.clone());
            }
        }

        let max_turns = definition.max_turns.unwrap_or(8);
        Self {
            definition,
            system_prompt,
            provider,
            tools: filtered,
            tool_rules,
            max_turns,
        }
    }
}

#[async_trait::async_trait]
impl AgentWorker for FileAgent {
    fn name(&self) -> &str {
        &self.definition.name
    }

    async fn handle(&self, task: &str) -> Result<String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut tool_ctx = ToolContext {
            working_dir: cwd,
            read_state: ReadFileState::new(),
            output_dir: std::env::temp_dir()
                .join(format!("wonderland-subagent-{}", uuid::Uuid::new_v4())),
            session_id: format!("subagent-{}", uuid::Uuid::new_v4()),
            todos: Vec::new(),
            background: crate::tools::background::BackgroundTaskRegistry::new(),
            mode: PermissionMode::BypassPermissions,
            pre_plan_mode: None,
        };

        let mut messages = vec![ChatMessage::user(task.to_string())];
        let mut final_text = String::new();
        for _turn in 0..self.max_turns {
            let response = self
                .provider
                .complete(&ModelRequest {
                    model: std::env::var("AGENT_MODEL").unwrap_or_default(),
                    system: self.system_prompt.clone(),
                    messages: messages.clone(),
                    tools: self.tools.tool_definitions(),
                    max_tokens: 4_096,
                    temperature: None,
                })
                .await?;
            let tool_uses: Vec<(String, String, serde_json::Value)> = response
                .tool_uses()
                .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
                .collect();
            messages.push(ChatMessage::assistant_blocks(response.blocks.clone()));
            if tool_uses.is_empty() {
                final_text = response.text();
                break;
            }
            if response.stop_reason == StopReason::ToolUse {
                final_text = response.text();
            }

            let mut results = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in tool_uses {
                // 白名单之外的工具一律拒绝（frontmatter tools 即权限）。
                let output =
                    match self.tools.find(&name) {
                        Some(tool) => {
                            if self.tool_rules.iter().any(|rule| {
                                rule.tool_name == name && rule.action == RuleAction::Allow
                            }) {
                                match tool.call(input, &mut tool_ctx).await {
                                    Ok(output) => output,
                                    Err(error) => crate::tools::ToolOutput::err(format!(
                                        "Tool '{name}' failed: {error:#}"
                                    )),
                                }
                            } else {
                                crate::tools::ToolOutput::err(format!(
                                    "Tool '{name}' is not in sub-agent allowlist"
                                ))
                            }
                        }
                        None => crate::tools::ToolOutput::err(format!("Unknown tool: {name}")),
                    };
                results.push(ContentBlock::tool_result(
                    id,
                    output.content,
                    output.is_error,
                ));
            }
            messages.push(ChatMessage::user_blocks(results));
        }

        if final_text.is_empty() {
            final_text = format!(
                "(sub-agent '{}' reached its turn limit without a final answer)",
                self.definition.name
            );
        }
        Ok(final_text)
    }
}

/// 把文件定义的子代理注册进目录；同名内置代理优先，不覆盖。
pub async fn register_file_agents(
    cwd: &Path,
    directory: &AgentDirectory,
    provider: Arc<dyn ModelProvider>,
    tools: &ToolRegistry,
) -> Vec<String> {
    let mut registered = Vec::new();
    for (definition, prompt, _file) in load_agent_definitions(cwd) {
        let existing = directory.names().await;
        if existing.contains(&definition.name) {
            continue;
        }
        let agent = FileAgent::new(definition.clone(), prompt, provider.clone(), tools);
        directory.register(Arc::new(agent)).await;
        registered.push(definition.name);
    }
    registered
}

/// 供系统提示词列出可用子代理。
pub async fn subagent_listing(directory: &AgentDirectory) -> String {
    let names = directory.names().await;
    if names.is_empty() {
        return String::new();
    }
    format!(
        "Available sub-agents for the Task tool: {}",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let raw = "---\nname: explore\ndescription: Explores the codebase\ntools:\n  - FileRead\n  - Grep\n---\n\nYou are an exploration agent.\n";
        let (definition, body) = parse_agent_markdown(raw).unwrap();
        assert_eq!(definition.name, "explore");
        assert_eq!(definition.description, "Explores the codebase");
        assert_eq!(
            definition.tools.unwrap(),
            vec!["FileRead".to_string(), "Grep".to_string()]
        );
        assert_eq!(body, "You are an exploration agent.");
    }

    #[test]
    fn parses_comma_separated_tools() {
        let raw = "---\nname: helper\ndescription: d\ntools: \"FileRead, Glob, Bash\"\n---\nbody";
        let (definition, _) = parse_agent_markdown(raw).unwrap();
        assert_eq!(
            definition.tools.unwrap(),
            vec![
                "FileRead".to_string(),
                "Glob".to_string(),
                "Bash".to_string()
            ]
        );
    }

    #[test]
    fn rejects_missing_frontmatter_or_name() {
        assert!(parse_agent_markdown("no frontmatter").is_err());
        assert!(parse_agent_markdown("---\ndescription: x\n---\nbody").is_err());
    }

    #[test]
    fn loads_definitions_from_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("explorer.md"),
            "---\nname: explorer\ndescription: Looks around\n---\nExplore carefully.",
        )
        .unwrap();
        std::fs::write(agents.join("broken.md"), "missing frontmatter").unwrap();

        let loaded = load_agent_definitions(dir.path());
        assert_eq!(loaded.len(), 1, "broken file skipped");
        assert_eq!(loaded[0].0.name, "explorer");
        assert_eq!(loaded[0].1, "Explore carefully.");
    }
}
