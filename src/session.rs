use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::TodoItem;
use crate::provider::{ChatMessage, Usage};

/// 一段持久化的会话：多轮消息历史加累计 token 用量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub usage: Usage,
    /// 会话级任务清单（TodoWrite 工具维护）；旧文件缺省为空。
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    /// 会话归属用户，用于按用户过滤会话列表（与长期记忆的隔离一致）。
    #[serde(default)]
    pub user_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `SessionStore::list` 返回的会话摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub message_count: usize,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// 基于文件系统的会话存储，每个会话一个 `<dir>/<id>.json` 文件。
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 会话存储目录（供调用方定位同级目录，如 tool-results）。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn load_or_create(&self, id: &str) -> anyhow::Result<Session> {
        if let Some(session) = self.load(id)? {
            return Ok(session);
        }
        let now = Utc::now();
        Ok(Session {
            id: id.to_string(),
            messages: Vec::new(),
            usage: Usage::default(),
            todos: Vec::new(),
            user_id: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let path = self.path_for(id);
        match fs::read(&path) {
            Ok(bytes) => {
                let session = serde_json::from_slice(&bytes)
                    .with_context(|| format!("invalid session file: {}", path.display()))?;
                Ok(Some(session))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// 保存会话并刷新 `updated_at`；先写临时文件再 rename，避免半写状态。
    pub fn save(&self, session: &mut Session) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir)?;
        session.updated_at = Utc::now();
        let bytes = serde_json::to_vec_pretty(session)?;
        let path = self.path_for(&session.id);
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, bytes)?;
        fs::rename(&temp, &path)?;
        Ok(())
    }

    /// 按 `updated_at` 降序列出所有可解析的会话摘要；损坏的文件跳过。
    pub fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        };
        let mut summaries = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let summary = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Session>(&bytes).ok())
                .map(|session| SessionSummary {
                    id: session.id,
                    message_count: session.messages.len(),
                    updated_at: session.updated_at,
                    user_id: session.user_id,
                });
            if let Some(summary) = summary {
                summaries.push(summary);
            }
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
        Ok(summaries)
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", sanitize_id(id)))
    }
}

/// 会话 id 净化为安全文件名：只保留字母数字、`-`、`_`，其余替换为 `_`，
/// 防止 `../` 之类的路径逃逸。
pub(crate) fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "session".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ContentBlock;

    #[test]
    fn load_or_create_returns_fresh_session_for_unknown_id() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());
        let session = store.load_or_create("new-session").unwrap();
        assert_eq!(session.id, "new-session");
        assert!(session.messages.is_empty());
        assert_eq!(session.usage, Usage::default());
        assert_eq!(session.created_at, session.updated_at);
        assert!(store.load("new-session").unwrap().is_none());
    }

    #[test]
    fn save_and_load_roundtrip_preserves_messages_and_usage() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());
        let mut session = store.load_or_create("round-trip").unwrap();
        session.messages.push(ChatMessage::user("hello"));
        session.messages.push(ChatMessage::assistant_blocks(vec![
            ContentBlock::text("working on it"),
            ContentBlock::tool_use("call_1", "Bash", serde_json::json!({"command": "ls"})),
        ]));
        session.usage += Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 5,
            cache_creation_tokens: 2,
        };
        store.save(&mut session).unwrap();

        let loaded = store.load("round-trip").unwrap().unwrap();
        assert_eq!(loaded.id, "round-trip");
        assert_eq!(loaded.messages, session.messages);
        assert_eq!(loaded.usage.input_tokens, 100);
        assert_eq!(loaded.usage.cache_read_tokens, 5);

        let created = store.load_or_create("round-trip").unwrap();
        assert_eq!(created.messages.len(), 2);
    }

    #[test]
    fn user_id_is_persisted_and_exposed_in_summaries() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());
        let mut alice = store.load_or_create("alice-session").unwrap();
        alice.user_id = Some("alice".to_string());
        store.save(&mut alice).unwrap();
        let mut bob = store.load_or_create("bob-session").unwrap();
        bob.user_id = Some("bob".to_string());
        store.save(&mut bob).unwrap();
        // 旧文件没有 user_id 字段，反序列化后应为 None 而不是报错。
        std::fs::write(
            directory.path().join("legacy.json"),
            serde_json::json!({
                "id": "legacy",
                "messages": [],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_read_tokens": 0,
                    "cache_creation_tokens": 0
                },
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            store.load("alice-session").unwrap().unwrap().user_id,
            Some("alice".to_string())
        );
        assert!(store.load("legacy").unwrap().unwrap().user_id.is_none());

        let summaries = store.list().unwrap();
        let alice_summary = summaries.iter().find(|s| s.id == "alice-session").unwrap();
        assert_eq!(alice_summary.user_id.as_deref(), Some("alice"));
        let legacy_summary = summaries.iter().find(|s| s.id == "legacy").unwrap();
        assert!(legacy_summary.user_id.is_none());
    }

    #[test]
    fn save_is_atomic_and_refreshes_updated_at() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());
        let mut session = store.load_or_create("atomic").unwrap();
        let created_at = session.created_at;
        store.save(&mut session).unwrap();
        assert!(session.updated_at >= created_at);

        session.messages.push(ChatMessage::user("again"));
        store.save(&mut session).unwrap();
        // 保存后磁盘上的内容与重新读取一致，且不存在遗留的临时文件。
        let raw = fs::read(directory.path().join("atomic.json")).unwrap();
        let parsed: Session = serde_json::from_slice(&raw).unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.updated_at, session.updated_at);
        assert!(!directory.path().join("atomic.json.tmp").exists());
    }

    #[test]
    fn session_id_is_sanitized_into_safe_file_name() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());
        let mut session = store.load_or_create("../../etc").unwrap();
        store.save(&mut session).unwrap();

        assert!(directory.path().join("______etc.json").exists());
        assert!(store.load("../../etc").unwrap().is_some());
        // 净化后的文件必须落在存储目录内部。
        let entries: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn list_returns_summaries_sorted_by_updated_at_descending() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());

        let mut oldest = store.load_or_create("oldest").unwrap();
        oldest.updated_at = Utc::now() - chrono::Duration::hours(2);
        store.save(&mut oldest).unwrap();
        // save 会刷新 updated_at，直接写回文件以控制排序。
        oldest.updated_at = Utc::now() - chrono::Duration::hours(2);
        let bytes = serde_json::to_vec_pretty(&oldest).unwrap();
        fs::write(directory.path().join("oldest.json"), bytes).unwrap();

        let mut newest = store.load_or_create("newest").unwrap();
        newest.messages.push(ChatMessage::user("hi"));
        newest.messages.push(ChatMessage::assistant_text("yo"));
        store.save(&mut newest).unwrap();

        fs::write(directory.path().join("broken.json"), b"not json").unwrap();

        let summaries = store.list().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, "newest");
        assert_eq!(summaries[0].message_count, 2);
        assert_eq!(summaries[1].id, "oldest");
        assert!(summaries[0].updated_at > summaries[1].updated_at);
    }
}
