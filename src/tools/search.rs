use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;

use super::{truncate_str, Tool, ToolContext, ToolOutput};

const MAX_GLOB_RESULTS: usize = 100;
const MAX_LINE_CHARS: usize = 500;

/// 遍历 base 下的文件：尊重 .gitignore、跳过 hidden，且永远跳过 .git。
/// 返回 (绝对路径, mtime)。
fn walk_files(base: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(base)
        .hidden(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((entry.path().to_path_buf(), mtime));
        }
    }
    out
}

/// 相对路径统一用 `/` 分隔，便于 globset 跨平台匹配。
fn relative_display(base: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(base)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, ToolOutput> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| ToolOutput::err(format!("invalid glob pattern '{pattern}': {e}")))
}

pub struct GlobTool;

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern (e.g. \"**/*.rs\"). Respects .gitignore, skips hidden \
         files and .git. Returns paths relative to the search root, newest first, \
         limited to 100 results. A pattern without any path separator also matches file names."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, e.g. \"**/*.rs\" or \"*.toml\""
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: pattern"));
        };
        let matcher = match compile_glob(pattern) {
            Ok(m) => m,
            Err(e) => return Ok(e),
        };
        let base = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => ctx.resolve_path(p),
            None => ctx.working_dir.clone(),
        };
        // 不含路径分隔符的模式（如 "*.rs"）额外按文件名匹配
        let bare = !pattern.contains('/') && !pattern.contains('\\');

        let mut matches: Vec<(String, SystemTime)> = Vec::new();
        for (path, mtime) in walk_files(&base) {
            let Some(rel) = relative_display(&base, &path) else {
                continue;
            };
            let name_match = bare
                && path
                    .file_name()
                    .is_some_and(|n| matcher.is_match(n.to_string_lossy().as_ref()));
            if matcher.is_match(&rel) || name_match {
                matches.push((rel, mtime));
            }
        }
        matches.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
        matches.truncate(MAX_GLOB_RESULTS);

        if matches.is_empty() {
            return Ok(ToolOutput::ok("No files matched"));
        }
        Ok(ToolOutput::ok(
            matches
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

pub struct GrepTool;

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regular expression. Modes: \"content\" (matching lines as \
         path:line:text), \"files_with_matches\" (default, newest first), \"count\" (path:count). \
         Respects .gitignore, skips hidden files, .git and binary files."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: working directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this glob, e.g. \"*.rs\""
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Result shape (default: files_with_matches)"
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default 250)"
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case-insensitive matching"
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: pattern"));
        };
        let case_insensitive = input.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);
        let regex = match RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::err(format!("invalid regex '{pattern}': {e}"))),
        };
        let base = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => ctx.resolve_path(p),
            None => ctx.working_dir.clone(),
        };
        let glob_matcher = match input.get("glob").and_then(|v| v.as_str()) {
            Some(g) => match compile_glob(g) {
                Ok(m) => Some(m),
                Err(e) => return Ok(e),
            },
            None => None,
        };
        let mode = input
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let head_limit = input
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(250);

        // (相对路径, mtime, 命中行, 命中数)
        struct FileMatch {
            rel: String,
            mtime: SystemTime,
            lines: Vec<(usize, String)>,
            count: usize,
        }

        let mut files: Vec<FileMatch> = Vec::new();
        for (path, mtime) in walk_files(&base) {
            let Some(rel) = relative_display(&base, &path) else {
                continue;
            };
            if let Some(m) = &glob_matcher {
                if !m.is_match(&rel) {
                    continue;
                }
            }
            let Ok(bytes) = tokio::fs::read(&path).await else {
                continue;
            };
            if bytes.contains(&0) {
                continue; // 跳过二进制文件
            }
            let text = String::from_utf8_lossy(&bytes);
            let mut lines = Vec::new();
            let mut count = 0usize;
            for (i, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    count += 1;
                    if lines.len() < head_limit {
                        let mut line = line.to_string();
                        truncate_str(&mut line, MAX_LINE_CHARS);
                        lines.push((i + 1, line));
                    }
                }
            }
            if count > 0 {
                files.push(FileMatch {
                    rel,
                    mtime,
                    lines,
                    count,
                });
            }
        }

        match mode {
            "content" => {
                let mut out = String::new();
                let mut emitted = 0usize;
                let mut truncated = false;
                'outer: for f in &files {
                    for (lineno, line) in &f.lines {
                        if emitted >= head_limit {
                            truncated = true;
                            break 'outer;
                        }
                        out.push_str(&format!("{}:{lineno}:{line}\n", f.rel));
                        emitted += 1;
                    }
                }
                if files.iter().any(|f| f.count > f.lines.len()) {
                    truncated = true;
                }
                if out.is_empty() {
                    return Ok(ToolOutput::ok("No matches found"));
                }
                Ok(ToolOutput {
                    content: out,
                    is_error: false,
                    truncated,
                    full_output_path: None,
                })
            }
            "count" => {
                let mut out = String::new();
                for f in files.iter().take(head_limit) {
                    out.push_str(&format!("{}:{}\n", f.rel, f.count));
                }
                if out.is_empty() {
                    return Ok(ToolOutput::ok("No matches found"));
                }
                Ok(ToolOutput::ok(out))
            }
            _ => {
                files.sort_by_key(|file| std::cmp::Reverse(file.mtime));
                if files.is_empty() {
                    return Ok(ToolOutput::ok("No matches found"));
                }
                Ok(ToolOutput::ok(
                    files
                        .iter()
                        .take(head_limit)
                        .map(|f| f.rel.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: dir.join("out"),
            session_id: "test".to_string(),
            todos: Vec::new(),
        }
    }

    /// 构造测试目录：
    /// src/a.rs、src/b.rs（含 "hello"）、docs/c.txt（含 "HELLO"）、
    /// .git/ignored.rs（含 "hello"）、.hidden.rs（含 "hello"）
    fn make_tree(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {}\nhello world\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "hello again\n").unwrap();
        std::fs::write(dir.join("docs/c.txt"), "HELLO upper\n").unwrap();
        std::fs::write(dir.join(".git/ignored.rs"), "hello from git\n").unwrap();
        std::fs::write(dir.join(".hidden.rs"), "hello hidden\n").unwrap();
    }

    #[tokio::test]
    async fn glob_matches_and_excludes_git_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(dir.path());
        let mut ctx = make_ctx(dir.path());

        let out = GlobTool
            .call(serde_json::json!({"pattern": "**/*.rs"}), &mut ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(out.content.contains("src/b.rs"), "{}", out.content);
        assert!(!out.content.contains("ignored.rs"), "{}", out.content);
        assert!(!out.content.contains(".hidden.rs"), "{}", out.content);

        // 无分隔符的模式按文件名匹配
        let out = GlobTool
            .call(serde_json::json!({"pattern": "*.txt"}), &mut ctx)
            .await
            .unwrap();
        assert!(out.content.contains("docs/c.txt"), "{}", out.content);
    }

    #[tokio::test]
    async fn glob_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let out = GlobTool
            .call(serde_json::json!({"pattern": "**/*.xyz"}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "No files matched");
    }

    #[tokio::test]
    async fn grep_files_with_matches_excludes_git() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(dir.path());
        let mut ctx = make_ctx(dir.path());

        let out = GrepTool
            .call(serde_json::json!({"pattern": "hello"}), &mut ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(out.content.contains("src/b.rs"), "{}", out.content);
        assert!(
            !out.content.contains("c.txt"),
            "case sensitive: {}",
            out.content
        );
        assert!(!out.content.contains("ignored.rs"), "{}", out.content);
        assert!(!out.content.contains(".hidden.rs"), "{}", out.content);
    }

    #[tokio::test]
    async fn grep_content_mode_and_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(dir.path());
        let mut ctx = make_ctx(dir.path());

        let out = GrepTool
            .call(
                serde_json::json!({"pattern": "hello", "output_mode": "content", "-i": true}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("src/a.rs:2:hello world"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("docs/c.txt:1:HELLO upper"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn grep_count_mode_and_glob_filter() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(dir.path());
        let mut ctx = make_ctx(dir.path());

        let out = GrepTool
            .call(
                serde_json::json!({"pattern": "hello", "output_mode": "count", "glob": "*.rs"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.content.contains("src/a.rs:1"), "{}", out.content);
        assert!(out.content.contains("src/b.rs:1"), "{}", out.content);
        assert!(!out.content.contains("c.txt"), "{}", out.content);
    }

    #[tokio::test]
    async fn grep_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"hello\0world").unwrap();
        let mut ctx = make_ctx(dir.path());

        let out = GrepTool
            .call(serde_json::json!({"pattern": "hello"}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "No matches found");
    }

    #[tokio::test]
    async fn grep_invalid_regex_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let out = GrepTool
            .call(serde_json::json!({"pattern": "([unclosed"}), &mut ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("invalid regex"));
    }
}
