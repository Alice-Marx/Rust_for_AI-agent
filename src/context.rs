use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Local;

const MAX_GIT_STATUS_CHARS: usize = 1000;
const MAX_INSTRUCTION_CHARS: usize = 40_000;
const MAX_INCLUDE_DEPTH: usize = 5;

/// 构建系统提示词时注入的运行环境信息。
#[derive(Debug, Clone)]
pub struct EnvironmentInfo {
    pub working_dir: PathBuf,
    pub platform: String,
    pub date: String,
    pub git: Option<GitInfo>,
}

#[derive(Debug, Clone)]
pub struct GitInfo {
    pub branch: String,
    pub status_short: String,
    pub recent_commits: Vec<String>,
}

/// 采集环境信息；非 git 仓库或 git 命令失败时 `git` 为 `None`，不报错。
pub fn gather_environment(cwd: &Path) -> EnvironmentInfo {
    EnvironmentInfo {
        working_dir: cwd.to_path_buf(),
        platform: std::env::consts::OS.to_string(),
        date: Local::now().format("%Y-%m-%d").to_string(),
        git: gather_git_info(cwd),
    }
}

fn gather_git_info(cwd: &Path) -> Option<GitInfo> {
    let inside = run_git(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() != "true" {
        return None;
    }
    let branch = run_git(cwd, &["branch", "--show-current"])
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            run_git(cwd, &["rev-parse", "--short", "HEAD"]).map(|sha| sha.trim().to_string())
        })?;
    let status_short = run_git(cwd, &["status", "--short"])
        .map(|status| truncate_chars(&status, MAX_GIT_STATUS_CHARS))
        .unwrap_or_default();
    let recent_commits = run_git(cwd, &["log", "--oneline", "-n", "5"])
        .map(|log| log.lines().map(str::to_string).collect())
        .unwrap_or_default();
    Some(GitInfo {
        branch,
        status_short,
        recent_commits,
    })
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 加载用户级与项目级指令文件，拼接为一段 Markdown。
///
/// 顺序即优先级从低到高（越靠后越优先，参考 Claude Code）：
/// 1. `~/.claude/CLAUDE.md`（用户级）
/// 2. 从文件系统根到 `cwd` 逐级的 `AGENTS.md`、`CLAUDE.md`、`.claude/CLAUDE.md`
///
/// 每个文件支持行内 `@path` include（相对该文件所在目录解析，递归展开，
/// 深度上限 5，循环引用自动跳过），总量截断到 40_000 字符。
pub fn load_project_instructions(cwd: &Path) -> String {
    let mut files = Vec::new();
    if let Some(home) = home_dir() {
        let user_level = home.join(".claude").join("CLAUDE.md");
        if user_level.is_file() {
            files.push(user_level);
        }
    }
    let ancestors: Vec<&Path> = cwd.ancestors().collect();
    for dir in ancestors.iter().rev() {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                files.push(candidate);
            }
        }
        let nested = dir.join(".claude").join("CLAUDE.md");
        if nested.is_file() {
            files.push(nested);
        }
    }

    let mut seen = HashSet::new();
    files.retain(|file| seen.insert(normalize_path(file)));

    let mut visited = HashSet::new();
    let mut sections = Vec::new();
    for file in files {
        if let Some(content) = expand_file(&file, &mut visited, 0) {
            sections.push(format!("# Contents of {}\n\n{}", file.display(), content));
        }
    }
    let mut joined = sections.join("\n\n");
    truncate_chars_in_place(&mut joined, MAX_INSTRUCTION_CHARS);
    joined
}

/// 读取一个指令文件并展开其中的 `@path` include。
/// `visited` 跨整个加载过程共享，用于去环与去重。
fn expand_file(path: &Path, visited: &mut HashSet<PathBuf>, depth: usize) -> Option<String> {
    if depth > MAX_INCLUDE_DEPTH || !visited.insert(normalize_path(path)) {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut expanded = String::new();
    for (index, line) in content.lines().enumerate() {
        if index > 0 {
            expanded.push('\n');
        }
        expanded.push_str(&expand_line(line, base, visited, depth));
    }
    Some(expanded)
}

/// 展开一行内所有 `@path` 引用；目标不存在、非文本、超深或已访问时保留原文。
fn expand_line(line: &str, base: &Path, visited: &mut HashSet<PathBuf>, depth: usize) -> String {
    let mut expanded = String::new();
    for token in line.split_inclusive(char::is_whitespace) {
        let (body, whitespace): (&str, &str) = match token.find(char::is_whitespace) {
            Some(index) => token.split_at(index),
            None => (token, ""),
        };
        let reference = body.strip_prefix('@').filter(|path| !path.is_empty());
        let replacement = reference.and_then(|path| {
            let target = base.join(path);
            target
                .is_file()
                .then(|| expand_file(&target, visited, depth + 1))
                .flatten()
        });
        match replacement {
            Some(content) => expanded.push_str(&content),
            None => expanded.push_str(body),
        }
        expanded.push_str(whitespace);
    }
    expanded
}

fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut text = text.to_string();
    truncate_chars_in_place(&mut text, max);
    text
}

fn truncate_chars_in_place(text: &mut String, max: usize) {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

const IDENTITY_SECTION: &str = r#"You are Wonderland, an AI coding assistant. You help users with software engineering tasks by reading, editing, and searching code, and by running commands.

## Tool Usage Rules

- Always Read a file before modifying it. Use FileEdit for precise, targeted replacements; use FileWrite only for new files or complete rewrites.
- Prefer the Grep and Glob tools for searching; do not use Bash for `grep` or `find`.
- Use Bash for builds, tests, git, and other command execution.
- When multiple tool calls have no dependencies between them, issue them in parallel.
"#;

const SAFETY_SECTION: &str = r#"## Safety Guidelines

- Never commit secrets, credentials, or private keys into the repository.
- Be careful before running destructive commands (such as `rm -rf`); confirm the intent and blast radius first.
"#;

/// 构建系统提示词：静态段（身份、工具规范、安全准则）在前，动态段
/// （附加指令、技能、环境信息、项目指令）在后，以保护 prompt cache。
pub fn build_system_prompt(
    env: &EnvironmentInfo,
    skills_section: Option<&str>,
    extra_instructions: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(IDENTITY_SECTION);
    prompt.push('\n');
    prompt.push_str(SAFETY_SECTION);

    if let Some(extra) = extra_instructions.filter(|value| !value.trim().is_empty()) {
        prompt.push_str("\n## Additional Instructions\n\n");
        prompt.push_str(extra.trim());
        prompt.push('\n');
    }

    if let Some(skills) = skills_section.filter(|value| !value.trim().is_empty()) {
        prompt.push_str("\n## Available Skills\n\n");
        prompt.push_str(skills.trim());
        prompt.push('\n');
    }

    prompt.push_str("\n## Environment\n\n");
    prompt.push_str(&format!("- Date: {}\n", env.date));
    prompt.push_str(&format!("- Platform: {}\n", env.platform));
    prompt.push_str(&format!(
        "- Working directory: {}\n",
        env.working_dir.display()
    ));
    if let Some(git) = &env.git {
        prompt.push_str(&format!("- Git branch: {}\n", git.branch));
        prompt.push_str("- Git status:\n\n```\n");
        if git.status_short.is_empty() {
            prompt.push_str("(clean)");
        } else {
            prompt.push_str(&git.status_short);
        }
        prompt.push_str("\n```\n");
        if !git.recent_commits.is_empty() {
            prompt.push_str("- Recent commits:\n");
            for commit in &git.recent_commits {
                prompt.push_str(&format!("  - {commit}\n"));
            }
        }
    }

    let instructions = load_project_instructions(&env.working_dir);
    if !instructions.is_empty() {
        prompt.push_str("\n## Project Instructions\n\n");
        prompt.push_str(
            "The following project and user instructions take precedence over the default behavior above:\n\n",
        );
        prompt.push_str(&instructions);
        prompt.push('\n');
    }

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn instructions_from_parent_and_child_are_ordered_low_to_high_priority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let child = root.join("sub");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("CLAUDE.md"), "parent instructions").unwrap();
        fs::write(root.join("AGENTS.md"), "parent agents").unwrap();
        fs::write(child.join("CLAUDE.md"), "child instructions").unwrap();
        fs::create_dir_all(child.join(".claude")).unwrap();
        fs::write(child.join(".claude").join("CLAUDE.md"), "child nested").unwrap();

        let result = load_project_instructions(&child);
        let parent_agents = result.find("parent agents").unwrap();
        let parent = result.find("parent instructions").unwrap();
        let child_pos = result.find("child instructions").unwrap();
        let nested = result.find("child nested").unwrap();
        // 同层按 AGENTS.md、CLAUDE.md 顺序；深层（更靠近 cwd）排在后面，优先级更高。
        assert!(parent_agents < parent);
        assert!(parent < child_pos);
        assert!(child_pos < nested);
        assert!(result.contains("# Contents of"));
        assert!(result.contains(&child.join("CLAUDE.md").display().to_string()));
    }

    #[test]
    fn at_include_expands_relative_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("extra.md"), "included content").unwrap();
        fs::write(
            root.join("CLAUDE.md"),
            "before\n@extra.md\nafter @missing.md tail",
        )
        .unwrap();

        let result = load_project_instructions(root);
        assert!(result.contains("included content"));
        // 不存在的引用保留原文。
        assert!(result.contains("@missing.md"));
        assert!(result.contains("tail"));
    }

    #[test]
    fn at_include_cycle_does_not_hang_or_panic() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.md"), "in a\n@b.md").unwrap();
        fs::write(root.join("b.md"), "in b\n@a.md").unwrap();
        fs::write(root.join("CLAUDE.md"), "start\n@a.md\nend").unwrap();

        let result = load_project_instructions(root);
        assert!(result.contains("in a"));
        assert!(result.contains("in b"));
        assert!(result.contains("end"));
    }

    #[test]
    fn instructions_are_truncated_to_max_chars() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("CLAUDE.md"),
            "x".repeat(MAX_INSTRUCTION_CHARS + 10_000),
        )
        .unwrap();

        let result = load_project_instructions(root);
        assert!(result.len() <= MAX_INSTRUCTION_CHARS);
        assert!(result.len() > MAX_INSTRUCTION_CHARS - 200);
    }

    #[test]
    fn gather_environment_in_non_git_dir_has_no_git_info() {
        let directory = tempfile::tempdir().unwrap();
        let env = gather_environment(directory.path());
        assert!(env.git.is_none());
        assert_eq!(env.working_dir, directory.path());
        assert_eq!(env.platform, std::env::consts::OS);
        assert_eq!(env.date.len(), 10);
        assert_eq!(&env.date[4..5], "-");
    }

    #[test]
    fn gather_environment_in_git_repo_collects_branch_and_commits() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let init = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(root)
            .output();
        let Ok(init) = init else { return };
        if !init.status.success() {
            return;
        }
        fs::write(root.join("file.txt"), "hello").unwrap();
        let add = Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(root)
            .output()
            .unwrap();
        let commit = Command::new("git")
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                "initial commit",
            ])
            .current_dir(root)
            .output()
            .unwrap();
        if !add.status.success() || !commit.status.success() {
            return;
        }
        fs::write(root.join("dirty.txt"), "dirty").unwrap();

        let env = gather_environment(root);
        let git = env.git.expect("git info inside a repo");
        assert_eq!(git.branch, "main");
        assert!(git.status_short.contains("dirty.txt"));
        assert_eq!(git.recent_commits.len(), 1);
        assert!(git.recent_commits[0].contains("initial commit"));
    }

    #[test]
    fn system_prompt_places_static_sections_before_dynamic_ones() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("CLAUDE.md"), "project rules here").unwrap();
        let env = gather_environment(directory.path());

        let prompt = build_system_prompt(&env, Some("skill list"), Some("extra rules"));

        let identity = prompt.find("You are Wonderland").unwrap();
        let safety = prompt.find("Safety Guidelines").unwrap();
        let extra = prompt.find("extra rules").unwrap();
        let skills = prompt.find("skill list").unwrap();
        let environment = prompt.find("## Environment").unwrap();
        let project = prompt.find("project rules here").unwrap();
        assert!(identity < safety);
        assert!(safety < extra);
        assert!(extra < skills);
        assert!(skills < environment);
        assert!(environment < project);
        assert!(prompt.contains(&env.date));
        assert!(prompt.contains("take precedence over the default behavior"));
    }

    #[test]
    fn system_prompt_omits_absent_dynamic_sections() {
        let directory = tempfile::tempdir().unwrap();
        let env = EnvironmentInfo {
            working_dir: directory.path().to_path_buf(),
            platform: "linux".to_string(),
            date: "2026-09-17".to_string(),
            git: None,
        };
        let prompt = build_system_prompt(&env, None, None);
        assert!(!prompt.contains("Available Skills"));
        assert!(!prompt.contains("Additional Instructions"));
        assert!(!prompt.contains("Git branch"));
        assert!(prompt.contains("Platform: linux"));
        assert!(prompt.contains("Date: 2026-09-17"));
    }
}
