use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

use super::{truncate_str, Tool, ToolContext, ToolOutput};

/// FileRead 单次输出的字符上限。
const MAX_READ_CHARS: usize = 100_000;

#[derive(Debug, Clone, Copy)]
pub struct FileReadRecord {
    pub mtime: SystemTime,
    pub content_hash: u64,
}

/// 会话内「已读文件」状态，用于 FileWrite/FileEdit 的读改写一致性检查。
#[derive(Debug, Clone, Default)]
pub struct ReadFileState {
    records: HashMap<PathBuf, FileReadRecord>,
}

impl ReadFileState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, path: &Path, content: &str) {
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        self.records.insert(
            path.to_path_buf(),
            FileReadRecord {
                mtime,
                content_hash: hash_content(content),
            },
        );
    }

    pub fn has_record(&self, path: &Path) -> bool {
        self.records.contains_key(path)
    }

    /// 检查文件在读后是否被外部修改。
    /// 无记录或文件不存在 → Ok；mtime 一致 → Ok；
    /// mtime 变化但内容 hash 相同 → Ok 并更新记录；内容也变了 → Err。
    pub fn check_stale(&mut self, path: &Path) -> Result<(), String> {
        let Some(record) = self.records.get(path).copied() else {
            return Ok(());
        };
        let Ok(metadata) = std::fs::metadata(path) else {
            return Ok(());
        };
        let Ok(mtime) = metadata.modified() else {
            return Ok(());
        };
        if mtime == record.mtime {
            return Ok(());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to re-read {}: {e}", path.display()))?;
        let hash = hash_content(&content);
        if hash != record.content_hash {
            return Err("File has been modified since it was last read".to_string());
        }
        self.records.insert(
            path.to_path_buf(),
            FileReadRecord {
                mtime,
                content_hash: hash,
            },
        );
        Ok(())
    }
}

fn hash_content(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn required_str<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolOutput::err(format!("missing required parameter: {key}")))
}

pub struct FileRead;

#[async_trait::async_trait]
impl Tool for FileRead {
    fn name(&self) -> &'static str {
        "FileRead"
    }

    fn description(&self) -> &'static str {
        "Read a text file. Output is line-numbered (`<line>\\t<content>`). \
         Use `offset` (1-based) and `limit` to page through large files; \
         output longer than 100,000 characters is truncated."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path, or path relative to the working directory"
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line number to start reading from (default 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["file_path"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn target_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        match input.get("file_path").and_then(|v| v.as_str()) {
            Some(p) => vec![ctx.resolve_path(p)],
            None => vec![],
        }
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let file_path = match required_str(&input, "file_path") {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        let path = ctx.resolve_path(file_path);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "failed to read {}: {e}",
                    path.display()
                )))
            }
        };

        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n.max(1) as usize)
            .unwrap_or(1);
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        let lines: Vec<&str> = content.lines().collect();
        let start = (offset - 1).min(lines.len());
        let end = limit.map_or(lines.len(), |l| (start + l).min(lines.len()));

        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{:>6}\t{}\n", start + i + 1, line));
        }

        let mut truncated = false;
        if out.len() > MAX_READ_CHARS {
            truncate_str(&mut out, MAX_READ_CHARS);
            out.push_str("\n... (output truncated at 100,000 characters)");
            truncated = true;
        }

        ctx.read_state.record(&path, &content);

        Ok(ToolOutput {
            content: out,
            is_error: false,
            truncated,
            full_output_path: None,
        })
    }
}

pub struct FileWrite;

#[async_trait::async_trait]
impl Tool for FileWrite {
    fn name(&self) -> &'static str {
        "FileWrite"
    }

    fn description(&self) -> &'static str {
        "Write content to a file, creating parent directories as needed. \
         If the file already exists it must have been read with FileRead first, \
         and it must not have been modified since."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path, or path relative to the working directory"
                },
                "content": {
                    "type": "string",
                    "description": "The exact content to write"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    fn target_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        match input.get("file_path").and_then(|v| v.as_str()) {
            Some(p) => vec![ctx.resolve_path(p)],
            None => vec![],
        }
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let file_path = match required_str(&input, "file_path") {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        let content = match required_str(&input, "content") {
            Ok(c) => c,
            Err(e) => return Ok(e),
        };
        let path = ctx.resolve_path(file_path);

        if path.exists() {
            if !ctx.read_state.has_record(&path) {
                return Ok(ToolOutput::err(format!(
                    "{} already exists and has not been read in this session; use FileRead first",
                    path.display()
                )));
            }
            if let Err(msg) = ctx.read_state.check_stale(&path) {
                return Ok(ToolOutput::err(msg));
            }
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        ctx.read_state.record(&path, content);

        Ok(ToolOutput::ok(format!(
            "Wrote {} bytes to {}",
            content.len(),
            path.display()
        )))
    }
}

pub struct FileEdit;

#[async_trait::async_trait]
impl Tool for FileEdit {
    fn name(&self) -> &'static str {
        "FileEdit"
    }

    fn description(&self) -> &'static str {
        "Replace an exact string in a file. The file must have been read with FileRead first. \
         Without `replace_all`, `old_string` must occur exactly once."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path, or path relative to the working directory"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text (must differ from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence of old_string (default false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn target_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        match input.get("file_path").and_then(|v| v.as_str()) {
            Some(p) => vec![ctx.resolve_path(p)],
            None => vec![],
        }
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let file_path = match required_str(&input, "file_path") {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        let old_string = match required_str(&input, "old_string") {
            Ok(s) => s,
            Err(e) => return Ok(e),
        };
        let new_string = match required_str(&input, "new_string") {
            Ok(s) => s,
            Err(e) => return Ok(e),
        };
        let replace_all = input
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let path = ctx.resolve_path(file_path);
        if !path.exists() {
            return Ok(ToolOutput::err(format!(
                "{} does not exist",
                path.display()
            )));
        }
        if !ctx.read_state.has_record(&path) {
            return Ok(ToolOutput::err(format!(
                "{} has not been read in this session; use FileRead first",
                path.display()
            )));
        }
        if let Err(msg) = ctx.read_state.check_stale(&path) {
            return Ok(ToolOutput::err(msg));
        }
        if old_string.is_empty() {
            return Ok(ToolOutput::err("old_string must not be empty"));
        }
        if old_string == new_string {
            return Ok(ToolOutput::err("old_string and new_string must differ"));
        }

        let content = tokio::fs::read_to_string(&path).await?;
        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolOutput::err(format!(
                "old_string not found in {}",
                path.display()
            )));
        }
        if !replace_all && count > 1 {
            return Ok(ToolOutput::err(format!(
                "old_string occurs {count} times in {}; pass replace_all=true or a more specific string",
                path.display()
            )));
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        tokio::fs::write(&path, &new_content).await?;
        ctx.read_state.record(&path, &new_content);

        Ok(ToolOutput::ok(format!(
            "Applied {count} replacement{} to {}",
            if count == 1 { "" } else { "s" },
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            read_state: ReadFileState::new(),
            output_dir: dir.join("out"),
            session_id: "test".to_string(),
            todos: Vec::new(),
        }
    }

    #[tokio::test]
    async fn file_read_line_numbers_and_offset_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let mut ctx = make_ctx(dir.path());

        let out = FileRead
            .call(serde_json::json!({"file_path": "a.txt"}), &mut ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("     1\tone\n"));
        assert!(out.content.contains("     4\tfour\n"));

        let out = FileRead
            .call(
                serde_json::json!({"file_path": "a.txt", "offset": 2, "limit": 2}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.content, "     2\ttwo\n     3\tthree\n");

        // 读取后应有 read 记录
        assert!(ctx.read_state.has_record(&dir.path().join("a.txt")));
    }

    #[tokio::test]
    async fn file_read_truncates_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(150_000);
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let mut ctx = make_ctx(dir.path());

        let out = FileRead
            .call(serde_json::json!({"file_path": "big.txt"}), &mut ctx)
            .await
            .unwrap();
        assert!(out.truncated);
        assert!(out.content.len() < 101_000);
        assert!(out.content.contains("output truncated"));
    }

    #[tokio::test]
    async fn file_read_missing_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let out = FileRead
            .call(serde_json::json!({"file_path": "nope.txt"}), &mut ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn file_write_creates_new_file_and_parents() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        let out = FileWrite
            .call(
                serde_json::json!({"file_path": "sub/dir/new.txt", "content": "hello"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/dir/new.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn file_write_overwrite_requires_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        let mut ctx = make_ctx(dir.path());

        let out = FileWrite
            .call(
                serde_json::json!({"file_path": "a.txt", "content": "new"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("FileRead first"));

        // 先读再写则成功
        FileRead
            .call(serde_json::json!({"file_path": "a.txt"}), &mut ctx)
            .await
            .unwrap();
        let out = FileWrite
            .call(
                serde_json::json!({"file_path": "a.txt", "content": "new"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn file_edit_exact_replace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo bar baz").unwrap();
        let mut ctx = make_ctx(dir.path());
        FileRead
            .call(serde_json::json!({"file_path": "a.txt"}), &mut ctx)
            .await
            .unwrap();

        let out = FileEdit
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "bar",
                    "new_string": "qux"
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "foo qux baz"
        );
    }

    #[tokio::test]
    async fn file_edit_rejects_multiple_matches_without_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo foo foo").unwrap();
        let mut ctx = make_ctx(dir.path());
        FileRead
            .call(serde_json::json!({"file_path": "a.txt"}), &mut ctx)
            .await
            .unwrap();

        let out = FileEdit
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "foo",
                    "new_string": "bar"
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("3 times"), "{}", out.content);
    }

    #[tokio::test]
    async fn file_edit_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo foo foo").unwrap();
        let mut ctx = make_ctx(dir.path());
        FileRead
            .call(serde_json::json!({"file_path": "a.txt"}), &mut ctx)
            .await
            .unwrap();

        let out = FileEdit
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "foo",
                    "new_string": "bar",
                    "replace_all": true
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "bar bar bar"
        );
    }

    #[tokio::test]
    async fn file_edit_requires_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo").unwrap();
        let mut ctx = make_ctx(dir.path());

        let out = FileEdit
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "foo",
                    "new_string": "bar"
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("FileRead first"));
    }

    #[tokio::test]
    async fn file_edit_rejects_externally_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "foo").unwrap();
        let mut ctx = make_ctx(dir.path());
        FileRead
            .call(serde_json::json!({"file_path": "a.txt"}), &mut ctx)
            .await
            .unwrap();

        // 模拟外部修改（保证 mtime 变化）
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "foo changed externally").unwrap();

        let out = FileEdit
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "foo",
                    "new_string": "bar"
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.content, "File has been modified since it was last read");
    }

    #[tokio::test]
    async fn file_edit_validation_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo").unwrap();
        let mut ctx = make_ctx(dir.path());
        FileRead
            .call(serde_json::json!({"file_path": "a.txt"}), &mut ctx)
            .await
            .unwrap();

        let out = FileEdit
            .call(
                serde_json::json!({"file_path": "a.txt", "old_string": "", "new_string": "x"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error && out.content.contains("must not be empty"));

        let out = FileEdit
            .call(
                serde_json::json!({"file_path": "a.txt", "old_string": "foo", "new_string": "foo"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error && out.content.contains("must differ"));

        let out = FileEdit
            .call(
                serde_json::json!({"file_path": "a.txt", "old_string": "zzz", "new_string": "x"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error && out.content.contains("not found"));
    }

    #[test]
    fn read_state_stale_check_same_content_new_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "same content").unwrap();
        let mut state = ReadFileState::new();
        state.record(&path, "same content");

        std::thread::sleep(std::time::Duration::from_millis(50));
        // mtime 变化但内容相同 → Ok 并更新记录
        std::fs::write(&path, "same content").unwrap();
        assert!(state.check_stale(&path).is_ok());

        // 内容变化 → Err
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "different").unwrap();
        assert!(state.check_stale(&path).is_err());

        // 无记录 / 文件不存在 → Ok
        assert!(state
            .check_stale(&dir.path().join("never-read.txt"))
            .is_ok());
        state.record(&path, "different");
        std::fs::remove_file(&path).unwrap();
        assert!(state.check_stale(&path).is_ok());
    }
}
