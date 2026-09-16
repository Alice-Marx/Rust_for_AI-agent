use std::{cmp::Ordering, path::PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{fs, sync::{Mutex, RwLock}};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Preference,
    Conversation,
    Task,
}

impl Default for MemoryKind {
    fn default() -> Self {
        Self::Conversation
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub kind: MemoryKind,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: f32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMatch {
    pub entry: MemoryEntry,
    pub score: f32,
}

#[derive(Clone)]
pub struct MemoryStore {
    path: PathBuf,
    entries: std::sync::Arc<RwLock<Vec<MemoryEntry>>>,
    persist_lock: std::sync::Arc<Mutex<()>>,
}

impl MemoryStore {
    pub async fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let entries = match fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid memory file: {}", path.display()))?,
            Ok(_) => Vec::new(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };

        Ok(Self {
            path,
            entries: std::sync::Arc::new(RwLock::new(entries)),
            persist_lock: std::sync::Arc::new(Mutex::new(())),
        })
    }

    pub async fn remember(
        &self,
        user_id: Option<String>,
        session_id: Option<String>,
        content: impl Into<String>,
        kind: MemoryKind,
        tags: Vec<String>,
        importance: f32,
    ) -> anyhow::Result<MemoryEntry> {
        let now = Utc::now();
        let entry = MemoryEntry {
            id: Uuid::new_v4(),
            user_id,
            session_id,
            kind,
            content: content.into(),
            tags,
            importance: importance.clamp(0.0, 1.0),
            created_at: now,
            updated_at: now,
        };

        let _persist_guard = self.persist_lock.lock().await;
        let snapshot = {
            let mut guard = self.entries.write().await;
            guard.push(entry.clone());
            guard.clone()
        };
        self.persist_unlocked(snapshot).await?;
        Ok(entry)
    }

    pub async fn search(
        &self,
        user_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Vec<MemoryMatch> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }

        let query_tokens = tokenize(&query);
        let guard = self.entries.read().await;
        let mut matches: Vec<_> = guard
            .iter()
            .filter(|entry| user_id.map(|id| entry.user_id.as_deref() == Some(id)).unwrap_or(true))
            .filter_map(|entry| {
                let searchable = format!("{} {}", entry.content, entry.tags.join(" ")).to_lowercase();
                let exact = searchable.contains(&query);
                let token_hits = query_tokens
                    .iter()
                    .filter(|token| searchable.contains(token.as_str()))
                    .count();
                let lexical = if query_tokens.is_empty() {
                    0.0
                } else {
                    token_hits as f32 / query_tokens.len() as f32
                };
                let recency = recency_score(entry.updated_at);
                let score = (if exact { 0.65 } else { 0.0 })
                    + lexical * 0.25
                    + entry.importance.clamp(0.0, 1.0) * 0.07
                    + recency * 0.03;

                (score > 0.0).then(|| MemoryMatch {
                    entry: entry.clone(),
                    score,
                })
            })
            .collect();

        matches.sort_by(|left, right| {
            right.score.partial_cmp(&left.score).unwrap_or(Ordering::Equal)
        });
        matches.truncate(limit);
        matches
    }

    pub async fn all(&self) -> Vec<MemoryEntry> {
        self.entries.read().await.clone()
    }

    async fn persist_unlocked(&self, entries: Vec<MemoryEntry>) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(&entries)?;
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, bytes).await?;
        fs::rename(&temp, &self.path).await?;
        Ok(())
    }
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split(|character: char| character.is_whitespace() || ",.!?;:，。！？；：".contains(character))
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn recency_score(updated_at: DateTime<Utc>) -> f32 {
    let age_days = (Utc::now() - updated_at).num_days().max(0) as f32;
    1.0 / (1.0 + age_days / 30.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_survives_reopening_and_can_be_retrieved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("memory.json");
        let store = MemoryStore::open(&path).await.unwrap();
        store
            .remember(
                Some("alice".to_string()),
                Some("session-1".to_string()),
                "Alice prefers concise Rust examples",
                MemoryKind::Preference,
                vec!["rust".to_string()],
                0.9,
            )
            .await
            .unwrap();
        drop(store);

        let reopened = MemoryStore::open(&path).await.unwrap();
        let results = reopened.search(Some("alice"), "Rust", 5).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("concise"));
    }
}
