//! 会话检索索引（SQLite）。
//!
//! 会话正文仍然以 JSON 文件为准（见 session.rs），本模块只维护一份可查询的
//! 派生索引：每次会话落盘后做一次 upsert，支持按关键词与用户检索、返回命中
//! 片段，把"找出上周改过 X 的那个会话"从线性扫描变成一次查询。
//!
//! 参考 deepseek-harness 的 session-query 分层：索引是派生数据，损坏可直接重建。

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::provider::{ChatMessage, ContentBlock};
use crate::session::{Session, SessionStore};

/// 一条命中摘要。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct IndexedSession {
    pub id: String,
    pub user_id: Option<String>,
    pub updated_at: String,
    pub message_count: usize,
    /// 命中位置附近的片段。
    pub snippet: String,
}

/// 会话索引。内部串行化访问 SQLite 连接。
pub struct SessionIndex {
    connection: Mutex<Connection>,
}

impl SessionIndex {
    /// 打开（必要时创建）索引库并建表。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建索引目录失败：{}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("打开会话索引失败：{}", path.display()))?;
        let index = Self {
            connection: Mutex::new(connection),
        };
        index.ensure_schema()?;
        Ok(index)
    }

    /// 内存库，供测试使用。
    pub fn open_in_memory() -> Result<Self> {
        let index = Self {
            connection: Mutex::new(Connection::open_in_memory()?),
        };
        index.ensure_schema()?;
        Ok(index)
    }

    fn ensure_schema(&self) -> Result<()> {
        let connection = self.lock();
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 user_id TEXT,
                 updated_at TEXT NOT NULL,
                 message_count INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
             CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
             CREATE TABLE IF NOT EXISTS messages (
                 session_id TEXT NOT NULL,
                 ord INTEGER NOT NULL,
                 role TEXT NOT NULL,
                 content TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);",
        )?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 写入/更新单个会话（幂等：先删旧消息再插入）。
    pub fn index_session(&self, session: &Session) -> Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session.id],
        )?;
        transaction.execute(
            "INSERT INTO sessions (id, user_id, updated_at, message_count)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 user_id = excluded.user_id,
                 updated_at = excluded.updated_at,
                 message_count = excluded.message_count",
            params![
                session.id,
                session.user_id,
                session.updated_at.to_rfc3339(),
                session.messages.len() as i64
            ],
        )?;
        for (ordinal, message) in session.messages.iter().enumerate() {
            transaction.execute(
                "INSERT INTO messages (session_id, ord, role, content) VALUES (?1, ?2, ?3, ?4)",
                params![
                    session.id,
                    ordinal as i64,
                    role_name(message),
                    render_message(message)
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// 删除一个会话的索引。
    pub fn remove(&self, id: &str) -> Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        transaction.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        transaction.commit()?;
        Ok(())
    }

    /// 关键词检索：大小写不敏感的子串匹配，命中消息正文或工具输出。
    pub fn search(
        &self,
        query: &str,
        user_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IndexedSession>> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.max(1) as i64;
        let pattern = format!("%{}%", escape_like(&needle));
        let connection = self.lock();

        let mut statement = connection.prepare(
            "SELECT s.id, s.user_id, s.updated_at, s.message_count,
                    (SELECT m.content FROM messages m
                      WHERE m.session_id = s.id AND lower(m.content) LIKE ?1 ESCAPE 'X'
                      ORDER BY m.ord LIMIT 1) AS hit,
                    (SELECT m.content FROM messages m
                      WHERE m.session_id = s.id
                      ORDER BY m.ord DESC LIMIT 1) AS last
               FROM sessions s
              WHERE (?2 IS NULL OR s.user_id = ?2)
                AND EXISTS (SELECT 1 FROM messages m
                             WHERE m.session_id = s.id
                               AND lower(m.content) LIKE ?1 ESCAPE 'X')
              ORDER BY s.updated_at DESC
              LIMIT ?3",
        )?;
        let rows = statement.query_map(params![pattern, user_id, limit], |row| {
            let hit: Option<String> = row.get(4)?;
            let last: Option<String> = row.get(5)?;
            Ok(IndexedSession {
                id: row.get(0)?,
                user_id: row.get(1)?,
                updated_at: row.get(2)?,
                message_count: row.get::<_, i64>(3)?.max(0) as usize,
                snippet: build_snippet(hit.as_deref(), last.as_deref(), &needle),
            })
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// 从会话目录全量重建索引，返回索引到的会话数。
    pub fn rebuild(&self, sessions: &SessionStore) -> Result<usize> {
        let directory = sessions.dir();
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(directory = %directory.display(), %error, "会话目录不可读，索引保持为空");
                return Ok(0);
            }
        };
        let mut indexed = 0usize;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            match serde_json::from_str::<Session>(&raw) {
                Ok(session) => {
                    if let Err(error) = self.index_session(&session) {
                        tracing::warn!(path = %path.display(), %error, "索引会话失败");
                    } else {
                        indexed += 1;
                    }
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "跳过损坏的会话文件");
                }
            }
        }
        Ok(indexed)
    }

    /// 返回 (会话数, 消息数)。
    pub fn stats(&self) -> Result<(usize, usize)> {
        let connection = self.lock();
        let sessions: i64 =
            connection.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let messages: i64 =
            connection.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        Ok((sessions.max(0) as usize, messages.max(0) as usize))
    }
}

fn role_name(message: &ChatMessage) -> &'static str {
    match message.role {
        crate::provider::Role::User => "user",
        crate::provider::Role::Assistant => "assistant",
    }
}

/// 单条消息的可检索正文。
fn render_message(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::ToolUse { name, input, .. } => Some(format!("[tool_use {name} {input}]")),
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some(if *is_error {
                format!("[tool_result error] {content}")
            } else {
                format!("[tool_result] {content}")
            }),
            // 思维链不进索引：它是过程噪声，不是可检索事实。
            ContentBlock::Thinking { .. } => None,
        })
        .filter(|chunk| !chunk.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 转义 LIKE 通配符。SQLite 的 ESCAPE 子句需要一个不会出现在模式里的转义字符，
/// 这里用 X，并把关键词里真正的百分号、下划线与 X 前缀上 X。
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '%' | '_' | 'X') {
            escaped.push('X');
        }
        escaped.push(character);
    }
    escaped
}

/// 命中片段：优先取命中消息中关键词附近的文本，其次取最后一条消息的开头。
fn build_snippet(hit: Option<&str>, last: Option<&str>, needle: &str) -> String {
    const WINDOW: usize = 80;
    if let Some(hit) = hit {
        let chars: Vec<char> = hit.chars().collect();
        let width = needle.chars().count().max(1);
        let match_char = chars
            .windows(width)
            .position(|window| {
                window
                    .iter()
                    .collect::<String>()
                    .to_lowercase()
                    .contains(needle)
            })
            .unwrap_or(0);
        let start = match_char.saturating_sub(WINDOW);
        let end = (match_char + width + WINDOW).min(chars.len());
        let mut snippet = String::new();
        if start > 0 {
            snippet.push('…');
        }
        snippet.extend(&chars[start..end]);
        if end < chars.len() {
            snippet.push('…');
        }
        return snippet;
    }
    last.map(|text| truncate_chars(text, 120))
        .unwrap_or_default()
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut text: String = value.chars().take(max).collect();
    if value.chars().count() > max {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ContentBlock, Role, Usage};
    use chrono::Utc;

    fn session(id: &str, user: &str, messages: Vec<ChatMessage>) -> Session {
        let now = Utc::now();
        Session {
            id: id.to_string(),
            messages,
            usage: Usage::default(),
            todos: Vec::new(),
            user_id: Some(user.to_string()),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn search_matches_message_body_case_insensitively() {
        let index = SessionIndex::open_in_memory().unwrap();
        index
            .index_session(&session(
                "s1",
                "alice",
                vec![ChatMessage::user("修复了 Compaction 的边界问题")],
            ))
            .unwrap();
        index
            .index_session(&session(
                "s2",
                "alice",
                vec![ChatMessage::assistant_text("无关内容")],
            ))
            .unwrap();

        let hits = index.search("compaction", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s1");
        assert_eq!(hits[0].user_id.as_deref(), Some("alice"));
        assert!(hits[0].snippet.contains("Compaction"));
        assert_eq!(hits[0].message_count, 1);
    }

    #[test]
    fn indexing_is_idempotent() {
        let index = SessionIndex::open_in_memory().unwrap();
        let value = session("s1", "alice", vec![ChatMessage::user("hello world")]);
        index.index_session(&value).unwrap();
        index.index_session(&value).unwrap();
        assert_eq!(index.stats().unwrap(), (1, 1));
    }

    #[test]
    fn reindexing_updates_content_and_count() {
        let index = SessionIndex::open_in_memory().unwrap();
        index
            .index_session(&session("s1", "alice", vec![ChatMessage::user("first")]))
            .unwrap();
        index
            .index_session(&session(
                "s1",
                "alice",
                vec![
                    ChatMessage::user("second"),
                    ChatMessage::assistant_text("reply"),
                ],
            ))
            .unwrap();
        assert!(index.search("first", None, 5).unwrap().is_empty());
        assert_eq!(index.search("second", None, 5).unwrap().len(), 1);
        assert_eq!(index.stats().unwrap().1, 2);
    }

    #[test]
    fn search_filters_by_user_and_respects_limit() {
        let index = SessionIndex::open_in_memory().unwrap();
        index
            .index_session(&session("a1", "alice", vec![ChatMessage::user("rust tip")]))
            .unwrap();
        index
            .index_session(&session(
                "a2",
                "alice",
                vec![ChatMessage::user("rust tip again")],
            ))
            .unwrap();
        index
            .index_session(&session(
                "b1",
                "bob",
                vec![ChatMessage::user("rust tip for bob")],
            ))
            .unwrap();

        assert_eq!(index.search("rust", Some("alice"), 10).unwrap().len(), 2);
        assert_eq!(index.search("rust", Some("bob"), 10).unwrap().len(), 1);
        assert_eq!(index.search("rust", None, 10).unwrap().len(), 3);
        assert_eq!(index.search("rust", None, 1).unwrap().len(), 1);
    }

    #[test]
    fn blank_query_and_empty_index_return_nothing() {
        let index = SessionIndex::open_in_memory().unwrap();
        assert!(index.search("   ", None, 10).unwrap().is_empty());
        assert!(index.search("anything", None, 10).unwrap().is_empty());
    }

    #[test]
    fn tool_outputs_are_searchable_but_thinking_is_not() {
        let index = SessionIndex::open_in_memory().unwrap();
        let message = ChatMessage::assistant_blocks(vec![
            ContentBlock::thinking("secret internal reasoning", None),
            ContentBlock::tool_use(
                "call_1",
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            ),
        ]);
        let tool_result = ChatMessage::user_blocks(vec![ContentBlock::tool_result(
            "call_1",
            "test result: ok. 42 passed",
            false,
        )]);
        index
            .index_session(&session("s1", "alice", vec![message, tool_result]))
            .unwrap();

        assert_eq!(index.search("cargo test", None, 5).unwrap().len(), 1);
        assert_eq!(index.search("42 passed", None, 5).unwrap().len(), 1);
        assert!(index
            .search("secret internal reasoning", None, 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn snippet_handles_cjk_and_emoji_without_panicking() {
        let index = SessionIndex::open_in_memory().unwrap();
        let long = format!(
            "前缀{}关键词{}后缀",
            "很长的中文".repeat(40),
            "R".repeat(40)
        );
        index
            .index_session(&session("s1", "alice", vec![ChatMessage::user(&long)]))
            .unwrap();
        let hits = index.search("关键词", None, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("关键词"));
        assert!(hits[0].snippet.chars().count() <= 200);
    }

    #[test]
    fn percent_and_underscore_are_literal_in_search() {
        let index = SessionIndex::open_in_memory().unwrap();
        index
            .index_session(&session(
                "s1",
                "alice",
                vec![ChatMessage::user("进度 100% 完成")],
            ))
            .unwrap();
        index
            .index_session(&session(
                "s2",
                "alice",
                vec![ChatMessage::user("进度 1000 完成")],
            ))
            .unwrap();
        let hits = index.search("100%", None, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s1");
    }

    #[test]
    fn remove_drops_the_session() {
        let index = SessionIndex::open_in_memory().unwrap();
        index
            .index_session(&session(
                "s1",
                "alice",
                vec![ChatMessage::user("cleanup me")],
            ))
            .unwrap();
        index.remove("s1").unwrap();
        assert!(index.search("cleanup", None, 5).unwrap().is_empty());
        assert_eq!(index.stats().unwrap(), (0, 0));
    }

    #[test]
    fn rebuild_scans_the_session_directory() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());
        let mut first = store.load_or_create("disk-1").unwrap();
        first.user_id = Some("alice".to_string());
        first
            .messages
            .push(ChatMessage::user("重建索引里的关键词 alpha"));
        store.save(&mut first).unwrap();
        std::fs::write(directory.path().join("broken.json"), "{ not json").unwrap();

        let index = SessionIndex::open_in_memory().unwrap();
        assert_eq!(index.rebuild(&store).unwrap(), 1);
        let hits = index.search("alpha", Some("alice"), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "disk-1");
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("does-not-exist"));
        let index = SessionIndex::open_in_memory().unwrap();
        assert_eq!(index.rebuild(&store).unwrap(), 0);
    }

    #[test]
    fn role_names_are_recorded() {
        let index = SessionIndex::open_in_memory().unwrap();
        index
            .index_session(&session(
                "s1",
                "alice",
                vec![
                    ChatMessage::user("user side marker"),
                    ChatMessage::assistant_text("assistant side marker"),
                ],
            ))
            .unwrap();
        let connection = index.lock();
        let mut statement = connection
            .prepare("SELECT role, content FROM messages ORDER BY ord")
            .unwrap();
        let rows: Vec<(String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "user");
        assert_eq!(rows[1].0, "assistant");
        assert!(rows[0].1.contains("user side marker"));
        let user = ChatMessage::user("x");
        assert_eq!(user.role, Role::User);
        assert_eq!(role_name(&user), "user");
    }
}
