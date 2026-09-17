use std::ops::AddAssign;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// 对话消息角色，serde 序列化为小写（"user" / "assistant"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 消息内容块：纯文本、工具调用、工具结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
        }
    }
}

/// 一条对话消息：一个角色加若干内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self::user_blocks(vec![ContentBlock::text(text)])
    }

    pub fn user_blocks(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::assistant_blocks(vec![ContentBlock::text(text)])
    }

    pub fn assistant_blocks(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }

    /// 拼接消息中所有 Text 块（用于离线模型的演示文案）。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 提供给模型的工具定义（与 provider 无关的中立格式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// 单次请求的 token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
    }
}

/// 模型停止生成的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

/// 一次模型调用请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    /// 模型名；为空字符串时由 provider 使用自己的默认模型。
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

/// 一次模型调用响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub blocks: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl ModelResponse {
    /// 拼接所有 Text 块为一段文本。
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 遍历所有 ToolUse 块，返回 (id, name, input)。
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> + '_ {
        self.blocks.iter().filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &'static str {
        "model-provider"
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse>;
}

/// Offline provider used by default, so the project can be run and tested
/// without credentials. Replace it with `OpenAiCompatibleModel` in production.
#[derive(Debug, Default)]
pub struct RuleBasedModel;

#[async_trait]
impl ModelProvider for RuleBasedModel {
    fn name(&self) -> &'static str {
        "offline"
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let user_text = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(ChatMessage::text)
            .unwrap_or_default();
        let answer = format!(
            "已完成请求。\n\n目标：{}\n\n这是离线演示模式的结果。配置 OPENAI_API_KEY 后将使用真实的 OpenAI-compatible 模型。",
            user_text.lines().next().unwrap_or("未提供目标")
        );
        Ok(ModelResponse {
            blocks: vec![ContentBlock::text(answer)],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleModel {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    provider_name: &'static str,
}

impl OpenAiCompatibleModel {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::new_with_optional_key(base_url, Some(api_key.into()), model, "openai-compatible")
    }

    pub fn new_without_api_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        provider_name: &'static str,
    ) -> Self {
        Self::new_with_optional_key(base_url, None, model, provider_name)
    }

    pub fn new_with_optional_key(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        provider_name: &'static str,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.filter(|value| !value.trim().is_empty()),
            model: model.into(),
            provider_name,
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleModel {
    fn name(&self) -> &'static str {
        self.provider_name
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let resolved = ModelRequest {
            model: if request.model.is_empty() {
                self.model.clone()
            } else {
                request.model.clone()
            },
            // 未指定时沿用旧版默认采样温度。
            temperature: request.temperature.or(Some(0.2)),
            ..request.clone()
        };
        let body = build_openai_request(&resolved);
        let request = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        let request = match &self.api_key {
            Some(api_key) => request.bearer_auth(api_key),
            None => request,
        };
        let value = request
            .send()
            .await
            .context("model request failed")?
            .error_for_status()
            .context("model returned an error status")?
            .json::<Value>()
            .await
            .context("invalid model response")?;

        parse_openai_response(&value)
    }
}

/// 把中立的 ModelRequest 转成 OpenAI chat.completions 请求体。
/// 纯函数，便于无网络单测。
pub fn build_openai_request(request: &ModelRequest) -> Value {
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({
            "role": "system",
            "content": request.system,
        }));
    }
    for message in &request.messages {
        match message.role {
            Role::User => {
                // Text 块聚合成 user 消息；ToolResult 块各自展开成 tool 消息。
                // 按块顺序自然保证 tool 消息紧跟在产生 tool_calls 的 assistant 消息之后。
                let mut text = String::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: chunk } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(chunk);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            if !text.is_empty() {
                                messages.push(json!({"role": "user", "content": text}));
                                text = String::new();
                            }
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        // user 消息里不应出现 ToolUse，忽略。
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                if !text.is_empty() {
                    messages.push(json!({"role": "user", "content": text}));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: chunk } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(chunk);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": input.to_string(),
                                },
                            }));
                        }
                        // assistant 消息里不应出现 ToolResult，忽略。
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
                let mut entry = Map::new();
                entry.insert("role".to_string(), json!("assistant"));
                entry.insert(
                    "content".to_string(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                if !tool_calls.is_empty() {
                    entry.insert("tool_calls".to_string(), Value::Array(tool_calls));
                }
                messages.push(Value::Object(entry));
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("messages".to_string(), Value::Array(messages));
    // max_tokens 必须显式发送，部分端点缺省时只返回极短的补全。
    body.insert("max_tokens".to_string(), json!(request.max_tokens));
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
    Value::Object(body)
}

/// 把 OpenAI chat.completions 响应体解析为中立的 ModelResponse。
/// 纯函数，便于无网络单测。
pub fn parse_openai_response(value: &Value) -> Result<ModelResponse> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .context("model returned no choices")?;
    let message = choice.get("message").cloned().unwrap_or(Value::Null);

    let mut blocks = Vec::new();
    if let Some(content) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.trim().is_empty())
    {
        blocks.push(ContentBlock::text(content));
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // arguments 是 JSON 字符串；解析失败时保留原文让工具校验层反馈给模型，
            // 而不是在这里报错中断 agent loop。
            let raw_arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = serde_json::from_str(raw_arguments)
                .unwrap_or_else(|_| json!({ "_invalid_arguments": raw_arguments }));
            blocks.push(ContentBlock::tool_use(id, name, input));
        }
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_tool_uses = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
    // length 优先于 tool_calls 判定（输出被截断时工具调用大概率不完整）。
    let stop_reason = match finish_reason {
        "length" => StopReason::MaxTokens,
        "tool_calls" => StopReason::ToolUse,
        "stop" => StopReason::EndTurn,
        other => StopReason::Other(other.to_string()),
    };
    // 部分兼容端点在带 tool_calls 时仍上报 stop，以实际内容为准。
    let stop_reason = match stop_reason {
        StopReason::EndTurn if has_tool_uses => StopReason::ToolUse,
        other => other,
    };

    // OpenAI 的 prompt_tokens 是总量，input_tokens 需扣除缓存命中部分；
    // 部分兼容端点不上报 usage，容错为默认值。
    let usage = match value.get("usage") {
        Some(usage) => {
            let prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let cached_tokens = usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_default();
            Usage {
                input_tokens: prompt_tokens.saturating_sub(cached_tokens),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                cache_read_tokens: cached_tokens,
                cache_creation_tokens: 0,
            }
        }
        None => Usage::default(),
    };

    Ok(ModelResponse {
        blocks,
        stop_reason,
        usage,
    })
}

pub fn provider_from_env() -> Result<Arc<dyn ModelProvider>> {
    let configured_provider = std::env::var("AGENT_PROVIDER")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let cliproxy_configured = std::env::var("CLIPROXYAPI_BASE_URL").is_ok()
        || std::env::var("CLIPROXYAPI_API_KEY").is_ok()
        || configured_provider == "cliproxyapi";
    let provider = if configured_provider.is_empty() {
        if cliproxy_configured {
            "cliproxyapi"
        } else if non_empty_env("OPENAI_API_KEY") {
            "openai"
        } else {
            "offline"
        }
    } else {
        configured_provider.as_str()
    };

    match provider {
        "cliproxyapi" | "cli-proxy-api" => {
            Ok(Arc::new(OpenAiCompatibleModel::new_with_optional_key(
                std::env::var("CLIPROXYAPI_BASE_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:8317/v1".to_string()),
                std::env::var("CLIPROXYAPI_API_KEY").ok(),
                std::env::var("CLIPROXYAPI_MODEL").unwrap_or_else(|_| "gpt-5.4".to_string()),
                "cliproxyapi",
            )))
        }
        "openai" | "openai-compatible" => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .context("OPENAI_API_KEY is required when AGENT_PROVIDER=openai")?;
            Ok(Arc::new(OpenAiCompatibleModel::new(
                std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
                api_key,
                std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
            )))
        }
        "offline" | "rule-based" => Ok(Arc::new(RuleBasedModel)),
        other => {
            anyhow::bail!("unsupported AGENT_PROVIDER={other}; use offline, openai, or cliproxyapi")
        }
    }
}

fn non_empty_env(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_request() -> ModelRequest {
        ModelRequest {
            model: "test-model".to_string(),
            system: "you are helpful".to_string(),
            messages: vec![ChatMessage::user("hello")],
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: None,
        }
    }

    #[test]
    fn chat_message_constructors() {
        let user = ChatMessage::user("hi");
        assert_eq!(user.role, Role::User);
        assert_eq!(user.content, vec![ContentBlock::text("hi")]);
        let assistant = ChatMessage::assistant_text("ok");
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.text(), "ok");
    }

    #[test]
    fn content_block_tool_result_serde_defaults() {
        let value = json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        });
        let block: ContentBlock = serde_json::from_value(value).unwrap();
        assert_eq!(
            block,
            ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "done".to_string(),
                is_error: false,
            }
        );
    }

    #[test]
    fn usage_add_assign_accumulates() {
        let mut total = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
        };
        total += Usage {
            input_tokens: 3,
            output_tokens: 7,
            cache_read_tokens: 4,
            cache_creation_tokens: 0,
        };
        assert_eq!(total.input_tokens, 13);
        assert_eq!(total.output_tokens, 12);
        assert_eq!(total.cache_read_tokens, 6);
        assert_eq!(total.cache_creation_tokens, 1);
    }

    #[test]
    fn model_response_text_and_tool_uses() {
        let response = ModelResponse {
            blocks: vec![
                ContentBlock::text("part1"),
                ContentBlock::tool_use("call_1", "Bash", json!({"command": "ls"})),
                ContentBlock::text("part2"),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };
        assert_eq!(response.text(), "part1\npart2");
        let tool_uses: Vec<_> = response.tool_uses().collect();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].0, "call_1");
        assert_eq!(tool_uses[0].1, "Bash");
        assert_eq!(tool_uses[0].2, &json!({"command": "ls"}));
    }

    #[test]
    fn build_request_injects_system_and_max_tokens() {
        let request = simple_request();
        let body = build_openai_request(&request);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["max_tokens"], 4096);
        assert!(body.get("tools").is_none());
        assert!(body.get("temperature").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "you are helpful"})
        );
        assert_eq!(messages[1], json!({"role": "user", "content": "hello"}));
    }

    #[test]
    fn build_request_skips_empty_system() {
        let request = ModelRequest {
            system: String::new(),
            ..simple_request()
        };
        let body = build_openai_request(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn build_request_serializes_tools() {
        let request = ModelRequest {
            tools: vec![ToolDefinition {
                name: "Bash".to_string(),
                description: "run commands".to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
            }],
            ..simple_request()
        };
        let body = build_openai_request(&request);
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "Bash",
                    "description": "run commands",
                    "parameters": {"type": "object", "properties": {}},
                }
            }])
        );
    }

    #[test]
    fn build_request_converts_assistant_tool_calls() {
        let request = ModelRequest {
            messages: vec![
                ChatMessage::user("run ls"),
                ChatMessage::assistant_blocks(vec![
                    ContentBlock::text("let me check"),
                    ContentBlock::tool_use("call_1", "Bash", json!({"command": "ls"})),
                ]),
            ],
            ..simple_request()
        };
        let body = build_openai_request(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[2],
            json!({
                "role": "assistant",
                "content": "let me check",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "Bash",
                        "arguments": "{\"command\":\"ls\"}",
                    }
                }]
            })
        );
    }

    #[test]
    fn build_request_assistant_tool_only_has_null_content() {
        let request = ModelRequest {
            messages: vec![ChatMessage::assistant_blocks(vec![ContentBlock::tool_use(
                "call_1",
                "Bash",
                json!({}),
            )])],
            ..simple_request()
        };
        let body = build_openai_request(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["content"], Value::Null);
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_request_expands_tool_results_before_trailing_text() {
        let request = ModelRequest {
            messages: vec![
                ChatMessage::assistant_blocks(vec![ContentBlock::tool_use(
                    "call_1",
                    "Bash",
                    json!({}),
                )]),
                ChatMessage::user_blocks(vec![
                    ContentBlock::tool_result("call_1", "file.txt", false),
                    ContentBlock::text("what next?"),
                ]),
            ],
            ..simple_request()
        };
        let body = build_openai_request(&request);
        let messages = body["messages"].as_array().unwrap();
        // tool 结果必须紧跟在 assistant 消息之后，之后的 text 再合成 user 消息。
        assert_eq!(
            messages[2],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "file.txt"})
        );
        assert_eq!(
            messages[3],
            json!({"role": "user", "content": "what next?"})
        );
    }

    #[test]
    fn build_request_multiple_tool_results_become_multiple_tool_messages() {
        let request = ModelRequest {
            messages: vec![
                ChatMessage::assistant_blocks(vec![
                    ContentBlock::tool_use("call_1", "Bash", json!({})),
                    ContentBlock::tool_use("call_2", "Grep", json!({})),
                ]),
                ChatMessage::user_blocks(vec![
                    ContentBlock::tool_result("call_1", "out1", false),
                    ContentBlock::tool_result("call_2", "out2", true),
                ]),
            ],
            ..simple_request()
        };
        let body = build_openai_request(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["tool_call_id"], "call_2");
        assert_eq!(messages[3]["role"], "tool");
    }

    #[test]
    fn parse_response_text_and_finish_reason_stop() {
        let value = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hello there"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let response = parse_openai_response(&value).unwrap();
        assert_eq!(response.text(), "hello there");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 5);
        assert_eq!(response.usage.cache_read_tokens, 0);
    }

    #[test]
    fn parse_response_tool_calls() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let response = parse_openai_response(&value).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        let tool_uses: Vec<_> = response.tool_uses().collect();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].2, &json!({"command": "ls"}));
    }

    #[test]
    fn parse_response_invalid_tool_arguments_are_preserved() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "Bash", "arguments": "not json"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let response = parse_openai_response(&value).unwrap();
        let tool_uses: Vec<_> = response.tool_uses().collect();
        assert_eq!(tool_uses[0].2, &json!({"_invalid_arguments": "not json"}));
    }

    #[test]
    fn parse_response_finish_reason_mapping() {
        let make = |finish_reason: &str, tool_calls: Value| {
            json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "x", "tool_calls": tool_calls},
                    "finish_reason": finish_reason
                }]
            })
        };
        let tool_call = json!([{
            "id": "c",
            "type": "function",
            "function": {"name": "Bash", "arguments": "{}"}
        }]);
        assert_eq!(
            parse_openai_response(&make("length", Value::Null))
                .unwrap()
                .stop_reason,
            StopReason::MaxTokens
        );
        // length 优先于 tool_calls：即使带了工具调用块也判为 MaxTokens。
        assert_eq!(
            parse_openai_response(&make("length", tool_call.clone()))
                .unwrap()
                .stop_reason,
            StopReason::MaxTokens
        );
        assert_eq!(
            parse_openai_response(&make("tool_calls", tool_call.clone()))
                .unwrap()
                .stop_reason,
            StopReason::ToolUse
        );
        // 兼容端点在带 tool_calls 时误报 stop，以实际内容为准。
        assert_eq!(
            parse_openai_response(&make("stop", tool_call))
                .unwrap()
                .stop_reason,
            StopReason::ToolUse
        );
        assert_eq!(
            parse_openai_response(&make("content_filter", Value::Null))
                .unwrap()
                .stop_reason,
            StopReason::Other("content_filter".to_string())
        );
    }

    #[test]
    fn parse_response_usage_deducts_cached_tokens() {
        let value = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 30}
            }
        });
        let response = parse_openai_response(&value).unwrap();
        assert_eq!(response.usage.input_tokens, 70);
        assert_eq!(response.usage.cache_read_tokens, 30);
        assert_eq!(response.usage.output_tokens, 20);
    }

    #[test]
    fn parse_response_missing_usage_defaults_to_zero() {
        let value = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        });
        let response = parse_openai_response(&value).unwrap();
        assert_eq!(response.usage, Usage::default());
    }

    #[test]
    fn parse_response_requires_choices() {
        let value = json!({"choices": []});
        assert!(parse_openai_response(&value).is_err());
    }

    #[tokio::test]
    async fn rule_based_model_returns_demo_text() {
        let model = RuleBasedModel;
        let request = ModelRequest {
            model: String::new(),
            system: "sys".to_string(),
            messages: vec![ChatMessage::user("第一行目标\n其余内容")],
            tools: Vec::new(),
            max_tokens: 1024,
            temperature: None,
        };
        let response = model.complete(&request).await.unwrap();
        assert_eq!(model.name(), "offline");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage, Usage::default());
        assert!(response.text().contains("第一行目标"));
        assert!(response.tool_uses().next().is_none());
    }
}
