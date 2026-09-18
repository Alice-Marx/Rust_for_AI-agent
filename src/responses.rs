//! OpenAI Responses API（`/v1/responses`）provider。
//!
//! 这是 Codex CLI 使用的协议，和 chat.completions 有三处关键差异，直接决定
//! GPT 系模型在本项目里的效率：
//! - `instructions` 与 `input` 分离：系统提示词不占 `input` 数组，缓存前缀更稳定；
//! - `store: false` + `include: ["reasoning.encrypted_content"]`：推理内容由客户端
//!   持有并原样回传，多轮之间保留思维链，而不是每轮重新推理；
//! - `prompt_cache_key` + `reasoning.summary`：配合会话级 key 提升缓存命中率，
//!   并让流式界面能显示推理摘要。
//!
//! 纯函数 `build_responses_request` / `parse_responses_response` / 流式累积器
//! 都可以无网络单测。

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Map, Value};

use crate::provider::{
    emit, ChatMessage, ContentBlock, ModelProvider, ModelRequest, ModelResponse, Role, SseBuffer,
    StopReason, StreamEvent, StreamSink, Usage,
};

/// Responses API 的默认端点（与 OpenAI 官方一致）。
pub const RESPONSES_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Clone)]
pub struct ResponsesModel {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    provider_name: &'static str,
    extra_headers: Vec<(String, String)>,
}

impl ResponsesModel {
    pub fn new(
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
            extra_headers: Vec::new(),
        }
    }

    /// 追加请求头（自建网关、代理链路可能需要）。
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    async fn send(&self, request: &ModelRequest, stream: bool) -> Result<reqwest::Response> {
        let resolved = ModelRequest {
            model: if request.model.is_empty() {
                self.model.clone()
            } else {
                request.model.clone()
            },
            ..request.clone()
        };
        let mut body = build_responses_request(&resolved);
        if !stream {
            if let Some(object) = body.as_object_mut() {
                object.remove("stream");
            }
        }
        let mut http = self.client.post(self.endpoint()).json(&body);
        for (name, value) in &self.extra_headers {
            http = http.header(name, value);
        }
        if let Some(api_key) = &self.api_key {
            http = http.bearer_auth(api_key);
        }
        let response = http.send().await.context("responses request failed")?;
        let status = response.status();
        if !status.is_success() {
            let raw = response.text().await.unwrap_or_default();
            bail!("responses API returned {status}: {raw}");
        }
        Ok(response)
    }
}

#[async_trait]
impl ModelProvider for ResponsesModel {
    fn name(&self) -> &'static str {
        self.provider_name
    }

    fn default_model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse> {
        let response = self.send(request, false).await?;
        let value: Value = response
            .json()
            .await
            .context("responses API returned invalid JSON")?;
        parse_responses_response(&value)
    }

    async fn complete_stream(
        &self,
        request: &ModelRequest,
        sink: Option<&StreamSink>,
    ) -> Result<ModelResponse> {
        let response = self.send(request, true).await?;
        let mut stream = response.bytes_stream();
        let mut decoder = SseBuffer::new();
        let mut accumulator = ResponsesStreamAccumulator::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("responses stream interrupted")?;
            for payload in decoder.push_bytes(&chunk) {
                let trimmed = payload.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "[DONE]" {
                    return accumulator.finish(sink);
                }
                let value: Value = serde_json::from_str(trimmed)
                    .with_context(|| format!("invalid responses stream chunk: {trimmed}"))?;
                accumulator.apply(&value, sink)?;
            }
        }
        accumulator.finish(sink)
    }
}

/// 把中立的 ModelRequest 转成 Responses API 请求体。纯函数。
pub fn build_responses_request(request: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    if !request.system.trim().is_empty() {
        body.insert("instructions".to_string(), json!(request.system));
    }
    body.insert(
        "input".to_string(),
        Value::Array(render_input_items(&request.messages)),
    );

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
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                            "strict": false,
                        })
                    })
                    .collect(),
            ),
        );
        body.insert("tool_choice".to_string(), json!("auto"));
        body.insert("parallel_tool_calls".to_string(), json!(true));
    }

    if let Some(effort) = request
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        // summary=auto 让流式界面能显示推理摘要（与 Codex 的 reasoning.summary 一致）。
        body.insert(
            "reasoning".to_string(),
            json!({ "effort": effort, "summary": "auto" }),
        );
    }
    // 关键三项：不落库、要回加密封装的推理内容、会话级缓存 key。
    body.insert("store".to_string(), json!(false));
    body.insert(
        "include".to_string(),
        json!(["reasoning.encrypted_content"]),
    );
    if let Some(key) = request
        .prompt_cache_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        body.insert("prompt_cache_key".to_string(), json!(key));
    }
    body.insert("stream".to_string(), json!(true));
    Value::Object(body)
}

/// 把会话消息渲染为 Responses API 的 input 数组。
fn render_input_items(messages: &[ChatMessage]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        for block in &message.content {
            match (message.role, block) {
                (_, ContentBlock::Text { text }) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    let kind = match message.role {
                        Role::User => "input_text",
                        Role::Assistant => "output_text",
                    };
                    items.push(json!({
                        "type": "message",
                        "role": match message.role {
                            Role::User => "user",
                            Role::Assistant => "assistant",
                        },
                        "content": [{ "type": kind, "text": text }],
                    }));
                }
                (Role::Assistant, ContentBlock::ToolUse { id, name, input }) => {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    }));
                }
                (
                    Role::User,
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    },
                ) => {
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    }));
                }
                (
                    Role::Assistant,
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    },
                ) => {
                    // encrypted_content 必须原样回传，否则模型会丢失上一轮推理。
                    if signature.as_deref().unwrap_or_default().is_empty() {
                        continue;
                    }
                    items.push(json!({
                        "type": "reasoning",
                        "summary": reasoning_summary(thinking),
                        "encrypted_content": signature,
                    }));
                }
                // user 消息里的 ToolUse/Thinking、assistant 消息里的 ToolResult 都不合法，忽略。
                (_, ContentBlock::ToolUse { .. })
                | (_, ContentBlock::ToolResult { .. })
                | (Role::User, ContentBlock::Thinking { .. }) => {}
            }
        }
    }
    items
}

fn reasoning_summary(text: &str) -> Value {
    if text.trim().is_empty() {
        json!([])
    } else {
        json!([{ "type": "summary_text", "text": text }])
    }
}

/// 解析非流式 Responses 响应体。纯函数。
pub fn parse_responses_response(value: &Value) -> Result<ModelResponse> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        bail!("responses API error: {message}");
    }
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .context("responses API response has no output array")?;
    let mut blocks = Vec::new();
    for item in output {
        if let Some(block) = item_to_block(item) {
            blocks.push(block);
        }
    }
    let has_tool_use = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
    let stop_reason = stop_reason_from_status(value, has_tool_use);
    let usage = value
        .get("usage")
        .map(parse_responses_usage)
        .unwrap_or_default();
    Ok(ModelResponse {
        blocks,
        stop_reason,
        usage,
    })
}

/// 单个 output item → 中立内容块。
fn item_to_block(item: &Value) -> Option<ContentBlock> {
    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "message" => {
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                None
            } else {
                Some(ContentBlock::text(text))
            }
        }
        "function_call" => {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = serde_json::from_str(arguments)
                .unwrap_or_else(|_| json!({ "_invalid_arguments": arguments }));
            Some(ContentBlock::tool_use(call_id, name, input))
        }
        "reasoning" => {
            let summary = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            let encrypted = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_string);
            if summary.trim().is_empty() && encrypted.is_none() {
                None
            } else {
                Some(ContentBlock::thinking(summary, encrypted))
            }
        }
        _ => None,
    }
}

/// Responses 的 usage：input_tokens 是总量，需要扣除缓存命中部分（与 OpenAI 一致）。
pub fn parse_responses_usage(usage: &Value) -> Usage {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cached_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    Usage {
        input_tokens: input_tokens.saturating_sub(cached_tokens),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cache_read_tokens: cached_tokens,
        cache_creation_tokens: 0,
    }
}

fn stop_reason_from_status(value: &Value, has_tool_use: bool) -> StopReason {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match status {
        "incomplete" => {
            let reason = value
                .get("incomplete_details")
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if reason == "max_output_tokens" {
                StopReason::MaxTokens
            } else {
                StopReason::Other(format!("incomplete:{reason}"))
            }
        }
        // completed / failed 之外的未知状态以内容为准
        _ => {
            if has_tool_use {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
    }
}

/// 流式增量里的单个 output item 累积状态。
#[derive(Debug, Default, Clone)]
enum StreamItem {
    #[default]
    Unknown,
    Message {
        text: String,
    },
    Reasoning {
        summary: String,
        encrypted: Option<String>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        started: bool,
    },
}

/// 把 Responses 的 SSE 事件流还原为中立的 ModelResponse。
#[derive(Debug, Default)]
pub struct ResponsesStreamAccumulator {
    items: Vec<StreamItem>,
    usage: Option<Usage>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    failure: Option<String>,
}

impl ResponsesStreamAccumulator {
    pub fn apply(&mut self, event: &Value, sink: Option<&StreamSink>) -> Result<()> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        match kind {
            "response.output_item.added" => {
                if let Some(item) = event.get("item") {
                    self.items.resize_with(index + 1, StreamItem::default);
                    self.items[index] = item_from_payload(item, sink);
                }
            }
            "response.output_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    let slot = self.slot(index);
                    if let StreamItem::Message { text } = slot {
                        text.push_str(delta);
                    }
                    emit(
                        sink,
                        StreamEvent::TextDelta {
                            text: delta.to_string(),
                        },
                    );
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    let slot = self.slot(index);
                    if let StreamItem::Reasoning { summary, .. } = slot {
                        summary.push_str(delta);
                    }
                    emit(
                        sink,
                        StreamEvent::ReasoningDelta {
                            text: delta.to_string(),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let slot = self.slot(index);
                if let StreamItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    started,
                } = slot
                {
                    arguments.push_str(delta);
                    let id = if call_id.is_empty() {
                        name.clone()
                    } else {
                        call_id.clone()
                    };
                    if !delta.is_empty() {
                        emit(
                            sink,
                            StreamEvent::ToolUseDelta {
                                id,
                                partial_json: delta.to_string(),
                            },
                        );
                    }
                    let _ = started;
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    self.items.resize_with(index + 1, StreamItem::default);
                    let previous = std::mem::take(&mut self.items[index]);
                    self.items[index] = merge_item(previous, item_from_payload(item, sink));
                }
            }
            "response.completed" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                if let Some(usage) = response.get("usage").filter(|value| !value.is_null()) {
                    let usage = parse_responses_usage(usage);
                    self.usage = Some(usage);
                    emit(sink, StreamEvent::Usage { usage });
                }
                if let Some(status) = response.get("status").and_then(Value::as_str) {
                    self.status = Some(status.to_string());
                }
            }
            "response.failed" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                self.failure = Some(
                    response
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| "response.failed".to_string()),
                );
            }
            "response.incomplete" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                self.status = Some("incomplete".to_string());
                self.incomplete_reason = response
                    .get("incomplete_details")
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "error" => {
                self.failure = Some(
                    event
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| "stream error".to_string()),
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn slot(&mut self, index: usize) -> &mut StreamItem {
        self.items.resize_with(index + 1, StreamItem::default);
        &mut self.items[index]
    }

    pub fn finish(self, sink: Option<&StreamSink>) -> Result<ModelResponse> {
        if let Some(message) = self.failure {
            bail!("responses stream failed: {message}");
        }
        let mut blocks = Vec::new();
        let mut has_tool_use = false;
        for item in &self.items {
            match item {
                StreamItem::Message { text } => {
                    if !text.trim().is_empty() {
                        blocks.push(ContentBlock::text(text.clone()));
                    }
                }
                StreamItem::Reasoning { summary, encrypted } => {
                    if !summary.trim().is_empty() || encrypted.is_some() {
                        blocks.push(ContentBlock::thinking(summary.clone(), encrypted.clone()));
                    }
                }
                StreamItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    started,
                } => {
                    if name.is_empty() && call_id.is_empty() {
                        continue;
                    }
                    has_tool_use = true;
                    let id = if call_id.is_empty() {
                        name.clone()
                    } else {
                        call_id.clone()
                    };
                    if !started {
                        emit(
                            sink,
                            StreamEvent::ToolUseStart {
                                id: id.clone(),
                                name: name.clone(),
                            },
                        );
                    }
                    emit(sink, StreamEvent::ToolUseStop { id: id.clone() });
                    let input = serde_json::from_str(arguments)
                        .unwrap_or_else(|_| json!({ "_invalid_arguments": arguments }));
                    blocks.push(ContentBlock::tool_use(id, name.clone(), input));
                }
                StreamItem::Unknown => {}
            }
        }
        let stop_reason = match self.status.as_deref() {
            Some("incomplete") => {
                if self.incomplete_reason.as_deref() == Some("max_output_tokens") {
                    StopReason::MaxTokens
                } else {
                    StopReason::Other(format!(
                        "incomplete:{}",
                        self.incomplete_reason.unwrap_or_default()
                    ))
                }
            }
            Some("failed") => StopReason::Other("failed".to_string()),
            _ if has_tool_use => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };
        emit(
            sink,
            StreamEvent::Stop {
                stop_reason: stop_reason.clone(),
            },
        );
        Ok(ModelResponse {
            blocks,
            stop_reason,
            usage: self.usage.unwrap_or_default(),
        })
    }
}

/// 把 output item 负载转成累积状态，并在开始时补发 ToolUseStart。
fn item_from_payload(item: &Value, sink: Option<&StreamSink>) -> StreamItem {
    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "message" => {
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            StreamItem::Message { text }
        }
        "reasoning" => {
            let summary = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            StreamItem::Reasoning {
                summary,
                encrypted: item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let started = !call_id.is_empty() || !name.is_empty();
            if started {
                emit(
                    sink,
                    StreamEvent::ToolUseStart {
                        id: if call_id.is_empty() {
                            name.clone()
                        } else {
                            call_id.clone()
                        },
                        name: name.clone(),
                    },
                );
            }
            StreamItem::FunctionCall {
                call_id,
                name,
                arguments,
                started,
            }
        }
        _ => StreamItem::Unknown,
    }
}

/// 合并 added 与 done 两次负载：done 携带完整参数与加密推理内容。
fn merge_item(previous: StreamItem, incoming: StreamItem) -> StreamItem {
    match (previous, incoming) {
        (
            StreamItem::FunctionCall {
                call_id,
                name,
                arguments,
                started,
            },
            StreamItem::FunctionCall {
                call_id: next_id,
                name: next_name,
                arguments: next_arguments,
                ..
            },
        ) => {
            let chosen_arguments = if next_arguments.is_empty() {
                arguments
            } else {
                next_arguments
            };
            StreamItem::FunctionCall {
                call_id: if next_id.is_empty() { call_id } else { next_id },
                name: if next_name.is_empty() {
                    name
                } else {
                    next_name
                },
                arguments: chosen_arguments,
                started,
            }
        }
        (StreamItem::Message { text }, StreamItem::Message { text: next }) => StreamItem::Message {
            text: if next.is_empty() { text } else { next },
        },
        (
            StreamItem::Reasoning { summary, encrypted },
            StreamItem::Reasoning {
                summary: next_summary,
                encrypted: next_encrypted,
            },
        ) => StreamItem::Reasoning {
            summary: if next_summary.is_empty() {
                summary
            } else {
                next_summary
            },
            encrypted: next_encrypted.or(encrypted),
        },
        (_, incoming) => incoming,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolDefinition;

    fn request() -> ModelRequest {
        ModelRequest {
            model: "gpt-5.4".to_string(),
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

    fn drain() -> (
        StreamSink,
        tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    #[test]
    fn request_separates_instructions_and_sets_codex_parity_fields() {
        let body = build_responses_request(&request());
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["instructions"], "you are helpful");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        // 没有推理档位时不发 reasoning 字段。
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn request_includes_reasoning_cache_key_and_tools() {
        let body = build_responses_request(&ModelRequest {
            reasoning_effort: Some("high".to_string()),
            prompt_cache_key: Some("session-42".to_string()),
            tools: vec![bash_tool()],
            ..request()
        });
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["prompt_cache_key"], "session-42");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        // Responses 的工具就是扁平定义，没有 function 嵌套。
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "Bash");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn empty_system_and_blank_key_are_omitted() {
        let body = build_responses_request(&ModelRequest {
            system: "  ".to_string(),
            prompt_cache_key: Some("   ".to_string()),
            ..request()
        });
        assert!(body.get("instructions").is_none());
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn round_trips_tool_calls_results_and_encrypted_reasoning() {
        let body = build_responses_request(&ModelRequest {
            messages: vec![
                ChatMessage::assistant_blocks(vec![
                    ContentBlock::thinking("let me check", Some("enc-1".to_string())),
                    ContentBlock::text("running"),
                    ContentBlock::tool_use("call_1", "Bash", json!({"command": "ls"})),
                ]),
                ChatMessage::user_blocks(vec![ContentBlock::tool_result(
                    "call_1", "file.txt", false,
                )]),
            ],
            ..request()
        });
        let items = body["input"].as_array().unwrap();
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[0]["encrypted_content"], "enc-1");
        assert_eq!(items[0]["summary"][0]["text"], "let me check");
        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[2]["arguments"], json!({"command": "ls"}).to_string());
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "call_1");
        assert_eq!(items[3]["output"], "file.txt");
    }

    #[test]
    fn reasoning_without_signature_is_not_resent() {
        let body = build_responses_request(&ModelRequest {
            messages: vec![ChatMessage::assistant_blocks(vec![ContentBlock::thinking(
                "unsigned", None,
            )])],
            ..request()
        });
        assert!(body["input"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parses_text_tool_call_reasoning_and_usage() {
        let value = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}], "encrypted_content": "enc"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
                {"type": "function_call", "call_id": "call_9", "name": "Bash", "arguments": "{\"command\":\"ls\"}"}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 20, "input_tokens_details": {"cached_tokens": 60}}
        });
        let response = parse_responses_response(&value).unwrap();
        assert_eq!(response.text(), "done");
        assert_eq!(response.blocks[0].as_thinking(), Some("thinking"));
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage.input_tokens, 40);
        assert_eq!(response.usage.cache_read_tokens, 60);
        assert_eq!(response.usage.output_tokens, 20);
        let tool_uses: Vec<_> = response.tool_uses().collect();
        assert_eq!(tool_uses[0].0, "call_9");
        assert_eq!(tool_uses[0].2, &json!({"command": "ls"}));
    }

    #[test]
    fn parses_incomplete_and_error_responses() {
        let truncated = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial"}]}]
        });
        assert_eq!(
            parse_responses_response(&truncated).unwrap().stop_reason,
            StopReason::MaxTokens
        );

        let failed = json!({"error": {"message": "quota exceeded"}});
        let error = parse_responses_response(&failed).unwrap_err();
        assert!(error.to_string().contains("quota exceeded"));

        assert!(parse_responses_response(&json!({})).is_err());
    }

    #[test]
    fn accumulator_rebuilds_streamed_text_and_tool_call() {
        let (sink, mut receiver) = drain();
        let mut accumulator = ResponsesStreamAccumulator::default();
        for event in [
            json!({"type": "response.created"}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"command\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "\"ls\"}"}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": "{\"command\":\"ls\"}"}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "content": []}}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "检查"}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "完成"}),
            json!({"type": "response.completed", "response": {"status": "completed",
                   "usage": {"input_tokens": 10, "output_tokens": 5}}}),
        ] {
            accumulator.apply(&event, Some(&sink)).unwrap();
        }
        let response = accumulator.finish(Some(&sink)).unwrap();
        assert_eq!(response.text(), "检查完成");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage.input_tokens, 10);
        let tool_uses: Vec<_> = response.tool_uses().collect();
        assert_eq!(tool_uses[0].2, &json!({"command": "ls"}));

        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { text } if text == "检查")));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolUseStart { id, .. } if id == "call_1")));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolUseStop { id } if id == "call_1")));
    }

    #[test]
    fn accumulator_reports_failures_and_incomplete_streams() {
        let (sink, _receiver) = drain();
        let mut failed = ResponsesStreamAccumulator::default();
        failed
            .apply(
                &json!({"type": "response.failed", "response": {"error": {"message": "rate limit"}}}),
                Some(&sink),
            )
            .unwrap();
        let error = failed.finish(Some(&sink)).unwrap_err();
        assert!(error.to_string().contains("rate limit"));

        let mut truncated = ResponsesStreamAccumulator::default();
        truncated
            .apply(&json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}}}), Some(&sink))
            .unwrap();
        assert_eq!(
            truncated.finish(Some(&sink)).unwrap().stop_reason,
            StopReason::MaxTokens
        );
    }

    #[test]
    fn accumulator_keeps_encrypted_reasoning_from_done_item() {
        let (sink, _receiver) = drain();
        let mut accumulator = ResponsesStreamAccumulator::default();
        accumulator
            .apply(
                &json!({"type": "response.output_item.added", "output_index": 0,
                        "item": {"type": "reasoning", "summary": []}}),
                Some(&sink),
            )
            .unwrap();
        accumulator
            .apply(
                &json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "先看文件"}),
                Some(&sink),
            )
            .unwrap();
        accumulator
            .apply(
                &json!({"type": "response.output_item.done", "output_index": 0,
                        "item": {"type": "reasoning", "summary": [{"type": "summary_text", "text": "先看文件"}],
                                 "encrypted_content": "enc-9"}}),
                Some(&sink),
            )
            .unwrap();
        let response = accumulator.finish(Some(&sink)).unwrap();
        match &response.blocks[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "先看文件");
                assert_eq!(signature.as_deref(), Some("enc-9"));
            }
            other => panic!("expected thinking block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provider_reports_its_default_model() {
        let model = ResponsesModel::new("http://127.0.0.1:1/v1", None, "gpt-5.4", "responses");
        assert_eq!(model.default_model().as_deref(), Some("gpt-5.4"));
        assert_eq!(model.name(), "responses");
        assert_eq!(model.endpoint(), "http://127.0.0.1:1/v1/responses");
    }
}
