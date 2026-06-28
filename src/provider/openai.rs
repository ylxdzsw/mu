use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::{self, MissedTickBehavior};

use crate::models::RequestOptions;

use super::{
    FinishReason, FunctionCall, Message, Provider, ProviderError, StreamEvent, StreamResult,
    ToolCall, Usage,
};

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChunkResponse {
    choices: Vec<ChunkChoice>,
    usage: Option<UsageJson>,
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
}

type ToolCallAccumulator = BTreeMap<usize, (Option<String>, Option<String>, String, String)>;

#[async_trait(?Send)]
impl Provider for OpenAiProvider {
    async fn stream_chat(
        &self,
        request: &RequestOptions,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut dyn FnMut(StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_chat_request_body(request, messages, tools);

        let mut req = self.client.post(&url).json(&body);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| ProviderError::Other(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if text.contains("context_length")
                || text.contains("maximum context length")
                || text.contains("context length")
            {
                return Err(ProviderError::ContextLength);
            }
            return Err(ProviderError::Other(format!("HTTP {status}: {text}")));
        }

        let mut response = response;
        let mut content = String::new();
        let mut tool_accum: ToolCallAccumulator = BTreeMap::new();
        let mut finish_reason = FinishReason::Stop;
        let mut usage: Option<Usage> = None;
        let mut reasoning_active = false;
        let mut tool_call_started = false;

        let mut buffer = String::new();
        let mut byte_buf: Vec<u8> = Vec::new();
        let mut tick = time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => {
                    chunk.map_err(|e| ProviderError::Other(e.to_string()))?
                }
                _ = tick.tick() => {
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
            consume_sse_buffer(
                &mut buffer,
                &mut content,
                &mut tool_accum,
                &mut finish_reason,
                &mut usage,
                &mut reasoning_active,
                &mut tool_call_started,
                on_event,
            )?;
        }
        // Flush any remaining undecodable trailing bytes (lossy) so a final
        // event without a trailing blank line is not silently dropped.
        if !byte_buf.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&byte_buf));
        }
        if !buffer.trim().is_empty() {
            buffer.push_str("\n\n");
            consume_sse_buffer(
                &mut buffer,
                &mut content,
                &mut tool_accum,
                &mut finish_reason,
                &mut usage,
                &mut reasoning_active,
                &mut tool_call_started,
                on_event,
            )?;
        }
        if reasoning_active {
            on_event(StreamEvent::ReasoningEnd)?;
        }

        let tool_calls = if tool_accum.is_empty() {
            None
        } else {
            Some(
                tool_accum
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
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            tool_calls,
        };

        Ok(StreamResult {
            message,
            finish_reason,
            usage,
        })
    }
}

fn build_chat_request_body(
    request: &RequestOptions,
    messages: &[Message],
    tools: &[Value],
) -> Value {
    let mut body = serde_json::json!({
        "model": request.model.as_str(),
        "messages": messages,
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    if let Some(effort) = request.effort {
        body["reasoning"] = serde_json::json!({ "effort": effort });
    }
    body
}

fn consume_sse_buffer(
    buffer: &mut String,
    content: &mut String,
    tool_accum: &mut ToolCallAccumulator,
    finish_reason: &mut FinishReason,
    usage: &mut Option<Usage>,
    reasoning_active: &mut bool,
    tool_call_started: &mut bool,
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
            let parsed: ChunkResponse = serde_json::from_str(data)
                .map_err(|e| ProviderError::Other(format!("SSE parse: {e}")))?;

            if let Some(u) = parsed.usage {
                *usage = Some(Usage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
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
                    if !*reasoning_active {
                        on_event(StreamEvent::ReasoningStart)?;
                        *reasoning_active = true;
                    }
                    on_event(StreamEvent::ReasoningDelta(text))?;
                } else if *reasoning_active
                    && (choice.delta.content.is_some()
                        || choice.delta.tool_calls.is_some()
                        || choice.finish_reason.is_some())
                {
                    on_event(StreamEvent::ReasoningEnd)?;
                    *reasoning_active = false;
                }

                if let Some(text) = choice.delta.content.clone() {
                    on_event(StreamEvent::TextDelta(text.clone()))?;
                    content.push_str(&text);
                }
                if let Some(ref tcs) = choice.delta.tool_calls {
                    if !*tool_call_started && !tcs.is_empty() {
                        on_event(StreamEvent::ToolCallStart)?;
                        *tool_call_started = true;
                    }
                    for tc in tcs {
                        let entry = tool_accum
                            .entry(tc.index)
                            .or_insert_with(|| (None, None, String::new(), "function".into()));
                        if let Some(ref id) = tc.id {
                            entry.0 = Some(id.clone());
                        }
                        if let Some(ref t) = tc.call_type {
                            entry.3 = t.clone();
                        }
                        if let Some(ref f) = tc.function {
                            if let Some(ref name) = f.name {
                                entry.1 = Some(name.clone());
                            }
                            if let Some(ref args) = f.arguments {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
                if let Some(ref reason) = choice.finish_reason {
                    *finish_reason = match reason.as_str() {
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
    use crate::models::EffortLevel;

    #[test]
    fn streams_deltas_and_accumulates_tool_calls() {
        let mut seen = String::new();
        let mut tool_call_starts = 0usize;
        let mut on_event = |event: StreamEvent| -> Result<(), ProviderError> {
            match event {
                StreamEvent::TextDelta(delta) => seen.push_str(&delta),
                StreamEvent::ToolCallStart => tool_call_starts += 1,
                _ => {}
            }
            Ok(())
        };

        let mut buffer = String::new();
        let mut content = String::new();
        let mut tool_accum = BTreeMap::new();
        let mut finish_reason = FinishReason::Stop;
        let mut usage = None;
        let mut reasoning_active = false;
        let mut tool_call_started = false;

        for chunk in [
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp/x\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5,\"total_tokens\":17}}\n\n",
            "data: [DONE]\n\n",
        ] {
            buffer.push_str(chunk);
            consume_sse_buffer(
                &mut buffer,
                &mut content,
                &mut tool_accum,
                &mut finish_reason,
                &mut usage,
                &mut reasoning_active,
                &mut tool_call_started,
                &mut on_event,
            )
            .unwrap();
        }

        assert_eq!(seen, "hello");
        assert_eq!(tool_call_starts, 1);
        assert_eq!(content, "hello");
        assert_eq!(finish_reason, FinishReason::ToolCalls);
        assert_eq!(usage.unwrap().total_tokens, 17);
        let calls: Vec<_> = tool_accum
            .into_values()
            .map(|(id, name, args, call_type)| ToolCall {
                id: id.unwrap_or_default(),
                call_type,
                function: FunctionCall {
                    name: name.unwrap_or_default(),
                    arguments: args,
                },
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/tmp/x\"}");
    }

    #[test]
    fn handles_crlf_event_framing() {
        let mut seen = String::new();
        let mut saw_reasoning = false;
        let mut on_event = |event: StreamEvent| -> Result<(), ProviderError> {
            match event {
                StreamEvent::TextDelta(delta) => seen.push_str(&delta),
                StreamEvent::ReasoningStart
                | StreamEvent::ReasoningDelta(_)
                | StreamEvent::ReasoningEnd => saw_reasoning = true,
                _ => {}
            }
            Ok(())
        };

        let mut buffer = String::new();
        let mut content = String::new();
        let mut tool_accum = BTreeMap::new();
        let mut finish_reason = FinishReason::Stop;
        let mut usage = None;
        let mut reasoning_active = false;
        let mut tool_call_started = false;

        buffer.push_str(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
        );
        consume_sse_buffer(
            &mut buffer,
            &mut content,
            &mut tool_accum,
            &mut finish_reason,
            &mut usage,
            &mut reasoning_active,
            &mut tool_call_started,
            &mut on_event,
        )
        .unwrap();

        assert_eq!(seen, "hi");
        assert!(!saw_reasoning);
        assert_eq!(finish_reason, FinishReason::Stop);
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
                StreamEvent::ToolCallStart | StreamEvent::Tick => {}
            }
            Ok(())
        };

        let mut buffer = String::new();
        let mut content = String::new();
        let mut tool_accum = BTreeMap::new();
        let mut finish_reason = FinishReason::Stop;
        let mut usage = None;
        let mut reasoning_active = false;
        let mut tool_call_started = false;

        for chunk in [
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":[{\"type\":\"reasoning_text\",\"text\":\"step 1\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ] {
            buffer.push_str(chunk);
            consume_sse_buffer(
                &mut buffer,
                &mut content,
                &mut tool_accum,
                &mut finish_reason,
                &mut usage,
                &mut reasoning_active,
                &mut tool_call_started,
                &mut on_event,
            )
            .unwrap();
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
        assert!(!reasoning_active);
    }

    #[test]
    fn boundary_prefers_earliest_separator() {
        // LF event followed by a CRLF event in one buffer.
        let mut buffer = String::from("data: [DONE]\n\ndata: [DONE]\r\n\r\n");
        assert_eq!(next_event_boundary(&buffer), Some((12, 2)));
        buffer.replace_range(..14, "");
        assert_eq!(next_event_boundary(&buffer), Some((12, 4)));
    }

    #[test]
    fn request_omits_reasoning_when_effort_is_unset() {
        let body = build_chat_request_body(
            &RequestOptions {
                model: "gpt-test".into(),
                effort: None,
            },
            &[],
            &[],
        );

        assert_eq!(body["model"], "gpt-test");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn request_includes_reasoning_effort_when_set() {
        let body = build_chat_request_body(
            &RequestOptions {
                model: "gpt-test".into(),
                effort: Some(EffortLevel::High),
            },
            &[],
            &[],
        );

        assert_eq!(body["reasoning"]["effort"], "high");
    }
}
