use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::{self, MissedTickBehavior};

use crate::models::RequestOptions;
use crate::provider::{
    ContentPart, FinishReason, FunctionCall, Message, ModelApi, NativeReplay, NativeReplayPayload,
    Provider, ProviderError, StreamEvent, StreamResult, ToolCall,
    ToolCallDelta as ProviderToolCallDelta, Usage, UserContent, classify_endpoint,
};

/// Bound the connect phase so a dead host fails fast instead of hanging the turn.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum gap between received stream bytes before the connection is treated as
/// black-holed. This is an inter-chunk idle bound, not a total-turn bound: a
/// model can legitimately reason silently for a long time, and GPT-5.x pauses
/// generation for several seconds mid-stream while safety classifiers run, so
/// the window is deliberately generous and only trips on true silence.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

pub struct OpenAiProvider {
    client: Client,
    endpoint: String,
    api: ModelApi,
    api_key: Option<String>,
    idle_timeout: Duration,
}

impl OpenAiProvider {
    pub fn new(endpoint: String, api_key: Option<String>) -> anyhow::Result<Self> {
        let endpoint = normalize_endpoint(&endpoint)?;
        let api = classify_endpoint(&endpoint)?;
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Ok(Self {
            client,
            endpoint,
            api,
            api_key,
            idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        })
    }
}

fn normalize_endpoint(endpoint: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(endpoint)?;
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    Ok(url.to_string())
}

#[derive(Debug, Deserialize)]
struct ChunkResponse {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    usage: Option<UsageJson>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ChunkDelta {
    content: Option<String>,
    reasoning_content: Option<Value>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    call_type: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize, Default)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageJson {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: PromptTokensDetailsJson,
    #[serde(default)]
    completion_tokens_details: CompletionTokensDetailsJson,
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
    #[serde(default)]
    prompt_cache_miss_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetailsJson {
    #[serde(default)]
    cached_tokens: u64,
    cache_creation_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionTokensDetailsJson {
    #[serde(default)]
    reasoning_tokens: u64,
}

type ToolCallAccumulator = BTreeMap<usize, (Option<String>, Option<String>, String, String)>;

struct StreamParseState {
    content: String,
    reasoning_content: String,
    tool_accum: ToolCallAccumulator,
    finish_reason: FinishReason,
    usage: Option<Usage>,
    reasoning_active: bool,
    tool_call_started: bool,
}

impl Default for StreamParseState {
    fn default() -> Self {
        Self {
            content: String::new(),
            reasoning_content: String::new(),
            tool_accum: BTreeMap::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
            reasoning_active: false,
            tool_call_started: false,
        }
    }
}

#[async_trait(?Send)]
impl Provider for OpenAiProvider {
    async fn stream_chat(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError> {
        match self.api {
            ModelApi::ChatCompletions => {
                self.stream_chat_completions(request, messages, tools, on_event)
                    .await
            }
            ModelApi::Responses => {
                self.stream_responses(request, messages, tools, on_event)
                    .await
            }
        }
    }
}

impl OpenAiProvider {
    async fn stream_chat_completions(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError> {
        let body = build_chat_request_body(request, &self.endpoint, messages, tools);

        let mut req = self.client.post(&self.endpoint).json(&body);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if is_context_length_error(status.as_u16(), &text) {
                return Err(ProviderError::ContextLength);
            }
            if status.as_u16() == 429 {
                return Err(ProviderError::RateLimit { message: text });
            }
            return Err(ProviderError::HttpStatus {
                status: status.as_u16(),
                body: text,
            });
        }

        let mut response = response;
        let mut stream_state = StreamParseState::default();

        let mut buffer = String::new();
        let mut byte_buf: Vec<u8> = Vec::new();
        let mut last_activity = std::time::Instant::now();
        let mut tick = time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => {
                    last_activity = std::time::Instant::now();
                    chunk.map_err(|e| ProviderError::Transport(e.to_string()))?
                }
                _ = tick.tick() => {
                    if last_activity.elapsed() > self.idle_timeout {
                        return Err(ProviderError::Transport(format!(
                            "stream idle for over {}s",
                            self.idle_timeout.as_secs()
                        )));
                    }
                    on_event(StreamEvent::Tick)?;
                    continue;
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            byte_buf.extend_from_slice(&chunk);
            // Decode only the longest valid UTF-8 prefix; keep trailing bytes
            // of a split multi-byte codepoint for the next chunk.
            let valid_up_to = match std::str::from_utf8(&byte_buf) {
                Ok(s) => {
                    buffer.push_str(s);
                    byte_buf.len()
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // SAFETY: bytes [..valid] are guaranteed valid UTF-8.
                    buffer.push_str(unsafe { std::str::from_utf8_unchecked(&byte_buf[..valid]) });
                    valid
                }
            };
            byte_buf.drain(..valid_up_to);
            consume_sse_buffer(&mut buffer, &mut stream_state, on_event)?;
        }
        // Flush any remaining undecodable trailing bytes (lossy) so a final
        // event without a trailing blank line is not silently dropped.
        if !byte_buf.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&byte_buf));
        }
        if !buffer.trim().is_empty() {
            buffer.push_str("\n\n");
            consume_sse_buffer(&mut buffer, &mut stream_state, on_event)?;
        }
        if stream_state.reasoning_active {
            on_event(StreamEvent::ReasoningEnd)?;
            stream_state.reasoning_active = false;
        }

        let has_tool_calls = !stream_state.tool_accum.is_empty();
        let tool_calls = if !has_tool_calls {
            None
        } else {
            Some(
                stream_state
                    .tool_accum
                    .into_values()
                    .map(|(id, name, args, call_type)| ToolCall {
                        id: id.unwrap_or_else(|| "call".into()),
                        call_type,
                        function: FunctionCall {
                            name: name.unwrap_or_default(),
                            arguments: args,
                        },
                    })
                    .collect(),
            )
        };

        let message = Message::Assistant {
            content: if stream_state.content.is_empty() {
                None
            } else {
                Some(stream_state.content)
            },
            reasoning_content: if stream_state.reasoning_content.is_empty() {
                None
            } else {
                Some(stream_state.reasoning_content.clone())
            },
            tool_calls,
            native_replay: if stream_state.reasoning_content.is_empty() || !has_tool_calls {
                None
            } else {
                Some(NativeReplay {
                    endpoint: self.endpoint.clone(),
                    model: request.model.model_id.clone(),
                    payload: NativeReplayPayload::ChatReasoning(
                        stream_state.reasoning_content.clone(),
                    ),
                })
            },
        };

        Ok(StreamResult {
            message,
            finish_reason: stream_state.finish_reason,
            usage: stream_state.usage,
        })
    }

    async fn stream_responses(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError> {
        let body = build_responses_request_body(request, &self.endpoint, messages, tools)?;
        let mut req = self.client.post(&self.endpoint).json(&body);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(classify_http_error(status.as_u16(), text));
        }

        let mut response = response;
        let mut state = ResponsesStreamState::default();
        let mut buffer = String::new();
        let mut byte_buf = Vec::new();
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
                    on_event(StreamEvent::Tick)?;
                    continue;
                }
            };
            let Some(chunk) = chunk else { break };
            byte_buf.extend_from_slice(&chunk);
            let valid_up_to = match std::str::from_utf8(&byte_buf) {
                Ok(text) => {
                    buffer.push_str(text);
                    byte_buf.len()
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // SAFETY: `valid_up_to` is guaranteed to end on UTF-8 boundary.
                    buffer.push_str(unsafe { std::str::from_utf8_unchecked(&byte_buf[..valid]) });
                    valid
                }
            };
            byte_buf.drain(..valid_up_to);
            consume_responses_sse_buffer(&mut buffer, &mut state, on_event)?;
        }
        if !byte_buf.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&byte_buf));
        }
        if !buffer.trim().is_empty() {
            buffer.push_str("\n\n");
            consume_responses_sse_buffer(&mut buffer, &mut state, on_event)?;
        }
        if state.reasoning_active {
            on_event(StreamEvent::ReasoningEnd)?;
        }
        if !state.terminal {
            return Err(ProviderError::Other(state.failure.unwrap_or_else(|| {
                "Responses stream ended before response.completed".into()
            })));
        }

        let output = state.output;
        let tool_calls = responses_tool_calls(&output);
        let content = if state.content.is_empty() {
            responses_output_text(&output)
        } else {
            Some(state.content)
        };
        let finish_reason = state.finish_reason.unwrap_or({
            if tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            }
        });
        Ok(StreamResult {
            message: Message::Assistant {
                content,
                reasoning_content: None,
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                native_replay: state.replayable.then(|| NativeReplay {
                    endpoint: self.endpoint.clone(),
                    model: request.model.model_id.clone(),
                    payload: NativeReplayPayload::ResponsesOutput(output),
                }),
            },
            finish_reason,
            usage: state.usage,
        })
    }
}

fn classify_http_error(status: u16, body: String) -> ProviderError {
    if is_context_length_error(status, &body) {
        ProviderError::ContextLength
    } else if status == 429 {
        ProviderError::RateLimit { message: body }
    } else {
        ProviderError::HttpStatus { status, body }
    }
}

fn build_responses_request_body(
    request: &RequestOptions,
    endpoint: &str,
    messages: &[Message],
    tools: &[Value],
) -> Result<Value, ProviderError> {
    let mut input = Vec::new();
    for message in messages {
        responses_input_items(message, endpoint, &request.model.model_id, &mut input)?;
    }
    let response_tools = tools
        .iter()
        .map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            let mut flat = serde_json::Map::new();
            flat.insert("type".into(), Value::String("function".into()));
            for key in ["name", "description", "parameters", "strict"] {
                if let Some(value) = function.get(key) {
                    flat.insert(key.into(), value.clone());
                }
            }
            Value::Object(flat)
        })
        .collect::<Vec<_>>();
    let mut body = serde_json::json!({
        "model": request.model.model_id,
        "input": input,
        "tools": response_tools,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"]
    });
    if let Some(effort) = request.model.effort.as_deref() {
        body["reasoning"] = serde_json::json!({ "effort": effort });
    }
    Ok(body)
}

fn responses_input_items(
    message: &Message,
    endpoint: &str,
    model: &str,
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    match message {
        Message::System { content } => input.push(serde_json::json!({
            "role": "system", "content": content
        })),
        Message::User { content } => input.push(serde_json::json!({
            "role": "user", "content": responses_user_content(content)?
        })),
        Message::Assistant {
            content,
            tool_calls,
            native_replay,
            ..
        } => {
            if let Some(NativeReplay {
                payload: NativeReplayPayload::ResponsesOutput(items),
                ..
            }) = native_replay
                .as_ref()
                .filter(|native| native.matches(endpoint, model))
            {
                input.extend(items.iter().cloned());
            } else {
                if let Some(content) = content {
                    input.push(serde_json::json!({ "role": "assistant", "content": content }));
                }
                if let Some(tool_calls) = tool_calls {
                    input.extend(tool_calls.iter().map(|call| {
                        serde_json::json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.function.name,
                            "arguments": call.function.arguments
                        })
                    }));
                }
            }
        }
        Message::Tool {
            content,
            tool_call_id,
        } => input.push(serde_json::json!({
            "type": "function_call_output", "call_id": tool_call_id, "output": content
        })),
    }
    Ok(())
}

fn responses_user_content(content: &UserContent) -> Result<Value, ProviderError> {
    match content {
        UserContent::Text(text) => Ok(Value::String(text.clone())),
        UserContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => Ok(serde_json::json!({
                    "type": "input_text", "text": text
                })),
                ContentPart::Attachment { attachment }
                    if attachment.media_type.starts_with("image/") =>
                {
                    Ok(serde_json::json!({
                        "type": "input_image",
                        "image_url": format!(
                            "data:{};base64,{}", attachment.media_type, base64_encode(&attachment.data)
                        )
                    }))
                }
                ContentPart::Attachment { attachment } => Err(ProviderError::Other(format!(
                    "Responses endpoints do not support audio attachment `{}` ({})",
                    attachment.filename, attachment.media_type
                ))),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
    }
}

#[derive(Default)]
struct ResponsesStreamState {
    content: String,
    output: Vec<Value>,
    usage: Option<Usage>,
    terminal: bool,
    replayable: bool,
    finish_reason: Option<FinishReason>,
    failure: Option<String>,
    reasoning_active: bool,
    tool_indexes: BTreeMap<usize, usize>,
}

fn consume_responses_sse_buffer(
    buffer: &mut String,
    state: &mut ResponsesStreamState,
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    while let Some((pos, sep_len)) = next_event_boundary(buffer) {
        let event = buffer[..pos].to_string();
        buffer.replace_range(..pos + sep_len, "");
        let data = event
            .lines()
            .filter_map(|line| line.trim().strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&data)
            .map_err(|error| ProviderError::SseParse(error.to_string()))?;
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.output_item.added" => {
                let item = &value["item"];
                if item["type"] == "reasoning" && !state.reasoning_active {
                    state.reasoning_active = true;
                    on_event(StreamEvent::ReasoningStart)?;
                } else if item["type"] == "function_call" {
                    let output_index = value["output_index"].as_u64().unwrap_or(0) as usize;
                    let tool_index = state.tool_indexes.len();
                    state.tool_indexes.insert(output_index, tool_index);
                    on_event(StreamEvent::ToolCallDelta(ProviderToolCallDelta {
                        index: tool_index,
                        id: item["call_id"].as_str().map(str::to_owned),
                        name: item["name"].as_str().map(str::to_owned),
                        arguments_delta: item["arguments"].as_str().unwrap_or("").to_string(),
                    }))?;
                }
            }
            "response.output_item.done" => {
                if value["item"]["type"] == "reasoning" && state.reasoning_active {
                    state.reasoning_active = false;
                    on_event(StreamEvent::ReasoningEnd)?;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = value["delta"].as_str() {
                    state.content.push_str(delta);
                    on_event(StreamEvent::TextDelta(delta.to_string()))?;
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = value["output_index"].as_u64().unwrap_or(0) as usize;
                let index = state
                    .tool_indexes
                    .get(&output_index)
                    .copied()
                    .unwrap_or(output_index);
                let delta = value["delta"].as_str().unwrap_or("").to_string();
                on_event(StreamEvent::ToolCallDelta(ProviderToolCallDelta {
                    index,
                    id: None,
                    name: None,
                    arguments_delta: delta,
                }))?;
            }
            "response.completed" => {
                state.terminal = true;
                state.replayable = true;
                state.output = value["response"]["output"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                state.usage = responses_usage(&value["response"]["usage"]);
            }
            "response.incomplete" => {
                state.terminal = true;
                state.output = value["response"]["output"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                state.usage = responses_usage(&value["response"]["usage"]);
                let reason = value["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("incomplete");
                state.finish_reason = Some(FinishReason::Other(reason.to_string()));
            }
            "response.failed" | "error" => {
                state.failure = Some(stream_error_message(
                    value.get("error").unwrap_or(&value["response"]["error"]),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn responses_usage(value: &Value) -> Option<Usage> {
    value.is_object().then(|| Usage {
        input_tokens: value["input_tokens"].as_u64().unwrap_or(0),
        cache_read_input_tokens: value["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        cache_write_input_tokens: value["input_tokens_details"]["cache_write_tokens"]
            .as_u64()
            .or_else(|| value["input_tokens_details"]["cache_creation_tokens"].as_u64()),
        output_tokens: value["output_tokens"].as_u64().unwrap_or(0),
        reasoning_output_tokens: value["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0),
        total_tokens: value["total_tokens"].as_u64().unwrap_or(0),
    })
}

fn responses_tool_calls(output: &[Value]) -> Vec<ToolCall> {
    output
        .iter()
        .filter(|item| item["type"] == "function_call")
        .map(|item| ToolCall {
            id: item["call_id"].as_str().unwrap_or("call").to_string(),
            call_type: "function".into(),
            function: FunctionCall {
                name: item["name"].as_str().unwrap_or("").to_string(),
                arguments: item["arguments"].as_str().unwrap_or("").to_string(),
            },
        })
        .collect()
}

fn responses_output_text(output: &[Value]) -> Option<String> {
    let text = output
        .iter()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|part| match part["type"].as_str() {
            Some("output_text") => part["text"].as_str(),
            Some("refusal") => part["refusal"].as_str(),
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn build_chat_request_body(
    request: &RequestOptions,
    endpoint: &str,
    messages: &[Message],
    tools: &[Value],
) -> Value {
    let mut body = serde_json::json!({
        "model": request.model.model_id.as_str(),
        "messages": messages.iter().map(|message| chat_message_json(message, endpoint, &request.model.model_id)).collect::<Vec<_>>(),
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    if let Some(effort) = request.model.effort.as_deref() {
        // Chat Completions uses a top-level `reasoning_effort` string. (The
        // nested `reasoning: { effort }` object is the Responses API shape and
        // is rejected by real OpenAI `/chat/completions`.)
        body["reasoning_effort"] = Value::String(effort.to_string());
    }
    body
}

fn chat_message_json(message: &Message, endpoint: &str, model: &str) -> Value {
    match message {
        Message::System { content } => serde_json::json!({
            "role": "system",
            "content": content,
        }),
        Message::User { content } => serde_json::json!({
            "role": "user",
            "content": user_content_json(content),
        }),
        Message::Assistant {
            content,
            reasoning_content: _,
            tool_calls,
            native_replay,
        } => {
            let mut value = serde_json::json!({
                "role": "assistant",
                "content": content,
            });
            if let Some(NativeReplay {
                payload: NativeReplayPayload::ChatReasoning(reasoning),
                ..
            }) = native_replay
                .as_ref()
                .filter(|native| native.matches(endpoint, model))
            {
                value["reasoning_content"] = Value::String(reasoning.clone());
            }
            if let Some(tool_calls) = tool_calls {
                value["tool_calls"] =
                    serde_json::to_value(tool_calls).expect("serializing tool calls");
            }
            value
        }
        Message::Tool {
            content,
            tool_call_id,
        } => serde_json::json!({
            "role": "tool",
            "content": content,
            "tool_call_id": tool_call_id,
        }),
    }
}

fn user_content_json(content: &UserContent) -> Value {
    match content {
        UserContent::Text(text) => Value::String(text.clone()),
        UserContent::Parts(parts) => Value::Array(parts.iter().map(content_part_json).collect()),
    }
}

fn content_part_json(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => serde_json::json!({
            "type": "text",
            "text": text,
        }),
        ContentPart::Attachment { attachment } if attachment.media_type.starts_with("image/") => {
            let encoded = base64_encode(&attachment.data);
            serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{encoded}", attachment.media_type),
                },
            })
        }
        ContentPart::Attachment { attachment } => {
            let format = match attachment.media_type.as_str() {
                "audio/wav" => "wav",
                "audio/mpeg" => "mp3",
                other => panic!("unsupported attachment media type reached provider: {other}"),
            };
            serde_json::json!({
                "type": "input_audio",
                "input_audio": {
                    "data": base64_encode(&attachment.data),
                    "format": format,
                },
            })
        }
    }
}

fn base64_encode(bytes: &[u8]) -> String {
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

fn consume_sse_buffer(
    buffer: &mut String,
    state: &mut StreamParseState,
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    while let Some((pos, sep_len)) = next_event_boundary(buffer) {
        let event = buffer[..pos].to_string();
        buffer.replace_range(..pos + sep_len, "");

        for line in event.lines() {
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }
            let data = line[5..].trim_start();
            if data == "[DONE]" {
                continue;
            }
            let parsed: ChunkResponse =
                serde_json::from_str(data).map_err(|e| ProviderError::SseParse(e.to_string()))?;

            if let Some(error) = parsed.error {
                return Err(ProviderError::Other(format!(
                    "stream error: {}",
                    stream_error_message(&error)
                )));
            }

            if let Some(u) = parsed.usage {
                let cache_read = u
                    .prompt_tokens_details
                    .cached_tokens
                    .max(u.prompt_cache_hit_tokens);
                let input_tokens = u
                    .prompt_tokens
                    .max(u.prompt_cache_hit_tokens + u.prompt_cache_miss_tokens);
                state.usage = Some(Usage {
                    input_tokens,
                    cache_read_input_tokens: cache_read,
                    cache_write_input_tokens: u.prompt_tokens_details.cache_creation_tokens,
                    output_tokens: u.completion_tokens,
                    reasoning_output_tokens: u.completion_tokens_details.reasoning_tokens,
                    total_tokens: u.total_tokens,
                });
            }

            if let Some(choice) = parsed.choices.first() {
                let reasoning_delta = choice
                    .delta
                    .reasoning_content
                    .as_ref()
                    .and_then(reasoning_text_from_value);
                if let Some(text) = reasoning_delta {
                    state.reasoning_content.push_str(&text);
                    if !state.reasoning_active {
                        on_event(StreamEvent::ReasoningStart)?;
                        state.reasoning_active = true;
                    }
                    on_event(StreamEvent::ReasoningDelta(text))?;
                } else if state.reasoning_active
                    && (choice.delta.content.is_some()
                        || choice.delta.tool_calls.is_some()
                        || choice.finish_reason.is_some())
                {
                    on_event(StreamEvent::ReasoningEnd)?;
                    state.reasoning_active = false;
                }

                if let Some(text) = choice.delta.content.clone() {
                    on_event(StreamEvent::TextDelta(text.clone()))?;
                    state.content.push_str(&text);
                }
                if let Some(ref tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        let entry = state
                            .tool_accum
                            .entry(tc.index)
                            .or_insert_with(|| (None, None, String::new(), "function".into()));
                        if let Some(id) = tc.id.as_deref().filter(|id| !id.is_empty()) {
                            entry.0 = Some(id.to_string());
                        }
                        if let Some(call_type) =
                            tc.call_type.as_deref().filter(|kind| !kind.is_empty())
                        {
                            entry.3 = call_type.to_string();
                        }
                        if let Some(ref f) = tc.function {
                            if let Some(name) = f.name.as_deref().filter(|name| !name.is_empty()) {
                                entry.1 = Some(name.to_string());
                            }
                            if let Some(ref args) = f.arguments {
                                entry.2.push_str(args);
                            }
                        }
                        let name = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.name.as_deref())
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned);
                        let arguments_delta = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.arguments.clone())
                            .unwrap_or_default();
                        on_event(StreamEvent::ToolCallDelta(ProviderToolCallDelta {
                            index: tc.index,
                            id: tc.id.clone(),
                            name,
                            arguments_delta,
                        }))?;
                        state.tool_call_started = true;
                    }
                }
                if let Some(ref reason) = choice.finish_reason {
                    state.finish_reason = match reason.as_str() {
                        "stop" => FinishReason::Stop,
                        "tool_calls" => FinishReason::ToolCalls,
                        other => FinishReason::Other(other.to_string()),
                    };
                }
            }
        }
    }

    Ok(())
}

fn stream_error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

/// Classify an error response as a context-length overflow.
///
/// Overflow is always a client error. We accept three independent signals so
/// differently-worded providers are still recognized:
///   1. HTTP 413 (Payload Too Large) — some gateways use this for prompt size.
///   2. A structured `error.code`/`error.type` of `context_length_exceeded`
///      (OpenAI et al.) or `string_above_max_length`.
///   3. A known context-overflow phrase in the body, but only for 4xx so a 5xx
///      stack trace that happens to mention "context length" is not misread.
fn is_context_length_error(status: u16, body: &str) -> bool {
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

fn reasoning_text_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => (!text.is_empty()).then(|| text.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                collect_reasoning_text(part, &mut out);
            }
            (!out.is_empty()).then_some(out)
        }
        Value::Object(map) => {
            let mut out = String::new();
            collect_reasoning_text(&Value::Object(map.clone()), &mut out);
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

fn collect_reasoning_text(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => out.push_str(text),
        Value::Array(parts) => {
            for part in parts {
                collect_reasoning_text(part, out);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(|value| value.as_str()) {
                out.push_str(text);
            }
            if let Some(value) = map.get("content") {
                collect_reasoning_text(value, out);
            }
            if let Some(value) = map.get("reasoning") {
                collect_reasoning_text(value, out);
            }
        }
        _ => {}
    }
}

/// Find the next SSE event boundary, accepting both `\n\n` (LF) and
/// `\r\n\r\n` (CRLF) framing. Returns the byte offset of the boundary and
/// the separator length.
fn next_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n");
    let crlf = buffer.find("\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => {
            if a <= b {
                Some((a, 2))
            } else {
                Some((b, 4))
            }
        }
        (Some(a), None) => Some((a, 2)),
        (None, Some(b)) => Some((b, 4)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ResolvedModelRef;
    const CHAT_ENDPOINT: &str = "https://example.test/v1/chat/completions";

    fn test_model(effort: Option<&str>) -> ResolvedModelRef {
        ResolvedModelRef {
            canonical: match effort {
                Some(level) => format!("test/gpt-test:{level}"),
                None => "test/gpt-test".into(),
            },
            provider_id: "test".into(),
            model_id: "gpt-test".into(),
            effort: effort.map(str::to_string),
        }
    }

    #[test]
    fn streams_deltas_and_accumulates_tool_calls() {
        let mut seen = String::new();
        let mut tool_call_deltas = Vec::new();
        let mut on_event = |event: StreamEvent| -> Result<(), ProviderError> {
            match event {
                StreamEvent::TextDelta(delta) => seen.push_str(&delta),
                StreamEvent::ToolCallDelta(delta) => tool_call_deltas.push(delta),
                _ => {}
            }
            Ok(())
        };

        let mut buffer = String::new();
        let mut state = StreamParseState::default();

        for chunk in [
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"title\\\":\\\"Inspect\\\",\\\"risk\\\":\\\"readonly\\\",\\\"command\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"\",\"arguments\":\"\\\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5,\"total_tokens\":17,\"prompt_tokens_details\":{\"cached_tokens\":3,\"cache_creation_tokens\":2},\"completion_tokens_details\":{\"reasoning_tokens\":4}}}\n\n",
            "data: [DONE]\n\n",
        ] {
            buffer.push_str(chunk);
            consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();
        }

        assert_eq!(seen, "hello");
        assert_eq!(tool_call_deltas.len(), 2);
        assert_eq!(tool_call_deltas[0].index, 0);
        assert_eq!(tool_call_deltas[0].id.as_deref(), Some("call_1"));
        assert_eq!(tool_call_deltas[0].name.as_deref(), Some("bash"));
        assert_eq!(
            tool_call_deltas[0].arguments_delta,
            "{\"title\":\"Inspect\",\"risk\":\"readonly\",\"command\":"
        );
        assert!(tool_call_deltas[1].name.is_none());
        assert_eq!(tool_call_deltas[1].arguments_delta, "\"pwd\"}");
        assert_eq!(state.tool_accum.get(&0).unwrap().1.as_deref(), Some("bash"));
        assert_eq!(state.content, "hello");
        assert_eq!(state.finish_reason, FinishReason::ToolCalls);
        let usage = state.usage.unwrap();
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(usage.cache_write_input_tokens, Some(2));
        assert_eq!(usage.total_tokens, 17);
    }

    #[test]
    fn accepts_standard_usage_chunk_with_empty_choices() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut buffer = concat!(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5,\"total_tokens\":17}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();
        let mut state = StreamParseState::default();

        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

        let usage = state.usage.unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.total_tokens, 17);
        assert_eq!(usage.cache_write_input_tokens, None);
    }

    #[test]
    fn maps_deepseek_prompt_cache_hit_and_miss_usage() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut buffer = concat!(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5,\"total_tokens\":17,\"prompt_cache_hit_tokens\":7,\"prompt_cache_miss_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();
        let mut state = StreamParseState::default();
        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

        let usage = state.usage.unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_input_tokens, 7);
        assert_eq!(usage.visible_input_tokens(), 5);
    }

    #[test]
    fn ignores_metadata_chunk_without_choices() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut buffer = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.4-mini\"}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();
        let mut state = StreamParseState::default();

        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

        assert!(state.content.is_empty());
        assert!(state.tool_accum.is_empty());
        assert!(state.usage.is_none());
    }

    #[test]
    fn reports_in_stream_error_payload() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut buffer =
            "data: {\"error\":{\"message\":\"upstream unavailable\",\"type\":\"server_error\"}}\n\n"
                .to_string();
        let mut state = StreamParseState::default();

        let error = consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap_err();

        assert!(matches!(
            error,
            ProviderError::Other(message) if message == "stream error: upstream unavailable"
        ));
    }

    #[test]
    fn preserves_done_marker_in_model_content() {
        let mut seen = String::new();
        let mut on_event = |event: StreamEvent| -> Result<(), ProviderError> {
            if let StreamEvent::TextDelta(delta) = event {
                seen.push_str(&delta);
            }
            Ok(())
        };
        let mut buffer = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"[DONE]\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();
        let mut state = StreamParseState::default();

        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

        assert_eq!(seen, "[DONE]");
        assert_eq!(state.content, "[DONE]");
        assert_eq!(state.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn reasoning_content_emits_start_delta_and_end() {
        let mut seen_events = Vec::new();
        let mut on_event = |event: StreamEvent| -> Result<(), ProviderError> {
            match event {
                StreamEvent::ReasoningStart => seen_events.push("reasoning_start".to_string()),
                StreamEvent::ReasoningDelta(text) => {
                    seen_events.push(format!("reasoning_delta:{text}"))
                }
                StreamEvent::ReasoningEnd => seen_events.push("reasoning_end".to_string()),
                StreamEvent::TextDelta(text) => seen_events.push(format!("text:{text}")),
                StreamEvent::ToolCallDelta(_) | StreamEvent::Tick => {}
            }
            Ok(())
        };

        let mut buffer = String::new();
        let mut state = StreamParseState::default();

        for chunk in [
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":[{\"type\":\"reasoning_text\",\"text\":\"step 1\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ] {
            buffer.push_str(chunk);
            consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();
        }

        assert_eq!(
            seen_events,
            vec![
                "reasoning_start".to_string(),
                "reasoning_delta:step 1".to_string(),
                "reasoning_end".to_string(),
                "text:done".to_string(),
            ]
        );
        assert!(!state.reasoning_active);
    }

    #[test]
    fn preserves_reasoning_content_verbatim_across_stream_chunks() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut buffer = String::new();
        let mut state = StreamParseState::default();

        for chunk in [
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"  first line\\n\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\\tsecond line  \"},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ] {
            buffer.push_str(chunk);
            consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();
        }

        assert_eq!(state.reasoning_content, "  first line\n\tsecond line  ");
    }

    #[test]
    fn request_includes_reasoning_effort_when_set() {
        let body = build_chat_request_body(
            &RequestOptions {
                model: test_model(Some("provider-custom")),
            },
            CHAT_ENDPOINT,
            &[],
            &[],
        );

        assert_eq!(body["reasoning_effort"], "provider-custom");
    }

    #[test]
    fn serializes_image_and_audio_attachments_for_chat_completions() {
        let messages = vec![Message::User {
            content: UserContent::Parts(vec![
                ContentPart::Text {
                    text: "inspect these".into(),
                },
                ContentPart::Attachment {
                    attachment: crate::provider::Attachment {
                        filename: "pixel.png".into(),
                        media_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    },
                },
                ContentPart::Attachment {
                    attachment: crate::provider::Attachment {
                        filename: "beeps.wav".into(),
                        media_type: "audio/wav".into(),
                        data: vec![4, 5, 6],
                    },
                },
            ]),
        }];

        let body = build_chat_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            CHAT_ENDPOINT,
            &messages,
            &[],
        );

        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
        assert_eq!(
            body["messages"][0]["content"][2],
            serde_json::json!({
                "type": "input_audio",
                "input_audio": { "data": "BAUG", "format": "wav" }
            })
        );
    }

    #[test]
    fn responses_request_is_stateless_and_transforms_tools_effort_and_images() {
        let messages = vec![Message::User {
            content: UserContent::Parts(vec![
                ContentPart::Text {
                    text: "inspect".into(),
                },
                ContentPart::Attachment {
                    attachment: crate::provider::Attachment {
                        filename: "pixel.png".into(),
                        media_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    },
                },
            ]),
        }];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "run a command",
                "parameters": {"type": "object"},
                "strict": true
            }
        })];
        let body = build_responses_request_body(
            &RequestOptions {
                model: test_model(Some("max")),
            },
            "https://api.test/v1/responses",
            &messages,
            &tools,
        )
        .unwrap();

        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["reasoning"]["effort"], "max");
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("conversation").is_none());
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["strict"], true);
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,AQID"
        );
    }

    #[test]
    fn responses_replays_matching_items_and_projects_mismatched_semantics() {
        let native_items = vec![
            serde_json::json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}),
            serde_json::json!({"type":"function_call","call_id":"call_1","name":"bash","arguments":"{}"}),
        ];
        let assistant = Message::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            }]),
            native_replay: Some(NativeReplay {
                endpoint: "https://api.test/v1/responses".into(),
                model: "gpt-test".into(),
                payload: NativeReplayPayload::ResponsesOutput(native_items.clone()),
            }),
        };
        let messages = vec![
            assistant,
            Message::Tool {
                content: "ok".into(),
                tool_call_id: "call_1".into(),
            },
        ];
        let matching = build_responses_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            "https://api.test/v1/responses",
            &messages,
            &[],
        )
        .unwrap();
        assert_eq!(matching["input"][0], native_items[0]);
        assert_eq!(matching["input"][1], native_items[1]);
        assert_eq!(matching["input"][2]["type"], "function_call_output");

        let switched = build_responses_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            "https://other.test/responses",
            &messages,
            &[],
        )
        .unwrap();
        assert_eq!(switched["input"][0]["type"], "function_call");
        assert!(switched.to_string().find("opaque").is_none());
    }

    #[test]
    fn switching_between_apis_keeps_semantics_without_foreign_native_state() {
        let call = ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        };
        let from_chat = Message::Assistant {
            content: None,
            reasoning_content: Some("private chat reasoning".into()),
            tool_calls: Some(vec![call.clone()]),
            native_replay: Some(NativeReplay {
                endpoint: CHAT_ENDPOINT.into(),
                model: "gpt-test".into(),
                payload: NativeReplayPayload::ChatReasoning("private chat reasoning".into()),
            }),
        };
        let responses = build_responses_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            "https://api.test/v1/responses",
            &[from_chat],
            &[],
        )
        .unwrap();
        assert_eq!(responses["input"][0]["type"], "function_call");
        assert!(!responses.to_string().contains("private chat reasoning"));

        let from_responses = Message::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![call]),
            native_replay: Some(NativeReplay {
                endpoint: "https://api.test/v1/responses".into(),
                model: "gpt-test".into(),
                payload: NativeReplayPayload::ResponsesOutput(vec![serde_json::json!({
                    "type": "reasoning", "encrypted_content": "opaque"
                })]),
            }),
        };
        let chat = build_chat_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            CHAT_ENDPOINT,
            &[from_responses],
            &[],
        );
        assert_eq!(chat["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert!(!chat.to_string().contains("opaque"));
    }

    #[test]
    fn responses_rejects_audio_locally() {
        let error = build_responses_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            "https://api.test/v1/responses",
            &[Message::User {
                content: UserContent::Parts(vec![ContentPart::Attachment {
                    attachment: crate::provider::Attachment {
                        filename: "sound.wav".into(),
                        media_type: "audio/wav".into(),
                        data: vec![1],
                    },
                }]),
            }],
            &[],
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("do not support audio attachment `sound.wav`")
        );
    }

    #[test]
    fn parses_responses_tool_stream_usage_and_exact_completed_output() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut on_event = |event| {
            events.push(event);
            Ok(())
        };
        let output = serde_json::json!([
            {"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[]},
            {"type":"function_call","id":"fc_1","call_id":"call_1","name":"bash","arguments":"{\"command\":\"pwd\"}"}
        ]);
        let mut buffer = format!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"reasoning\",\"id\":\"rs_1\"}}}}\n\n\
             data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"reasoning\"}}}}\n\n\
             data: {{\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\",\"arguments\":\"\"}}}}\n\n\
             data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{{\\\"command\\\":\\\"pwd\\\"}}\"}}\n\n\
             data: {{\"type\":\"response.completed\",\"response\":{{\"output\":{},\"usage\":{{\"input_tokens\":20,\"input_tokens_details\":{{\"cached_tokens\":8}},\"output_tokens\":7,\"output_tokens_details\":{{\"reasoning_tokens\":4}},\"total_tokens\":27}}}}}}\n\n",
            output
        );
        consume_responses_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

        assert!(state.terminal);
        assert!(state.replayable);
        assert_eq!(state.output, output.as_array().unwrap().clone());
        let usage = state.usage.unwrap();
        assert_eq!(usage.cache_read_input_tokens, 8);
        assert_eq!(usage.reasoning_output_tokens, 4);
        assert!(matches!(events[0], StreamEvent::ReasoningStart));
        assert!(events.iter().any(|event| matches!(event,
            StreamEvent::ToolCallDelta(delta) if delta.id.as_deref() == Some("call_1")
        )));
        assert_eq!(
            responses_tool_calls(&state.output)[0].function.arguments,
            "{\"command\":\"pwd\"}"
        );
    }

    #[test]
    fn responses_incomplete_maps_finish_reason_without_replayable_completion() {
        let mut state = ResponsesStreamState::default();
        let mut on_event = |_event| Ok(());
        let mut buffer = "data: {\"type\":\"response.incomplete\",\"response\":{\"output\":[],\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5}}}\n\n".to_string();
        consume_responses_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

        assert!(state.terminal);
        assert!(!state.replayable);
        assert!(matches!(state.finish_reason,
            Some(FinishReason::Other(ref reason)) if reason == "max_output_tokens"
        ));
    }

    #[test]
    fn replays_chat_reasoning_only_for_matching_origin() {
        let messages = vec![Message::Assistant {
            content: None,
            reasoning_content: Some("  exact\\ntrace  ".into()),
            tool_calls: None,
            native_replay: Some(NativeReplay {
                endpoint: CHAT_ENDPOINT.into(),
                model: "gpt-test".into(),
                payload: NativeReplayPayload::ChatReasoning("  exact\\ntrace  ".into()),
            }),
        }];

        let matching = build_chat_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            CHAT_ENDPOINT,
            &messages,
            &[],
        );
        assert_eq!(
            matching["messages"][0]["reasoning_content"],
            "  exact\\ntrace  "
        );
        let mismatched = build_chat_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            "https://other.test/chat/completions",
            &messages,
            &[],
        );
        assert!(mismatched["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn detects_context_length_errors_across_shapes() {
        // Structured OpenAI-style code.
        assert!(is_context_length_error(
            400,
            r#"{"error":{"message":"too long","code":"context_length_exceeded"}}"#
        ));
        // HTTP 413 regardless of body.
        assert!(is_context_length_error(413, "Payload Too Large"));
        // Anthropic-style prose on a 400.
        assert!(is_context_length_error(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#
        ));
        // Generic phrasing.
        assert!(is_context_length_error(
            400,
            "This model's maximum context length is 128000 tokens"
        ));
    }

    #[test]
    fn does_not_misclassify_unrelated_errors() {
        // Unrelated 400 (bad request) must not be treated as overflow.
        assert!(!is_context_length_error(
            400,
            r#"{"error":{"message":"invalid 'model' parameter","code":"model_not_found"}}"#
        ));
        // A 5xx whose body coincidentally mentions context length must not
        // trigger reactive compaction.
        assert!(!is_context_length_error(
            500,
            "internal error in context length calculator"
        ));
        // 401 auth failure.
        assert!(!is_context_length_error(401, "invalid api key"));
    }

    #[tokio::test]
    async fn stalled_stream_trips_idle_timeout() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server: accept, send a valid SSE prelude and one partial chunk, then
        // hold the socket open forever without sending the boundary or [DONE].
        // The response is close-delimited (no Content-Length / chunked framing),
        // so reqwest keeps reading until EOF — which never comes, modeling a
        // black-holed connection.
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            // Read at least the request line/headers so the client finishes
            // sending before we respond.
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Connection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            // Stall: never send more bytes and never close. The client observes
            // silence rather than EOF.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let mut provider =
            OpenAiProvider::new(format!("http://{addr}/chat/completions"), None).unwrap();
        provider.idle_timeout = Duration::from_millis(200);

        let request = RequestOptions {
            model: test_model(None),
        };
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let result = provider
            .stream_chat(&request, &[], &[], &mut on_event)
            .await;

        server.abort();

        match result {
            Err(ProviderError::Transport(message)) => {
                assert!(
                    message.contains("idle"),
                    "unexpected transport error: {message}"
                );
            }
            other => panic!("expected transport idle-timeout error, got {other:?}"),
        }
    }
}
