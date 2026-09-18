//! Anthropic Messages API（`/v1/messages`）原生 provider。
//!
//! 为什么需要它：OpenAI 兼容层能调用 Claude，但会丢掉三件 Claude Code 依赖的能力 ——
//! 扩展思考（`thinking`）、显式提示缓存断点（`cache_control`）、以及需要原样回传的
//! thinking 块签名。这里按官方 Messages API 直连，使 Claude 模型在本项目里的
//! 工具调用与缓存命中行为与官方客户端一致。
//!
//! 纯函数 `build_anthropic_request` / `parse_anthropic_response` 便于无网络单测。

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Map, Value};

use futures_util::StreamExt;

use crate::model_profile::thinking_budget_tokens;
use crate::provider::{
    emit, ChatMessage, ContentBlock, ModelProvider, ModelRequest, ModelResponse, Role, SseBuffer,
    StopReason, StreamEvent, StreamSink, Usage,
};

/// Anthropic API 版本头，与官方 SDK 默认值一致。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// 开启 thinking 时为输出预留的额外 token（Anthropic 要求 max_tokens > budget_tokens）。
const THINKING_HEADROOM: u32 = 1_024;

#[derive(Clone)]
pub struct AnthropicModel {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl AnthropicModel {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: crate::connection::model_client(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` / `ANTHROPIC_MODEL`。
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or_else(|| std::env::var("ANTHROPIC_AUTH_TOKEN").ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .context("ANTHROPIC_API_KEY is required when AGENT_PROVIDER=anthropic")?;
        Ok(Self::new(
            std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string()),
            api_key,
            std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".to_string()),
        ))
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl ModelProvider for AnthropicModel {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn default_model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    async fn list_models(&self) -> Result<Vec<crate::cliproxy::CliProxyModel>> {
        let value: Value = self
            .client
            .get(format!("{}/models", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(serde_json::from_value(value["data"].clone())?)
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let resolved = ModelRequest {
            model: if request.model.is_empty() {
                self.model.clone()
            } else {
                request.model.clone()
            },
            ..request.clone()
        };
        let body = build_anthropic_request(&resolved);
        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .context("anthropic request failed")?;
        let status = response.status();
        let raw = response
            .text()
            .await
            .context("anthropic response body is not readable")?;
        if !status.is_success() {
            // 把上游错误体带进错误信息，便于定位模型名/参数问题。
            bail!("anthropic returned {status}: {raw}");
        }
        let value: Value =
            serde_json::from_str(&raw).context("anthropic returned invalid JSON response")?;
        parse_anthropic_response(&value)
    }

    async fn complete_stream(
        &self,
        request: &ModelRequest,
        sink: Option<&StreamSink>,
    ) -> Result<ModelResponse> {
        let resolved = ModelRequest {
            model: if request.model.is_empty() {
                self.model.clone()
            } else {
                request.model.clone()
            },
            ..request.clone()
        };
        let body = build_anthropic_request_with_stream(&resolved, true);
        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .context("anthropic stream request failed")?;
        let status = response.status();
        if !status.is_success() {
            let raw = response.text().await.unwrap_or_default();
            anyhow::bail!("anthropic returned {status}: {raw}");
        }
        let mut stream = response.bytes_stream();
        let mut decoder = SseBuffer::new();
        let mut accumulator = AnthropicStreamAccumulator::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("anthropic stream interrupted")?;
            for payload in decoder.push_bytes(&chunk) {
                let trimmed = payload.trim();
                if trimmed.is_empty() || trimmed == "[DONE]" {
                    continue;
                }
                let value: Value = serde_json::from_str(trimmed)
                    .with_context(|| format!("invalid anthropic stream chunk: {trimmed}"))?;
                accumulator.apply(&value, sink)?;
                if matches!(
                    value.get("type").and_then(Value::as_str),
                    Some("message_stop" | "error")
                ) {
                    return accumulator.finish(sink);
                }
            }
        }
        accumulator.finish(sink)
    }
}

fn cache_control() -> Value {
    json!({"type": "ephemeral"})
}

/// 把中立的 ModelRequest 转成 Anthropic Messages API 请求体。
/// 纯函数，便于无网络单测。
pub fn build_anthropic_request(request: &ModelRequest) -> Value {
    build_anthropic_request_with_stream(request, false)
}

/// 同 build_anthropic_request，但可开启 SSE 流式。
pub fn build_anthropic_request_with_stream(request: &ModelRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    if stream {
        body.insert("stream".to_string(), json!(true));
    }

    let adaptive =
        request.model.contains("claude-opus-4-6") || request.model.contains("claude-sonnet-4-6");
    let budget_tokens = request
        .reasoning_effort
        .as_deref()
        .filter(|effort| !matches!(*effort, "off" | "none"))
        .filter(|_| !adaptive)
        .map(|effort| thinking_budget_tokens(Some(effort)));
    // Anthropic 要求 max_tokens 严格大于 thinking 预算。
    let max_tokens = match budget_tokens {
        Some(budget) => request
            .max_tokens
            .max(budget.saturating_add(THINKING_HEADROOM)),
        None => request.max_tokens,
    };
    body.insert("max_tokens".to_string(), json!(max_tokens));

    // system 段是第一个缓存断点：身份/工具规范/项目指令都落在静态前缀里。
    if !request.system.trim().is_empty() {
        body.insert(
            "system".to_string(),
            json!([{
                "type": "text",
                "text": request.system,
                "cache_control": cache_control(),
            }]),
        );
    }

    body.insert(
        "messages".to_string(),
        Value::Array(render_messages(&request.messages)),
    );
    if !request.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            Value::Array(render_tools(&request.tools)),
        );
    }

    if let Some(effort) = request
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| !matches!(*value, "off" | "none"))
    {
        if adaptive {
            body.insert("thinking".into(), json!({"type":"adaptive"}));
            body.insert("output_config".into(), json!({"effort":effort}));
            return Value::Object(body);
        }
        let budget = thinking_budget_tokens(Some(effort));
        if budget < max_tokens {
            body.insert(
                "thinking".to_string(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
            // Anthropic 在开启 thinking 时要求 temperature 保持默认值。
            return Value::Object(body);
        }
    }

    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    Value::Object(body)
}

/// 渲染 messages，并给最后一条消息的最后一个内容块打上会话前缀缓存断点。
fn render_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut rendered = Vec::new();
    for message in messages {
        let mut blocks = Vec::new();
        for block in &message.content {
            match (message.role, block) {
                (_, ContentBlock::Text { text }) => {
                    if !text.trim().is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                (Role::Assistant, ContentBlock::ToolUse { id, name, input }) => {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input,
                    }));
                }
                (
                    Role::Assistant,
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    },
                ) => {
                    // 签名缺失的 thinking 块不能回传，Anthropic 会直接拒绝请求。
                    if let Some(signature) = signature.as_deref().filter(|value| !value.is_empty())
                    {
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": thinking,
                            "signature": signature,
                        }));
                    }
                }
                (
                    Role::User,
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    },
                ) => {
                    let mut entry = Map::new();
                    entry.insert("type".to_string(), json!("tool_result"));
                    entry.insert("tool_use_id".to_string(), json!(tool_use_id));
                    entry.insert(
                        "content".to_string(),
                        json!([{"type": "text", "text": content}]),
                    );
                    if *is_error {
                        entry.insert("is_error".to_string(), json!(true));
                    }
                    blocks.push(Value::Object(entry));
                }
                (
                    Role::Assistant,
                    ContentBlock::ProviderReasoning {
                        provider, payload, ..
                    },
                ) if provider == "anthropic" => blocks.push(payload.clone()),
                // user 消息里的 ToolUse、assistant 消息里的 ToolResult 都不合法；忽略。
                // user 消息里的 Thinking 同样不回传。
                (_, ContentBlock::ToolUse { .. })
                | (_, ContentBlock::ToolResult { .. })
                | (Role::User, ContentBlock::Thinking { .. })
                | (_, ContentBlock::ProviderReasoning { .. }) => {}
            }
        }
        if blocks.is_empty() {
            continue;
        }
        rendered.push(json!({
            "role": match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            },
            "content": blocks,
        }));
    }

    // 会话前缀断点：打在最后一条消息的最后一个内容块上，新增的一轮只增量计费。
    if let Some(last) = rendered.last_mut() {
        if let Some(block) = last
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|blocks| blocks.last_mut())
            .and_then(Value::as_object_mut)
        {
            block.insert("cache_control".to_string(), cache_control());
        }
    }
    rendered
}

/// 工具定义 + 最后一个工具的缓存断点。
fn render_tools(tools: &[crate::provider::ToolDefinition]) -> Vec<Value> {
    let last_index = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut entry = Map::new();
            entry.insert("name".to_string(), json!(tool.name));
            entry.insert("description".to_string(), json!(tool.description));
            entry.insert("input_schema".to_string(), tool.input_schema.clone());
            if index == last_index {
                entry.insert("cache_control".to_string(), cache_control());
            }
            Value::Object(entry)
        })
        .collect()
}

/// 把 Anthropic Messages API 响应体解析为中立的 ModelResponse。纯函数。
pub fn parse_anthropic_response(value: &Value) -> Result<ModelResponse> {
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .context("anthropic response has no content array")?;

    let mut blocks = Vec::new();
    for block in content {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    blocks.push(ContentBlock::text(text));
                }
            }
            "thinking" => {
                if let Some(thinking) = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    blocks.push(ContentBlock::thinking(
                        thinking,
                        block
                            .get("signature")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    ));
                }
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                blocks.push(ContentBlock::tool_use(id, name, input));
            }
            "redacted_thinking" => {
                blocks.push(ContentBlock::ProviderReasoning {
                    provider: "anthropic".into(),
                    summary: String::new(),
                    payload: block.clone(),
                });
            }
            // Unknown block types are ignored.
            _ => {}
        }
    }

    let has_tool_uses = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
    let stop_reason = match value
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "" => {
            if has_tool_uses {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
        other => StopReason::Other(other.to_string()),
    };

    let usage = match value.get("usage") {
        Some(usage) => Usage {
            // Anthropic 的 input_tokens 本身就不含缓存命中与缓存写入部分。
            input_tokens: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            output_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            cache_read_tokens: usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            cache_creation_tokens: usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        },
        None => Usage::default(),
    };

    Ok(ModelResponse {
        blocks,
        stop_reason,
        usage,
    })
}

/// Anthropic SSE 事件累积器：把流式事件还原为中立的 ModelResponse。
#[derive(Debug, Default)]
pub struct AnthropicStreamAccumulator {
    blocks: Vec<AnthropicStreamBlock>,
    input_usage: Usage,
    output_tokens: u64,
    stop_reason: Option<StopReason>,
    failure: Option<String>,
}

#[derive(Debug, Clone, Default)]
enum AnthropicStreamBlock {
    Redacted {
        payload: Value,
    },
    #[default]
    Unknown,
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
}

impl AnthropicStreamAccumulator {
    pub fn apply(&mut self, event: &Value, sink: Option<&StreamSink>) -> Result<()> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match kind {
            "message_start" => {
                if let Some(usage) = event
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    self.input_usage = parse_stream_usage(usage);
                }
            }
            "content_block_start" => {
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                let started = match block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text" => AnthropicStreamBlock::Text {
                        text: block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                    "redacted_thinking" => AnthropicStreamBlock::Redacted {
                        payload: block.clone(),
                    },
                    "thinking" => AnthropicStreamBlock::Thinking {
                        thinking: block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        signature: String::new(),
                    },
                    "tool_use" => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        emit(
                            sink,
                            StreamEvent::ToolUseStart {
                                id: id.clone(),
                                name: name.clone(),
                            },
                        );
                        AnthropicStreamBlock::ToolUse {
                            id,
                            name,
                            json: String::new(),
                        }
                    }
                    _ => AnthropicStreamBlock::Unknown,
                };
                self.set_block(index, started);
            }
            "content_block_delta" => {
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let AnthropicStreamBlock::Text { text: buffer } = self.slot(index) {
                            buffer.push_str(text);
                        }
                        if !text.is_empty() {
                            emit(
                                sink,
                                StreamEvent::TextDelta {
                                    text: text.to_string(),
                                },
                            );
                        }
                    }
                    "thinking_delta" => {
                        let text = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let AnthropicStreamBlock::Thinking { thinking, .. } = self.slot(index) {
                            thinking.push_str(text);
                        }
                        if !text.is_empty() {
                            emit(
                                sink,
                                StreamEvent::ReasoningDelta {
                                    text: text.to_string(),
                                },
                            );
                        }
                    }
                    "signature_delta" => {
                        let signature = delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let AnthropicStreamBlock::Thinking {
                            signature: buffer, ..
                        } = self.slot(index)
                        {
                            buffer.push_str(signature);
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let id = match self.slot(index) {
                            AnthropicStreamBlock::ToolUse { id, json, .. } => {
                                json.push_str(partial);
                                id.clone()
                            }
                            _ => String::new(),
                        };
                        if !partial.is_empty() && !id.is_empty() {
                            emit(
                                sink,
                                StreamEvent::ToolUseDelta {
                                    id,
                                    partial_json: partial.to_string(),
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let AnthropicStreamBlock::ToolUse { id, .. } = self.slot(index) {
                    emit(sink, StreamEvent::ToolUseStop { id: id.clone() });
                }
            }
            "message_delta" => {
                if let Some(stop) = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(match stop {
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        other => StopReason::Other(other.to_string()),
                    });
                }
                if let Some(output) = event
                    .get("usage")
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.output_tokens = output;
                }
            }
            "error" => {
                self.failure = Some(
                    event
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| "anthropic stream error".to_string()),
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn slot(&mut self, index: usize) -> &mut AnthropicStreamBlock {
        while self.blocks.len() <= index {
            self.blocks.push(AnthropicStreamBlock::Unknown);
        }
        &mut self.blocks[index]
    }

    fn set_block(&mut self, index: usize, block: AnthropicStreamBlock) {
        while self.blocks.len() <= index {
            self.blocks.push(AnthropicStreamBlock::Unknown);
        }
        self.blocks[index] = block;
    }

    pub fn finish(self, sink: Option<&StreamSink>) -> Result<ModelResponse> {
        if let Some(message) = self.failure {
            anyhow::bail!("anthropic stream failed: {message}");
        }
        anyhow::ensure!(
            self.stop_reason.is_some(),
            "anthropic stream disconnected before stop_reason"
        );
        let mut blocks = Vec::new();
        let mut has_tool_use = false;
        for block in &self.blocks {
            match block {
                AnthropicStreamBlock::Text { text } => {
                    if !text.trim().is_empty() {
                        blocks.push(ContentBlock::text(text.clone()));
                    }
                }
                AnthropicStreamBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    if !thinking.trim().is_empty() || !signature.is_empty() {
                        blocks.push(ContentBlock::thinking(
                            thinking.clone(),
                            if signature.is_empty() {
                                None
                            } else {
                                Some(signature.clone())
                            },
                        ));
                    }
                }
                AnthropicStreamBlock::ToolUse { id, name, json } => {
                    if id.is_empty() && name.is_empty() {
                        continue;
                    }
                    has_tool_use = true;
                    let input = serde_json::from_str(json)
                        .unwrap_or_else(|_| json!({ "_invalid_arguments": json }));
                    blocks.push(ContentBlock::tool_use(id.clone(), name.clone(), input));
                }
                AnthropicStreamBlock::Redacted { payload } => {
                    blocks.push(ContentBlock::ProviderReasoning {
                        provider: "anthropic".into(),
                        summary: String::new(),
                        payload: payload.clone(),
                    })
                }
                AnthropicStreamBlock::Unknown => {}
            }
        }
        let usage = Usage {
            input_tokens: self.input_usage.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.input_usage.cache_read_tokens,
            cache_creation_tokens: self.input_usage.cache_creation_tokens,
        };
        let stop_reason = match self.stop_reason {
            Some(StopReason::EndTurn) if has_tool_use => StopReason::ToolUse,
            Some(reason) => reason,
            None if has_tool_use => StopReason::ToolUse,
            None => StopReason::EndTurn,
        };
        emit(sink, StreamEvent::Usage { usage });
        emit(
            sink,
            StreamEvent::Stop {
                stop_reason: stop_reason.clone(),
            },
        );
        Ok(ModelResponse {
            blocks,
            stop_reason,
            usage,
        })
    }
}

fn parse_stream_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cache_read_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cache_creation_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolDefinition;

    fn request() -> ModelRequest {
        ModelRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: "you are helpful".to_string(),
            messages: vec![ChatMessage::user("hello")],
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: None,
            reasoning_effort: None,
            prompt_cache_key: None,
        }
    }

    fn bash_tool() -> ToolDefinition {
        ToolDefinition {
            name: "Bash".to_string(),
            description: "run commands".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn system_becomes_cached_text_block() {
        let body = build_anthropic_request(&request());
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "you are helpful");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert!(body.get("temperature").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn empty_system_is_omitted() {
        let body = build_anthropic_request(&ModelRequest {
            system: "   ".to_string(),
            ..request()
        });
        assert!(body.get("system").is_none());
    }

    #[test]
    fn tools_are_rendered_with_last_tool_cache_breakpoint() {
        let body = build_anthropic_request(&ModelRequest {
            tools: vec![bash_tool(), bash_tool()],
            ..request()
        });
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "Bash");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn request_without_tools_omits_tools_field() {
        let body = build_anthropic_request(&request());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn last_message_block_gets_cache_breakpoint() {
        let body = build_anthropic_request(&request());
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn thinking_enabled_bumps_max_tokens_and_drops_temperature() {
        let body = build_anthropic_request(&ModelRequest {
            max_tokens: 1024,
            temperature: Some(0.7),
            reasoning_effort: Some("low".to_string()),
            prompt_cache_key: None,
            ..request()
        });
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
        // max_tokens 必须大于 thinking 预算，否则 Anthropic 直接拒绝请求。
        assert_eq!(body["max_tokens"], 3072);
        assert!(body.get("temperature").is_none());

        // 已经足够大的 max_tokens 不会被下调。
        let wide = build_anthropic_request(&ModelRequest {
            max_tokens: 32_000,
            reasoning_effort: Some("high".to_string()),
            prompt_cache_key: None,
            ..request()
        });
        assert_eq!(wide["thinking"]["budget_tokens"], 16_000);
        assert_eq!(wide["max_tokens"], 32_000);
    }

    #[test]
    fn tool_results_render_as_tool_result_blocks() {
        let body = build_anthropic_request(&ModelRequest {
            messages: vec![
                ChatMessage::assistant_blocks(vec![ContentBlock::tool_use(
                    "call_1",
                    "Bash",
                    json!({"command": "ls"}),
                )]),
                ChatMessage::user_blocks(vec![ContentBlock::tool_result(
                    "call_1", "file.txt", false,
                )]),
            ],
            ..request()
        });
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["name"], "Bash");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "call_1");
        assert!(messages[1]["content"][0].get("is_error").is_none());
    }

    #[test]
    fn error_tool_result_sets_is_error() {
        let body = build_anthropic_request(&ModelRequest {
            messages: vec![ChatMessage::user_blocks(vec![ContentBlock::tool_result(
                "call_1", "boom", true,
            )])],
            ..request()
        });
        assert_eq!(body["messages"][0]["content"][0]["is_error"], true);
    }

    #[test]
    fn thinking_blocks_round_trip_only_with_signature() {
        let body = build_anthropic_request(&ModelRequest {
            messages: vec![ChatMessage::assistant_blocks(vec![
                ContentBlock::thinking("signed", Some("sig-1".to_string())),
                ContentBlock::thinking("unsigned", None),
                ContentBlock::text("answer"),
            ])],
            ..request()
        });
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["signature"], "sig-1");
        assert_eq!(blocks[1]["type"], "text");
    }

    #[test]
    fn parses_text_thinking_and_tool_use() {
        let value = json!({
            "content": [
                {"type": "thinking", "thinking": "let me think", "signature": "sig"},
                {"type": "text", "text": "working on it"},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 4,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 7
            }
        });
        let response = parse_anthropic_response(&value).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.text(), "working on it");
        assert_eq!(response.blocks[0].as_thinking(), Some("let me think"));
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.cache_read_tokens, 100);
        assert_eq!(response.usage.cache_creation_tokens, 7);
        let tool_uses: Vec<_> = response.tool_uses().collect();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].1, "Bash");
        assert_eq!(tool_uses[0].2, &json!({"command": "ls"}));
    }

    #[test]
    fn parses_stop_reasons_and_missing_usage() {
        let text_only = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn"
        });
        let response = parse_anthropic_response(&text_only).unwrap();
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage, Usage::default());

        let truncated = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "max_tokens"
        });
        assert_eq!(
            parse_anthropic_response(&truncated).unwrap().stop_reason,
            StopReason::MaxTokens
        );

        let refusal = json!({"content": [], "stop_reason": "refusal"});
        assert_eq!(
            parse_anthropic_response(&refusal).unwrap().stop_reason,
            StopReason::Other("refusal".to_string())
        );
    }

    #[test]
    fn missing_content_array_is_an_error() {
        assert!(parse_anthropic_response(&json!({"id": "msg_1"})).is_err());
    }

    #[test]
    fn response_without_stop_reason_infers_from_content() {
        let value = json!({
            "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]
        });
        assert_eq!(
            parse_anthropic_response(&value).unwrap().stop_reason,
            StopReason::ToolUse
        );
    }

    #[test]
    fn redacted_thinking_is_preserved() {
        let value = json!({
            "content": [
                {"type": "redacted_thinking", "data": "xxx"},
                {"type": "text", "text": "ok"}
            ],
            "stop_reason": "end_turn"
        });
        let response = parse_anthropic_response(&value).unwrap();
        assert_eq!(response.blocks.len(), 2);
        assert_eq!(response.text(), "ok");
    }
}
