//! One-time, expiring tool approvals for interactive SSE clients.
use crate::{
    permissions::{PermissionHandler, PermissionPrompt},
    provider::{StreamEvent, StreamSink},
};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::oneshot;
type Pending = Mutex<HashMap<String, oneshot::Sender<bool>>>;
fn pending() -> &'static Pending {
    static P: OnceLock<Pending> = OnceLock::new();
    P.get_or_init(Mutex::default)
}
struct Guard(String);
impl Drop for Guard {
    fn drop(&mut self) {
        pending()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}
pub struct InteractiveHandler(pub StreamSink);
#[async_trait::async_trait]
impl PermissionHandler for InteractiveHandler {
    async fn ask(&self, prompt: &PermissionPrompt) -> bool {
        let id = uuid::Uuid::new_v4().to_string();
        let guard = Guard(id.clone());
        let (tx, rx) = oneshot::channel();
        pending()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), tx);
        if self
            .0
            .send(StreamEvent::PermissionRequest {
                id,
                tool: prompt.tool_name.clone(),
                input: prompt.details.clone(),
                reason: prompt.description.clone(),
            })
            .is_err()
        {
            return false;
        }
        let result = tokio::time::timeout(Duration::from_secs(300), rx).await;
        drop(guard);
        matches!(result, Ok(Ok(true)))
    }
}
pub fn answer(id: &str, allow: bool) -> bool {
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id)
        .is_some_and(|tx| tx.send(allow).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn approval_is_one_time_and_cancel_removes_it() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            InteractiveHandler(tx)
                .ask(&PermissionPrompt {
                    tool_name: "Bash".into(),
                    description: "test".into(),
                    rule_content: None,
                    details: serde_json::json!({"command":"echo test"}),
                })
                .await
        });
        let Some(StreamEvent::PermissionRequest { id, input, .. }) = rx.recv().await else {
            panic!("missing prompt")
        };
        assert_eq!(input["command"], "echo test");
        assert!(answer(&id, true));
        assert!(!answer(&id, true));
        assert!(task.await.unwrap());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            InteractiveHandler(tx)
                .ask(&PermissionPrompt {
                    tool_name: "Bash".into(),
                    description: "cancel".into(),
                    rule_content: None,
                    details: serde_json::json!({}),
                })
                .await
        });
        let Some(StreamEvent::PermissionRequest { id, .. }) = rx.recv().await else {
            panic!("missing prompt")
        };
        task.abort();
        let _ = task.await;
        assert!(!answer(&id, true));
    }
}
