use std::path::PathBuf;

use anyhow::Result;

use super::{Tool, ToolContext, ToolOutput};

/// 借鉴 Codex 的 apply_patch（V4A）补丁格式：
///
/// ```text
/// *** Begin Patch
/// *** Add File: path/to/new.rs
/// +fn main() {}
/// *** Update File: path/to/existing.rs
/// @@ fn main
///  context line
/// -old line
/// +new line
/// *** Move to: path/renamed.rs
/// *** Delete File: path/obsolete.rs
/// *** End Patch
/// ```
///
/// Update File 的每个 hunk 用「上下文 + 删除行」组成的旧文本在新文件里
/// 定位（精确 → 去尾空白 → 空白归一三级容错），无需行号。
pub struct ApplyPatch;

#[derive(Debug, PartialEq, Eq)]
pub enum FileChange {
    Add {
        path: String,
        contents: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<PatchChunk>,
    },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PatchChunk {
    /// `@@` 定位提示：文件中包含该子串的行作为查找起点，不属于内容。
    pub marker: Option<String>,
    /// 旧文本行（上下文行 + 删除行，按出现顺序）。
    pub old_lines: Vec<String>,
    /// 新文本行（上下文行 + 新增行）。
    pub new_lines: Vec<String>,
}

#[derive(Debug)]
enum ParseState {
    Idle,
    InAdd {
        path: String,
        contents: Vec<String>,
    },
    InUpdate {
        path: String,
        move_to: Option<String>,
        chunks: Vec<PatchChunk>,
        current: Option<PatchChunk>,
    },
}

/// 解析补丁文本；语法错误返回人类可读的错误信息。
pub fn parse_patch(patch: &str) -> Result<Vec<FileChange>, String> {
    let mut changes: Vec<FileChange> = Vec::new();
    let mut state = ParseState::Idle;
    let mut started = false;

    for (index, raw) in patch.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.strip_prefix('\u{feff}').unwrap_or(raw);
        let trimmed = line.trim_end();

        if !started {
            if trimmed == "*** Begin Patch" {
                started = true;
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            return Err(format!(
                "line {line_no}: expected '*** Begin Patch', got '{trimmed}'"
            ));
        }

        // "*** Move to:" 只在 Update 段内合法，不触发段闭合。
        if let Some(new_path) = trimmed.strip_prefix("*** Move to: ") {
            match &mut state {
                ParseState::InUpdate { move_to, .. } => {
                    *move_to = Some(new_path.trim().to_string());
                    continue;
                }
                _ => {
                    return Err(format!(
                        "line {line_no}: '*** Move to:' is only valid inside '*** Update File'"
                    ))
                }
            }
        }

        // 其余 "***" 指令（含 End Patch）：先闭合当前段，再分派。
        if is_patch_terminator(trimmed) || trimmed.starts_with("*** ") {
            match &mut state {
                ParseState::InAdd { path, contents } => {
                    changes.push(FileChange::Add {
                        path: path.clone(),
                        contents: std::mem::take(contents),
                    });
                }
                ParseState::InUpdate {
                    path,
                    move_to,
                    chunks,
                    current,
                } => {
                    flush_chunk(chunks, current);
                    changes.push(FileChange::Update {
                        path: path.clone(),
                        move_to: move_to.clone(),
                        chunks: std::mem::take(chunks),
                    });
                }
                ParseState::Idle => {}
            }
            state = ParseState::Idle;

            if is_patch_terminator(trimmed) {
                return Ok(changes);
            }
            if let Some(path) = trimmed.strip_prefix("*** Add File: ") {
                state = ParseState::InAdd {
                    path: path.trim().to_string(),
                    contents: Vec::new(),
                };
            } else if let Some(path) = trimmed.strip_prefix("*** Update File: ") {
                state = ParseState::InUpdate {
                    path: path.trim().to_string(),
                    move_to: None,
                    chunks: Vec::new(),
                    current: None,
                };
            } else if let Some(path) = trimmed.strip_prefix("*** Delete File: ") {
                changes.push(FileChange::Delete {
                    path: path.trim().to_string(),
                });
            } else {
                return Err(format!(
                    "line {line_no}: unrecognized patch directive '{trimmed}'"
                ));
            }
            continue;
        }

        // "@@" 开启新 chunk：marker 只做定位提示，不进入内容行。
        if let Some(context) = trimmed.strip_prefix("@@") {
            if let ParseState::InUpdate {
                chunks, current, ..
            } = &mut state
            {
                flush_chunk(chunks, current);
                let marker = context.trim();
                if !marker.is_empty() {
                    current.get_or_insert_with(PatchChunk::default).marker =
                        Some(marker.to_string());
                }
                continue;
            }
            return Err(format!(
                "line {line_no}: '@@' is only valid inside '*** Update File:'"
            ));
        }

        // 段内内容行。
        match &mut state {
            ParseState::Idle => {
                if !trimmed.is_empty() {
                    return Err(format!(
                        "line {line_no}: content outside a '*** ... File:' section: '{trimmed}'"
                    ));
                }
            }
            ParseState::InAdd { contents, .. } => {
                if let Some(rest) = line.strip_prefix('+') {
                    contents.push(rest.to_string());
                } else if line.trim().is_empty() {
                    contents.push(String::new());
                } else {
                    return Err(format!(
                        "line {line_no}: lines in '*** Add File:' must start with '+'"
                    ));
                }
            }
            ParseState::InUpdate { current, .. } => {
                if let Some(rest) = line.strip_prefix('+') {
                    current
                        .get_or_insert_with(PatchChunk::default)
                        .new_lines
                        .push(rest.to_string());
                } else if let Some(rest) = line.strip_prefix('-') {
                    current
                        .get_or_insert_with(PatchChunk::default)
                        .old_lines
                        .push(rest.to_string());
                } else if let Some(rest) = line.strip_prefix(' ') {
                    let chunk = current.get_or_insert_with(PatchChunk::default);
                    chunk.old_lines.push(rest.to_string());
                    chunk.new_lines.push(rest.to_string());
                } else if line.trim().is_empty() {
                    // 空行视作上下文空行，便于编辑含空行的文件。
                    let chunk = current.get_or_insert_with(PatchChunk::default);
                    chunk.old_lines.push(String::new());
                    chunk.new_lines.push(String::new());
                } else {
                    return Err(format!(
                        "line {line_no}: lines in '*** Update File:' must start with ' ', '+', '-' or '@@'"
                    ));
                }
            }
        }
    }

    if !started {
        return Err("patch does not start with '*** Begin Patch'".to_string());
    }
    Err("patch is missing '*** End Patch' terminator".to_string())
}

fn is_patch_terminator(line: &str) -> bool {
    line.trim_end() == "*** End Patch" || line.trim_end() == "*** End of File"
}

/// 把进行中的 chunk 落位（有内容时才压入）。
fn flush_chunk(chunks: &mut Vec<PatchChunk>, current: &mut Option<PatchChunk>) {
    if let Some(chunk) = current.take() {
        if !chunk.old_lines.is_empty() || !chunk.new_lines.is_empty() {
            chunks.push(chunk);
        }
    }
}

/// 三级容错定位：精确相等 → 去尾空白 → 空白归一。
/// 返回匹配到的起始行号；找不到返回 None。
pub fn seek_sequence(haystack: &[String], needle: &[String], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    for level in [
        MatchLevel::Exact,
        MatchLevel::TrimEnd,
        MatchLevel::Whitespace,
    ] {
        for start in from..=haystack.len() - needle.len() {
            let window = &haystack[start..start + needle.len()];
            if lines_match(level, window, needle) {
                return Some(start);
            }
        }
        // 未命中时回到文件头再扫一遍（chunk 可能出现在 earlier 位置）。
        for start in 0..from.min(haystack.len() - needle.len()) {
            let window = &haystack[start..start + needle.len()];
            if lines_match(level, window, needle) {
                return Some(start);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum MatchLevel {
    Exact,
    TrimEnd,
    Whitespace,
}

fn lines_match(level: MatchLevel, window: &[String], needle: &[String]) -> bool {
    window.iter().zip(needle.iter()).all(|(a, b)| match level {
        MatchLevel::Exact => a == b,
        MatchLevel::TrimEnd => a.trim_end() == b.trim_end(),
        MatchLevel::Whitespace => {
            a.split_whitespace().collect::<Vec<_>>() == b.split_whitespace().collect::<Vec<_>>()
        }
    })
}

/// 把补丁应用到磁盘：返回人类可读的执行报告（每个文件一行）。
pub fn apply_patch(patch: &str, ctx: &ToolContext) -> Result<String> {
    let changes = parse_patch(patch).map_err(|error| anyhow::anyhow!("{error}"))?;
    if changes.is_empty() {
        return Err(anyhow::anyhow!("patch contains no file changes"));
    }

    let mut report = Vec::with_capacity(changes.len());
    for change in &changes {
        match change {
            FileChange::Add { path, contents } => {
                let target = ctx.resolve_path(path);
                if target.exists() {
                    return Err(anyhow::anyhow!("Add File failed: {} already exists", path));
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, render_lines(contents))?;
                report.push(format!("Added {path} ({} lines)", contents.len()));
            }
            FileChange::Delete { path } => {
                let target = ctx.resolve_path(path);
                if !target.is_file() {
                    return Err(anyhow::anyhow!(
                        "Delete File failed: {} does not exist",
                        path
                    ));
                }
                std::fs::remove_file(&target)?;
                report.push(format!("Deleted {path}"));
            }
            FileChange::Update {
                path,
                move_to,
                chunks,
            } => {
                let target = ctx.resolve_path(path);
                let source = std::fs::read_to_string(&target).map_err(|error| {
                    anyhow::anyhow!("Update File failed: cannot read {path}: {error}")
                })?;
                let updated = apply_update_chunks(path, &source, chunks)?;
                let final_path = match move_to {
                    Some(new_path) => {
                        let dest = ctx.resolve_path(new_path);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::rename(&target, &dest)?;
                        dest
                    }
                    None => target,
                };
                std::fs::write(&final_path, updated)?;
                match move_to {
                    Some(new_path) => report.push(format!(
                        "Updated {path} -> {new_path} ({} chunks)",
                        chunks.len()
                    )),
                    None => report.push(format!("Updated {path} ({} chunks)", chunks.len())),
                }
            }
        }
    }
    Ok(report.join("\n"))
}

fn render_lines(lines: &[String]) -> String {
    let mut out = lines.join("\n");
    if !lines.is_empty() {
        out.push('\n');
    }
    out
}

fn apply_update_chunks(path: &str, source: &str, chunks: &[PatchChunk]) -> Result<String> {
    let trailing_newline = source.ends_with('\n');
    let mut lines: Vec<String> = source
        .strip_suffix('\n')
        .unwrap_or(source)
        .split('\n')
        .map(str::to_string)
        .collect();

    let mut cursor = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        // `@@` 定位提示：从光标处找第一行包含 marker 子串的位置作为起点。
        let search_from = match &chunk.marker {
            Some(marker) => {
                let marker_position = lines[cursor.min(lines.len().saturating_sub(1))..]
                    .iter()
                    .position(|line| line.contains(marker.as_str()))
                    .map(|offset| cursor + offset)
                    .or_else(|| lines.iter().position(|line| line.contains(marker.as_str())));
                match marker_position {
                    Some(position) => position,
                    None => {
                        return Err(anyhow::anyhow!(
                            "Update File failed: chunk {} of {path} marker '{marker}' not found",
                            index + 1
                        ))
                    }
                }
            }
            None => cursor,
        };

        if chunk.old_lines.is_empty() {
            // 纯插入 hunk：marker 行之后（无 marker 时光标处）插入。
            let insert_at = (search_from + 1).min(lines.len());
            for (offset, line) in chunk.new_lines.iter().enumerate() {
                lines.insert(insert_at + offset, line.clone());
            }
            cursor = insert_at + chunk.new_lines.len().saturating_sub(1);
            continue;
        }
        let Some(position) = seek_sequence(&lines, &chunk.old_lines, search_from) else {
            return Err(anyhow::anyhow!(
                "Update File failed: chunk {} of {path} not found (context: {:?})",
                index + 1,
                chunk.old_lines.first().map(String::as_str).unwrap_or("")
            ));
        };
        lines.splice(
            position..position + chunk.old_lines.len(),
            chunk.new_lines.iter().cloned(),
        );
        cursor = position + chunk.new_lines.len();
    }

    let mut out = lines.join("\n");
    if trailing_newline || !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

fn patch_target_paths(patch: &str, ctx: &ToolContext) -> Vec<PathBuf> {
    parse_patch(patch)
        .unwrap_or_default()
        .iter()
        .map(|change| match change {
            FileChange::Add { path, .. }
            | FileChange::Delete { path }
            | FileChange::Update { path, .. } => ctx.resolve_path(path),
        })
        .collect()
}

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "ApplyPatch"
    }

    fn description(&self) -> &str {
        "Apply a multi-file patch in the V4A format (borrowed from Codex). One call can add, \
         update, rename and delete several files atomically. Update hunks locate their target \
         by context lines (fuzzy matching tolerates trailing-whitespace differences), so no \
         line numbers are needed. Format:\n\
         *** Begin Patch\n\
         *** Add File: path\n+new line\n\
         *** Update File: path\n@@ optional locate hint\n context\n-old\n+new\n\
         *** Move to: renamed/path\n\
         *** Delete File: path\n\
         *** End Patch\n\
         Prefer FileEdit for single small replacements; prefer ApplyPatch when changing \
         several files or several regions in one file."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "The full patch text starting with '*** Begin Patch' and ending with '*** End Patch'"
                }
            },
            "required": ["patch"]
        })
    }

    fn target_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        input
            .get("patch")
            .and_then(|v| v.as_str())
            .map(|patch| patch_target_paths(patch, ctx))
            .unwrap_or_default()
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(patch) = input.get("patch").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: patch"));
        };
        if patch.trim().is_empty() {
            return Ok(ToolOutput::err("patch must not be empty"));
        }
        match apply_patch(patch, ctx) {
            Ok(report) => Ok(ToolOutput::ok(format!(
                "Patch applied successfully:\n{report}"
            ))),
            Err(error) => Ok(ToolOutput::err(format!("{error:#}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: dir.join("out"),
            session_id: "test".to_string(),
            todos: Vec::new(),
            background: super::super::background::BackgroundTaskRegistry::new(),
            mode: crate::permissions::PermissionMode::Default,
            pre_plan_mode: None,
        }
    }

    #[test]
    fn parses_add_update_delete() {
        let patch = "\
*** Begin Patch
*** Add File: src/new.rs
+fn added() {}
*** Update File: src/main.rs
@@ fn main
-    println!(\"old\");
+    println!(\"new\");
*** Move to: src/renamed.rs
*** Delete File: src/old.rs
*** End Patch";
        let changes = parse_patch(patch).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(
            changes[0],
            FileChange::Add {
                path: "src/new.rs".to_string(),
                contents: vec!["fn added() {}".to_string()],
            }
        );
        let FileChange::Update {
            path,
            move_to,
            chunks,
        } = &changes[1]
        else {
            panic!("expected update");
        };
        assert_eq!(path, "src/main.rs");
        assert_eq!(move_to.as_deref(), Some("src/renamed.rs"));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].marker, Some("fn main".to_string()));
        assert_eq!(chunks[0].old_lines, vec!["    println!(\"old\");"]);
        assert_eq!(chunks[0].new_lines, vec!["    println!(\"new\");"]);
        assert_eq!(
            changes[2],
            FileChange::Delete {
                path: "src/old.rs".to_string(),
            }
        );
    }

    #[test]
    fn rejects_missing_terminator_and_bad_prefix() {
        assert!(parse_patch("*** Begin Patch\n*** Add File: a\n+x").is_err());
        assert!(parse_patch("hello\n*** Begin Patch\n*** End Patch").is_err());
        assert!(parse_patch("*** Begin Patch\n*** Add File: a\nx\n*** End Patch").is_err());
    }

    #[test]
    fn seek_matches_with_whitespace_tolerance() {
        let haystack: Vec<String> = vec![
            "fn main() {".to_string(),
            "    let x = 1;".to_string(),
            "}".to_string(),
        ];
        let exact = vec!["    let x = 1;".to_string()];
        assert_eq!(seek_sequence(&haystack, &exact, 0), Some(1));
        let trimmed = vec!["let x = 1;".to_string()];
        assert_eq!(seek_sequence(&haystack, &trimmed, 0), Some(1));
        let normalized = vec!["let   x = 1;".to_string()];
        assert_eq!(seek_sequence(&haystack, &normalized, 0), Some(1));
        let missing = vec!["no such line".to_string()];
        assert_eq!(seek_sequence(&haystack, &missing, 0), None);
    }

    #[tokio::test]
    async fn applies_multi_file_patch_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn keep() {}\nfn old_name() {\n    println!(\"v1\");\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        let mut ctx = make_ctx(dir.path());

        let patch = "\
*** Begin Patch
*** Add File: notes.md
+# Notes
+wrote by ApplyPatch
*** Update File: lib.rs
@@ fn old_name
-    println!(\"v1\");
+    println!(\"v2\");
*** Delete File: gone.txt
*** End Patch";
        let tool = ApplyPatch;
        let out = tool
            .call(serde_json::json!({"patch": patch}), &mut ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
            "fn keep() {}\nfn old_name() {\n    println!(\"v2\");\n}\n"
        );
        assert!(dir.path().join("notes.md").is_file());
        assert!(!dir.path().join("gone.txt").exists());
        assert!(out.content.contains("Added notes.md"));
        assert!(out.content.contains("Updated lib.rs"));
        assert!(out.content.contains("Deleted gone.txt"));
    }

    #[tokio::test]
    async fn supports_rename_and_fuzzy_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta  \ngamma\n").unwrap();
        let mut ctx = make_ctx(dir.path());

        let patch = "\
*** Begin Patch
*** Update File: a.txt
-beta
+BETA
*** Move to: b.txt
*** End Patch";
        let tool = ApplyPatch;
        let out = tool
            .call(serde_json::json!({"patch": patch}), &mut ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(!dir.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn missing_context_is_reported_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let mut ctx = make_ctx(dir.path());
        let patch = "\
*** Begin Patch
*** Update File: a.txt
-this line does not exist
+replacement
*** End Patch";
        let out = ApplyPatch
            .call(serde_json::json!({"patch": patch}), &mut ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not found"), "{}", out.content);
        // 文件保持原样。
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn add_existing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "x\n").unwrap();
        let mut ctx = make_ctx(dir.path());
        let patch = "*** Begin Patch\n*** Add File: dup.txt\n+y\n*** End Patch";
        let out = ApplyPatch
            .call(serde_json::json!({"patch": patch}), &mut ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("already exists"), "{}", out.content);
    }
}
