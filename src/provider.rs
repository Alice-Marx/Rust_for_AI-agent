use std::ops::AddAssign;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

/// 订阅模式下托管的 CLIProxyAPI 进程（进程级单例，Drop 即停止）。
static SUBSCRIPTION_MANAGER: OnceLock<crate::subscription::SubscriptionManager> = OnceLock::new();
/// 托管 sidecar 的就绪端点，供管理 API（登录、账号、模型）复用。
static SUBSCRIPTION_ENDPOINT: OnceLock<crate::subscription::SubscriptionEndpoint> = OnceLock::new();

/// 当前进程托管的订阅端点（未启用订阅模式时为 None）。
pub fn active_subscription_endpoint() -> Option<&'static crate::subscription::SubscriptionEndpoint>
{
    SUBSCRIPTION_ENDPOINT.get()
}

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
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
    /// 模型的推理/思考内容（Anthropic thinking 块、DeepSeek `reasoning_content`）。
    /// `signature` 仅 Anthropic 使用：回传历史时必须原样带回，否则请求会被拒绝。
    ProviderReasoning {
        provider: String,
        summary: String,
        payload: Value,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
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

    pub fn thinking(thinking: impl Into<String>, signature: Option<String>) -> Self {
        Self::Thinking {
            thinking: thinking.into(),
            signature,
        }
    }

    pub fn as_thinking(&self) -> Option<&str> {
        match self {
            Self::Thinking { thinking, .. } => Some(thinking),
            Self::ProviderReasoning { summary, .. } => Some(summary),
            _ => None,
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

    /// 拼接消息中所有 Thinking 块。
    pub fn reasoning(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_thinking)
            .collect::<Vec<_>>()
            .join(
                "
",
            )
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

/// 流式增量事件。
///
/// provider 层产生前五类（文本/思维链/工具调用增量）与 Usage / Stop；
/// agent 层补充 TurnStart / ToolCall / ToolResult / Completed / Failed，
/// 因此同一个通道既可用于打字机展示，也可用于工具执行进度展示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    PermissionRequest {
        id: String,
        tool: String,
        input: Value,
        reason: String,
    },
    /// 助手文本增量。
    TextDelta { text: String },
    /// 思维链增量（Anthropic thinking / DeepSeek reasoning_content）。
    ReasoningDelta { text: String },
    /// 工具调用开始（拿到 id 与名字）。
    ToolUseStart { id: String, name: String },
    /// 工具调用参数 JSON 片段。
    ToolUseDelta { id: String, partial_json: String },
    /// 工具调用参数结束。
    ToolUseStop { id: String },
    /// agent 层：工具开始执行（参数已解析）。
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// agent 层：工具执行结果。
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    /// agent 层：新一轮模型调用开始。
    TurnStart { turn: usize },
    /// token 用量。
    Usage { usage: Usage },
    /// 停止原因。
    Stop { stop_reason: StopReason },
    /// agent 层：整个 run 结束。
    Completed {
        output: String,
        turns: usize,
        tool_calls: usize,
    },
    /// agent 层：run 失败。
    Failed { message: String },
}

/// 事件接收端。发送失败（接收端已关闭）视为正常，不中断模型调用。
pub type StreamSink = tokio::sync::mpsc::UnboundedSender<StreamEvent>;

/// 向 sink 发送事件，忽略接收端已关闭的情况。
pub fn emit(sink: Option<&StreamSink>, event: StreamEvent) {
    if let Some(sink) = sink {
        let _ = sink.send(event);
    }
}

/// SSE 增量解码器：把任意切片拼成完整的 data: 负载。
///
/// 只依赖 data: 行；event: / id: / retry: 与注释行被忽略，
/// 多行 data: 按规范用换行拼接。
#[derive(Debug, Default)]
pub struct SseBuffer {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    skip_lf: bool,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffer bytes until a whole line exists: TCP chunks may split a UTF-8 codepoint.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                let line = String::from_utf8_lossy(&self.buffer);
                if line.is_empty() {
                    if !self.data_lines.is_empty() {
                        events.push(self.data_lines.join("\n"));
                        self.data_lines.clear();
                    }
                } else if let Some(rest) = line.strip_prefix("data:") {
                    // SSE strips exactly one ASCII space, preserving indentation.
                    self.data_lines
                        .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                } else if line == "data" {
                    self.data_lines.push(String::new());
                }
                self.buffer.clear();
            } else {
                self.buffer.push(byte);
            }
        }
        events
    }

    /// 送入一段文本，返回本次完整的事件负载。
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.push_bytes(chunk.as_bytes())
    }
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
    /// 推理档位（low / medium / high）。OpenAI 兼容端点映射为 reasoning_effort，
    /// Anthropic 映射为 thinking.budget_tokens；None 表示不下发推理参数。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 提示缓存键（Kimi CLI / Codex 都用会话 id 作为 key）。
    /// OpenAI 兼容端点映射为 prompt_cache_key，Responses API 同样。
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
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

    /// provider 自己的默认模型名，供 AgentRuntime 解析模型能力档案。
    fn default_model(&self) -> Option<String> {
        None
    }

    /// 该 provider 实际可用的 wire 协议名（单协议 provider 返回自身名字）。
    fn available_protocols(&self) -> Vec<&'static str> {
        vec![self.name()]
    }

    async fn list_models(&self) -> Result<Vec<crate::cliproxy::CliProxyModel>> {
        Ok(self
            .default_model()
            .into_iter()
            .map(|id| crate::cliproxy::CliProxyModel {
                id,
                object: "model".into(),
                owned_by: Some(self.name().into()),
            })
            .collect())
    }
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse>;

    /// 流式调用。默认实现退化为非流式，并把结果一次性展开为增量事件，
    /// 因此每个 provider 都能被流式前端统一消费。
    async fn complete_stream(
        &self,
        request: &ModelRequest,
        sink: Option<&StreamSink>,
    ) -> Result<ModelResponse> {
        let response = self.complete(request).await?;
        replay_response_as_events(&response, sink);
        Ok(response)
    }
}

/// 把一次非流式响应展开为增量事件（默认流式实现与离线 provider 共用）。
pub fn replay_response_as_events(response: &ModelResponse, sink: Option<&StreamSink>) {
    for block in &response.blocks {
        match block {
            ContentBlock::Text { text } => {
                emit(sink, StreamEvent::TextDelta { text: text.clone() })
            }
            ContentBlock::Thinking { thinking, .. }
            | ContentBlock::ProviderReasoning {
                summary: thinking, ..
            } => emit(
                sink,
                StreamEvent::ReasoningDelta {
                    text: thinking.clone(),
                },
            ),
            ContentBlock::ToolUse { id, name, input } => {
                emit(
                    sink,
                    StreamEvent::ToolUseStart {
                        id: id.clone(),
                        name: name.clone(),
                    },
                );
                emit(
                    sink,
                    StreamEvent::ToolUseDelta {
                        id: id.clone(),
                        partial_json: input.to_string(),
                    },
                );
                emit(sink, StreamEvent::ToolUseStop { id: id.clone() });
            }
            ContentBlock::ToolResult { .. } => {}
        }
    }
    emit(
        sink,
        StreamEvent::Usage {
            usage: response.usage,
        },
    );
    emit(
        sink,
        StreamEvent::Stop {
            stop_reason: response.stop_reason.clone(),
        },
    );
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
            client: crate::connection::model_client(),
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

    fn default_model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    async fn list_models(&self) -> Result<Vec<crate::cliproxy::CliProxyModel>> {
        let mut request = self
            .client
            .get(format!("{}/models", self.base_url))
            .timeout(std::time::Duration::from_secs(20));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?.error_for_status()?;
        let value: Value = response.json().await?;
        Ok(serde_json::from_value(value["data"].clone())?)
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
            temperature: request.temperature.or(Some(0.2)),
            ..request.clone()
        };
        let mut body = build_openai_request(&resolved);
        if let Some(object) = body.as_object_mut() {
            object.insert("stream".to_string(), json!(true));
            // include_usage 让最后一个 chunk 带上 usage（缓存命中统计依赖它）。
            object.insert(
                "stream_options".to_string(),
                json!({ "include_usage": true }),
            );
        }
        let http = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        let http = match &self.api_key {
            Some(api_key) => http.bearer_auth(api_key),
            None => http,
        };
        let response = http.send().await.context("model stream request failed")?;
        let status = response.status();
        if !status.is_success() {
            let raw = response.text().await.unwrap_or_default();
            anyhow::bail!("model returned {status}: {raw}");
        }

        let mut stream = response.bytes_stream();
        let mut decoder = SseBuffer::new();
        let mut accumulator = OpenAiStreamAccumulator::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("model stream interrupted")?;
            for payload in decoder.push_bytes(&chunk) {
                let trimmed = payload.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "[DONE]" {
                    anyhow::ensure!(
                        accumulator.stop_reason.is_some(),
                        "model stream ended before finish_reason"
                    );
                    return Ok(accumulator.finish(sink));
                }
                let value: Value = serde_json::from_str(trimmed)
                    .with_context(|| format!("invalid stream chunk: {trimmed}"))?;
                accumulator.apply(&value, sink)?;
            }
        }
        anyhow::ensure!(
            accumulator.stop_reason.is_some(),
            "model stream disconnected before finish_reason"
        );
        Ok(accumulator.finish(sink))
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
                        // user 消息里不应出现 ToolUse 与 Thinking，忽略。
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::ProviderReasoning { .. } => {}
                    }
                }
                if !text.is_empty() {
                    messages.push(json!({"role": "user", "content": text}));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut reasoning = String::new();
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
                        ContentBlock::Thinking {
                            thinking,
                            signature: None,
                        } => reasoning.push_str(thinking),
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::ProviderReasoning { .. } => {}
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
                let model = request.model.to_ascii_lowercase();
                if (model.contains("deepseek")
                    || model.contains("kimi")
                    || model.contains("moonshot"))
                    && !reasoning.is_empty()
                {
                    entry.insert("reasoning_content".to_string(), json!(reasoning));
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
    let model = request.model.to_ascii_lowercase();
    let vendor_thinking =
        model.contains("deepseek") || model.contains("kimi") || model.contains("moonshot");
    if let Some(temperature) = request.temperature.filter(|_| !vendor_thinking) {
        body.insert("temperature".to_string(), json!(temperature));
    }
    // 只有模型能力档案声明支持 reasoning 时才由 AgentRuntime 下发该字段。
    if let Some(effort) = request
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if vendor_thinking {
            body.insert("thinking".to_string(), json!({"type": if matches!(effort, "off" | "none") { "disabled" } else { "enabled" }}));
            if (model.contains("kimi-k3")
                || model.contains("kimi-k2.8")
                || model.contains("kimi-k2.7")
                || model.contains("kimi-k2.6"))
                && matches!(effort, "low" | "high" | "max")
            {
                body["thinking"]["effort"] = json!(effort);
                body["thinking"]["keep"] = json!("all");
            }
            if model.contains("deepseek") && matches!(effort, "low" | "high" | "max") {
                body.insert("reasoning_effort".into(), json!(effort));
            }
        } else {
            body.insert("reasoning_effort".to_string(), json!(effort));
        }
    }
    // Send prompt_cache_key only to model families whose wire contract supports it.
    if let Some(key) = request
        .prompt_cache_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| model.starts_with("gpt-") || model.starts_with("o3") || model.starts_with("o4"))
    {
        body.insert("prompt_cache_key".to_string(), json!(key));
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
    // DeepSeek / Kimi / Qwen 等端点把思维链放在 reasoning_content（部分端点用
    // reasoning）。工具续轮需要原样保留推理内容。
    for key in ["reasoning_content", "reasoning"] {
        if let Some(reasoning) = message
            .get(key)
            .and_then(Value::as_str)
            .filter(|reasoning| !reasoning.trim().is_empty())
        {
            blocks.push(ContentBlock::thinking(reasoning, None));
            break;
        }
    }
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

    let usage = value
        .get("usage")
        .map(parse_openai_usage)
        .unwrap_or_default();

    Ok(ModelResponse {
        blocks,
        stop_reason,
        usage,
    })
}

/// 解析 OpenAI 兼容 usage。prompt_tokens 是总量，input_tokens 需扣除缓存命中部分；
/// 部分兼容端点不上报 prompt_tokens_details，容错为 0。
pub fn parse_openai_usage(usage: &Value) -> Usage {
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

/// chat.completions 流式增量里的单个工具调用累积状态。
#[derive(Debug, Default, Clone)]
struct OpenAiToolCallAccum {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

/// 把 chat.completions 的增量 chunk 还原为中立的 ModelResponse。
#[derive(Debug, Default)]
pub struct OpenAiStreamAccumulator {
    text: String,
    reasoning: String,
    tool_calls: Vec<OpenAiToolCallAccum>,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
}

impl OpenAiStreamAccumulator {
    /// 处理一个 SSE data 负载（已解析为 JSON）。
    pub fn apply(&mut self, chunk: &Value, sink: Option<&StreamSink>) -> Result<()> {
        if let Some(error) = chunk.get("error").filter(|value| !value.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            anyhow::bail!("model stream error: {message}");
        }
        if let Some(usage) = chunk.get("usage").filter(|value| !value.is_null()) {
            let usage = parse_openai_usage(usage);
            self.usage = Some(usage);
            emit(sink, StreamEvent::Usage { usage });
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
        for key in ["reasoning_content", "reasoning"] {
            if let Some(reasoning) = delta
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.reasoning.push_str(reasoning);
                emit(
                    sink,
                    StreamEvent::ReasoningDelta {
                        text: reasoning.to_string(),
                    },
                );
                break;
            }
        }
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.text.push_str(text);
            emit(
                sink,
                StreamEvent::TextDelta {
                    text: text.to_string(),
                },
            );
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(OpenAiToolCallAccum::default());
                }
                let slot = &mut self.tool_calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot.id = id.to_string();
                }
                if let Some(name) = call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                {
                    slot.name.push_str(name);
                }
                if !slot.started && !slot.id.is_empty() && !slot.name.is_empty() {
                    slot.started = true;
                    emit(
                        sink,
                        StreamEvent::ToolUseStart {
                            id: slot.id.clone(),
                            name: slot.name.clone(),
                        },
                    );
                }
                if let Some(arguments) = call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    slot.arguments.push_str(arguments);
                    if slot.started {
                        emit(
                            sink,
                            StreamEvent::ToolUseDelta {
                                id: slot.id.clone(),
                                partial_json: arguments.to_string(),
                            },
                        );
                    }
                }
            }
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(match finish {
                "length" => StopReason::MaxTokens,
                "tool_calls" => StopReason::ToolUse,
                "stop" => StopReason::EndTurn,
                other => StopReason::Other(other.to_string()),
            });
        }
        Ok(())
    }

    /// 收尾：产出中立的 ModelResponse，并补齐 tool_use 结束事件。
    pub fn finish(self, sink: Option<&StreamSink>) -> ModelResponse {
        let mut blocks = Vec::new();
        if !self.reasoning.trim().is_empty() {
            blocks.push(ContentBlock::thinking(self.reasoning.clone(), None));
        }
        if !self.text.trim().is_empty() {
            blocks.push(ContentBlock::text(self.text.trim_end()));
        }
        let mut has_tool_use = false;
        for call in &self.tool_calls {
            if call.id.is_empty() && call.name.is_empty() {
                continue;
            }
            has_tool_use = true;
            if !call.started {
                emit(
                    sink,
                    StreamEvent::ToolUseStart {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    },
                );
                if !call.arguments.is_empty() {
                    emit(
                        sink,
                        StreamEvent::ToolUseDelta {
                            id: call.id.clone(),
                            partial_json: call.arguments.clone(),
                        },
                    );
                }
            }
            emit(
                sink,
                StreamEvent::ToolUseStop {
                    id: call.id.clone(),
                },
            );
            let input = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| json!({ "_invalid_arguments": call.arguments.clone() }));
            blocks.push(ContentBlock::tool_use(
                call.id.clone(),
                call.name.clone(),
                input,
            ));
        }
        let stop_reason = match self.stop_reason {
            Some(StopReason::EndTurn) if has_tool_use => StopReason::ToolUse,
            Some(reason) => reason,
            None if has_tool_use => StopReason::ToolUse,
            None => StopReason::EndTurn,
        };
        let usage = self.usage.unwrap_or_default();
        emit(
            sink,
            StreamEvent::Stop {
                stop_reason: stop_reason.clone(),
            },
        );
        ModelResponse {
            blocks,
            stop_reason,
            usage,
        }
    }
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
        } else if non_empty_env("ANTHROPIC_API_KEY") || non_empty_env("ANTHROPIC_AUTH_TOKEN") {
            "anthropic"
        } else {
            "offline"
        }
    } else {
        configured_provider.as_str()
    };

    // 厂商别名优先：deepseek / kimi / qwen / glm / grok / gemini / openrouter / ollama
    // 直接可用，无需手写 base URL。
    if let Some(preset) = crate::vendors::vendor_preset(provider) {
        let (base_url, api_key, model) =
            crate::vendors::resolve_vendor(preset, &|name| std::env::var(name).ok())?;
        return Ok(Arc::new(OpenAiCompatibleModel::new_with_optional_key(
            base_url,
            api_key,
            model,
            preset.names[0],
        )));
    }

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
        "anthropic" | "claude" => Ok(Arc::new(crate::anthropic::AnthropicModel::from_env()?)),
        "openai" | "openai-compatible" => {
            // AGENT_BASE_URL / AGENT_API_KEY / AGENT_MODEL 可指向任意
            // OpenAI-compatible 网关（自建代理、企业网关、其他云厂商）。
            let api_key = std::env::var("AGENT_API_KEY")
                .ok()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .context(
                    "OPENAI_API_KEY (or AGENT_API_KEY) is required when AGENT_PROVIDER=openai",
                )?;
            let base_url = std::env::var("AGENT_BASE_URL")
                .ok()
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let model = std::env::var("AGENT_MODEL")
                .ok()
                .or_else(|| std::env::var("OPENAI_MODEL").ok())
                .unwrap_or_else(|| "gpt-4o-mini".to_string());
            let chat = Arc::new(OpenAiCompatibleModel::new(
                base_url.clone(),
                api_key.clone(),
                model.clone(),
            ));
            // 显式指定 AGENT_WIRE 时按用户意图装配完整三条路径；
            // 否则只在官方端点补 Responses，避免把不支持的网关打挂。
            if configured_wire().is_some() {
                let responses = Arc::new(crate::responses::ResponsesModel::new(
                    base_url.clone(),
                    Some(api_key.clone()),
                    model.clone(),
                    "openai-responses",
                ));
                let anthropic = Arc::new(crate::anthropic::AnthropicModel::new(
                    base_url, api_key, model,
                ));
                return Ok(Arc::new(
                    crate::router::ProtocolRouter::new(chat)
                        .with_responses(responses)
                        .with_anthropic(anthropic)
                        .with_forced(configured_wire()),
                ));
            }
            if openai_supports_responses(&base_url) {
                let responses = Arc::new(crate::responses::ResponsesModel::new(
                    base_url,
                    Some(api_key),
                    model,
                    "openai-responses",
                ));
                Ok(Arc::new(
                    crate::router::ProtocolRouter::new(chat)
                        .with_responses(responses)
                        .with_forced(configured_wire()),
                ))
            } else {
                Ok(chat)
            }
        }
        "offline" | "rule-based" => Ok(Arc::new(RuleBasedModel)),
        other => {
            anyhow::bail!(
                "unsupported AGENT_PROVIDER={other}; use offline, openai, anthropic, cliproxyapi, or a vendor alias ({})",
                crate::vendors::VENDOR_PRESETS
                    .iter()
                    .map(|preset| preset.names[0])
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

/// 异步 provider 构造：订阅模式下会拉起并托管 CLIProxyAPI sidecar。
///
/// 三种模式的协议栈不同，这是效率对齐的核心：
/// - 订阅（CLIProxyAPI）：chat + responses + anthropic 三条路径都可用，按模型族路由；
/// - openai：官方端点时同时启用 chat 与 responses（GPT-5 走 Responses，等价 Codex）；
/// - 其它兼容厂商：只有 chat.completions。
pub async fn provider_from_env_async() -> Result<Arc<dyn ModelProvider>> {
    let configured = std::env::var("AGENT_PROVIDER")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let subscription_mode = configured == "subscription" || configured == "cliproxyapi";
    if subscription_mode {
        return ensure_subscription().await;
    }
    provider_from_env()
}

pub async fn ensure_subscription() -> Result<Arc<dyn ModelProvider>> {
    ensure_subscription_with_options(None, configured_wire()).await
}

pub async fn ensure_subscription_with_options(
    model: Option<&str>,
    wire: Option<crate::model_profile::WireProtocol>,
) -> Result<Arc<dyn ModelProvider>> {
    static START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = START.lock().await;
    if let Ok(base_url) = std::env::var("CLIPROXYAPI_BASE_URL") {
        if !base_url.trim().is_empty() {
            let endpoint = crate::subscription::SubscriptionEndpoint {
                management_url: std::env::var("CLIPROXYAPI_MANAGEMENT_URL").unwrap_or_else(|_| {
                    format!(
                        "{}/v0/management",
                        base_url.trim_end_matches('/').trim_end_matches("/v1")
                    )
                }),
                api_key: std::env::var("CLIPROXYAPI_API_KEY").unwrap_or_default(),
                management_key: std::env::var("CLIPROXYAPI_MANAGEMENT_KEY").unwrap_or_default(),
                base_url,
                port: 0,
            };
            let _ = SUBSCRIPTION_ENDPOINT.set(endpoint.clone());
            return Ok(subscription_provider(&endpoint, model, wire));
        }
    }
    if let Some(manager) = SUBSCRIPTION_MANAGER.get() {
        return Ok(subscription_provider(
            &manager.ensure_running().await?,
            model,
            wire,
        ));
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let data_dir = crate::subscription::default_data_dir();
    let binary = match crate::subscription::locate_binary(&exe_dir, &data_dir) {
        Some(path) => path,
        None => {
            crate::subscription::download_binary(
                crate::subscription::DEFAULT_VERSION,
                &data_dir.join("bin"),
            )
            .await?
        }
    };
    let settings =
        crate::subscription::load_or_create_settings(&data_dir.join("launcher-settings.json"))?;
    let port = crate::subscription::configured_port(&data_dir)?;
    let manager =
        crate::subscription::SubscriptionManager::new(crate::subscription::SubscriptionConfig {
            binary,
            data_dir,
            settings,
            port,
        });
    let endpoint = manager.ensure_running().await?;
    // 进程存活期内必须持有 manager，否则 sidecar 句柄会被丢弃。
    let _ = SUBSCRIPTION_MANAGER.set(manager);
    let _ = SUBSCRIPTION_ENDPOINT.set(endpoint.clone());
    Ok(subscription_provider(&endpoint, model, wire))
}

pub async fn stop_subscription() {
    if let Some(manager) = SUBSCRIPTION_MANAGER.get() {
        manager.stop().await;
    }
}

/// 订阅端点上的协议栈。
fn subscription_provider(
    endpoint: &crate::subscription::SubscriptionEndpoint,
    model: Option<&str>,
    wire: Option<crate::model_profile::WireProtocol>,
) -> Arc<dyn ModelProvider> {
    let default_model = model
        .map(str::to_owned)
        .or_else(|| std::env::var("AGENT_MODEL").ok())
        .or_else(|| std::env::var("CLIPROXYAPI_MODEL").ok())
        .unwrap_or_else(|| "gpt-5.4".to_string());
    let chat = Arc::new(OpenAiCompatibleModel::new_with_optional_key(
        endpoint.base_url.clone(),
        Some(endpoint.api_key.clone()),
        default_model.clone(),
        "subscription-chat",
    ));
    let responses = Arc::new(crate::responses::ResponsesModel::new(
        endpoint.base_url.clone(),
        Some(endpoint.api_key.clone()),
        default_model.clone(),
        "subscription-responses",
    ));
    let anthropic = Arc::new(crate::anthropic::AnthropicModel::new(
        endpoint.base_url.clone(),
        endpoint.api_key.clone(),
        default_model,
    ));
    Arc::new(
        crate::router::ProtocolRouter::new(chat)
            .with_responses(responses)
            .with_anthropic(anthropic)
            .with_forced(wire),
    )
}

/// AGENT_WIRE 的显式覆盖。
pub fn configured_wire() -> Option<crate::model_profile::WireProtocol> {
    std::env::var("AGENT_WIRE")
        .ok()
        .and_then(|value| crate::model_profile::WireProtocol::parse(&value))
}

/// 只在官方 OpenAI 端点或显式配置时才启用 Responses 路径，
/// 避免把不支持 /responses 的兼容网关打挂。
fn openai_supports_responses(base_url: &str) -> bool {
    configured_wire() == Some(crate::model_profile::WireProtocol::Responses)
        || base_url.contains("api.openai.com")
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
            reasoning_effort: None,
            prompt_cache_key: None,
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
            reasoning_effort: None,
            prompt_cache_key: None,
        };
        let response = model.complete(&request).await.unwrap();
        assert_eq!(model.name(), "offline");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage, Usage::default());
        assert!(response.text().contains("第一行目标"));
        assert!(response.tool_uses().next().is_none());
    }
}
