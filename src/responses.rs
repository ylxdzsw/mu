use std::collections::BTreeMap;

use serde_json::Value;

use crate::models::RequestOptions;
use crate::provider::{
    ContentPart, FinishReason, FunctionCall, HttpProvider, Message, NativeReplay,
    NativeReplayPayload, ProviderError, ReasoningVisibility, SseEvent, StreamEvent, StreamResult,
    ToolCall, ToolCallDelta as ProviderToolCallDelta, Usage, UserContent, base64_encode,
    next_event_boundary, stream_error_message,
};

pub(crate) async fn stream(
    provider: &HttpProvider,
    request: &RequestOptions,
    messages: &[Message],
    tools: &[Value],
    on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
) -> Result<StreamResult, ProviderError> {
    let body = build_responses_request_body(request, &provider.endpoint, messages, tools)?;
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
                endpoint: provider.endpoint.clone(),
                model: request.model.model_id.clone(),
                payload: NativeReplayPayload::ResponsesOutput(output),
            }),
        },
        finish_reason,
        usage: state.usage,
    })
}

pub(crate) fn build_responses_request_body(
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
    let mut reasoning = serde_json::Map::new();
    reasoning.insert("summary".into(), Value::String("auto".into()));
    if let Some(effort) = request.model.effort.as_deref() {
        reasoning.insert("effort".into(), Value::String(effort.to_string()));
    }
    body["reasoning"] = Value::Object(reasoning);
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
pub(crate) struct ResponsesStreamState {
    pub(crate) content: String,
    pub(crate) output: Vec<Value>,
    pub(crate) usage: Option<Usage>,
    pub(crate) terminal: bool,
    pub(crate) replayable: bool,
    pub(crate) finish_reason: Option<FinishReason>,
    pub(crate) failure: Option<String>,
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
            .map_err(|error| ProviderError::SseParse(error.to_string()))?;
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.output_item.added" => {
                let item = &value["item"];
                if item["type"] == "reasoning" && !state.reasoning_active {
                    state.reasoning_active = true;
                    let output_index = value["output_index"].as_u64().unwrap_or(0) as usize;
                    state.reasoning_output_index = Some(output_index);
                    on_event(StreamEvent::ReasoningStart(ReasoningVisibility::Opaque))?;
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
                let output_index = value["output_index"].as_u64().unwrap_or(0) as usize;
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

pub(crate) fn responses_tool_calls(output: &[Value]) -> Vec<ToolCall> {
    output
        .iter()
        .filter(|item| item["type"] == "function_call")
        .map(|item| ToolCall {
            id: item["call_id"].as_str().unwrap_or("call").to_string(),
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
