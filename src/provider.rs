use std::{
    fmt,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime};
use clap::ValueEnum;
use percent_encoding::percent_decode_str;
use reqwest::Client;
use serde::{
    Deserialize, Serialize,
    de::{Error as _, MapAccess, Visitor},
};
use serde_json::Value;
use tokio::time::{self, MissedTickBehavior};

use crate::config::Config;
use crate::models::{ResolvedModelChoice, ResolvedModelRef};

#[derive(Debug, Clone)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        items: Vec<AssistantItem>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantItem {
    Reasoning { text: Option<String> },
    Text { text: String },
    BashCall(ToolCall),
}

impl Message {
    pub fn assistant(
        content: Option<String>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
        native_replay: Option<NativeReplay>,
    ) -> Self {
        let mut items = Vec::new();
        if let Some(text) = reasoning {
            items.push(AssistantItem::Reasoning { text: Some(text) });
        }
        if let Some(text) = content {
            items.push(AssistantItem::Text { text });
        }
        items.extend(
            tool_calls
                .into_iter()
                .flatten()
                .map(AssistantItem::BashCall),
        );
        Self::Assistant {
            items,
            native_replay,
        }
    }

    pub fn assistant_text(&self) -> Option<String> {
        let Self::Assistant { items, .. } = self else {
            return None;
        };
        let mut text = String::new();
        let mut present = false;
        for item in items {
            if let AssistantItem::Text { text: part } = item {
                text.push_str(part);
                present = true;
            }
        }
        present.then_some(text)
    }

    pub fn assistant_tool_calls(&self) -> Vec<&ToolCall> {
        let Self::Assistant { items, .. } = self else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| match item {
                AssistantItem::BashCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    pub fn approx_tokens(&self) -> u64 {
        message_approx_tokens(self, true)
    }
}

fn message_approx_tokens(message: &Message, keep_native_replay: bool) -> u64 {
    match message {
        Message::System { content } => approx_tokens(content),
        Message::User { content } => user_content_approx_tokens(content),
        Message::Assistant {
            items,
            native_replay,
        } => {
            let semantic = items
                .iter()
                .map(|item| match item {
                    AssistantItem::Reasoning { .. } => 0,
                    AssistantItem::Text { text } => approx_tokens(text),
                    AssistantItem::BashCall(call) => {
                        approx_tokens(&serde_json::to_string(call).unwrap_or_default())
                    }
                })
                .sum::<u64>();
            match native_replay
                .as_ref()
                .filter(|_| keep_native_replay)
                .map(|native| &native.payload)
            {
                Some(NativeReplayPayload::ChatReasoning(reasoning)) => {
                    semantic.saturating_add(approx_tokens(reasoning))
                }
                Some(NativeReplayPayload::ResponsesOutput(items))
                | Some(NativeReplayPayload::AnthropicContent(items)) => {
                    approx_tokens(&serde_json::to_string(items).unwrap_or_default())
                }
                None => semantic,
            }
        }
        Message::Tool {
            content,
            attachments,
            ..
        } => attachments
            .iter()
            .fold(approx_tokens(content), |tokens, item| {
                tokens.saturating_add(attachment_approx_tokens(&item.attachment))
            }),
    }
}

fn user_content_approx_tokens(content: &UserContent) -> u64 {
    match content {
        UserContent::Text(text) => approx_tokens(text),
        UserContent::Parts(parts) => parts.iter().fold(0u64, |tokens, part| {
            tokens.saturating_add(match part {
                ContentPart::Text { text } => approx_tokens(text),
                ContentPart::Attachment { attachment } => attachment_approx_tokens(attachment),
            })
        }),
    }
}

fn attachment_approx_tokens(attachment: &Attachment) -> u64 {
    // Media tokenization is provider- and model-specific. Charge a bounded,
    // nonzero amount based on compressed bytes until a provider reports the
    // actual request total.
    (attachment.data.len() as u64)
        .div_ceil(256)
        .clamp(1_024, 16_384)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeReplay {
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
    pub(crate) fn api(&self) -> ModelApi {
        self.payload.api()
    }
}

impl NativeReplayPayload {
    pub(crate) fn api(&self) -> ModelApi {
        match self {
            NativeReplayPayload::ChatReasoning(_) => ModelApi::ChatCompletions,
            NativeReplayPayload::ResponsesOutput(_) => ModelApi::Responses,
            NativeReplayPayload::AnthropicContent(_) => ModelApi::AnthropicMessages,
        }
    }
}

impl NativeReplay {
    fn origin(&self) -> ReplayOrigin {
        ReplayOrigin {
            api: self.api().name().to_string(),
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
    target_api: ModelApi,
) -> Vec<Message> {
    filter_native_replay(messages, |native| {
        native_replay_compatible_for_config(native, config, target, target_api)
    })
}

pub fn estimate_messages_tokens(
    messages: &[Message],
    config: &Config,
    target: &ResolvedModelRef,
    target_api: ModelApi,
) -> u64 {
    messages
        .iter()
        .map(|message| {
            let keep_native = match message {
                Message::Assistant {
                    native_replay: Some(native),
                    ..
                } => native_replay_compatible_for_config(native, config, target, target_api),
                _ => true,
            };
            message_approx_tokens(message, keep_native)
        })
        .sum()
}

fn native_replay_compatible_for_config(
    native: &NativeReplay,
    config: &Config,
    target: &ResolvedModelRef,
    target_api: ModelApi,
) -> bool {
    if native.api() != target_api {
        return false;
    }
    target_api == ModelApi::ChatCompletions
        || config.replay_key(&native.provider_id, &native.model)
            == config.replay_key(&target.provider_id, &target.model_id)
}

pub fn filter_native_replay_for_origins(
    messages: &[Message],
    target_api: ModelApi,
    origins: &[ReplayOrigin],
) -> Vec<Message> {
    filter_native_replay(messages, |native| {
        native.api() == target_api && origins.contains(&native.origin())
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

impl ModelApi {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "chat_completions" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    pub fn request_format(self) -> &'static str {
        match self {
            Self::ChatCompletions => "openai.chat_completions.v1",
            Self::Responses => "openai.responses.v1",
            Self::AnthropicMessages => "anthropic.messages.v1",
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub model: ResolvedModelRef,
    pub cache_key: Option<String>,
    pub messages: Vec<Message>,
    pub bash: bool,
}

impl Request {
    pub fn json(&self, api: ModelApi) -> Result<Value, ProviderError> {
        let tools = self.tools();
        match api {
            ModelApi::ChatCompletions => {
                Ok(crate::chat_completions::build_request_body(self, &tools))
            }
            ModelApi::Responses => crate::responses::build_request_body(self, &tools),
            ModelApi::AnthropicMessages => crate::anthropic::build_request_body(self, &tools),
        }
    }

    pub fn tools(&self) -> Vec<Value> {
        if self.bash {
            crate::bash::tool_definitions()
        } else {
            Vec::new()
        }
    }
}

/// Bound the connect phase so a dead host fails fast instead of hanging the turn.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum gap between stream bytes. This is an inter-chunk idle bound, not a
/// total-turn bound; models may legitimately reason for a long time.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_mins(10);

pub struct HttpProvider {
    client: Client,
    pub(crate) endpoint: String,
    request_url: String,
    api: ModelApi,
    api_key: Option<String>,
    pub(crate) idle_timeout: Duration,
}

struct ParsedEndpoint {
    endpoint: String,
    request_url: String,
    api: ModelApi,
    unix_socket: Option<PathBuf>,
}

pub(crate) enum SseEvent {
    Data(String),
    Tick,
}

impl HttpProvider {
    pub fn new(endpoint: String, api_key: Option<String>) -> anyhow::Result<Self> {
        let parsed = parse_endpoint(&endpoint)?;
        let mut client = Client::builder().connect_timeout(CONNECT_TIMEOUT);
        if let Some(socket) = parsed.unix_socket.as_ref() {
            client = client.unix_socket(socket.as_path());
        }
        let client = client.build()?;
        Ok(Self {
            client,
            endpoint: parsed.endpoint,
            request_url: parsed.request_url,
            api: parsed.api,
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
            let retry_after = parse_retry_after(
                response.headers().get(reqwest::header::RETRY_AFTER),
                SystemTime::now(),
            );
            let body = response.text().await.unwrap_or_default();
            return Err(classify_http_error(status, body, retry_after));
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
        let mut request = self.client.post(&self.request_url).json(body);
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

fn parse_endpoint(endpoint: &str) -> anyhow::Result<ParsedEndpoint> {
    if endpoint.starts_with("http+unix://") {
        return parse_http_unix_endpoint(endpoint);
    }

    let url = normalized_url(endpoint, endpoint)?;
    let api = classify_endpoint_path(endpoint, url.path())?;
    let endpoint = url.to_string();
    Ok(ParsedEndpoint {
        request_url: endpoint.clone(),
        endpoint,
        api,
        unix_socket: None,
    })
}

fn parse_http_unix_endpoint(endpoint: &str) -> anyhow::Result<ParsedEndpoint> {
    let rest = endpoint
        .strip_prefix("http+unix://")
        .expect("http+unix prefix checked");
    let (encoded_socket, request_path) = rest.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("invalid provider endpoint `{endpoint}`: http+unix requires a request path")
    })?;
    if encoded_socket.is_empty() || !valid_encoded_socket(encoded_socket) {
        anyhow::bail!(
            "invalid provider endpoint `{endpoint}`: socket path must be percent-encoded"
        );
    }
    let socket = percent_decode_str(encoded_socket)
        .decode_utf8()
        .map_err(|error| {
            anyhow::anyhow!(
                "invalid provider endpoint `{endpoint}`: socket path is not UTF-8: {error}"
            )
        })?
        .into_owned();
    if socket.contains('\0') || !std::path::Path::new(&socket).is_absolute() {
        anyhow::bail!(
            "invalid provider endpoint `{endpoint}`: socket path must decode to an absolute path"
        );
    }

    let request_url = normalized_url(&format!("http://localhost/{request_path}"), endpoint)?;
    let api = classify_endpoint_path(endpoint, request_url.path())?;
    let mut suffix = request_url.path().to_string();
    if let Some(query) = request_url.query() {
        suffix.push('?');
        suffix.push_str(query);
    }
    if let Some(fragment) = request_url.fragment() {
        suffix.push('#');
        suffix.push_str(fragment);
    }
    Ok(ParsedEndpoint {
        endpoint: format!("http+unix://{encoded_socket}{suffix}"),
        request_url: request_url.to_string(),
        api,
        unix_socket: Some(PathBuf::from(socket)),
    })
}

fn valid_encoded_socket(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') => {
                index += 1;
            }
            b'%' if bytes
                .get(index + 1..index + 3)
                .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit)) =>
            {
                index += 3;
            }
            _ => return false,
        }
    }
    true
}

fn normalized_url(url: &str, endpoint: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(url)
        .map_err(|error| anyhow::anyhow!("invalid provider endpoint `{endpoint}`: {error}"))?;
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    Ok(url)
}

fn classify_endpoint_path(endpoint: &str, path: &str) -> anyhow::Result<ModelApi> {
    let path = path.trim_end_matches('/');
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
    Ok(parse_endpoint(endpoint)?.api)
}

#[derive(Debug, Clone)]
pub enum UserContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl UserContent {
    #[cfg(test)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub arguments: String,
}

// MapAccess exposes duplicate keys before Value deserialization collapses them.
struct UniqueToolArguments(Value);

impl<'de> Deserialize<'de> for UniqueToolArguments {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UniqueToolArgumentsVisitor;

        impl<'de> Visitor<'de> for UniqueToolArgumentsVisitor {
            type Value = UniqueToolArguments;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object with unique keys")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut object = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if object.contains_key(&key) {
                        return Err(A::Error::custom(format!("duplicate key `{key}`")));
                    }
                    object.insert(key, value);
                }
                Ok(UniqueToolArguments(Value::Object(object)))
            }
        }

        deserializer.deserialize_map(UniqueToolArgumentsVisitor)
    }
}

pub(crate) fn parse_completed_tool_arguments(arguments: &str) -> Result<Value, ProviderError> {
    let arguments = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str::<UniqueToolArguments>(arguments)
        .map(|arguments| arguments.0)
        .map_err(|error| {
            ProviderError::Protocol(format!("invalid completed tool arguments: {error}"))
        })
}

pub(crate) fn validate_completed_tool_arguments(arguments: &str) -> Result<String, ProviderError> {
    parse_completed_tool_arguments(arguments)?;
    Ok(if arguments.trim().is_empty() {
        "{}".into()
    } else {
        arguments.into()
    })
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
    pub fn context_total(&self) -> Option<u64> {
        let component_total = self.input_tokens.saturating_add(self.output_tokens);
        if self.total_tokens == 0 {
            return (component_total > 0).then_some(component_total);
        }
        (self.total_tokens >= component_total).then_some(self.total_tokens)
    }

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
    pub arguments_delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Resume,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    ContextLength {
        detail: String,
    },
    RequestTooLarge {
        status: Option<u16>,
        detail: String,
    },
    ModelUnavailable {
        detail: String,
    },
    AuthFailed {
        detail: String,
    },
    Overloaded {
        status: Option<u16>,
        retry_after: Option<Duration>,
        detail: String,
    },
    RateLimit {
        retry_after: Option<Duration>,
        detail: String,
    },
    BadRequestPermanent {
        status: Option<u16>,
        detail: String,
    },
    Transport(String),
    Protocol(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderDisposition {
    ContextRecovery,
    Advance,
    Retry,
    Fail,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContextLength { detail } => {
                write!(formatter, "context length exceeded: {detail}")
            }
            Self::RequestTooLarge { status, detail }
            | Self::BadRequestPermanent { status, detail } => {
                write_status_detail(formatter, *status, detail)
            }
            Self::ModelUnavailable { detail } | Self::AuthFailed { detail } => {
                formatter.write_str(detail)
            }
            Self::Overloaded {
                status,
                retry_after,
                detail,
            } => {
                write_status_detail(formatter, *status, detail)?;
                write_retry_after(formatter, *retry_after)
            }
            Self::RateLimit {
                retry_after,
                detail,
            } => {
                write!(formatter, "HTTP 429: {detail}")?;
                write_retry_after(formatter, *retry_after)
            }
            Self::Transport(message) => write!(formatter, "transport error: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::ContextLength { .. } => "context_length",
            Self::RequestTooLarge { .. } => "request_too_large",
            Self::ModelUnavailable { .. } => "unavailable",
            Self::AuthFailed { .. } => "auth",
            Self::Overloaded { .. } => "overloaded",
            Self::RateLimit { .. } => "rate_limit",
            Self::BadRequestPermanent { .. } => "bad_request",
            Self::Transport(_) => "transport",
            Self::Protocol(_) => "protocol",
        }
    }

    pub fn disposition(&self) -> ProviderDisposition {
        match self {
            Self::ContextLength { .. } => ProviderDisposition::ContextRecovery,
            Self::ModelUnavailable { .. } | Self::AuthFailed { .. } => ProviderDisposition::Advance,
            Self::Overloaded { .. } | Self::RateLimit { .. } | Self::Transport(_) => {
                ProviderDisposition::Retry
            }
            Self::RequestTooLarge { .. } | Self::BadRequestPermanent { .. } | Self::Protocol(_) => {
                ProviderDisposition::Fail
            }
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Overloaded { retry_after, .. } | Self::RateLimit { retry_after, .. } => {
                *retry_after
            }
            _ => None,
        }
    }

    pub fn diagnostic(&self) -> Value {
        let mut diagnostic = serde_json::json!({"message": self.to_string()});
        if let Some(status) = self.status() {
            diagnostic["status"] = Value::from(status);
        }
        if let Some(retry_after) = self.retry_after() {
            diagnostic["retry_after_ms"] =
                Value::from(u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX));
        }
        diagnostic
    }

    fn status(&self) -> Option<u16> {
        match self {
            Self::RequestTooLarge { status, .. }
            | Self::Overloaded { status, .. }
            | Self::BadRequestPermanent { status, .. } => *status,
            Self::RateLimit { .. } => Some(429),
            _ => None,
        }
    }
}

fn write_retry_after(
    formatter: &mut fmt::Formatter<'_>,
    retry_after: Option<Duration>,
) -> fmt::Result {
    if let Some(retry_after) = retry_after {
        write!(formatter, " (Retry-After: {}s)", retry_after.as_secs())?;
    }
    Ok(())
}

fn write_status_detail(
    formatter: &mut fmt::Formatter<'_>,
    status: Option<u16>,
    detail: &str,
) -> fmt::Result {
    match status {
        Some(status) => write!(formatter, "HTTP {status}: {detail}"),
        None => formatter.write_str(detail),
    }
}

#[async_trait(?Send)]
pub trait Provider: Send + Sync {
    fn endpoint(&self) -> &str {
        ""
    }

    fn api(&self) -> ModelApi {
        ModelApi::ChatCompletions
    }

    async fn stream(
        &self,
        request: &Request,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError>;
}

#[async_trait(?Send)]
impl Provider for HttpProvider {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn api(&self) -> ModelApi {
        self.api
    }

    async fn stream(
        &self,
        request: &Request,
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError> {
        match self.api {
            ModelApi::ChatCompletions => {
                crate::chat_completions::stream(self, request, on_event).await
            }
            ModelApi::Responses => crate::responses::stream(self, request, on_event).await,
            ModelApi::AnthropicMessages => crate::anthropic::stream(self, request, on_event).await,
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

pub const MAX_PROVIDER_RETRY_AFTER: Duration = Duration::from_secs(60);

pub fn provider_retry_limit(model: &ResolvedModelChoice) -> u32 {
    if model.is_floating() { 3 } else { 5 }
}

pub fn provider_retry_delay(retry_ordinal: u32) -> Duration {
    Duration::from_secs(match retry_ordinal {
        0 | 1 => 1,
        2 => 2,
        _ => 4,
    })
}

pub fn effective_retry_delay(error: &ProviderError, retry_ordinal: u32) -> Duration {
    provider_retry_delay(retry_ordinal).max(error.retry_after().unwrap_or_default())
}

pub(crate) fn classify_http_error(
    status: u16,
    body: String,
    retry_after: Option<Duration>,
) -> ProviderError {
    let parsed = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    let error = parsed.get("error").unwrap_or(&parsed);
    classify_provider_error(Some(status), error, &body, retry_after)
}

pub(crate) fn classify_stream_error(error: &Value) -> ProviderError {
    let error = error
        .get("error")
        .filter(|nested| nested.is_object())
        .unwrap_or(error);
    let message = stream_error_message(error);
    let status = embedded_status(error).or_else(|| bracketed_status(&message));
    classify_provider_error(status, error, &error.to_string(), None)
}

pub(crate) fn stream_error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

fn classify_provider_error(
    status: Option<u16>,
    error: &Value,
    raw: &str,
    retry_after: Option<Duration>,
) -> ProviderError {
    if let Some(nested) = nested_gateway_error(error) {
        let nested_status = embedded_status(&nested).or(status);
        return classify_provider_error(
            nested_status,
            nested.get("error").unwrap_or(&nested),
            &nested.to_string(),
            retry_after,
        );
    }

    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(raw)
        .trim();
    let detail = if message.is_empty() {
        raw.trim()
    } else {
        message
    }
    .to_string();
    let code_is = |expected: &str| {
        code.eq_ignore_ascii_case(expected) || error_type.eq_ignore_ascii_case(expected)
    };

    if code_is("context_length_exceeded") || code_is("string_above_max_length") {
        return ProviderError::ContextLength { detail };
    }
    if code_is("model_not_found") || code_is("not_found_error") || code_is("Router.Unavailable") {
        return ProviderError::ModelUnavailable { detail };
    }
    if code_is("authentication_error") || code_is("invalid_api_key") || code_is("permission_error")
    {
        return ProviderError::AuthFailed { detail };
    }
    if code_is("rate_limit_exceeded") || code_is("rate_limit_error") {
        return ProviderError::RateLimit {
            retry_after,
            detail,
        };
    }
    if code_is("stream_read_error") || code_is("upstream_error") {
        return ProviderError::Transport(detail);
    }
    if code_is("server_error") || code_is("overloaded_error") {
        return ProviderError::Overloaded {
            status,
            retry_after,
            detail,
        };
    }

    match status {
        Some(401) => return ProviderError::AuthFailed { detail },
        Some(404) => return ProviderError::ModelUnavailable { detail },
        Some(403) if has_auth_evidence(message) => {
            return ProviderError::AuthFailed { detail };
        }
        Some(429) => {
            return ProviderError::RateLimit {
                retry_after,
                detail,
            };
        }
        _ => {}
    }

    let request_error = status.is_none_or(|status| (400..500).contains(&status));
    if request_error && has_context_evidence(message) {
        return ProviderError::ContextLength { detail };
    }

    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("tokens per min")
        || contains_word(&lower, "tpm")
    {
        return ProviderError::RateLimit {
            retry_after,
            detail,
        };
    }
    if lower.contains("no available channel") {
        return ProviderError::ModelUnavailable { detail };
    }
    if lower.contains("upstream authentication failed") || has_explicit_auth_marker(&lower) {
        return ProviderError::AuthFailed { detail };
    }
    if lower.contains("queue is full")
        || lower.contains("cpu overloaded")
        || lower.contains("bad gateway")
    {
        return ProviderError::Overloaded {
            status,
            retry_after,
            detail,
        };
    }

    match status {
        Some(413) => ProviderError::RequestTooLarge { status, detail },
        Some(408 | 425 | 500 | 502 | 503 | 504 | 529) => ProviderError::Overloaded {
            status,
            retry_after,
            detail,
        },
        Some(status @ 500..=599) => ProviderError::Overloaded {
            status: Some(status),
            retry_after,
            detail,
        },
        Some(status @ 400..=499) => ProviderError::BadRequestPermanent {
            status: Some(status),
            detail,
        },
        _ => ProviderError::BadRequestPermanent { status, detail },
    }
}

fn nested_gateway_error(error: &Value) -> Option<Value> {
    let raw = error
        .pointer("/metadata/raw")
        .or_else(|| error.pointer("/error/metadata/raw"))?
        .as_str()?;
    serde_json::from_str(raw).ok()
}

fn embedded_status(error: &Value) -> Option<u16> {
    ["status", "status_code", "http_status"]
        .into_iter()
        .find_map(|key| {
            error
                .get(key)
                .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
                .and_then(|status| u16::try_from(status).ok())
                .filter(|status| (100..=599).contains(status))
        })
}

fn bracketed_status(message: &str) -> Option<u16> {
    let bytes = message.as_bytes();
    bytes.windows(5).find_map(|window| {
        (window[0] == b'[' && window[4] == b']' && window[1..4].iter().all(u8::is_ascii_digit))
            .then(|| std::str::from_utf8(&window[1..4]).ok()?.parse().ok())
            .flatten()
    })
}

fn has_context_evidence(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if lower.contains("range of input length") {
        return true;
    }
    let subject =
        lower.contains("context") || lower.contains("prompt") || lower.contains("input length");
    let overflow = lower.contains("overflow")
        || lower.contains("exceed")
        || lower.contains("above max")
        || lower.contains("maximum")
        || lower.contains("too long");
    subject && overflow
}

fn has_auth_evidence(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    has_explicit_auth_marker(&lower)
        || lower.contains("model access")
        || lower.contains("model_not_found")
}

fn has_explicit_auth_marker(lower: &str) -> bool {
    [
        "authentication",
        "authorization",
        "api key",
        "api-key",
        "access denied",
        "permission",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(index, matched)| {
        let before = haystack[..index].chars().next_back();
        let after = haystack[index + matched.len()..].chars().next();
        before.is_none_or(|c| !c.is_ascii_alphanumeric())
            && after.is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

fn parse_retry_after(
    value: Option<&reqwest::header::HeaderValue>,
    now: SystemTime,
) -> Option<Duration> {
    let value = value?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let timestamp = DateTime::parse_from_rfc2822(value)
        .map(|date| date.timestamp())
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT")
                .map(|date| date.and_utc().timestamp())
        })
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
                .map(|date| date.and_utc().timestamp())
        })
        .ok()?;
    let timestamp = u64::try_from(timestamp).ok()?;
    let target = UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))?;
    Some(target.duration_since(now).unwrap_or_default())
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
        CompactionConfig, LimitsConfig, ModelConfig, OrderedMap, ProviderConfig, RedactionConfig,
        TerminalBellConfig,
    };
    use crate::models::ResolvedModelRef;

    #[test]
    fn assistant_constructor_uses_chat_canonical_order() {
        assert!(matches!(
            Message::assistant(
                Some("text".into()),
                Some(String::new()),
                Some(vec![
                    ToolCall {
                        id: "first".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "second".into(),
                        arguments: "{}".into(),
                    },
                ]),
                None,
            ),
            Message::Assistant { items, .. }
                if matches!(
                    items.as_slice(),
                    [
                        AssistantItem::Reasoning { text: Some(reasoning) },
                        AssistantItem::Text { text },
                        AssistantItem::BashCall(ToolCall { id: first, .. }),
                        AssistantItem::BashCall(ToolCall { id: second, .. }),
                    ] if reasoning.is_empty()
                        && text == "text"
                        && first == "first"
                        && second == "second"
                )
        ));
    }

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
            trap: crate::bash::TrapLevel::Off,
            auto_resume: false,
            soft_interrupt: crate::config::bundled_test_default("/soft_interrupt"),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    fn replay_message(payload: NativeReplayPayload) -> Message {
        Message::assistant(
            Some("semantic".into()),
            None,
            None,
            Some(NativeReplay {
                provider_id: "source".into(),
                endpoint: "https://source.test/v1/chat/completions".into(),
                model: "source-model".into(),
                payload,
            }),
        )
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
    fn context_estimation_uses_native_replay_instead_of_duplicate_semantics() {
        let native = vec![serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type":"output_text","text":"native"}]
        })];
        let message = replay_message(NativeReplayPayload::ResponsesOutput(native.clone()));
        assert_eq!(
            message.approx_tokens(),
            approx_tokens(&serde_json::to_string(&native).unwrap())
        );

        let shared = replay_config(Some("shared"), Some("shared"));
        assert_eq!(
            estimate_messages_tokens(
                std::slice::from_ref(&message),
                &shared,
                &target_model(),
                ModelApi::Responses,
            ),
            message.approx_tokens()
        );

        let incompatible = replay_config(Some("source"), Some("target"));
        assert_eq!(
            estimate_messages_tokens(
                &[message],
                &incompatible,
                &target_model(),
                ModelApi::Responses,
            ),
            approx_tokens("semantic")
        );

        let chat = Message::assistant(
            Some("semantic".into()),
            Some("reasoning".into()),
            None,
            Some(NativeReplay {
                provider_id: "source".into(),
                endpoint: "https://source.test/v1/chat/completions".into(),
                model: "source-model".into(),
                payload: NativeReplayPayload::ChatReasoning("reasoning".into()),
            }),
        );
        assert_eq!(
            chat.approx_tokens(),
            approx_tokens("semantic") + approx_tokens("reasoning")
        );
    }

    #[test]
    fn context_total_rejects_missing_or_inconsistent_usage() {
        assert_eq!(Usage::default().context_total(), None);
        assert_eq!(
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            }
            .context_total(),
            Some(15)
        );
        assert_eq!(
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 14,
                ..Usage::default()
            }
            .context_total(),
            None
        );
        assert_eq!(
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..Usage::default()
            }
            .context_total(),
            Some(15)
        );
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
        assert_eq!(
            classify_endpoint("http+unix://%2Frun%2Fprovider.sock/v1/responses?route=a").unwrap(),
            ModelApi::Responses
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
    fn parses_http_unix_endpoint() {
        use std::path::Path;

        let endpoint = "http+unix://%2Frun%2Fprovider.sock/v1/responses/?route=a";
        let parsed = parse_endpoint(endpoint).unwrap();

        assert_eq!(
            parsed.endpoint,
            "http+unix://%2Frun%2Fprovider.sock/v1/responses?route=a"
        );
        assert_eq!(parsed.request_url, "http://localhost/v1/responses?route=a");
        assert_eq!(
            parsed.unix_socket.as_deref(),
            Some(Path::new("/run/provider.sock"))
        );

        for endpoint in [
            "http+unix://%2Frun%2Fprovider.sock",
            "http+unix://run%2Fprovider.sock/v1/responses",
            "http+unix://%2Grun%2Fprovider.sock/v1/responses",
        ] {
            assert!(parse_endpoint(endpoint).is_err(), "accepted {endpoint}");
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
    fn http_and_stream_errors_share_semantic_classification() {
        let cases = [
            (
                429,
                serde_json::json!({
                    "code": "rate_limit_exceeded",
                    "message": "TPM rate limit exceeded; reduce the amount"
                }),
                "rate_limit",
                ProviderDisposition::Retry,
            ),
            (
                400,
                serde_json::json!({
                    "code": "invalid_request_error",
                    "message": "tool schema has too many tokens"
                }),
                "bad_request",
                ProviderDisposition::Fail,
            ),
            (
                400,
                serde_json::json!({
                    "type": "invalid_request_error",
                    "message": "input must be within the range of input length [1, 32768]"
                }),
                "context_length",
                ProviderDisposition::ContextRecovery,
            ),
            (
                404,
                serde_json::json!({
                    "code": "model_not_found",
                    "message": "model missing"
                }),
                "unavailable",
                ProviderDisposition::Advance,
            ),
            (
                503,
                serde_json::json!({
                    "type": "server_error",
                    "message": "[503] queue is full"
                }),
                "overloaded",
                ProviderDisposition::Retry,
            ),
            (
                502,
                serde_json::json!({
                    "code": "stream_read_error",
                    "type": "upstream_error",
                    "message": "upstream disconnected"
                }),
                "transport",
                ProviderDisposition::Retry,
            ),
        ];

        for (status, error, class, disposition) in cases {
            let http = classify_http_error(
                status,
                serde_json::json!({"error": &error}).to_string(),
                None,
            );
            let stream = classify_stream_error(&error);
            assert_eq!(http.class(), class);
            assert_eq!(stream.class(), class);
            assert_eq!(http.disposition(), disposition);
            assert_eq!(stream.disposition(), disposition);
        }
    }

    #[test]
    fn classifies_gateway_wrappers_and_rejects_weak_context_phrases() {
        let wrapped_auth = serde_json::json!({
            "error": {
                "message": "Upstream request failed",
                "metadata": {
                    "raw": "{\"error\":{\"type\":\"authentication_error\",\"message\":\"Upstream authentication failed\"}}"
                }
            }
        });
        assert!(matches!(
            classify_http_error(502, wrapped_auth.to_string(), None),
            ProviderError::AuthFailed { .. }
        ));
        assert!(matches!(
            classify_http_error(
                400,
                r#"{"error":{"message":"Upstream request failed"}}"#.into(),
                None
            ),
            ProviderError::BadRequestPermanent { .. }
        ));
        assert!(matches!(
            classify_http_error(413, "Payload Too Large".into(), None),
            ProviderError::RequestTooLarge { .. }
        ));
        assert!(matches!(
            classify_http_error(
                400,
                r#"{"error":{"message":"reduce the length of this tool schema"}}"#.into(),
                None
            ),
            ProviderError::BadRequestPermanent { .. }
        ));
        assert!(matches!(
            classify_http_error(
                400,
                "This model's maximum context length is 128000 tokens".into(),
                None
            ),
            ProviderError::ContextLength { .. }
        ));
        assert!(!matches!(
            classify_http_error(
                500,
                "internal error in context length calculator".into(),
                None
            ),
            ProviderError::ContextLength { .. }
        ));
    }

    #[test]
    fn retry_after_is_parsed_and_takes_precedence() {
        let error = ProviderError::Overloaded {
            status: Some(503),
            retry_after: Some(Duration::from_secs(20)),
            detail: "busy".into(),
        };
        assert_eq!(effective_retry_delay(&error, 2), Duration::from_secs(20));

        let now = UNIX_EPOCH + Duration::from_secs(1_784_000_000);
        let seconds = reqwest::header::HeaderValue::from_static("20");
        assert_eq!(
            parse_retry_after(Some(&seconds), now),
            Some(Duration::from_secs(20))
        );
        let malformed = reqwest::header::HeaderValue::from_static("later");
        assert_eq!(parse_retry_after(Some(&malformed), now), None);
        let date = reqwest::header::HeaderValue::from_static("Thu, 01 Jan 1970 00:01:00 GMT");
        assert_eq!(
            parse_retry_after(Some(&date), UNIX_EPOCH),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            parse_retry_after(Some(&date), UNIX_EPOCH + Duration::from_secs(120)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn replay_filter_respects_keys_and_api() {
        let messages = vec![replay_message(NativeReplayPayload::ResponsesOutput(vec![
            serde_json::json!({"type":"reasoning","encrypted_content":"opaque"}),
        ]))];
        let target = target_model();

        let separate = replay_config(None, None);
        let filtered =
            filter_native_replay_for_config(&messages, &separate, &target, ModelApi::Responses);
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: None,
                ..
            }
        ));

        let shared = replay_config(Some("compatible"), Some("compatible"));
        let filtered =
            filter_native_replay_for_config(&messages, &shared, &target, ModelApi::Responses);
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: Some(_),
                ..
            }
        ));

        let changed = replay_config(Some("compatible"), Some("replacement"));
        let filtered =
            filter_native_replay_for_config(&messages, &changed, &target, ModelApi::Responses);
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: None,
                ..
            }
        ));

        let config = replay_config(Some("source"), Some("target"));
        let messages = vec![replay_message(NativeReplayPayload::ChatReasoning(
            "trace".into(),
        ))];
        let filtered = filter_native_replay_for_config(
            &messages,
            &config,
            &target_model(),
            ModelApi::ChatCompletions,
        );
        assert!(matches!(
            &filtered[0],
            Message::Assistant {
                native_replay: Some(_),
                ..
            }
        ));

        let config = replay_config(Some("compatible"), Some("compatible"));
        let messages = vec![replay_message(NativeReplayPayload::ResponsesOutput(vec![
            serde_json::json!({"type":"reasoning","encrypted_content":"opaque"}),
        ]))];
        let filtered = filter_native_replay_for_config(
            &messages,
            &config,
            &target_model(),
            ModelApi::ChatCompletions,
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
