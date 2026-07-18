use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::models::RequestOptions;
use crate::provider::{
    ContentPart, FinishReason, FunctionCall, HttpProvider, Message, NativeReplay,
    NativeReplayPayload, ProviderError, ReasoningVisibility, SseEvent, StreamEvent, StreamResult,
    ToolCall, ToolCallDelta as ProviderToolCallDelta, Usage, UserContent, base64_encode,
    stream_error_message,
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

type ToolCallAccumulator = BTreeMap<usize, (Option<String>, Option<String>, String)>;

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

pub(crate) async fn stream(
    provider: &HttpProvider,
    request: &RequestOptions,
    messages: &[Message],
    tools: &[Value],
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<StreamResult, ProviderError> {
    let body = build_chat_request_body(request, &provider.endpoint, messages, tools);
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

    let has_tool_calls = !state.tool_accum.is_empty();
    let tool_calls = has_tool_calls.then(|| {
        state
            .tool_accum
            .into_values()
            .map(|(id, name, arguments)| ToolCall {
                id: id.unwrap_or_else(|| "call".into()),
                function: FunctionCall {
                    name: name.unwrap_or_default(),
                    arguments,
                },
            })
            .collect()
    });
    let message = Message::Assistant {
        content: (!state.content.is_empty()).then_some(state.content),
        reasoning_content: (!state.reasoning_content.is_empty())
            .then_some(state.reasoning_content.clone()),
        tool_calls,
        native_replay: (!state.reasoning_content.is_empty() && has_tool_calls).then(|| {
            NativeReplay {
                endpoint: provider.endpoint.clone(),
                model: request.model.model_id.clone(),
                payload: NativeReplayPayload::ChatReasoning(state.reasoning_content),
            }
        }),
    };
    Ok(StreamResult {
        message,
        finish_reason: state.finish_reason,
        usage: state.usage,
    })
}

fn build_chat_request_body(
    request: &RequestOptions,
    endpoint: &str,
    messages: &[Message],
    tools: &[Value],
) -> Value {
    let mut body = serde_json::json!({
        "model": request.model.model_id.as_str(),
        "messages": chat_messages_json(messages, endpoint, &request.model.model_id),
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

fn chat_messages_json(messages: &[Message], endpoint: &str, model: &str) -> Vec<Value> {
    let mut serialized = Vec::new();
    let mut pending_tool_artifacts = Vec::new();
    for message in messages {
        let mut values = chat_message_json(message, endpoint, model);
        if matches!(message, Message::Tool { .. }) {
            serialized.push(values.remove(0));
            pending_tool_artifacts.extend(values);
        } else {
            serialized.append(&mut pending_tool_artifacts);
            serialized.append(&mut values);
        }
    }
    serialized.append(&mut pending_tool_artifacts);
    serialized
}

fn chat_message_json(message: &Message, endpoint: &str, model: &str) -> Vec<Value> {
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
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": &call.function,
                            })
                        })
                        .collect(),
                );
            }
            vec![value]
        }
        Message::Tool {
            content,
            artifacts,
            tool_call_id,
        } => {
            let mut messages = vec![serde_json::json!({
                "role": "tool",
                "content": content,
                "tool_call_id": tool_call_id,
            })];
            if !artifacts.is_empty() {
                let mut content = vec![serde_json::json!({
                    "type": "text",
                    "text": format!(
                        "Images returned by the preceding tool call `{tool_call_id}`."
                    ),
                })];
                content.extend(artifacts.iter().map(|artifact| {
                    serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!(
                                "data:{};base64,{}",
                                artifact.attachment.media_type,
                                base64_encode(&artifact.attachment.data)
                            ),
                            "detail": artifact.detail.to_string(),
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
    use crate::provider::{Provider, is_context_length_error};
    use crate::responses::{
        ResponsesStreamState, build_responses_request_body, consume_responses_sse_buffer,
        responses_tool_calls,
    };
    use std::time::Duration;
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
                StreamEvent::ReasoningStart(ReasoningVisibility::StreamedTrace) => {
                    seen_events.push("reasoning_start".to_string())
                }
                StreamEvent::ReasoningStart(ReasoningVisibility::Opaque) => {
                    seen_events.push("opaque_reasoning_start".to_string())
                }
                StreamEvent::ReasoningDelta(text) => {
                    seen_events.push(format!("reasoning_delta:{text}"))
                }
                StreamEvent::ReasoningSummaryDelta { part_index, text } => {
                    seen_events.push(format!("reasoning_summary_delta:{part_index}:{text}"))
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
        assert_eq!(body["reasoning"]["summary"], "auto");
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
    fn responses_request_includes_summary_without_explicit_effort() {
        let body = build_responses_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            "https://api.test/v1/responses",
            &[],
            &[],
        )
        .unwrap();

        assert_eq!(body["reasoning"], serde_json::json!({"summary": "auto"}));
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
                artifacts: Vec::new(),
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
    fn serializes_tool_images_natively_for_responses_and_as_chat_fallback() {
        let artifact = crate::provider::ToolArtifact {
            attachment: crate::provider::Attachment {
                filename: "tool.png".into(),
                media_type: "image/png".into(),
                data: b"png".to_vec(),
            },
            detail: crate::provider::ImageDetail::Original,
        };
        let messages = vec![Message::Tool {
            content: "Viewed image".into(),
            artifacts: vec![artifact],
            tool_call_id: "call-image".into(),
        }];

        let responses = build_responses_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            "https://api.test/v1/responses",
            &messages,
            &[],
        )
        .unwrap();
        assert_eq!(responses["input"][0]["output"][0]["type"], "input_text");
        assert_eq!(responses["input"][0]["output"][1]["type"], "input_image");
        assert_eq!(responses["input"][0]["output"][1]["detail"], "original");

        let chat = build_chat_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            CHAT_ENDPOINT,
            &messages,
            &[],
        );
        assert_eq!(chat["messages"][0]["role"], "tool");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(
            chat["messages"][1]["content"][1]["image_url"]["detail"],
            "original"
        );
    }

    #[test]
    fn chat_serializes_parallel_tool_replies_before_image_fallbacks() {
        let artifact = |filename: &str| crate::provider::ToolArtifact {
            attachment: crate::provider::Attachment {
                filename: filename.into(),
                media_type: "image/png".into(),
                data: b"png".to_vec(),
            },
            detail: crate::provider::ImageDetail::Auto,
        };
        let messages = vec![
            Message::Tool {
                content: "first".into(),
                artifacts: vec![artifact("first.png")],
                tool_call_id: "call-1".into(),
            },
            Message::Tool {
                content: "second".into(),
                artifacts: vec![artifact("second.png")],
                tool_call_id: "call-2".into(),
            },
        ];
        let chat = build_chat_request_body(
            &RequestOptions {
                model: test_model(None),
            },
            CHAT_ENDPOINT,
            &messages,
            &[],
        );
        assert_eq!(chat["messages"][0]["tool_call_id"], "call-1");
        assert_eq!(chat["messages"][1]["tool_call_id"], "call-2");
        assert_eq!(chat["messages"][2]["role"], "user");
        assert_eq!(chat["messages"][3]["role"], "user");
    }

    #[test]
    fn switching_between_apis_keeps_semantics_without_foreign_native_state() {
        let call = ToolCall {
            id: "call_1".into(),
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
        assert_eq!(chat["messages"][0]["tool_calls"][0]["type"], "function");
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
        assert!(matches!(
            events[0],
            StreamEvent::ReasoningStart(ReasoningVisibility::Opaque)
        ));
        assert!(events.iter().any(|event| matches!(event,
            StreamEvent::ToolCallDelta(delta) if delta.id.as_deref() == Some("call_1")
        )));
        assert_eq!(
            responses_tool_calls(&state.output)[0].function.arguments,
            "{\"command\":\"pwd\"}"
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
            HttpProvider::new(format!("http://{addr}/chat/completions"), None).unwrap();
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
