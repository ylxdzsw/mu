use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
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
    ReasoningStart,
    ReasoningDelta(String),
    ReasoningEnd,
    ToolCallDelta(ToolCallDelta),
    Tick,
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

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("context length exceeded")]
    ContextLength,
    #[error("HTTP 429: {message}")]
    RateLimit { message: String },
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("SSE parse: {0}")]
    SseParse(String),
    #[error("{0}")]
    Other(String),
}

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

pub fn approx_tokens(s: &str) -> u64 {
    (s.len() as u64).div_ceil(4)
}

pub fn build_provider(config: &Config, provider_id: &str) -> anyhow::Result<Arc<dyn Provider>> {
    let provider = config.provider(provider_id)?;
    let api_key = config.api_key_for_provider(provider_id)?;
    Ok(Arc::new(crate::openai::OpenAiProvider::new(
        provider.base_url.clone(),
        api_key,
    )) as Arc<dyn Provider>)
}
