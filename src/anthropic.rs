use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::models::RequestOptions;
use crate::provider::{
    Attachment, ContentPart, FinishReason, FunctionCall, HttpProvider, Message, NativeReplay,
    NativeReplayPayload, ProviderError, ReasoningVisibility, SseEvent, StreamEvent, StreamResult,
    ToolCall, ToolCallDelta, Usage, UserContent, base64_encode, stream_error_message,
};

const MAX_OUTPUT_TOKENS: u64 = 64_000;

pub(crate) async fn stream(
    provider: &HttpProvider,
    request: &RequestOptions,
    messages: &[Message],
    tools: &[Value],
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<StreamResult, ProviderError> {
    let body = build_request_body(request, &provider.endpoint, messages, tools)?;
    let mut state = AnthropicStreamState::default();
    provider
        .stream_sse(&body, &mut |event| match event {
            SseEvent::Tick => on_event(StreamEvent::Tick),
            SseEvent::Data(data) => consume_event(&data, &mut state, on_event),
        })
        .await?;

    if !state.terminal {
        return Err(ProviderError::Transport(
            "Anthropic stream ended before message_stop".into(),
        ));
    }
    let blocks = state.completed_blocks()?;
    let tool_calls = tool_calls_from_blocks(&blocks)?;
    if state.stop_reason.as_deref() == Some("tool_use") && tool_calls.is_empty() {
        return Err(ProviderError::SseParse(
            "Anthropic stopped for tool_use without a tool_use block".into(),
        ));
    }
    if state.stop_reason.as_deref() == Some("model_context_window_exceeded") {
        return Err(ProviderError::ContextLength);
    }
    let finish_reason = finish_reason(state.stop_reason.as_deref(), &tool_calls);
    let usage = state.usage.finish();
    let native_response = Some(serde_json::json!({
        "type": "message",
        "role": "assistant",
        "model": request.model.model_id,
        "content": &blocks,
        "stop_reason": &state.stop_reason,
        "stop_sequence": null,
        "usage": usage.as_ref().map(|usage| serde_json::json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
            "cache_creation_input_tokens": usage.cache_write_input_tokens,
        })),
    }));

    Ok(StreamResult {
        message: Message::Assistant {
            content: text_from_blocks(&blocks),
            reasoning_content: None,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            native_replay: Some(NativeReplay {
                provider_id: request.model.provider_id.clone(),
                endpoint: provider.endpoint.clone(),
                model: request.model.model_id.clone(),
                payload: NativeReplayPayload::AnthropicContent(blocks),
            }),
        },
        finish_reason,
        usage,
        native_response,
    })
}

pub(crate) fn build_request_body(
    request: &RequestOptions,
    endpoint: &str,
    messages: &[Message],
    tools: &[Value],
) -> Result<Value, ProviderError> {
    let mut system = None;
    let mut wire_messages = Vec::new();
    let mut saw_non_system = false;

    for message in messages {
        match message {
            Message::System { content } => {
                if saw_non_system || system.is_some() {
                    return Err(ProviderError::Other(
                        "Anthropic requires exactly one leading system message".into(),
                    ));
                }
                system = Some(content.clone());
            }
            Message::User { content } => {
                saw_non_system = true;
                append_message(&mut wire_messages, "user", user_blocks(content)?)?;
            }
            Message::Assistant {
                content,
                tool_calls,
                native_replay,
                ..
            } => {
                saw_non_system = true;
                let blocks = assistant_blocks(
                    content.as_deref(),
                    tool_calls.as_deref(),
                    native_replay.as_ref(),
                    &request.model.provider_id,
                    endpoint,
                    &request.model.model_id,
                )?;
                if !blocks.is_empty() {
                    append_message(&mut wire_messages, "assistant", blocks)?;
                }
            }
            Message::Tool {
                content,
                attachments,
                tool_call_id,
            } => {
                saw_non_system = true;
                let result_content = if attachments.is_empty() {
                    Value::String(content.clone())
                } else {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": content,
                    })];
                    for attachment in attachments {
                        if !attachment.attachment.media_type.starts_with("image/") {
                            return Err(ProviderError::Other(format!(
                                "Anthropic Messages does not support tool attachment `{}` ({})",
                                attachment.attachment.filename, attachment.attachment.media_type
                            )));
                        }
                        parts.push(image_block(&attachment.attachment));
                    }
                    Value::Array(parts)
                };
                append_message(
                    &mut wire_messages,
                    "user",
                    vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": result_content,
                    })],
                )?;
            }
        }
    }

    let system = system.ok_or_else(|| {
        ProviderError::Other("Anthropic request is missing the leading system message".into())
    })?;
    let anthropic_tools = tools.iter().map(convert_tool).collect::<Vec<_>>();
    let mut body = serde_json::json!({
        "model": request.model.model_id,
        "system": system,
        "messages": wire_messages,
        "tools": anthropic_tools,
        "stream": true,
        "max_tokens": MAX_OUTPUT_TOKENS,
        "thinking": {
            "type": "adaptive",
            "display": "summarized",
        },
        "cache_control": {
            "type": "ephemeral",
        },
    });
    if let Some(effort) = request.model.effort.as_deref() {
        body["output_config"] = serde_json::json!({ "effort": effort });
    }
    Ok(body)
}

fn convert_tool(tool: &Value) -> Value {
    let function = tool.get("function").unwrap_or(tool);
    let mut converted = serde_json::Map::new();
    for key in ["name", "description"] {
        if let Some(value) = function.get(key) {
            converted.insert(key.into(), value.clone());
        }
    }
    converted.insert(
        "input_schema".into(),
        function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
    );
    Value::Object(converted)
}

fn append_message(
    messages: &mut Vec<Value>,
    role: &str,
    mut blocks: Vec<Value>,
) -> Result<(), ProviderError> {
    if let Some(last) = messages.last_mut()
        && last["role"].as_str() == Some(role)
    {
        let content = last["content"].as_array_mut().ok_or_else(|| {
            ProviderError::Other(
                "invalid Anthropic message content while assembling request".into(),
            )
        })?;
        content.append(&mut blocks);
        return Ok(());
    }
    messages.push(serde_json::json!({
        "role": role,
        "content": blocks,
    }));
    Ok(())
}

fn user_blocks(content: &UserContent) -> Result<Vec<Value>, ProviderError> {
    match content {
        UserContent::Text(text) => Ok(vec![serde_json::json!({
            "type": "text",
            "text": text,
        })]),
        UserContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => Ok(serde_json::json!({
                    "type": "text",
                    "text": text,
                })),
                ContentPart::Attachment { attachment }
                    if attachment.media_type.starts_with("image/") =>
                {
                    Ok(image_block(attachment))
                }
                ContentPart::Attachment { attachment } => Err(ProviderError::Other(format!(
                    "Anthropic Messages does not support audio attachment `{}` ({})",
                    attachment.filename, attachment.media_type
                ))),
            })
            .collect(),
    }
}

fn image_block(attachment: &Attachment) -> Value {
    serde_json::json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": attachment.media_type,
            "data": base64_encode(&attachment.data),
        },
    })
}

fn assistant_blocks(
    content: Option<&str>,
    tool_calls: Option<&[ToolCall]>,
    native_replay: Option<&NativeReplay>,
    provider_id: &str,
    endpoint: &str,
    model: &str,
) -> Result<Vec<Value>, ProviderError> {
    if let Some(NativeReplay {
        payload: NativeReplayPayload::AnthropicContent(blocks),
        ..
    }) = native_replay.filter(|native| native.matches(provider_id, endpoint, model))
    {
        return Ok(blocks.clone());
    }

    let mut blocks = Vec::new();
    if let Some(content) = content {
        blocks.push(serde_json::json!({
            "type": "text",
            "text": content,
        }));
    }
    if let Some(tool_calls) = tool_calls {
        for call in tool_calls {
            let input: Value = serde_json::from_str(&call.function.arguments).map_err(|error| {
                ProviderError::Other(format!(
                    "invalid JSON arguments for Anthropic tool call `{}`: {error}",
                    call.id
                ))
            })?;
            if !input.is_object() {
                return Err(ProviderError::Other(format!(
                    "Anthropic tool call `{}` arguments must be a JSON object",
                    call.id
                )));
            }
            blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.function.name,
                "input": input,
            }));
        }
    }
    Ok(blocks)
}

#[derive(Default)]
struct AnthropicStreamState {
    blocks: Vec<Option<Value>>,
    open_blocks: BTreeSet<usize>,
    tool_indexes: BTreeMap<usize, usize>,
    tool_arguments: BTreeMap<usize, String>,
    reasoning_index: Option<usize>,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
    terminal: bool,
}

impl AnthropicStreamState {
    fn block_mut(&mut self, index: usize) -> Result<&mut Value, ProviderError> {
        self.blocks
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| {
                ProviderError::SseParse(format!(
                    "Anthropic delta references missing content block {index}"
                ))
            })
    }

    fn completed_blocks(&self) -> Result<Vec<Value>, ProviderError> {
        if !self.open_blocks.is_empty() {
            return Err(ProviderError::SseParse(format!(
                "Anthropic stream ended with unclosed content blocks: {:?}",
                self.open_blocks
            )));
        }
        if self.blocks.iter().any(Option::is_none) {
            return Err(ProviderError::SseParse(
                "Anthropic stream left a gap in content block indexes".into(),
            ));
        }
        Ok(self.blocks.iter().flatten().cloned().collect())
    }
}

#[derive(Default)]
struct AnthropicUsage {
    seen: bool,
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
}

impl AnthropicUsage {
    fn update(&mut self, value: &Value) {
        if !value.is_object() {
            return;
        }
        self.seen = true;
        replace_u64(&mut self.input_tokens, &value["input_tokens"]);
        replace_u64(
            &mut self.cache_creation_input_tokens,
            &value["cache_creation_input_tokens"],
        );
        replace_u64(
            &mut self.cache_read_input_tokens,
            &value["cache_read_input_tokens"],
        );
        replace_u64(&mut self.output_tokens, &value["output_tokens"]);
        replace_u64(
            &mut self.thinking_tokens,
            &value["output_tokens_details"]["thinking_tokens"],
        );
    }

    fn finish(&self) -> Option<Usage> {
        self.seen.then(|| {
            let input_tokens = self
                .input_tokens
                .saturating_add(self.cache_creation_input_tokens)
                .saturating_add(self.cache_read_input_tokens);
            Usage {
                input_tokens,
                cache_read_input_tokens: self.cache_read_input_tokens,
                cache_write_input_tokens: Some(self.cache_creation_input_tokens),
                output_tokens: self.output_tokens,
                reasoning_output_tokens: self.thinking_tokens,
                total_tokens: input_tokens.saturating_add(self.output_tokens),
            }
        })
    }
}

fn replace_u64(target: &mut u64, value: &Value) {
    if let Some(value) = value.as_u64() {
        *target = value;
    }
}

fn consume_event(
    data: &str,
    state: &mut AnthropicStreamState,
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    let value: Value =
        serde_json::from_str(data).map_err(|error| ProviderError::SseParse(error.to_string()))?;
    match value["type"].as_str().unwrap_or("") {
        "message_start" => state.usage.update(&value["message"]["usage"]),
        "content_block_start" => {
            let index = event_index(&value)?;
            if state.blocks.len() <= index {
                state.blocks.resize(index + 1, None);
            }
            if state.blocks[index].is_some() || !state.open_blocks.insert(index) {
                return Err(ProviderError::SseParse(format!(
                    "Anthropic content block {index} started more than once"
                )));
            }
            let block = value["content_block"].clone();
            match block["type"].as_str().unwrap_or("") {
                "thinking" => {
                    if state.reasoning_index.replace(index).is_some() {
                        return Err(ProviderError::SseParse(
                            "Anthropic started overlapping thinking blocks".into(),
                        ));
                    }
                    on_event(StreamEvent::ReasoningStart(ReasoningVisibility::Opaque))?;
                }
                "tool_use" => {
                    let tool_index = state.tool_indexes.len();
                    state.tool_indexes.insert(index, tool_index);
                    state.tool_arguments.insert(index, String::new());
                    on_event(StreamEvent::ToolCallDelta(ToolCallDelta {
                        index: tool_index,
                        id: block["id"].as_str().map(str::to_owned),
                        name: block["name"].as_str().map(str::to_owned),
                        arguments_delta: String::new(),
                    }))?;
                }
                _ => {}
            }
            state.blocks[index] = Some(block);
        }
        "content_block_delta" => {
            let index = event_index(&value)?;
            if !state.open_blocks.contains(&index) {
                return Err(ProviderError::SseParse(format!(
                    "Anthropic delta references closed content block {index}"
                )));
            }
            let delta = &value["delta"];
            match delta["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    let text = required_str(delta, "text", "Anthropic text delta")?;
                    append_json_string(state.block_mut(index)?, "text", text);
                    on_event(StreamEvent::TextDelta(text.to_string()))?;
                }
                "thinking_delta" => {
                    let text = required_str(delta, "thinking", "Anthropic thinking delta")?;
                    append_json_string(state.block_mut(index)?, "thinking", text);
                    on_event(StreamEvent::ReasoningSummaryDelta {
                        part_index: 0,
                        text: text.to_string(),
                    })?;
                }
                "signature_delta" => {
                    let signature = required_str(delta, "signature", "Anthropic signature delta")?;
                    append_json_string(state.block_mut(index)?, "signature", signature);
                }
                "input_json_delta" => {
                    let partial =
                        required_str(delta, "partial_json", "Anthropic tool input delta")?;
                    state
                        .tool_arguments
                        .get_mut(&index)
                        .ok_or_else(|| {
                            ProviderError::SseParse(format!(
                                "Anthropic tool delta references missing block {index}"
                            ))
                        })?
                        .push_str(partial);
                    let tool_index = state.tool_indexes.get(&index).copied().ok_or_else(|| {
                        ProviderError::SseParse(format!(
                            "Anthropic tool delta references missing block {index}"
                        ))
                    })?;
                    on_event(StreamEvent::ToolCallDelta(ToolCallDelta {
                        index: tool_index,
                        id: None,
                        name: None,
                        arguments_delta: partial.to_string(),
                    }))?;
                }
                "citations_delta" => {
                    let citation = delta["citation"].clone();
                    let block = state.block_mut(index)?;
                    if !block["citations"].is_array() {
                        block["citations"] = Value::Array(Vec::new());
                    }
                    if let Some(citations) = block["citations"].as_array_mut() {
                        citations.push(citation);
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = event_index(&value)?;
            if !state.open_blocks.remove(&index) {
                return Err(ProviderError::SseParse(format!(
                    "Anthropic stop references missing content block {index}"
                )));
            }
            if let Some(arguments) = state.tool_arguments.remove(&index)
                && !arguments.is_empty()
            {
                let input: Value = serde_json::from_str(&arguments).map_err(|error| {
                    ProviderError::SseParse(format!(
                        "invalid Anthropic tool input for block {index}: {error}"
                    ))
                })?;
                if !input.is_object() {
                    return Err(ProviderError::SseParse(format!(
                        "Anthropic tool input for block {index} is not an object"
                    )));
                }
                state.block_mut(index)?["input"] = input;
            }
            if state.reasoning_index == Some(index) {
                state.reasoning_index = None;
                on_event(StreamEvent::ReasoningEnd)?;
            }
        }
        "message_delta" => {
            if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                state.stop_reason = Some(reason.to_string());
            }
            state.usage.update(&value["usage"]);
        }
        "message_stop" => state.terminal = true,
        "error" => return Err(stream_error(&value["error"])),
        "ping" => {}
        _ => {}
    }
    Ok(())
}

fn event_index(value: &Value) -> Result<usize, ProviderError> {
    let index = value["index"].as_u64().ok_or_else(|| {
        ProviderError::SseParse("Anthropic content block event is missing index".into())
    })?;
    usize::try_from(index).map_err(|_| {
        ProviderError::SseParse(format!(
            "Anthropic content block index {index} is too large"
        ))
    })
}

fn required_str<'a>(value: &'a Value, key: &str, context: &str) -> Result<&'a str, ProviderError> {
    value[key]
        .as_str()
        .ok_or_else(|| ProviderError::SseParse(format!("{context} is missing `{key}`")))
}

fn append_json_string(value: &mut Value, key: &str, suffix: &str) {
    if let Some(current) = value[key].as_str() {
        value[key] = Value::String(format!("{current}{suffix}"));
    } else {
        value[key] = Value::String(suffix.to_string());
    }
}

fn stream_error(error: &Value) -> ProviderError {
    let message = stream_error_message(error);
    match error["type"].as_str().unwrap_or("") {
        "overloaded_error" => ProviderError::HttpStatus {
            status: 529,
            body: message,
        },
        "rate_limit_error" => ProviderError::RateLimit { message },
        "authentication_error" | "not_found_error" | "permission_error" => {
            ProviderError::Unavailable(format!(
                "{}: {message}",
                error["type"].as_str().unwrap_or("provider unavailable")
            ))
        }
        _ => ProviderError::Other(format!("Anthropic stream error: {message}")),
    }
}

fn text_from_blocks(blocks: &[Value]) -> Option<String> {
    let text = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn tool_calls_from_blocks(blocks: &[Value]) -> Result<Vec<ToolCall>, ProviderError> {
    blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| {
            let id = block["id"].as_str().ok_or_else(|| {
                ProviderError::SseParse("Anthropic tool_use block is missing `id`".into())
            })?;
            let name = block["name"].as_str().ok_or_else(|| {
                ProviderError::SseParse("Anthropic tool_use block is missing `name`".into())
            })?;
            let input = &block["input"];
            if !input.is_object() {
                return Err(ProviderError::SseParse(format!(
                    "Anthropic tool_use block `{id}` has non-object input"
                )));
            }
            Ok(ToolCall {
                id: id.to_string(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: serde_json::to_string(input)
                        .map_err(|error| ProviderError::SseParse(error.to_string()))?,
                },
            })
        })
        .collect()
}

fn finish_reason(reason: Option<&str>, tool_calls: &[ToolCall]) -> FinishReason {
    match reason {
        Some("end_turn" | "stop_sequence") => FinishReason::Stop,
        Some("tool_use") => FinishReason::ToolCalls,
        Some(other) => FinishReason::Other(other.to_string()),
        None if tool_calls.is_empty() => FinishReason::Stop,
        None => FinishReason::ToolCalls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ResolvedModelRef;
    use crate::provider::{ImageDetail, ToolAttachment};

    const ENDPOINT: &str = "https://api.anthropic.test/v1/messages";

    fn request(effort: Option<&str>) -> RequestOptions {
        RequestOptions {
            model: ResolvedModelRef {
                canonical: "anthropic/claude-opus-5".into(),
                provider_id: "anthropic".into(),
                model_id: "claude-opus-5".into(),
                effort: effort.map(str::to_owned),
            },
        }
    }

    fn system() -> Message {
        Message::System {
            content: "system".into(),
        }
    }

    fn consume(
        state: &mut AnthropicStreamState,
        events: &mut Vec<StreamEvent>,
        value: Value,
    ) -> Result<(), ProviderError> {
        consume_event(&value.to_string(), state, &mut |event| {
            events.push(event);
            Ok(())
        })
    }

    #[test]
    fn builds_latest_messages_request_with_fixed_limits_and_tools() {
        let body = build_request_body(
            &request(Some("max")),
            ENDPOINT,
            &[
                system(),
                Message::User {
                    content: "hello".into(),
                },
            ],
            &[serde_json::json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "Run Bash",
                    "parameters": {
                        "type": "object",
                        "properties": { "command": { "type": "string" } },
                        "required": ["command"],
                    },
                    "strict": true,
                },
            })],
        )
        .unwrap();

        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "system");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "max");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["input_schema"]["required"][0], "command");
        assert!(body["tools"][0].get("strict").is_none());
    }

    #[test]
    fn serializes_images_without_detail_and_rejects_audio_during_assembly() {
        let image = Attachment {
            filename: "pixel.png".into(),
            media_type: "image/png".into(),
            data: vec![1, 2, 3],
        };
        let body = build_request_body(
            &request(None),
            ENDPOINT,
            &[
                system(),
                Message::User {
                    content: UserContent::Parts(vec![
                        ContentPart::Attachment {
                            attachment: image.clone(),
                        },
                        ContentPart::Text {
                            text: "inspect".into(),
                        },
                    ]),
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "toolu_1".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"pwd"}"#.into(),
                        },
                    }]),
                    native_replay: None,
                },
                Message::Tool {
                    content: "Viewed image".into(),
                    attachments: vec![ToolAttachment {
                        attachment: image,
                        detail: ImageDetail::Original,
                        object_sha256: None,
                    }],
                    tool_call_id: "toolu_1".into(),
                },
            ],
            &[],
        )
        .unwrap();

        let user_image = &body["messages"][0]["content"][0];
        assert_eq!(user_image["type"], "image");
        assert_eq!(user_image["source"]["data"], "AQID");
        assert!(user_image.get("detail").is_none());
        let tool_image = &body["messages"][2]["content"][0]["content"][1];
        assert_eq!(tool_image["type"], "image");
        assert!(tool_image.get("detail").is_none());

        let error = build_request_body(
            &request(None),
            ENDPOINT,
            &[
                system(),
                Message::User {
                    content: UserContent::Parts(vec![ContentPart::Attachment {
                        attachment: Attachment {
                            filename: "voice.wav".into(),
                            media_type: "audio/wav".into(),
                            data: vec![0],
                        },
                    }]),
                },
            ],
            &[],
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not support audio attachment")
        );
    }

    #[test]
    fn replays_exact_native_blocks_only_for_their_origin() {
        let native_blocks = vec![
            serde_json::json!({
                "type": "thinking",
                "thinking": "summary",
                "signature": "opaque",
            }),
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_1",
                "name": "bash",
                "input": { "command": "pwd" },
            }),
        ];
        let message = Message::Assistant {
            content: Some("semantic fallback".into()),
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "toolu_1".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: r#"{"command":"pwd"}"#.into(),
                },
            }]),
            native_replay: Some(NativeReplay {
                provider_id: "anthropic".into(),
                endpoint: ENDPOINT.into(),
                model: "claude-opus-5".into(),
                payload: NativeReplayPayload::AnthropicContent(native_blocks.clone()),
            }),
        };

        let matching =
            build_request_body(&request(None), ENDPOINT, &[system(), message.clone()], &[])
                .unwrap();
        assert_eq!(
            matching["messages"][0]["content"],
            Value::Array(native_blocks)
        );

        let foreign = build_request_body(
            &request(None),
            "https://other.test/v1/messages",
            &[system(), message],
            &[],
        )
        .unwrap();
        assert_eq!(foreign["messages"][0]["content"][0]["type"], "text");
        assert_eq!(foreign["messages"][0]["content"][1]["type"], "tool_use");
    }

    #[test]
    fn assembles_streamed_thinking_tools_text_usage_and_replay_blocks() {
        let mut state = AnthropicStreamState::default();
        let mut events = Vec::new();
        let frames = [
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "usage": {
                        "input_tokens": 10,
                        "cache_creation_input_tokens": 2,
                        "cache_read_input_tokens": 3,
                    },
                },
            }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "thinking",
                    "thinking": "",
                    "signature": "",
                },
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "thinking_delta", "thinking": "plan" },
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "signature_delta", "signature": "sig" },
            }),
            serde_json::json!({ "type": "content_block_stop", "index": 0 }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "bash",
                    "input": {},
                },
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": "{\"command\":\"pwd\"}",
                },
            }),
            serde_json::json!({ "type": "content_block_stop", "index": 1 }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": { "type": "text", "text": "" },
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": { "type": "text_delta", "text": "done" },
            }),
            serde_json::json!({ "type": "content_block_stop", "index": 2 }),
            serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": {
                    "output_tokens": 9,
                    "output_tokens_details": { "thinking_tokens": 4 },
                },
            }),
            serde_json::json!({ "type": "message_stop" }),
        ];
        for frame in frames {
            consume(&mut state, &mut events, frame).unwrap();
        }

        let blocks = state.completed_blocks().unwrap();
        assert_eq!(blocks[0]["thinking"], "plan");
        assert_eq!(blocks[0]["signature"], "sig");
        assert_eq!(blocks[1]["input"]["command"], "pwd");
        assert_eq!(blocks[2]["text"], "done");
        let calls = tool_calls_from_blocks(&blocks).unwrap();
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.arguments, r#"{"command":"pwd"}"#);
        assert_eq!(text_from_blocks(&blocks).as_deref(), Some("done"));
        assert!(matches!(
            finish_reason(state.stop_reason.as_deref(), &calls),
            FinishReason::ToolCalls
        ));
        assert!(matches!(
            finish_reason(Some("refusal"), &[]),
            FinishReason::Other(reason) if reason == "refusal"
        ));

        let usage = state.usage.finish().unwrap();
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(usage.cache_write_input_tokens, Some(2));
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.reasoning_output_tokens, 4);
        assert_eq!(usage.total_tokens, 24);
        assert!(state.terminal);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ReasoningStart(ReasoningVisibility::Opaque)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ReasoningSummaryDelta { part_index: 0, text }
                if text == "plan"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallDelta(delta)
                if delta.id.as_deref() == Some("toolu_1")
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "done"))
        );
    }

    #[test]
    fn rejects_malformed_streams_and_maps_retryable_errors() {
        let mut state = AnthropicStreamState::default();
        let mut events = Vec::new();
        consume(
            &mut state,
            &mut events,
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
        )
        .unwrap();
        assert!(
            state
                .completed_blocks()
                .unwrap_err()
                .to_string()
                .contains("unclosed")
        );

        let error = consume(
            &mut AnthropicStreamState::default(),
            &mut Vec::new(),
            serde_json::json!({
                "type": "error",
                "error": {
                    "type": "overloaded_error",
                    "message": "busy",
                },
            }),
        )
        .unwrap_err();
        assert!(error.retryable_for_live_turn());

        let malformed = Message::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "toolu_bad".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "{".into(),
                },
            }]),
            native_replay: None,
        };
        let error =
            build_request_body(&request(None), ENDPOINT, &[system(), malformed], &[]).unwrap_err();
        assert!(error.to_string().contains("toolu_bad"));
    }
}
