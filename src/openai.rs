use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::{self, MissedTickBehavior};

use crate::models::RequestOptions;
use crate::provider::{
    FinishReason, FunctionCall, Message, Provider, ProviderError, StreamEvent, StreamResult,
    ToolCall, ToolCallDelta as ProviderToolCallDelta, Usage,
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
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetailsJson {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionTokensDetailsJson {
    #[serde(default)]
    reasoning_tokens: u64,
}

type ToolCallAccumulator = BTreeMap<usize, (Option<String>, Option<String>, String, String)>;

struct StreamParseState {
    content: String,
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
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_chat_request_body(request, messages, tools);

        let mut req = self.client.post(&url).json(&body);
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
        let mut tick = time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => {
                    chunk.map_err(|e| ProviderError::Transport(e.to_string()))?
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

        let tool_calls = if stream_state.tool_accum.is_empty() {
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
            tool_calls,
        };

        Ok(StreamResult {
            message,
            finish_reason: stream_state.finish_reason,
            usage: stream_state.usage,
        })
    }
}

fn build_chat_request_body(
    request: &RequestOptions,
    messages: &[Message],
    tools: &[Value],
) -> Value {
    let mut body = serde_json::json!({
        "model": request.model.model_id.as_str(),
        "messages": messages,
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    if let Some(effort) = request.model.effort {
        // Chat Completions uses a top-level `reasoning_effort` string. (The
        // nested `reasoning: { effort }` object is the Responses API shape and
        // is rejected by real OpenAI `/chat/completions`.)
        body["reasoning_effort"] = Value::String(effort.as_str().to_string());
    }
    body
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
                state.usage = Some(Usage {
                    input_tokens: u.prompt_tokens,
                    cache_read_input_tokens: u.prompt_tokens_details.cached_tokens,
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
    use crate::models::{EffortLevel, ResolvedModelRef};

    fn test_model(effort: Option<EffortLevel>) -> ResolvedModelRef {
        ResolvedModelRef {
            canonical: match effort {
                Some(level) => format!("test/gpt-test:{level}"),
                None => "test/gpt-test".into(),
            },
            provider_id: "test".into(),
            model_id: "gpt-test".into(),
            effort,
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
        assert_eq!(state.usage.unwrap().total_tokens, 17);
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
    fn request_includes_reasoning_effort_when_set() {
        let body = build_chat_request_body(
            &RequestOptions {
                model: test_model(Some(EffortLevel::High)),
            },
            &[],
            &[],
        );

        assert_eq!(body["reasoning_effort"], "high");
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
}
