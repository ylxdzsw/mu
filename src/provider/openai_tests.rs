use super::*;
use crate::models::ResolvedModelRef;

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
    let mut state = StreamParseState::default();

    for chunk in [
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp/x\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5,\"total_tokens\":17,\"prompt_tokens_details\":{\"cached_tokens\":3,\"cache_creation_tokens\":2},\"completion_tokens_details\":{\"reasoning_tokens\":4}}}\n\n",
        "data: [DONE]\n\n",
    ] {
        buffer.push_str(chunk);
        consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();
    }

    assert_eq!(seen, "hello");
    assert_eq!(tool_call_starts, 1);
    assert_eq!(state.content, "hello");
    assert_eq!(state.finish_reason, FinishReason::ToolCalls);
    let usage = state.usage.unwrap();
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.cache_read_input_tokens, 3);
    assert_eq!(usage.cache_write_input_tokens, 2);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.reasoning_output_tokens, 4);
    assert_eq!(usage.total_tokens, 17);
    let calls: Vec<_> = state
        .tool_accum
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
    let mut state = StreamParseState::default();

    buffer.push_str(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
    );
    consume_sse_buffer(&mut buffer, &mut state, &mut on_event).unwrap();

    assert_eq!(seen, "hi");
    assert!(!saw_reasoning);
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
            StreamEvent::ToolCallStart | StreamEvent::Tick => {}
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
            model: test_model(None),
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
            model: test_model(Some(EffortLevel::High)),
        },
        &[],
        &[],
    );

    assert_eq!(body["reasoning"]["effort"], "high");
}
