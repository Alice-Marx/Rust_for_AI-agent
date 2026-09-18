//! 自定义斜杠命令：`.claude/commands/*.md`（借鉴 Claude Code）。
//!
//! 每个 markdown 文件就是一个命令，正文是发给 Agent 的提示词模板：
//!
//! ```text
//! ---
//! description: 汇总当前分支的未提交改动
//! argument-hint: [路径]
//! ---
//! 请阅读 $ARGUMENTS 下的改动并给出审查意见，重点看并发与错误处理。
//! ```
//!
//! 模板支持 `$ARGUMENTS`（全部参数）与 `$1` / `$2`（位置参数，最多 9 个）。
//! 子目录形成命名空间，例如 `.claude/commands/frontend/component.md` → `/frontend:component`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// 一个自定义命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// 不带前导斜杠的命令名，子目录用 `:` 连接。
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// 提示词模板正文。
    pub body: String,
    pub source: PathBuf,
}

/// 命令目录（按优先级从低到高）。
pub fn command_directories(cwd: &Path) -> Vec<PathBuf> {
    vec![
        cwd.join(".claude").join("commands"),
        cwd.join(".wonderland").join("commands"),
    ]
}

#[derive(Debug, Default, serde::Deserialize)]
struct CommandFrontmatter {
    #[serde(default)]
    description: Option<serde_yaml::Value>,
    #[serde(default, alias = "argument-hint", alias = "argumentHint")]
    argument_hint: Option<serde_yaml::Value>,
}

/// frontmatter 里 `description: 代码审查` 是字符串，而 `argument-hint: [路径]`
/// 会被 YAML 解析成序列；这里统一转成展示用文本。
fn frontmatter_text(value: serde_yaml::Value) -> Option<String> {
    let text = match value {
        serde_yaml::Value::Null => return None,
        serde_yaml::Value::String(text) => text,
        serde_yaml::Value::Sequence(items) => items
            .into_iter()
            .map(|item| match item {
                serde_yaml::Value::String(text) => format!("[{text}]"),
                other => serde_yaml::to_string(&other)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => serde_yaml::to_string(&other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// 加载目录下的全部命令；高优先级目录里的同名命令覆盖低优先级。
pub fn load_commands(cwd: &Path) -> Vec<CustomCommand> {
    let mut commands: Vec<CustomCommand> = Vec::new();
    for directory in command_directories(cwd) {
        let mut files = Vec::new();
        collect_markdown(&directory, &directory, &mut files);
        files.sort();
        for (name, file) in files {
            match parse_command_file(&name, &file) {
                Ok(command) => {
                    commands.retain(|existing| existing.name != command.name);
                    commands.push(command);
                }
                Err(error) => {
                    tracing::warn!(file = %file.display(), %error, "invalid custom command, skipped");
                }
            }
        }
    }
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    commands
}

fn collect_markdown(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_markdown(root, &path, files);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let name = relative
            .with_extension("")
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(":");
        files.push((name, path));
    }
}

/// 解析单个命令文件。纯函数（不触碰文件系统）便于单测。
pub fn parse_command(name: &str, raw: &str, source: PathBuf) -> Result<CustomCommand> {
    let name = name.trim().trim_start_matches('/').to_string();
    if name.is_empty() {
        bail!("command name is empty");
    }
    let (frontmatter, body) = split_frontmatter(raw)?;
    let parsed: CommandFrontmatter = if frontmatter.trim().is_empty() {
        CommandFrontmatter::default()
    } else {
        match serde_yaml::from_str(frontmatter) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!(%error, "invalid command frontmatter, using body only");
                CommandFrontmatter::default()
            }
        }
    };
    let body = body.trim().to_string();
    if body.is_empty() {
        bail!("command body is empty");
    }
    let description = parsed
        .description
        .and_then(frontmatter_text)
        .unwrap_or_else(|| first_line(&body));
    Ok(CustomCommand {
        name,
        description,
        argument_hint: parsed.argument_hint.and_then(frontmatter_text),
        body,
        source,
    })
}

fn parse_command_file(name: &str, path: &Path) -> Result<CustomCommand> {
    let raw = std::fs::read_to_string(path)?;
    parse_command(name, &raw, path.to_path_buf())
}

fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut line = line.to_string();
            if line.chars().count() > 80 {
                line = line.chars().take(77).collect::<String>() + "...";
            }
            line
        })
        .unwrap_or_else(|| "（无描述）".to_string())
}

/// 拆分 `---` YAML frontmatter 与正文；没有 frontmatter 时返回空 YAML。
pub fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(after_open) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return Ok(("", raw));
    };
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        if line_without_newline == "---" {
            return Ok((&after_open[..offset], &after_open[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("frontmatter starts with --- but has no closing --- line")
}

/// 把 `$ARGUMENTS` / `$1`..`$9` 替换为实际参数。
pub fn expand(command: &CustomCommand, arguments: &str) -> String {
    let arguments = arguments.trim();
    let positional: Vec<&str> = arguments.split_whitespace().collect();
    let mut rendered = command.body.clone();
    for index in (1..=9).rev() {
        let value = positional.get(index - 1).copied().unwrap_or("");
        rendered = rendered.replace(&format!("${index}"), value);
    }
    rendered = rendered.replace("$ARGUMENTS", arguments);
    // 模板里没有占位符时，把参数追加到末尾，避免用户输入被悄悄丢掉。
    if !command.body.contains("$ARGUMENTS")
        && !(1..=9).any(|index| command.body.contains(&format!("${index}")))
        && !arguments.is_empty()
    {
        rendered.push_str("\n\n");
        rendered.push_str(arguments);
    }
    rendered.trim().to_string()
}

/// 在已加载的命令里按名查找（同时接受带与不带前导斜杠的写法）。
pub fn find_command<'a>(commands: &'a [CustomCommand], name: &str) -> Option<&'a CustomCommand> {
    let name = name.trim().trim_start_matches('/');
    commands.iter().find(|command| command.name == name)
}

/// 供 `/help` 展示的一行摘要。
pub fn render_command_list(commands: &[CustomCommand]) -> String {
    if commands.is_empty() {
        return "（当前项目没有自定义命令；在 .claude/commands/*.md 中添加）".to_string();
    }
    commands
        .iter()
        .map(|command| {
            let hint = command
                .argument_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            format!("/{}{}  {}", command.name, hint, command.description)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// frontmatter 之外的元数据（预留给未来的工具白名单等扩展）。
pub fn frontmatter_map(raw: &str) -> HashMap<String, String> {
    let Ok((frontmatter, _)) = split_frontmatter(raw) else {
        return HashMap::new();
    };
    serde_yaml::from_str::<HashMap<String, serde_yaml::Value>>(frontmatter)
        .map(|map| {
            map.into_iter()
                .map(|(key, value)| {
                    let rendered = match value {
                        serde_yaml::Value::String(text) => text,
                        other => serde_yaml::to_string(&other)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                    };
                    (key, rendered)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn parses_frontmatter_description_and_hint() {
        let command = parse_command(
            "review",
            "---\ndescription: 代码审查\nargument-hint: [路径]\n---\n请审查 $ARGUMENTS。\n",
            PathBuf::from("review.md"),
        )
        .unwrap();
        assert_eq!(command.name, "review");
        assert_eq!(command.description, "代码审查");
        assert_eq!(command.argument_hint.as_deref(), Some("[路径]"));
        assert_eq!(command.body, "请审查 $ARGUMENTS。");
    }

    #[test]
    fn description_falls_back_to_first_body_line() {
        let command = parse_command(
            "plain",
            "# 标题\n\n做第一件有用的事。\n",
            PathBuf::from("plain.md"),
        )
        .unwrap();
        assert_eq!(command.description, "做第一件有用的事。");
    }

    #[test]
    fn yaml_flow_hint_and_broken_frontmatter_are_tolerated() {
        // 未加引号的 `[路径]` 会被 YAML 解析成序列，需要还原成展示文本。
        let command = parse_command(
            "review",
            "---
description: 审查
argument-hint: [路径]
---
body",
            PathBuf::from("review.md"),
        )
        .unwrap();
        assert_eq!(command.argument_hint.as_deref(), Some("[路径]"));
        assert_eq!(command.description, "审查");

        // 官方写法是带引号的字符串。
        let quoted = parse_command(
            "review",
            "---
argument-hint: \"[issue-number] [priority]\"
---
body",
            PathBuf::from("review.md"),
        )
        .unwrap();
        assert_eq!(
            quoted.argument_hint.as_deref(),
            Some("[issue-number] [priority]")
        );

        // frontmatter 写坏时不丢正文，只回退到正文首行做描述。
        let broken = parse_command(
            "broken",
            "---
argument-hint: [a] [b]
---
正文照旧",
            PathBuf::from("broken.md"),
        )
        .unwrap();
        assert_eq!(broken.description, "正文照旧");
        assert!(broken.argument_hint.is_none());
    }

    #[test]
    fn empty_body_is_rejected() {
        assert!(parse_command("x", "---\ndescription: y\n---\n\n", PathBuf::from("x.md")).is_err());
        assert!(parse_command("  ", "body", PathBuf::from("x.md")).is_err());
    }

    #[test]
    fn expands_arguments_and_positionals() {
        let command = parse_command(
            "fix",
            "第一个=$1 第二个=$2 全部=$ARGUMENTS",
            PathBuf::from("fix.md"),
        )
        .unwrap();
        assert_eq!(expand(&command, "a b"), "第一个=a 第二个=b 全部=a b");
        // 缺少的位置参数替换为空串。
        assert_eq!(expand(&command, "a"), "第一个=a 第二个= 全部=a");
    }

    #[test]
    fn appends_arguments_when_template_has_no_placeholder() {
        let command = parse_command("noop", "只看不改。", PathBuf::from("noop.md")).unwrap();
        assert_eq!(expand(&command, "额外要求"), "只看不改。\n\n额外要求");
        assert_eq!(expand(&command, "  "), "只看不改。");
    }

    #[test]
    fn loads_commands_with_namespaces_and_override() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            &root.join(".claude").join("commands").join("review.md"),
            "---\ndescription: 旧描述\n---\n旧正文 $ARGUMENTS",
        );
        write(
            &root
                .join(".claude")
                .join("commands")
                .join("frontend")
                .join("component.md"),
            "生成组件 $1",
        );
        write(
            &root.join(".wonderland").join("commands").join("review.md"),
            "---\ndescription: 新描述\n---\n新正文 $ARGUMENTS",
        );
        // 非 markdown 与损坏文件都被忽略。
        write(
            &root.join(".claude").join("commands").join("note.txt"),
            "忽略我",
        );
        write(
            &root.join(".claude").join("commands").join("broken.md"),
            "   ",
        );

        let commands = load_commands(root);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["frontend:component", "review"]);

        let review = find_command(&commands, "/review").unwrap();
        assert_eq!(review.description, "新描述");
        assert!(review.body.starts_with("新正文"));

        let component = find_command(&commands, "frontend:component").unwrap();
        assert_eq!(expand(component, "Button"), "生成组件 Button");
        assert!(find_command(&commands, "missing").is_none());
    }

    #[test]
    fn render_command_list_handles_empty_and_hint() {
        assert!(render_command_list(&[]).contains(".claude/commands"));
        let command = parse_command(
            "review",
            "---\ndescription: 审查改动\nargument-hint: [路径]\n---\nbody",
            PathBuf::from("review.md"),
        )
        .unwrap();
        let rendered = render_command_list(&[command]);
        assert!(rendered.contains("/review [路径]"));
        assert!(rendered.contains("审查改动"));
    }

    #[test]
    fn missing_directory_yields_no_commands() {
        let directory = tempfile::tempdir().unwrap();
        assert!(load_commands(directory.path()).is_empty());
    }
}
