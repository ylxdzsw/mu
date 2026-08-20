use std::collections::BTreeMap;

use serde_json::Value;

use crate::provider::{
    AssistantItem, ContentPart, FinishReason, HttpProvider, Message, NativeReplay,
    NativeReplayPayload, ProviderError, ReasoningVisibility, Request, SseEvent, StreamEvent,
    StreamResult, ToolCall, ToolCallDelta as ProviderToolCallDelta, Usage, UserContent,
    base64_encode, classify_stream_error, next_event_boundary,
};

pub(crate) async fn stream(
    provider: &HttpProvider,
    request: &Request,
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<StreamResult, ProviderError> {
    let body = request.json(crate::provider::ModelApi::Responses)?;
    let mut state = ResponsesStreamState::default();
    provider
        .stream_sse(&body, &mut |event| match event {
            SseEvent::Tick => on_event(StreamEvent::Tick),
            SseEvent::Data(data) => {
                let mut frame = format!("data: {data}\n\n");
                consume_responses_sse_buffer(&mut frame, &mut state, on_event)
            }
        })
        .await?;
    if state.reasoning_active {
        on_event(StreamEvent::ReasoningEnd)?;
    }
    if !state.terminal {
        return Err(ProviderError::Transport(
            "Responses stream ended before response.completed".into(),
        ));
    }

    let native_response = state.native_response;
    let output = state.output;
    let mut items = responses_items(&output)?;
    if !state.content.is_empty()
        && !items
            .iter()
            .any(|item| matches!(item, AssistantItem::Text { .. }))
    {
        let position = items
            .iter()
            .position(|item| matches!(item, AssistantItem::BashCall(_)))
            .unwrap_or(items.len());
        items.insert(
            position,
            AssistantItem::Text {
                text: state.content,
            },
        );
    }
    let tool_calls = items
        .iter()
        .filter(|item| matches!(item, AssistantItem::BashCall(_)))
        .count();
    let finish_reason = state.finish_reason.unwrap_or({
        if tool_calls == 0 {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        }
    });
    Ok(StreamResult {
        message: Message::Assistant {
            items,
            native_replay: state.replayable.then(|| NativeReplay {
                provider_id: request.model.provider_id.clone(),
                endpoint: provider.endpoint.clone(),
                model: request.model.model_id.clone(),
                payload: NativeReplayPayload::ResponsesOutput(output),
            }),
        },
        finish_reason,
        usage: state.usage,
        native_response,
    })
}

pub(crate) fn build_request_body(
    request: &Request,
    tools: &[Value],
) -> Result<Value, ProviderError> {
    let mut input = Vec::new();
    for message in &request.messages {
        responses_input_items(message, &mut input)?;
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
    if let Some(cache_key) = request.cache_key.as_deref() {
        body["prompt_cache_key"] = Value::String(cache_key.to_string());
    }
    if let Some(max_output_tokens) = request.max_output_tokens {
        body["max_output_tokens"] = Value::from(max_output_tokens);
    }
    let mut reasoning = serde_json::Map::new();
    reasoning.insert("summary".into(), Value::String("auto".into()));
    if let Some(effort) = request.model.effort.as_deref() {
        reasoning.insert("effort".into(), Value::String(effort.to_string()));
    }
    body["reasoning"] = Value::Object(reasoning);
    Ok(body)
}

fn responses_input_items(message: &Message, input: &mut Vec<Value>) -> Result<(), ProviderError> {
    match message {
        Message::System { content } => input.push(serde_json::json!({
            "role": "system", "content": content
        })),
        Message::User { content } => input.push(serde_json::json!({
            "role": "user", "content": responses_user_content(content)?
        })),
        Message::Assistant {
            items,
            native_replay,
        } => {
            if let Some(NativeReplay {
                payload: NativeReplayPayload::ResponsesOutput(items),
                ..
            }) = native_replay.as_ref()
            {
                input.extend(items.iter().cloned());
            } else {
                if let Some(content) = message.assistant_text() {
                    input.push(serde_json::json!({ "role": "assistant", "content": content }));
                }
                input.extend(items.iter().filter_map(|item| {
                    if let AssistantItem::BashCall(call) = item {
                        Some(serde_json::json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": "bash",
                            "arguments": call.arguments
                        }))
                    } else {
                        None
                    }
                }));
            }
        }
        Message::Tool {
            content,
            attachments,
            tool_call_id,
        } => {
            let output = if attachments.is_empty() {
                Value::String(content.clone())
            } else {
                let mut parts = vec![serde_json::json!({
                    "type": "input_text",
                    "text": content,
                })];
                parts.extend(attachments.iter().map(|attachment| {
                    serde_json::json!({
                        "type": "input_image",
                        "image_url": format!(
                            "data:{};base64,{}",
                            attachment.attachment.media_type,
                            base64_encode(&attachment.attachment.data)
                        ),
                        "detail": attachment.detail.to_string(),
                    })
                }));
                Value::Array(parts)
            };
            input.push(serde_json::json!({
                "type": "function_call_output", "call_id": tool_call_id, "output": output
            }));
        }
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
                ContentPart::Attachment { attachment } => Err(ProviderError::Protocol(format!(
                    "Responses endpoints do not support audio attachment `{}` ({})",
                    attachment.filename, attachment.media_type
                ))),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
    }
}

#[derive(Default)]
pub(crate) struct ResponsesStreamState {
    pub(crate) content: String,
    pub(crate) output: Vec<Value>,
    pub(crate) streamed_output: BTreeMap<usize, Value>,
    pub(crate) usage: Option<Usage>,
    pub(crate) native_response: Option<Value>,
    pub(crate) terminal: bool,
    pub(crate) replayable: bool,
    pub(crate) finish_reason: Option<FinishReason>,
    pub(crate) reasoning_active: bool,
    pub(crate) reasoning_output_index: Option<usize>,
    pub(crate) tool_indexes: BTreeMap<usize, usize>,
}

pub(crate) fn consume_responses_sse_buffer(
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
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.output_item.added" => {
                let item = &value["item"];
                let output_index = value["output_index"].as_u64().unwrap_or(0) as usize;
                merge_output_item(&mut state.streamed_output, output_index, item);
                if item["type"] == "reasoning" && !state.reasoning_active {
                    state.reasoning_active = true;
                    state.reasoning_output_index = Some(output_index);
                    on_event(StreamEvent::ReasoningStart(ReasoningVisibility::Opaque))?;
                } else if item["type"] == "function_call" {
                    let tool_index = state.tool_indexes.len();
                    state.tool_indexes.insert(output_index, tool_index);
                    on_event(StreamEvent::ToolCallDelta(ProviderToolCallDelta {
                        index: tool_index,
                        arguments_delta: item["arguments"].as_str().unwrap_or("").to_string(),
                    }))?;
                }
            }
            "response.output_item.done" => {
                let output_index = value["output_index"].as_u64().unwrap_or(0) as usize;
                merge_output_item(&mut state.streamed_output, output_index, &value["item"]);
                if value["item"]["type"] == "reasoning"
                    && state.reasoning_active
                    && state.reasoning_output_index == Some(output_index)
                {
                    state.reasoning_active = false;
                    state.reasoning_output_index = None;
                    on_event(StreamEvent::ReasoningEnd)?;
                }
            }
            "response.reasoning_summary_text.delta" => {
                let output_index = value["output_index"].as_u64().unwrap_or(u64::MAX) as usize;
                let summary_index = value["summary_index"].as_u64().unwrap_or(u64::MAX) as usize;
                if state.reasoning_active
                    && state.reasoning_output_index == Some(output_index)
                    && let Some(delta) = value["delta"].as_str()
                {
                    on_event(StreamEvent::ReasoningSummaryDelta {
                        part_index: summary_index,
                        text: delta.to_string(),
                    })?;
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
                    arguments_delta: delta,
                }))?;
            }
            "response.completed" => {
                state.terminal = true;
                state.replayable = true;
                retain_native_response(state, &value["response"]);
                state.usage = responses_usage(&value["response"]["usage"]);
            }
            "response.incomplete" => {
                state.terminal = true;
                retain_native_response(state, &value["response"]);
                state.usage = responses_usage(&value["response"]["usage"]);
                let reason = value["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("incomplete");
                state.finish_reason = Some(FinishReason::Other(reason.to_string()));
            }
            "response.failed" => return Err(responses_stream_error(&value["response"]["error"])),
            "error" => return Err(responses_stream_error(&value)),
            _ => {}
        }
    }
    Ok(())
}

fn merge_output_item(output: &mut BTreeMap<usize, Value>, index: usize, item: &Value) {
    match output.get_mut(&index) {
        Some(existing) => merge_json(existing, item),
        None => {
            output.insert(index, item.clone());
        }
    }
}

fn merge_json(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, source) => *target = source.clone(),
    }
}

fn retain_native_response(state: &mut ResponsesStreamState, response: &Value) {
    let mut output = state.streamed_output.clone();
    for (index, item) in response["output"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        let matching_index = output
            .iter()
            .find_map(|(index, existing)| same_output_item(existing, &item).then_some(*index));
        let target = matching_index
            .or_else(|| {
                output
                    .get(&index)
                    .is_none_or(|existing| existing["type"] == item["type"])
                    .then_some(index)
            })
            .unwrap_or_else(|| output.keys().next_back().map_or(0, |index| index + 1));
        merge_output_item(&mut output, target, &item);
    }
    state.output = output.into_values().collect();
    let mut native_response = response.clone();
    if let Some(object) = native_response.as_object_mut() {
        object.insert("output".into(), Value::Array(state.output.clone()));
    }
    state.native_response = Some(native_response);
}

fn same_output_item(left: &Value, right: &Value) -> bool {
    ["id", "call_id"].into_iter().any(|key| {
        left[key]
            .as_str()
            .zip(right[key].as_str())
            .is_some_and(|(left, right)| !left.is_empty() && left == right)
    })
}

fn responses_stream_error(error: &Value) -> ProviderError {
    classify_stream_error(error)
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

#[cfg(test)]
pub(crate) fn responses_tool_calls(output: &[Value]) -> Result<Vec<ToolCall>, ProviderError> {
    responses_items(output).map(|items| {
        items
            .into_iter()
            .filter_map(|item| match item {
                AssistantItem::BashCall(call) => Some(call),
                _ => None,
            })
            .collect()
    })
}

fn responses_items(output: &[Value]) -> Result<Vec<AssistantItem>, ProviderError> {
    let mut items = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("reasoning") => items.push(AssistantItem::Reasoning { text: None }),
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    let text = match part["type"].as_str() {
                        Some("output_text") => part["text"].as_str(),
                        Some("refusal") => part["refusal"].as_str(),
                        _ => None,
                    };
                    if let Some(text) = text {
                        items.push(AssistantItem::Text {
                            text: text.to_string(),
                        });
                    }
                }
            }
            Some("function_call") => {
                let id = item["call_id"].as_str().unwrap_or("call");
                let name = item["name"].as_str().unwrap_or("");
                if name != "bash" {
                    return Err(ProviderError::Protocol(format!(
                        "Responses function call `{id}` calls unsupported tool `{name}`"
                    )));
                }
                let arguments = item["arguments"].as_str().unwrap_or("");
                let arguments = if arguments.trim().is_empty() {
                    "{}".into()
                } else {
                    let value: Value = serde_json::from_str(arguments).map_err(|error| {
                        ProviderError::Protocol(format!(
                            "invalid completed Responses tool arguments for `{id}`: {error}"
                        ))
                    })?;
                    if !value.is_object() {
                        return Err(ProviderError::Protocol(format!(
                            "completed Responses tool arguments for `{id}` must be a JSON object"
                        )));
                    }
                    arguments.to_string()
                };
                items.push(AssistantItem::BashCall(ToolCall {
                    id: id.to_string(),
                    arguments,
                }));
            }
            _ => {}
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderDisposition;

    fn consume(
        state: &mut ResponsesStreamState,
        events: &mut Vec<StreamEvent>,
        buffer: &mut String,
    ) -> Result<(), ProviderError> {
        consume_responses_sse_buffer(buffer, state, &mut |event| {
            events.push(event);
            Ok(())
        })
    }

    #[test]
    fn waits_for_complete_crlf_frames_and_preserves_the_remainder() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut buffer =
            "data: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"hel\"}\r\n"
                .to_string();

        consume(&mut state, &mut events, &mut buffer).unwrap();
        assert!(events.is_empty());
        assert!(!buffer.is_empty());

        buffer.push_str(
            "\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n\
             data: {\"type\":\"response.output_text.delta\"",
        );
        consume(&mut state, &mut events, &mut buffer).unwrap();

        assert_eq!(state.content, "hello");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.first(),
            Some(StreamEvent::TextDelta(text)) if text == "hel"
        ));
        assert!(matches!(
            events.get(1),
            Some(StreamEvent::TextDelta(text)) if text == "lo"
        ));
        assert_eq!(buffer, "data: {\"type\":\"response.output_text.delta\"");
    }

    #[test]
    fn accumulates_text_refusal_and_usage() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut buffer = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.refusal.delta\",\"delta\":\" no\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":3,\"cache_creation_tokens\":2},\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":4},\"total_tokens\":17}}}\n\n",
        )
        .to_string();

        consume(&mut state, &mut events, &mut buffer).unwrap();

        assert_eq!(state.content, "hello no");
        assert!(state.terminal);
        assert!(state.replayable);
        let usage = state.usage.unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(usage.cache_write_input_tokens, Some(2));
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.reasoning_output_tokens, 4);
        assert_eq!(usage.total_tokens, 17);
    }

    #[test]
    fn retains_streamed_encrypted_reasoning_missing_from_terminal_snapshot() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut buffer = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"opaque-state\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"visible\"}]}],\"usage\":{}}}\n\n",
        )
        .to_string();

        consume(&mut state, &mut events, &mut buffer).unwrap();

        assert_eq!(state.output[0]["encrypted_content"], "opaque-state");
        assert_eq!(state.output[0]["summary"][0]["text"], "visible");
        assert_eq!(
            state.native_response.as_ref().unwrap()["output"][0]["encrypted_content"],
            "opaque-state"
        );
    }

    #[test]
    fn terminal_subset_merges_with_streamed_output_by_item_identity() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut buffer = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"bash\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"command\\\":\\\"pwd\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"pwd\\\"}\"}]}}\n\n"
        )
        .to_string();

        consume(&mut state, &mut events, &mut buffer).unwrap();

        assert_eq!(responses_tool_calls(&state.output).unwrap().len(), 1);
        assert!(state.output.iter().any(|item| item["id"] == "rs_1"));
    }

    #[test]
    fn keeps_dense_indexes_for_interleaved_tool_calls() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut buffer = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":5,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"bash\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"bash\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":2,\"delta\":\"second\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":5,\"delta\":\"first\"}\n\n",
        )
        .to_string();

        consume(&mut state, &mut events, &mut buffer).unwrap();

        let deltas = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallDelta(delta) => Some(delta),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0].index, 0);
        assert_eq!(deltas[1].index, 1);
        assert_eq!(deltas[2].index, 1);
        assert_eq!(deltas[2].arguments_delta, "second");
        assert_eq!(deltas[3].index, 0);
        assert_eq!(deltas[3].arguments_delta, "first");
    }

    #[test]
    fn falls_back_to_completed_output_text_and_refusals() {
        let output = serde_json::json!([
            {
                "type": "message",
                "content": [
                    {"type": "output_text", "text": "first"},
                    {"type": "refusal", "refusal": " second"},
                    {"type": "other", "text": "ignored"}
                ]
            },
            {"type": "reasoning", "content": [{"type": "output_text", "text": "private"}]},
            {
                "type": "function_call",
                "call_id": "call-1",
                "name": "bash",
                "arguments": "{\"command\":\"true\"}"
            },
            {"type": "message", "content": [{"type": "output_text", "text": " third"}]}
        ]);

        let items = responses_items(output.as_array().unwrap()).unwrap();
        assert!(matches!(
            items.as_slice(),
            [
                AssistantItem::Text { text: first },
                AssistantItem::Text { text: second },
                AssistantItem::Reasoning { text: None },
                AssistantItem::BashCall(ToolCall { id, .. }),
                AssistantItem::Text { text: third },
            ] if first == "first" && second == " second" && id == "call-1" && third == " third"
        ));
    }

    #[test]
    fn classifies_response_error_envelopes() {
        for (buffer, class, disposition) in [
            (
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"generation failed\"}}}\n\n",
                "overloaded",
                ProviderDisposition::Retry,
            ),
            (
                "data: {\"type\":\"error\",\"error\":{\"code\":\"stream_read_error\",\"message\":\"upstream disconnected\",\"type\":\"upstream_error\"}}\n\n",
                "transport",
                ProviderDisposition::Retry,
            ),
            (
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"stream_read_error\",\"message\":\"upstream disconnected\",\"type\":\"upstream_error\"}}}\n\n",
                "transport",
                ProviderDisposition::Retry,
            ),
            (
                "data: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"slow down\"}\n\n",
                "rate_limit",
                ProviderDisposition::Retry,
            ),
            (
                "data: {\"type\":\"error\",\"code\":\"invalid_prompt\",\"message\":\"unsupported input\"}\n\n",
                "bad_request",
                ProviderDisposition::Fail,
            ),
        ] {
            let error = consume(
                &mut ResponsesStreamState::default(),
                &mut Vec::new(),
                &mut buffer.to_string(),
            )
            .unwrap_err();
            assert_eq!(error.class(), class);
            assert_eq!(error.disposition(), disposition);
        }
    }

    #[test]
    fn malformed_json_is_a_parse_error_without_partial_state() {
        let mut state = ResponsesStreamState::default();
        let mut events = Vec::new();
        let mut buffer =
            "data: {\"type\":\"response.output_text.delta\",\"delta\":}\n\n".to_string();

        let error = consume(&mut state, &mut events, &mut buffer).unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        assert!(state.content.is_empty());
        assert!(!state.terminal);
        assert!(events.is_empty());
    }

    #[test]
    fn completed_tool_arguments_must_be_json_objects() {
        let output = |arguments: &str| {
            vec![serde_json::json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "bash",
                "arguments": arguments,
            })]
        };

        assert_eq!(
            responses_tool_calls(&output(" \n")).unwrap()[0].arguments,
            "{}"
        );
        for invalid in ["{", "[]", "\"text\"", "1", "true", "null"] {
            assert!(matches!(
                responses_tool_calls(&output(invalid)),
                Err(ProviderError::Protocol(_))
            ));
        }
        let mut unsupported = output("{}");
        unsupported[0]["name"] = Value::String("python".into());
        assert!(matches!(
            responses_tool_calls(&unsupported),
            Err(ProviderError::Protocol(_))
        ));
    }
}
