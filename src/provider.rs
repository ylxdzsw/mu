use std::{fmt, time::Duration};

use async_trait::async_trait;
use clap::ValueEnum;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{self, MissedTickBehavior};

use crate::config::Config;
use crate::models::RequestOptions;

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
        /// Exact protocol-native continuation state, replayed only when its
        /// endpoint and wire model still match the current request.
        native_replay: Option<NativeReplay>,
    },
    Tool {
        content: String,
        artifacts: Vec<ToolArtifact>,
        tool_call_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeReplay {
    pub endpoint: String,
    pub model: String,
    pub payload: NativeReplayPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "api", content = "data", rename_all = "snake_case")]
pub enum NativeReplayPayload {
    ChatReasoning(String),
    ResponsesOutput(Vec<Value>),
}

impl NativeReplay {
    pub fn matches(&self, endpoint: &str, model: &str) -> bool {
        self.endpoint == endpoint && self.model == model
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelApi {
    ChatCompletions,
    Responses,
}

/// Bound the connect phase so a dead host fails fast instead of hanging the turn.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum gap between stream bytes. This is an inter-chunk idle bound, not a
/// total-turn bound; models may legitimately reason for a long time.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

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
        let mut request = self.client.post(&self.endpoint).json(body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
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
    } else {
        anyhow::bail!(
            "unsupported provider endpoint `{endpoint}`; path must end in `/chat/completions` or `/responses`"
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
pub struct ToolArtifact {
    pub attachment: Attachment,
    pub detail: ImageDetail,
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

#[derive(Debug, Clone, Default)]
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
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningStart(ReasoningVisibility),
    ReasoningDelta(String),
    ReasoningSummaryDelta(String),
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
            Self::HttpStatus { status, body } => write!(formatter, "HTTP {status}: {body}"),
            Self::Transport(message) => write!(formatter, "transport error: {message}"),
            Self::SseParse(message) => write!(formatter, "SSE parse: {message}"),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn retryable_for_live_turn(&self) -> bool {
        match self {
            ProviderError::RateLimit { .. } => true,
            ProviderError::HttpStatus { status, .. } => *status >= 500,
            ProviderError::Transport(_) => true,
            ProviderError::ContextLength | ProviderError::SseParse(_) | ProviderError::Other(_) => {
                false
            }
        }
    }
}

#[async_trait(?Send)]
pub trait Provider: Send + Sync {
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
    use super::*;

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
        for endpoint in [
            "https://gateway.test/v1",
            "https://gateway.test/v1/Responses",
            "https://gateway.test/v1/chat/completions/extra",
        ] {
            assert!(classify_endpoint(endpoint).is_err(), "accepted {endpoint}");
        }
    }
}
