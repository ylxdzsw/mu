use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::provider::{
    AssistantItem, ContentPart, FinishReason, HttpProvider, Message, NativeReplay,
    NativeReplayPayload, ProviderError, ReasoningVisibility, Request, SseEvent, StreamEvent,
    StreamResult, ToolCall, ToolCallDelta as ProviderToolCallDelta, Usage, UserContent,
    base64_encode, classify_stream_error, next_event_boundary, validate_completed_tool_arguments,
};

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
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize, Default)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageJson {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetailsJson>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetailsJson>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetailsJson {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionTokensDetailsJson {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

type ToolCallAccumulator = BTreeMap<usize, (Option<String>, Option<String>, String)>;

struct StreamParseState {
    content: String,
    reasoning_content: String,
    reasoning_content_present: bool,
    tool_accum: ToolCallAccumulator,
    finish_reason: FinishReason,
    terminal_finish_seen: bool,
    usage: Option<Usage>,
    reasoning_active: bool,
    tool_call_started: bool,
}

impl Default for StreamParseState {
    fn default() -> Self {
        Self {
            content: String::new(),
            reasoning_content: String::new(),
            reasoning_content_present: false,
            tool_accum: BTreeMap::new(),
            finish_reason: FinishReason::Stop,
            terminal_finish_seen: false,
            usage: None,
            reasoning_active: false,
            tool_call_started: false,
        }
    }
}

pub(crate) async fn stream(
    provider: &HttpProvider,
    request: &Request,
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<StreamResult, ProviderError> {
    let body = request.json(crate::provider::ModelApi::ChatCompletions)?;
    let mut state = StreamParseState::default();
    provider
        .stream_sse(&body, &mut |event| match event {
            SseEvent::Tick => on_event(StreamEvent::Tick),
            SseEvent::Data(data) => {
                let mut frame = format!("data: {data}\n\n");
                consume_sse_buffer(&mut frame, &mut state, on_event)
            }
        })
        .await?;
    if state.reasoning_active {
        on_event(StreamEvent::ReasoningEnd)?;
    }
    state.finish_reason = finalized_finish_reason(&state);

    let has_tool_calls = !state.tool_accum.is_empty();
    let validate_arguments = matches!(state.finish_reason, FinishReason::ToolCalls);
    let tool_calls = has_tool_calls
        .then(|| completed_tool_calls(state.tool_accum, validate_arguments))
        .transpose()?;
    let content = (!state.content.is_empty()).then_some(state.content);
    let reasoning_content = state
        .reasoning_content_present
        .then_some(state.reasoning_content.clone());
    let native_response = Some(serde_json::json!({
        "object": "chat.completion",
        "model": request.model.model_id,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "reasoning_content": reasoning_content,
                "tool_calls": tool_calls.as_ref().map(|calls| calls.iter().map(|call| serde_json::json!({
                    "id": &call.id,
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": &call.arguments,
                    },
                })).collect::<Vec<_>>()),
            },
            "finish_reason": match &state.finish_reason {
                FinishReason::Stop => "stop",
                FinishReason::ToolCalls => "tool_calls",
                FinishReason::Resume => "stop",
                FinishReason::Other(reason) => reason,
            },
        }],
        "usage": state.usage.as_ref().map(|usage| serde_json::json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
            "prompt_tokens_details": {
                "cached_tokens": usage.cache_read_input_tokens,
                "cache_creation_tokens": usage.cache_write_input_tokens,
            },
            "completion_tokens_details": {
                "reasoning_tokens": usage.reasoning_output_tokens,
            },
        })),
    }));
    let native_replay = (state.reasoning_content_present && has_tool_calls).then(|| NativeReplay {
        provider_id: request.model.provider_id.clone(),
        endpoint: provider.endpoint.clone(),
        model: request.model.model_id.clone(),
        payload: NativeReplayPayload::ChatReasoning(state.reasoning_content),
    });
    let message = Message::assistant(content, reasoning_content, tool_calls, native_replay);
    Ok(StreamResult {
        message,
        finish_reason: state.finish_reason,
        usage: state.usage,
        native_response,
    })
}

pub(crate) fn build_request_body(request: &Request, tools: &[Value]) -> Value {
    let mut body = serde_json::json!({
        "model": request.model.model_id.as_str(),
        "messages": chat_messages_json(&request.messages),
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    if let Some(cache_key) = request.cache_key.as_deref() {
        body["prompt_cache_key"] = Value::String(cache_key.to_string());
    }
    if let Some(effort) = request.model.effort.as_deref() {
        // Chat Completions uses a top-level `reasoning_effort` string. (The
        // nested `reasoning: { effort }` object is the Responses API shape and
        // is rejected by real OpenAI `/chat/completions`.)
        body["reasoning_effort"] = Value::String(effort.to_string());
    }
    body
}

fn chat_messages_json(messages: &[Message]) -> Vec<Value> {
    let mut serialized = Vec::new();
    let mut pending_tool_attachments = Vec::new();
    for message in messages {
        let mut values = chat_message_json(message);
        if matches!(message, Message::Tool { .. }) {
            serialized.push(values.remove(0));
            pending_tool_attachments.extend(values);
        } else {
            serialized.append(&mut pending_tool_attachments);
            serialized.append(&mut values);
        }
    }
    serialized.append(&mut pending_tool_attachments);
    serialized
}

fn chat_message_json(message: &Message) -> Vec<Value> {
    match message {
        Message::System { content } => vec![serde_json::json!({
            "role": "system",
            "content": content,
        })],
        Message::User { content } => vec![serde_json::json!({
            "role": "user",
            "content": user_content_json(content),
        })],
        Message::Assistant {
            items,
            native_replay,
        } => {
            let content = items
                .iter()
                .filter_map(|item| match item {
                    AssistantItem::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            let content = items
                .iter()
                .any(|item| matches!(item, AssistantItem::Text { .. }))
                .then_some(content);
            let tool_calls = items.iter().filter_map(|item| match item {
                AssistantItem::BashCall(call) => Some(call),
                _ => None,
            });
            let mut value = serde_json::json!({
                "role": "assistant",
                "content": content,
            });
            if let Some(NativeReplay {
                payload: NativeReplayPayload::ChatReasoning(reasoning),
                ..
            }) = native_replay.as_ref()
            {
                value["reasoning_content"] = Value::String(reasoning.clone());
            }
            let tool_calls = tool_calls.collect::<Vec<_>>();
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .into_iter()
                        .map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": &call.arguments,
                                },
                            })
                        })
                        .collect(),
                );
            }
            vec![value]
        }
        Message::Tool {
            content,
            attachments,
            tool_call_id,
        } => {
            let mut messages = vec![serde_json::json!({
                "role": "tool",
                "content": content,
                "tool_call_id": tool_call_id,
            })];
            if !attachments.is_empty() {
                let mut content = vec![serde_json::json!({
                    "type": "text",
                    "text": format!(
                        "Images returned by the preceding tool call `{tool_call_id}`."
                    ),
                })];
                content.extend(attachments.iter().map(|attachment| {
                    serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!(
                                "data:{};base64,{}",
                                attachment.attachment.media_type,
                                base64_encode(&attachment.attachment.data)
                            ),
                            "detail": attachment.detail.to_string(),
                        },
                    })
                }));
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content,
                }));
            }
            messages
        }
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
                serde_json::from_str(data).map_err(|e| ProviderError::Protocol(e.to_string()))?;

            if let Some(error) = parsed.error {
                return Err(classify_stream_error(&error));
            }

            if let Some(u) = parsed.usage {
                let prompt_tokens_details = u.prompt_tokens_details.unwrap_or_default();
                let completion_tokens_details = u.completion_tokens_details.unwrap_or_default();
                let prompt_tokens = u.prompt_tokens.unwrap_or(0);
                let completion_tokens = u.completion_tokens.unwrap_or(0);
                let prompt_cache_hit_tokens = u.prompt_cache_hit_tokens.unwrap_or(0);
                let prompt_cache_miss_tokens = u.prompt_cache_miss_tokens.unwrap_or(0);
                let cache_read = prompt_tokens_details
                    .cached_tokens
                    .unwrap_or(0)
                    .max(prompt_cache_hit_tokens);
                let input_tokens = prompt_tokens
                    .max(prompt_cache_hit_tokens.saturating_add(prompt_cache_miss_tokens));
                let total_tokens = u
                    .total_tokens
                    .unwrap_or_else(|| input_tokens.saturating_add(completion_tokens));
                state.usage = Some(Usage {
                    input_tokens,
                    cache_read_input_tokens: cache_read,
                    cache_write_input_tokens: prompt_tokens_details.cache_creation_tokens,
                    output_tokens: completion_tokens,
                    reasoning_output_tokens: completion_tokens_details
                        .reasoning_tokens
                        .unwrap_or(0),
                    total_tokens,
                });
            }

            if let Some(choice) = parsed.choices.first() {
                if choice.delta.reasoning_content.is_some() {
                    state.reasoning_content_present = true;
                }
                let reasoning_delta = choice
                    .delta
                    .reasoning_content
                    .as_ref()
                    .and_then(reasoning_text_from_value);
                if let Some(text) = reasoning_delta {
                    state.reasoning_content.push_str(&text);
                    if !state.reasoning_active {
                        on_event(StreamEvent::ReasoningStart(
                            ReasoningVisibility::StreamedTrace,
                        ))?;
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
                            .or_insert_with(|| (None, None, String::new()));
                        if let Some(id) = tc.id.as_deref().filter(|id| !id.is_empty()) {
                            entry.0 = Some(id.to_string());
                        }
                        if let Some(ref f) = tc.function {
                            if let Some(name) = f.name.as_deref().filter(|name| !name.is_empty()) {
                                entry.1 = Some(name.to_string());
                            }
                            if let Some(ref args) = f.arguments {
                                entry.2.push_str(args);
                            }
                        }
                        let arguments_delta = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.arguments.clone())
                            .unwrap_or_default();
                        on_event(StreamEvent::ToolCallDelta(ProviderToolCallDelta {
                            index: tc.index,
                            arguments_delta,
                        }))?;
                        state.tool_call_started = true;
                    }
                }
                if let Some(ref reason) = choice.finish_reason {
                    state.terminal_finish_seen = true;
                    state.finish_reason = match reason.as_str() {
                        "stop" => FinishReason::Stop,
                        "tool_calls" => FinishReason::ToolCalls,
                        // Some gateways encode transport failures as terminal finish reasons.
                        "network_error" => {
                            return Err(ProviderError::Transport(
                                "provider ended response with finish_reason=network_error".into(),
                            ));
                        }
                        other => FinishReason::Other(other.to_string()),
                    };
                }
            }
        }
    }

    Ok(())
}

fn finalized_finish_reason(state: &StreamParseState) -> FinishReason {
    if state.terminal_finish_seen
        && state.finish_reason == FinishReason::Stop
        && !state.reasoning_content.is_empty()
        && state.content.is_empty()
        && state.tool_accum.is_empty()
    {
        FinishReason::Resume
    } else {
        state.finish_reason.clone()
    }
}

fn completed_tool_calls(
    tool_accum: ToolCallAccumulator,
    validate_arguments: bool,
) -> Result<Vec<ToolCall>, ProviderError> {
    tool_accum
        .into_values()
        .map(|(id, name, arguments)| {
            let id = id.unwrap_or_else(|| "call".into());
            let name = name.unwrap_or_default();
            if name != "bash" {
                return Err(ProviderError::Protocol(format!(
                    "Chat Completions tool call `{id}` calls unsupported tool `{name}`"
                )));
            }
            let arguments = if validate_arguments {
                validate_completed_tool_arguments(&arguments)?
            } else {
                arguments
            };
            Ok(ToolCall { id, arguments })
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ResolvedModelRef;
    use crate::provider::{ModelApi, Provider};
    use crate::responses::{ResponsesStreamState, consume_responses_sse_buffer};
    use std::time::Duration;
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

    fn request(effort: Option<&str>, messages: Vec<Message>, bash: bool) -> Request {
        Request {
            model: test_model(effort),
            cache_key: None,
            messages,
            bash,
        }
    }

    fn body(
        api: ModelApi,
        effort: Option<&str>,
        messages: Vec<Message>,
        bash: bool,
    ) -> Result<Value, ProviderError> {
        request(effort, messages, bash).json(api)
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
        assert_eq!(
            tool_call_deltas[0].arguments_delta,
            "{\"title\":\"Inspect\",\"risk\":\"readonly\",\"command\":"
        );
        assert_eq!(tool_call_deltas[1].arguments_delta, "\"pwd\"}");
        assert_eq!(state.finish_reason, FinishReason::ToolCalls);
        let usage = state.usage.unwrap();
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(usage.cache_write_input_tokens, Some(2));
        assert_eq!(usage.total_tokens, 17);
    }

    #[test]
    fn distinguishes_omitted_reasoning_from_explicit_empty_reasoning() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut buffer =
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\"},\"finish_reason\":null}]}\n\n"
                .to_string();
        let mut state = StreamParseState::default();
        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();
        assert!(state.reasoning_content_present);
        assert!(state.reasoning_content.is_empty());

        let mut buffer =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":null}]}\n\n".to_string();
        let mut state = StreamParseState::default();
        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();
        assert!(!state.reasoning_content_present);
    }

    #[test]
    fn accepts_usage_chunk_with_empty_choices_and_null_details() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut buffer = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3,\"prompt_tokens_details\":null,\"completion_tokens_details\":null}}\n\n".to_string();
        let mut state = StreamParseState::default();
        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();
        let usage = state.usage.unwrap();
        assert_eq!(usage.total_tokens, 11);

        let mut buffer =
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":\"8\"}}\n\n".to_string();
        assert!(matches!(
            consume_sse_buffer(&mut buffer, &mut StreamParseState::default(), &mut on_event),
            Err(ProviderError::Protocol(_))
        ));
    }

    #[test]
    fn validates_only_completed_tool_call_arguments() {
        let calls = |arguments: &str| {
            BTreeMap::from([(
                0,
                (Some("call_1".into()), Some("bash".into()), arguments.into()),
            )])
        };

        assert_eq!(
            completed_tool_calls(calls("  \n"), true).unwrap()[0].arguments,
            "{}"
        );
        let preserved = "  {\"command\":\"pwd\"} \n";
        assert_eq!(
            completed_tool_calls(calls(preserved), true).unwrap()[0].arguments,
            preserved
        );
        for invalid in ["{", "[]", "\"text\"", "1", "true", "null"] {
            assert!(
                matches!(
                    completed_tool_calls(calls(invalid), true),
                    Err(ProviderError::Protocol(_))
                ),
                "accepted {invalid}"
            );
        }
        assert!(matches!(
            completed_tool_calls(
                calls(r#"{"command":"pwd","command":"whoami"}"#),
                true
            ),
            Err(ProviderError::Protocol(message)) if message.contains("duplicate key `command`")
        ));
        assert_eq!(
            completed_tool_calls(calls("{"), false).unwrap()[0].arguments,
            "{"
        );
        assert!(matches!(
            completed_tool_calls(
                BTreeMap::from([(
                    0,
                    (Some("call_1".into()), Some("python".into()), "{}".into())
                )]),
                true
            ),
            Err(ProviderError::Protocol(_))
        ));
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
    fn classifies_in_stream_error_payload() {
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let mut frame =
            "data: {\"error\":{\"message\":\"upstream unavailable\",\"type\":\"server_error\"}}\n\n"
                .to_string();
        assert!(matches!(
            consume_sse_buffer(
                &mut frame,
                &mut StreamParseState::default(),
                &mut on_event
            ),
            Err(ProviderError::Overloaded { detail, .. }) if detail == "upstream unavailable"
        ));

        let mut frame =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"network_error\"}]}\n\n"
                .to_string();
        assert!(matches!(
            consume_sse_buffer(
                &mut frame,
                &mut StreamParseState::default(),
                &mut on_event
            ),
            Err(ProviderError::Transport(message)) if message.contains("network_error")
        ));
    }

    #[test]
    fn reasoning_only_stop_is_resumable() {
        let mut state = StreamParseState::default();
        let mut frame = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"still working\"},\"finish_reason\":\"stop\"}]}\n\n".to_string();
        consume_sse_buffer(&mut frame, &mut state, &mut |_| Ok(())).unwrap();

        assert_eq!(finalized_finish_reason(&state), FinishReason::Resume);
    }

    #[test]
    fn reasoning_content_emits_start_delta_and_end() {
        let mut events = Vec::new();
        let mut on_event = |event: StreamEvent| -> Result<(), ProviderError> {
            events.push(event);
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

        assert!(matches!(
            events.as_slice(),
            [
                StreamEvent::ReasoningStart(ReasoningVisibility::StreamedTrace),
                StreamEvent::ReasoningDelta(reasoning),
                StreamEvent::ReasoningEnd,
                StreamEvent::TextDelta(text),
            ] if reasoning == "step 1" && text == "done"
        ));
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

        let body = body(
            ModelApi::ChatCompletions,
            Some("provider-custom"),
            messages,
            false,
        )
        .unwrap();

        assert_eq!(body["reasoning_effort"], "provider-custom");
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
        let body = body(ModelApi::Responses, Some("max"), messages, true).unwrap();

        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("conversation").is_none());
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,AQID"
        );
    }

    #[test]
    fn responses_replays_native_items_exactly() {
        let native_items = vec![
            serde_json::json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}),
            serde_json::json!({"type":"function_call","call_id":"call_1","name":"bash","arguments":"{}"}),
        ];
        let assistant = Message::assistant(
            None,
            None,
            Some(vec![ToolCall {
                id: "call_1".into(),
                arguments: "{}".into(),
            }]),
            Some(NativeReplay {
                provider_id: "test".into(),
                endpoint: "https://api.test/v1/responses".into(),
                model: "gpt-test".into(),
                payload: NativeReplayPayload::ResponsesOutput(native_items.clone()),
            }),
        );
        let messages = vec![
            assistant,
            Message::Tool {
                content: "ok".into(),
                attachments: Vec::new(),
                tool_call_id: "call_1".into(),
            },
        ];
        let matching = body(ModelApi::Responses, None, messages, false).unwrap();
        assert_eq!(matching["input"][0], native_items[0]);
        assert_eq!(matching["input"][1], native_items[1]);
        assert_eq!(matching["input"][2]["type"], "function_call_output");
    }

    #[test]
    fn serializes_tool_images_natively_for_responses_and_as_chat_fallback() {
        let attachment = crate::provider::ToolAttachment {
            attachment: crate::provider::Attachment {
                filename: "tool.png".into(),
                media_type: "image/png".into(),
                data: b"png".to_vec(),
            },
            detail: crate::provider::ImageDetail::Original,
            object_sha256: None,
        };
        let messages = vec![Message::Tool {
            content: "Viewed image".into(),
            attachments: vec![attachment],
            tool_call_id: "call-image".into(),
        }];

        let responses = body(ModelApi::Responses, None, messages.clone(), false).unwrap();
        assert_eq!(responses["input"][0]["output"][0]["type"], "input_text");
        assert_eq!(responses["input"][0]["output"][1]["type"], "input_image");
        assert_eq!(responses["input"][0]["output"][1]["detail"], "original");

        let chat = body(ModelApi::ChatCompletions, None, messages, false).unwrap();
        assert_eq!(chat["messages"][0]["role"], "tool");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(
            chat["messages"][1]["content"][1]["image_url"]["detail"],
            "original"
        );
    }

    #[test]
    fn chat_serializes_parallel_tool_replies_before_image_fallbacks() {
        let attachment = |filename: &str| crate::provider::ToolAttachment {
            attachment: crate::provider::Attachment {
                filename: filename.into(),
                media_type: "image/png".into(),
                data: b"png".to_vec(),
            },
            detail: crate::provider::ImageDetail::Auto,
            object_sha256: None,
        };
        let messages = vec![
            Message::Tool {
                content: "first".into(),
                attachments: vec![attachment("first.png")],
                tool_call_id: "call-1".into(),
            },
            Message::Tool {
                content: "second".into(),
                attachments: vec![attachment("second.png")],
                tool_call_id: "call-2".into(),
            },
        ];
        let chat = body(ModelApi::ChatCompletions, None, messages, false).unwrap();
        assert_eq!(chat["messages"][0]["tool_call_id"], "call-1");
        assert_eq!(chat["messages"][1]["tool_call_id"], "call-2");
        assert_eq!(chat["messages"][2]["role"], "user");
        assert_eq!(chat["messages"][3]["role"], "user");
    }

    #[test]
    fn responses_rejects_audio_locally() {
        let error = body(
            ModelApi::Responses,
            None,
            vec![Message::User {
                content: UserContent::Parts(vec![ContentPart::Attachment {
                    attachment: crate::provider::Attachment {
                        filename: "sound.wav".into(),
                        media_type: "audio/wav".into(),
                        data: vec![1],
                    },
                }]),
            }],
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("do not support audio attachment `sound.wav`")
        );
    }

    #[test]
    fn streams_every_summary_part_for_the_active_reasoning_item() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut on_event = |event| {
            events.push(event);
            Ok(())
        };
        let mut buffer = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":1,\"summary_index\":0,\"delta\":\"ignored output\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":2,\"summary_index\":1,\"delta\":\"ignored part\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":2,\"summary_index\":0,\"delta\":\"**Inspecting\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":2,\"summary_index\":0,\"delta\":\" renderer**\\n\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"reasoning\"}}\n\n",
        )
        .to_string();

        consume_responses_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

        assert!(matches!(
            events.first(),
            Some(StreamEvent::ReasoningStart(ReasoningVisibility::Opaque))
        ));
        assert!(matches!(
            events.get(1),
            Some(StreamEvent::ReasoningSummaryDelta { part_index: 1, text })
                if text == "ignored part"
        ));
        assert!(matches!(
            events.get(2),
            Some(StreamEvent::ReasoningSummaryDelta { part_index: 0, text })
                if text == "**Inspecting"
        ));
        assert!(matches!(
            events.get(3),
            Some(StreamEvent::ReasoningSummaryDelta { part_index: 0, text })
                if text == " renderer**\n"
        ));
        assert!(matches!(events.get(4), Some(StreamEvent::ReasoningEnd)));
        assert_eq!(events.len(), 5);
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
            HttpProvider::new(format!("http://{addr}/chat/completions"), None).unwrap();
        provider.idle_timeout = Duration::from_millis(200);

        let request = request(None, vec![], false);
        let mut on_event = |_event: StreamEvent| -> Result<(), ProviderError> { Ok(()) };
        let result = provider.stream(&request, &mut on_event).await;

        server.abort();

        assert!(matches!(result, Err(ProviderError::Transport(_))));
    }

    #[tokio::test]
    async fn streams_over_http_unix_endpoint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        let tmp = crate::random::create_temp_dir(&std::env::temp_dir(), "mu-http-unix-").unwrap();
        let socket_path = tmp.join("provider.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }

            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"socket ok\"},",
                "\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        let encoded_socket = socket_path.to_str().unwrap().replace('/', "%2F");
        let endpoint = format!("http+unix://{encoded_socket}/chat/completions?route=local");
        let provider = HttpProvider::new(endpoint, None).unwrap();

        let mut text = String::new();
        let mut on_event = |event| {
            if let StreamEvent::TextDelta(delta) = event {
                text.push_str(&delta);
            }
            Ok(())
        };
        provider
            .stream(&request(None, vec![], false), &mut on_event)
            .await
            .unwrap();

        let wire_request = server.await.unwrap();
        assert!(wire_request.starts_with("POST /chat/completions?route=local HTTP/1.1\r\n"));
        assert!(
            wire_request
                .to_ascii_lowercase()
                .contains("\r\nhost: localhost\r\n")
        );
        assert_eq!(text, "socket ok");

        std::fs::remove_dir_all(tmp).unwrap();
    }
}
