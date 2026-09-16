use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system_prompt: String,
    pub user_prompt: String,
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub text: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse>;
}

/// Offline provider used by default, so the project can be run and tested
/// without credentials. Replace it with `OpenAiCompatibleModel` in production.
#[derive(Debug, Default)]
pub struct RuleBasedModel;

#[async_trait]
impl ModelProvider for RuleBasedModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let answer = format!(
            "已完成请求。\n\n目标：{}\n\n这是离线演示模式的结果。配置 OPENAI_API_KEY 后将使用真实的 OpenAI-compatible 模型。",
            request.user_prompt.lines().next().unwrap_or("未提供目标")
        );
        Ok(ModelResponse {
            prompt_tokens: approximate_tokens(&request.system_prompt, &request.user_prompt),
            completion_tokens: approximate_tokens("", &answer),
            text: answer,
        })
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleModel {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiCompatibleModel {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let body = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage { role: "system", content: request.system_prompt },
                ChatMessage { role: "user", content: request.user_prompt },
            ],
            temperature: 0.2,
        };
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("model request failed")?
            .error_for_status()
            .context("model returned an error status")?
            .json::<ChatResponse>()
            .await
            .context("invalid model response")?;

        let text = response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|value| !value.trim().is_empty())
            .context("model returned no text")?;
        let usage = response.usage.unwrap_or(Usage { prompt_tokens: None, completion_tokens: None });

        Ok(ModelResponse {
            prompt_tokens: usage.prompt_tokens.unwrap_or_default(),
            completion_tokens: usage.completion_tokens.unwrap_or_default(),
            text,
        })
    }
}

pub fn provider_from_env() -> Result<Arc<dyn ModelProvider>> {
    match std::env::var("OPENAI_API_KEY").ok().filter(|value| !value.is_empty()) {
        Some(api_key) => Ok(Arc::new(OpenAiCompatibleModel::new(
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            api_key,
            std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
        ))),
        None => Ok(Arc::new(RuleBasedModel)),
    }
}

fn approximate_tokens(system: &str, user: &str) -> u32 {
    ((system.chars().count() + user.chars().count()) as u32 / 4).max(1)
}
