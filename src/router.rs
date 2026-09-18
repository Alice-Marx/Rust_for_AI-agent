//! 按模型选择 wire 协议的 provider 路由。
//!
//! 同一个后端（本地 CLIProxyAPI 订阅代理、OpenAI、自建网关）可以同时提供
//! chat.completions、Responses 与 Anthropic Messages 三条路径。官方客户端各自
//! 使用不同协议，本模块按模型族选择 Responses、Messages 或 Chat Completions。
//! 协议选择并不保证与对应官方客户端的完整行为或效率相同。

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::model_profile::{protocol_for, WireProtocol};
use crate::provider::{ModelProvider, ModelRequest, ModelResponse, StreamSink};

/// 三条协议路径的组合。
pub struct ProtocolRouter {
    chat: Arc<dyn ModelProvider>,
    responses: Option<Arc<dyn ModelProvider>>,
    anthropic: Option<Arc<dyn ModelProvider>>,
    /// 无模型名时的兜底模型（来自 chat 路径）。
    default_model: Option<String>,
    /// 强制协议（AGENT_WIRE），设置后忽略模型族判断。
    forced: Option<WireProtocol>,
}

impl ProtocolRouter {
    pub fn new(chat: Arc<dyn ModelProvider>) -> Self {
        Self {
            default_model: chat.default_model(),
            chat,
            responses: None,
            anthropic: None,
            forced: None,
        }
    }

    pub fn with_responses(mut self, responses: Arc<dyn ModelProvider>) -> Self {
        self.responses = Some(responses);
        self
    }

    pub fn with_anthropic(mut self, anthropic: Arc<dyn ModelProvider>) -> Self {
        self.anthropic = Some(anthropic);
        self
    }

    pub fn with_forced(mut self, forced: Option<WireProtocol>) -> Self {
        self.forced = forced;
        self
    }

    /// 本路由实际可用的协议（用于健康检查与自检输出）。
    pub fn available_protocols_owned(&self) -> Vec<&'static str> {
        let mut protocols = vec![self.chat.name()];
        if let Some(responses) = &self.responses {
            protocols.push(responses.name());
        }
        if let Some(anthropic) = &self.anthropic {
            protocols.push(anthropic.name());
        }
        protocols
    }

    /// 选中的协议（未降级前）。
    pub fn preferred_protocol(&self, model: &str) -> WireProtocol {
        match self.forced {
            Some(forced) => forced,
            None if model.trim().is_empty() => self
                .default_model
                .as_deref()
                .map(protocol_for)
                .unwrap_or(WireProtocol::ChatCompletions),
            None => protocol_for(model),
        }
    }

    /// 选择实际使用的 provider；目标协议不可用时降级到 chat.completions。
    pub fn select(&self, model: &str) -> &Arc<dyn ModelProvider> {
        match self.preferred_protocol(model) {
            WireProtocol::Responses => self.responses.as_ref().unwrap_or(&self.chat),
            WireProtocol::AnthropicMessages => self.anthropic.as_ref().unwrap_or(&self.chat),
            WireProtocol::ChatCompletions => &self.chat,
        }
    }
}

#[async_trait]
impl ModelProvider for ProtocolRouter {
    fn name(&self) -> &'static str {
        "protocol-router"
    }

    fn default_model(&self) -> Option<String> {
        self.default_model.clone()
    }

    fn available_protocols(&self) -> Vec<&'static str> {
        self.available_protocols_owned()
    }

    async fn list_models(&self) -> Result<Vec<crate::cliproxy::CliProxyModel>> {
        self.chat.list_models().await
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let provider = self.select(&request.model);
        provider.complete(request).await
    }

    async fn complete_stream(
        &self,
        request: &ModelRequest,
        sink: Option<&StreamSink>,
    ) -> Result<ModelResponse> {
        let provider = self.select(&request.model);
        provider.complete_stream(request, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ContentBlock, ModelResponse, StopReason, Usage};
    use serde_json::json;

    fn named(name: &'static str) -> Arc<dyn ModelProvider> {
        Arc::new(FakeProvider { name })
    }

    struct FakeProvider {
        name: &'static str,
    }

    #[async_trait]
    impl ModelProvider for FakeProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn default_model(&self) -> Option<String> {
            Some(format!("{}-model", self.name))
        }

        async fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse {
                blocks: vec![ContentBlock::text(self.name)],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    fn request(model: &str) -> ModelRequest {
        ModelRequest {
            model: model.to_string(),
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 128,
            temperature: None,
            reasoning_effort: None,
            prompt_cache_key: None,
        }
    }

    fn router() -> ProtocolRouter {
        ProtocolRouter::new(named("chat"))
            .with_responses(named("responses"))
            .with_anthropic(named("anthropic"))
    }

    #[tokio::test]
    async fn dispatches_each_model_family_to_its_native_protocol() {
        let router = router();
        assert_eq!(
            router
                .complete(&request("claude-sonnet-4-5"))
                .await
                .unwrap()
                .text(),
            "anthropic"
        );
        assert_eq!(
            router
                .complete(&request("gpt-5.4-codex"))
                .await
                .unwrap()
                .text(),
            "responses"
        );
        assert_eq!(
            router
                .complete(&request("deepseek-chat"))
                .await
                .unwrap()
                .text(),
            "chat"
        );
        assert_eq!(
            router
                .complete(&request("kimi-k2-0905-preview"))
                .await
                .unwrap()
                .text(),
            "chat"
        );
    }

    #[tokio::test]
    async fn falls_back_to_chat_when_protocol_is_unavailable() {
        let router = ProtocolRouter::new(named("chat"));
        assert_eq!(
            router
                .complete(&request("claude-sonnet-4-5"))
                .await
                .unwrap()
                .text(),
            "chat"
        );
        assert_eq!(
            router.complete(&request("gpt-5.4")).await.unwrap().text(),
            "chat"
        );
        assert_eq!(router.available_protocols_owned(), vec!["chat"]);
    }

    #[tokio::test]
    async fn forced_protocol_overrides_model_family() {
        let router = router().with_forced(Some(WireProtocol::ChatCompletions));
        assert_eq!(
            router
                .complete(&request("claude-opus-4-1"))
                .await
                .unwrap()
                .text(),
            "chat"
        );
        assert_eq!(
            router.preferred_protocol("claude-opus-4-1"),
            WireProtocol::ChatCompletions
        );
    }

    #[tokio::test]
    async fn empty_model_uses_chat_and_reports_default_model() {
        let router = router();
        assert_eq!(
            router.preferred_protocol("  "),
            WireProtocol::ChatCompletions
        );
        assert_eq!(router.default_model().as_deref(), Some("chat-model"));
        assert_eq!(router.complete(&request("")).await.unwrap().text(), "chat");
    }

    #[tokio::test]
    async fn streaming_is_routed_to_the_same_provider() {
        let router = router();
        let (sink, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let response = router
            .complete_stream(&request("gpt-5.4"), Some(&sink))
            .await
            .unwrap();
        // gpt-5 走 responses 路径，验证流式没有绕路。
        assert_eq!(response.text(), "responses");
        // 假 provider 没实现流式，走默认实现：文本 + usage + stop 三个事件。
        let mut events = 0;
        while receiver.try_recv().is_ok() {
            events += 1;
        }
        assert_eq!(events, 3);
    }

    #[test]
    fn available_protocols_lists_configured_paths() {
        let router = router();
        assert_eq!(
            router.available_protocols_owned(),
            vec!["chat", "responses", "anthropic"]
        );
        assert_eq!(
            router.preferred_protocol("gpt-5.1"),
            WireProtocol::Responses
        );
        assert_eq!(json!(null), json!(null));
    }
}
