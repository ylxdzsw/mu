pub mod openai;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
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
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct StreamResult {
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
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
    #[error("{0}")]
    Other(String),
}

#[async_trait(?Send)]
pub trait Provider: Send + Sync {
    async fn stream_chat(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[Value],
        on_text_delta: &mut dyn FnMut(String) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError>;
}

pub fn approx_tokens(s: &str) -> u64 {
    ((s.len() as u64) + 3) / 4
}
