use std::{fmt, time::Duration};

use async_trait::async_trait;
use clap::ValueEnum;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{self, MissedTickBehavior};

use crate::config::Config;
use crate::models::{RequestOptions, ResolvedModelChoice, ResolvedModelRef};

#[derive(Debug, Clone)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        content: Option<String>,
        /// Opaque provider reasoning. This is persisted and replayed verbatim
        /// only for models that require it (for example DeepSeek thinking mode).
        reasoning_content: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
        /// Exact protocol-native continuation state. Request assembly filters
        /// this by current replay key and API before the adapter serializes it.
        native_replay: Option<NativeReplay>,
    },
    Tool {
        content: String,
        attachments: Vec<ToolAttachment>,
        tool_call_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeReplay {
    #[serde(default)]
    pub provider_id: String,
    pub endpoint: String,
    pub model: String,
    pub payload: NativeReplayPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ReplayOrigin {
    pub api: String,
    pub provider_id: String,
    pub endpoint: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "api", content = "data", rename_all = "snake_case")]
pub enum NativeReplayPayload {
    ChatReasoning(String),
    ResponsesOutput(Vec<Value>),
    AnthropicContent(Vec<Value>),
}

impl NativeReplay {
    fn api_name(&self) -> &'static str {
        match &self.payload {
            NativeReplayPayload::ChatReasoning(_) => "chat_completions",
            NativeReplayPayload::ResponsesOutput(_) => "responses",
            NativeReplayPayload::AnthropicContent(_) => "anthropic_messages",
        }
    }

    fn origin(&self) -> ReplayOrigin {
        ReplayOrigin {
            api: self.api_name().to_string(),
            provider_id: self.provider_id.clone(),
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
        }
    }
}

fn filter_native_replay(
    messages: &[Message],
    mut keep: impl FnMut(&NativeReplay) -> bool,
) -> Vec<Message> {
    let mut messages = messages.to_vec();
    for message in &mut messages {
        if let Message::Assistant { native_replay, .. } = message
            && native_replay.as_ref().is_some_and(|native| !keep(native))
        {
            *native_replay = None;
        }
    }
    messages
}

pub fn filter_native_replay_for_config(
    messages: &[Message],
    config: &Config,
    target: &ResolvedModelRef,
    target_api: &str,
) -> Vec<Message> {
    let target_key = config.replay_key(&target.provider_id, &target.model_id);
    filter_native_replay(messages, |native| {
        native.api_name() == target_api
            && config.replay_key(&native.provider_id, &native.model) == target_key
    })
}

pub fn filter_native_replay_for_origins(
    messages: &[Message],
    target_api: &str,
    origins: &[ReplayOrigin],
) -> Vec<Message> {
    filter_native_replay(messages, |native| {
        native.api_name() == target_api && origins.contains(&native.origin())
    })
}

pub fn filter_native_replay_for_legacy_origin(
    messages: &[Message],
    target_api: &str,
    provider_id: &str,
    endpoint: &str,
    model: &str,
) -> Vec<Message> {
    filter_native_replay(messages, |native| {
        native.api_name() == target_api
            && native.provider_id == provider_id
            && native.endpoint == endpoint
            && native.model == model
    })
}

pub fn native_replay_origins(messages: &[Message]) -> Vec<ReplayOrigin> {
    let mut origins = Vec::new();
    for message in messages {
        if let Message::Assistant {
            native_replay: Some(native),
            ..
        } = message
        {
            let origin = native.origin();
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
    }
    origins
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelApi {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

/// Bound the connect phase so a dead host fails fast instead of hanging the turn.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum gap between stream bytes. This is an inter-chunk idle bound, not a
/// total-turn bound; models may legitimately reason for a long time.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_mins(10);

pub struct HttpProvider {
    client: Client,
    pub(crate) endpoint: String,
    api: ModelApi,
    api_key: Option<String>,
    pub(crate) idle_timeout: Duration,
}

pub(crate) enum SseEvent {
    Data(String),
    Tick,
}

impl HttpProvider {
    pub fn new(endpoint: String, api_key: Option<String>) -> anyhow::Result<Self> {
        let mut url = reqwest::Url::parse(&endpoint)?;
        let path = url.path().trim_end_matches('/').to_string();
        url.set_path(&path);
        let endpoint = url.to_string();
        let api = classify_endpoint(&endpoint)?;
        let client = Client::builder().connect_timeout(CONNECT_TIMEOUT).build()?;
        Ok(Self {
            client,
            endpoint,
            api,
            api_key,
            idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        })
    }

    pub(crate) async fn stream_sse(
        &self,
        body: &Value,
        on_sse: &mut dyn FnMut(SseEvent) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        let response = self
            .post_request(body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(classify_http_error(status, body));
        }

        let mut response = response;
        let mut buffer = String::new();
        let mut byte_buffer = Vec::new();
        let mut last_activity = std::time::Instant::now();
        let mut tick = time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => {
                    last_activity = std::time::Instant::now();
                    chunk.map_err(|error| ProviderError::Transport(error.to_string()))?
                }
                _ = tick.tick() => {
                    if last_activity.elapsed() > self.idle_timeout {
                        return Err(ProviderError::Transport(format!(
                            "stream idle for over {}s", self.idle_timeout.as_secs()
                        )));
                    }
                    on_sse(SseEvent::Tick)?;
                    continue;
                }
            };
            let Some(chunk) = chunk else { break };
            byte_buffer.extend_from_slice(&chunk);
            let valid_up_to = match std::str::from_utf8(&byte_buffer) {
                Ok(text) => {
                    buffer.push_str(text);
                    byte_buffer.len()
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // SAFETY: `valid_up_to` is guaranteed to end on a UTF-8 boundary.
                    buffer
                        .push_str(unsafe { std::str::from_utf8_unchecked(&byte_buffer[..valid]) });
                    valid
                }
            };
            byte_buffer.drain(..valid_up_to);
            consume_sse_events(&mut buffer, on_sse)?;
        }

        if !byte_buffer.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&byte_buffer));
        }
        if !buffer.trim().is_empty() {
            buffer.push_str("\n\n");
            consume_sse_events(&mut buffer, on_sse)?;
        }
        Ok(())
    }

    fn post_request(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut request = self.client.post(&self.endpoint).json(body);
        if let Some(key) = &self.api_key {
            request = match self.api {
                ModelApi::AnthropicMessages => request.header("x-api-key", key),
                ModelApi::ChatCompletions | ModelApi::Responses => request.bearer_auth(key),
            };
        }
        if self.api == ModelApi::AnthropicMessages {
            request = request.header("anthropic-version", "2023-06-01");
        }
        request
    }
}

fn consume_sse_events(
    buffer: &mut String,
    on_sse: &mut dyn FnMut(SseEvent) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    while let Some((position, separator_length)) = next_event_boundary(buffer) {
        let event = buffer[..position].to_string();
        buffer.replace_range(..position + separator_length, "");
        let data = event
            .lines()
            .filter_map(|line| line.trim().strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if !data.is_empty() {
            on_sse(SseEvent::Data(data))?;
        }
    }
    Ok(())
}

pub fn classify_endpoint(endpoint: &str) -> anyhow::Result<ModelApi> {
    let parsed = reqwest::Url::parse(endpoint)
        .map_err(|error| anyhow::anyhow!("invalid provider endpoint `{endpoint}`: {error}"))?;
    let path = parsed.path().trim_end_matches('/');
    if path.ends_with("/chat/completions") {
        Ok(ModelApi::ChatCompletions)
    } else if path.ends_with("/responses") {
        Ok(ModelApi::Responses)
    } else if path.ends_with("/messages") {
        Ok(ModelApi::AnthropicMessages)
    } else {
        anyhow::bail!(
            "unsupported provider endpoint `{endpoint}`; path must end in `/chat/completions`, `/responses`, or `/messages`"
        )
    }
}

#[derive(Debug, Clone)]
pub enum UserContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl UserContent {
    pub fn text(&self) -> String {
        match self {
            UserContent::Text(text) => text.clone(),
            UserContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text, .. } => Some(text.as_str()),
                    ContentPart::Attachment { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl From<String> for UserContent {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for UserContent {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

#[derive(Debug, Clone)]
pub enum ContentPart {
    Text { text: String },
    Attachment { attachment: Attachment },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub media_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lower")]
pub enum ImageDetail {
    #[default]
    Auto,
    Low,
    High,
    Original,
}

impl std::fmt::Display for ImageDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::High => "high",
            Self::Original => "original",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAttachment {
    pub attachment: Attachment,
    pub detail: ImageDetail,
    pub(crate) object_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// `None` means the provider did not report cache-write usage.
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    pub fn visible_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cache_read_input_tokens)
            .saturating_sub(self.cache_write_input_tokens.unwrap_or(0))
    }

    pub fn visible_output_tokens(&self) -> u64 {
        self.output_tokens
    }
}

#[derive(Debug, Clone)]
pub struct StreamResult {
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
    pub native_response: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningStart(ReasoningVisibility),
    ReasoningDelta(String),
    ReasoningSummaryDelta { part_index: usize, text: String },
    ReasoningEnd,
    ToolCallDelta(ToolCallDelta),
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningVisibility {
    StreamedTrace,
    Opaque,
}

#[derive(Debug, Clone, Default)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Other(String),
}

#[derive(Debug)]
pub enum ProviderError {
    ContextLength,
    RateLimit { message: String },
    Unavailable(String),
    HttpStatus { status: u16, body: String },
    Transport(String),
    SseParse(String),
    Other(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContextLength => formatter.write_str("context length exceeded"),
            Self::RateLimit { message } => write!(formatter, "HTTP 429: {message}"),
            Self::Unavailable(message) => formatter.write_str(message),
            Self::HttpStatus { status, body } => write!(formatter, "HTTP {status}: {body}"),
            Self::Transport(message) => write!(formatter, "transport error: {message}"),
            Self::SseParse(message) => write!(formatter, "SSE parse: {message}"),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::ContextLength => "context_length",
            Self::RateLimit { .. } => "rate_limit",
            Self::Unavailable(_) => "unavailable",
            Self::HttpStatus { .. } => "http",
            Self::Transport(_) => "transport",
            Self::SseParse(_) => "protocol",
            Self::Other(_) => "provider",
        }
    }

    pub fn retryable_for_live_turn(&self) -> bool {
        match self {
            ProviderError::RateLimit { .. } => true,
            ProviderError::HttpStatus { status, .. } => *status >= 500,
            ProviderError::Transport(_) => true,
            ProviderError::ContextLength
            | ProviderError::Unavailable(_)
            | ProviderError::SseParse(_)
            | ProviderError::Other(_) => false,
        }
    }

    pub fn fallback_immediately(&self) -> bool {
        match self {
            ProviderError::Unavailable(_) => true,
            ProviderError::HttpStatus {
                status: 401 | 404, ..
            } => true,
            ProviderError::HttpStatus { status: 403, body } => {
                let body = body.to_ascii_lowercase();
                [
                    "authentication",
                    "authorization",
                    "api key",
                    "access denied",
                    "permission",
                    "model access",
                    "model_not_found",
                ]
                .iter()
                .any(|marker| body.contains(marker))
            }
            ProviderError::ContextLength
            | ProviderError::RateLimit { .. }
            | ProviderError::HttpStatus { .. }
            | ProviderError::Transport(_)
            | ProviderError::SseParse(_)
            | ProviderError::Other(_) => false,
        }
    }
}

#[async_trait(?Send)]
pub trait Provider: Send + Sync {
    fn request_format(&self) -> &'static str {
        "test.v1"
    }

    fn endpoint(&self) -> &str {
        ""
    }

    fn api_name(&self) -> &'static str {
        "test"
    }

    fn native_request(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<Value, ProviderError> {
        Ok(serde_json::json!({
            "model": request.model.model_id,
            "message_count": messages.len(),
            "tools": tools,
        }))
    }

    async fn stream_chat(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError>;
}

#[async_trait(?Send)]
impl Provider for HttpProvider {
    fn request_format(&self) -> &'static str {
        match self.api {
            ModelApi::ChatCompletions => "openai.chat_completions.v1",
            ModelApi::Responses => "openai.responses.v1",
            ModelApi::AnthropicMessages => "anthropic.messages.v1",
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn api_name(&self) -> &'static str {
        match self.api {
            ModelApi::ChatCompletions => "chat_completions",
            ModelApi::Responses => "responses",
            ModelApi::AnthropicMessages => "anthropic_messages",
        }
    }

    fn native_request(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<Value, ProviderError> {
        match self.api {
            ModelApi::ChatCompletions => Ok(crate::chat_completions::build_chat_request_body(
                request,
                &self.endpoint,
                messages,
                tools,
            )),
            ModelApi::Responses => crate::responses::build_responses_request_body(
                request,
                &self.endpoint,
                messages,
                tools,
            ),
            ModelApi::AnthropicMessages => {
                crate::anthropic::build_request_body(request, &self.endpoint, messages, tools)
            }
        }
    }

    async fn stream_chat(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError> {
        match self.api {
            ModelApi::ChatCompletions => {
                crate::chat_completions::stream(self, request, messages, tools, on_event).await
            }
            ModelApi::Responses => {
                crate::responses::stream(self, request, messages, tools, on_event).await
            }
            ModelApi::AnthropicMessages => {
                crate::anthropic::stream(self, request, messages, tools, on_event).await
            }
        }
    }
}

pub fn approx_tokens(s: &str) -> u64 {
    (s.len() as u64).div_ceil(4)
}

pub fn build_provider(config: &Config, provider_id: &str) -> anyhow::Result<Box<dyn Provider>> {
    let provider = config.provider(provider_id)?;
    let api_key = config.api_key_for_provider(provider_id)?;
    Ok(Box::new(HttpProvider::new(
        provider.endpoint.clone(),
        api_key,
    )?))
}

pub fn advance_provider(
    config: &Config,
    model: &mut ResolvedModelChoice,
    provider: &mut Box<dyn Provider>,
) -> anyhow::Result<Option<(String, String)>> {
    let previous = model.active_model().provider_id.clone();
    if !model.advance() {
        return Ok(None);
    }
    let next = model.active_model().provider_id.clone();
    *provider = build_provider(config, &next)?;
    Ok(Some((previous, next)))
}

pub(crate) fn classify_http_error(status: u16, body: String) -> ProviderError {
    if is_context_length_error(status, &body) {
        ProviderError::ContextLength
    } else if status == 429 {
        ProviderError::RateLimit { message: body }
    } else {
        ProviderError::HttpStatus { status, body }
    }
}

pub(crate) fn stream_error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

pub(crate) fn is_context_length_error(status: u16, body: &str) -> bool {
    if status == 413 {
        return true;
    }
    let is_client_error = (400..500).contains(&status);
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let code = value["error"]["code"]
            .as_str()
            .or_else(|| value["error"]["type"].as_str())
            .unwrap_or("");
        if code.eq_ignore_ascii_case("context_length_exceeded")
            || code.eq_ignore_ascii_case("string_above_max_length")
        {
            return true;
        }
    }
    if !is_client_error {
        return false;
    }
    const PATTERNS: &[&str] = &[
        "context_length_exceeded",
        "context length",
        "maximum context length",
        "context window",
        "exceeds the context",
        "exceed the context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "maximum number of tokens",
        "reduce the length",
        "reduce the amount",
    ];
    let lower = body.to_ascii_lowercase();
    PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn next_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n");
    let crlf = buffer.find("\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) if a <= b => Some((a, 2)),
        (Some(_), Some(b)) => Some((b, 4)),
        (Some(a), None) => Some((a, 2)),
        (None, Some(b)) => Some((b, 4)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        CompactionConfig, GuardrailConfig, LimitsConfig, ModelConfig, OrderedMap, ProviderConfig,
        RedactionConfig, TerminalBellConfig,
    };
    use crate::models::ResolvedModelRef;

    fn replay_config(source_key: Option<&str>, target_key: Option<&str>) -> Config {
        Config {
            providers: OrderedMap::from_iter([
                (
                    "source".into(),
                    ProviderConfig {
                        endpoint: "https://source.test/v1/chat/completions".into(),
                        api_key_env: String::new(),
                        models: OrderedMap::from_iter([(
                            "source-model".into(),
                            ModelConfig {
                                context_window: None,
                                supported_efforts: None,
                                replay_key: source_key.map(str::to_string),
                            },
                        )]),
                    },
                ),
                (
                    "target".into(),
                    ProviderConfig {
                        endpoint: "https://target.test/v1/chat/completions".into(),
                        api_key_env: String::new(),
                        models: OrderedMap::from_iter([(
                            "target-model".into(),
                            ModelConfig {
                                context_window: None,
                                supported_efforts: None,
                                replay_key: target_key.map(str::to_string),
                            },
                        )]),
                    },
                ),
            ]),
            output: Default::default(),
            line_wrapping: true,
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    fn replay_message(payload: NativeReplayPayload) -> Message {
        Message::Assistant {
            content: Some("semantic".into()),
            reasoning_content: None,
            tool_calls: None,
            native_replay: Some(NativeReplay {
                provider_id: "source".into(),
                endpoint: "https://source.test/v1/chat/completions".into(),
                model: "source-model".into(),
                payload,
            }),
        }
    }

    fn target_model() -> ResolvedModelRef {
        ResolvedModelRef {
            canonical: "target/target-model".into(),
            provider_id: "target".into(),
            model_id: "target-model".into(),
            effort: None,
        }
    }

    #[test]
    fn classifies_only_supported_endpoint_paths() {
        assert_eq!(
            classify_endpoint("https://gateway.test/v1/chat/completions?route=a").unwrap(),
            ModelApi::ChatCompletions
        );
        assert_eq!(
            classify_endpoint("https://gateway.test/custom/responses/").unwrap(),
            ModelApi::Responses
        );
        assert_eq!(
            classify_endpoint("https://gateway.test/v1/messages?route=a").unwrap(),
            ModelApi::AnthropicMessages
        );
        for endpoint in [
            "https://gateway.test/v1",
            "https://gateway.test/v1/Responses",
            "https://gateway.test/v1/message",
            "https://gateway.test/v1/chat/completions/extra",
        ] {
            assert!(classify_endpoint(endpoint).is_err(), "accepted {endpoint}");
        }
    }

    #[test]
    fn anthropic_requests_use_native_headers_without_bearer_auth() {
        let provider = HttpProvider::new(
            "https://api.anthropic.test/v1/messages".into(),
            Some("test-key".into()),
        )
        .unwrap();
        let request = provider
            .post_request(&serde_json::json!({ "model": "claude-test" }))
            .build()
            .unwrap();

        assert_eq!(request.headers()["x-api-key"], "test-key");
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert!(request.headers().get("authorization").is_none());
    }

    #[test]
    fn fallback_policy_distinguishes_availability_from_request_errors() {
        assert!(
            ProviderError::HttpStatus {
                status: 401,
                body: "unauthorized".into(),
            }
            .fallback_immediately()
        );
        assert!(
            ProviderError::HttpStatus {
                status: 403,
                body: r#"{"error":{"code":"model_not_found"}}"#.into(),
            }
            .fallback_immediately()
        );
        assert!(
            !ProviderError::HttpStatus {
                status: 403,
                body: "content policy rejected the request".into(),
            }
            .fallback_immediately()
        );
        assert!(
            !ProviderError::HttpStatus {
                status: 400,
                body: "unsupported reasoning effort".into(),
            }
            .fallback_immediately()
        );
        assert!(ProviderError::Transport("stream ended".into()).retryable_for_live_turn());
        assert!(!ProviderError::SseParse("bad JSON".into()).retryable_for_live_turn());
    }

    #[test]
    fn current_replay_keys_reinterpret_existing_history_within_one_api() {
        let messages = vec![replay_message(NativeReplayPayload::ChatReasoning(
            "trace".into(),
        ))];
        let target = target_model();

        let separate = replay_config(None, None);
        let filtered =
            filter_native_replay_for_config(&messages, &separate, &target, "chat_completions");
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: None,
                ..
            }
        ));

        let shared = replay_config(Some("compatible"), Some("compatible"));
        let filtered =
            filter_native_replay_for_config(&messages, &shared, &target, "chat_completions");
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: Some(_),
                ..
            }
        ));

        let changed = replay_config(Some("compatible"), Some("replacement"));
        let filtered =
            filter_native_replay_for_config(&messages, &changed, &target, "chat_completions");
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: None,
                ..
            }
        ));
    }

    #[test]
    fn replay_key_never_crosses_api_variants() {
        let config = replay_config(Some("compatible"), Some("compatible"));
        let messages = vec![replay_message(NativeReplayPayload::ResponsesOutput(vec![
            serde_json::json!({"type":"reasoning","encrypted_content":"opaque"}),
        ]))];
        let filtered = filter_native_replay_for_config(
            &messages,
            &config,
            &target_model(),
            "chat_completions",
        );
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: None,
                ..
            }
        ));
    }
}
