use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Map, Value};
use tokio::time::sleep;

use crate::cli::OutputFormat;
use crate::compaction;
use crate::config::Config;
use crate::guardrail::{Guardrail, GuardrailOutcome, bash_risk};
use crate::models::RequestOptions;
use crate::provider::{
    FinishReason, Message, Provider, ProviderError, StreamEvent, ToolCall, ToolCallDelta, Usage,
};
use crate::renderer::Renderer;
use crate::store::{ReviewRecord, Store, ToolCallRecord};
use crate::tools::{ExecutionMode, ToolContext, ToolResult, missing_tool_message};
use crate::{bash, tools};
use bash::RunningBash;

pub struct TurnResult {
    pub usage: Usage,
}

struct ConcurrentBashExecution<'a> {
    call: &'a ToolCall,
    args: Value,
    running: Option<RunningBash>,
    streamed_len: usize,
    completed: Option<(Result<ToolResult>, Duration)>,
}

#[derive(Default)]
struct StreamingToolPreview {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    rendered: bool,
}

#[derive(Default)]
struct StreamingToolPreviews {
    entries: Vec<StreamingToolPreview>,
    next_to_render: usize,
}

pub struct AgentLoop<'a> {
    pub config: &'a Config,
    pub provider: Arc<dyn Provider>,
    pub store: &'a Store,
    pub session_id: &'a str,
    pub request: RequestOptions,
    pub model_context_window: Option<u64>,
    pub renderer: &'a mut Renderer,
    pub state_dir: &'a Path,
    pub system_prompt: String,
}

impl<'a> AgentLoop<'a> {
    pub async fn run_turn(&mut self) -> Result<TurnResult> {
        bash::reset_cancellation_state();
        bash::install_signal_forwarder();
        compaction::maybe_compact(
            self.store,
            self.config,
            self.session_id,
            &self.request,
            self.model_context_window,
            self.provider.as_ref(),
            &self.system_prompt,
        )
        .await?;

        let mut guardrail = if self.config.guardrail.enabled {
            Some(Guardrail::new(
                self.config,
                &self.request.model,
                self.provider.clone(),
            ))
        } else {
            None
        };

        let mut context = self.load_pending_context()?;

        let tool_definitions = tools::tool_definitions();
        let max_iter = self.config.limits.max_iterations;

        let mut total_usage = Usage::default();
        let mut overflow_retries: u32 = 0;
        let mut live_provider_retries: u32 = 0;
        const MAX_OVERFLOW_RETRIES: u32 = 3;
        const MAX_LIVE_PROVIDER_RETRIES: u32 = 3;

        for iteration in 0..max_iter {
            let mut tool_previews = StreamingToolPreviews::default();
            let stream_result = loop {
                let mut on_stream_event = |event: StreamEvent| -> Result<(), ProviderError> {
                    let result = match event {
                        StreamEvent::TextDelta(text) => self.renderer.assistant_text(&text),
                        StreamEvent::ReasoningStart => self.renderer.reasoning_start(),
                        StreamEvent::ReasoningDelta(text) => self.renderer.reasoning_delta(&text),
                        StreamEvent::ReasoningEnd => self.renderer.reasoning_end(None),
                        StreamEvent::ToolCallDelta(delta) => {
                            handle_tool_call_delta(self.renderer, &mut tool_previews, delta)
                        }
                        StreamEvent::Tick => self.renderer.thinking_tick(),
                    };
                    result.map_err(|e| ProviderError::Other(e.to_string()))
                };
                let result = self
                    .provider
                    .stream_chat(
                        &self.request,
                        &context,
                        &tool_definitions,
                        &mut on_stream_event,
                    )
                    .await;
                self.renderer.assistant_end()?;
                match &result {
                    Ok(stream_result) => {
                        let usage = stream_result
                            .usage
                            .as_ref()
                            .map(|u| (u.visible_input_tokens(), u.visible_output_tokens()));
                        self.renderer.reasoning_end(usage)?;
                    }
                    Err(_) => self.renderer.cancel_live_state()?,
                }
                match result {
                    Ok(r) => break r,
                    Err(ProviderError::ContextLength)
                        if overflow_retries < MAX_OVERFLOW_RETRIES =>
                    {
                        overflow_retries += 1;
                        compaction::run_compaction(
                            self.store,
                            self.config,
                            self.session_id,
                            &self.request,
                            self.provider.as_ref(),
                            &self.system_prompt,
                        )
                        .await?;
                        context = self.load_pending_context()?;
                    }
                    Err(ProviderError::ContextLength) => {
                        bail!("context length exceeded even after compaction");
                    }
                    Err(error)
                        if error.retryable_for_live_turn()
                            && live_provider_retries < MAX_LIVE_PROVIDER_RETRIES =>
                    {
                        live_provider_retries += 1;
                        let retry_count =
                            self.store.increment_pending_retry_count(self.session_id)?;
                        let pending =
                            self.store.pending_turn(self.session_id)?.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "session missing pending turn during provider retry recovery"
                                )
                            })?;
                        self.renderer.turn_retry(
                            "auto",
                            retry_count,
                            Some(MAX_LIVE_PROVIDER_RETRIES as u64),
                            pending.checkpoint_message_id,
                            &error.to_string(),
                        )?;
                        context = self.load_pending_context()?;
                    }
                    Err(e) => bail!("provider error: {e}"),
                }
            };

            if let Some(u) = &stream_result.usage {
                total_usage.input_tokens += u.input_tokens;
                total_usage.cache_read_input_tokens += u.cache_read_input_tokens;
                total_usage.cache_write_input_tokens += u.cache_write_input_tokens;
                total_usage.output_tokens += u.output_tokens;
                total_usage.reasoning_output_tokens += u.reasoning_output_tokens;
                total_usage.total_tokens = u.total_tokens;
            }

            let msg_id = self
                .store
                .advance_pending_checkpoint_with_message(self.session_id, &stream_result.message)?;
            context.push(stream_result.message.clone());

            match stream_result.finish_reason {
                FinishReason::Stop => break,
                FinishReason::ToolCalls => {
                    let tool_calls = match &stream_result.message {
                        Message::Assistant { tool_calls, .. } => tool_calls
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("missing tool_calls"))?,
                        _ => bail!("expected assistant message with tool calls"),
                    };

                    let mut cursor = 0;
                    while cursor < tool_calls.len() {
                        if bash::cancellation_requested() {
                            bail!("turn interrupted");
                        }
                        let args = parse_tool_args(&tool_calls[cursor]);
                        let concurrent = self.concurrent_tool_call_eligible(
                            guardrail.as_ref(),
                            &tool_calls[cursor],
                            &args,
                        );

                        if !concurrent {
                            let tc = &tool_calls[cursor];

                            // Guardrail: review destructive bash calls before execution.
                            // Runs before tool_start so denied commands never print a `$` line.
                            if let Some(g) = guardrail.as_mut()
                                && tc.function.name == "bash"
                            {
                                let risk = bash_risk(&args);
                                if risk.is_none() {
                                    let err = anyhow::anyhow!(
                                        "bash tool call missing required `risk` field"
                                    );
                                    self.persist_tool_result(
                                        msg_id,
                                        tc,
                                        Err(err),
                                        Duration::ZERO,
                                        &mut context,
                                        true,
                                    )?;
                                    cursor += 1;
                                    continue;
                                }
                                if g.should_review(risk.as_deref().unwrap_or("")) {
                                    let args_for_review = args.clone();
                                    let action_json =
                                        serde_json::to_string(&args_for_review).unwrap_or_default();
                                    let script = args_for_review
                                        .get("script")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    match g.assess(&args_for_review, &context).await {
                                        GuardrailOutcome::Allow(a) => {
                                            let risk_level = a.risk_level.to_string();
                                            let user_auth_level = a.user_auth_level.to_string();
                                            self.renderer.guardrail_verdict(
                                                true,
                                                &risk_level,
                                                &user_auth_level,
                                                &a.reason,
                                                script,
                                            )?;
                                            self.store.record_review(ReviewRecord {
                                                session_id: self.session_id,
                                                tool_call_id: Some(&tc.id),
                                                action_json: &action_json,
                                                risk_level: &risk_level,
                                                user_auth_level: &user_auth_level,
                                                outcome: a.outcome(),
                                                reason: Some(&a.reason),
                                            })?;
                                        }
                                        GuardrailOutcome::Deny(a) => {
                                            let risk_level = a.risk_level.to_string();
                                            let user_auth_level = a.user_auth_level.to_string();
                                            self.renderer.guardrail_verdict(
                                                false,
                                                &risk_level,
                                                &user_auth_level,
                                                &a.reason,
                                                script,
                                            )?;
                                            self.store.record_review(ReviewRecord {
                                                session_id: self.session_id,
                                                tool_call_id: Some(&tc.id),
                                                action_json: &action_json,
                                                risk_level: &risk_level,
                                                user_auth_level: &user_auth_level,
                                                outcome: a.outcome(),
                                                reason: Some(&a.reason),
                                            })?;
                                            if let Some((consec, recent)) =
                                                g.circuit_breaker_tripped()
                                            {
                                                self.renderer.notice(&format!(
                                                    "[mu] guardrail: aborting turn — {consec} consecutive denials ({recent} in recent window)"
                                                ))?;
                                                bail!("guardrail circuit breaker tripped");
                                            }
                                            let deny_err = anyhow::anyhow!(
                                                "guardrail: action rejected — risk_level {} exceeds user_auth_level {} ({}). \
                                                 Do not work around this; stop and ask the user to authorize, \
                                                 or choose a less destructive approach.",
                                                a.risk_level,
                                                a.user_auth_level,
                                                a.reason
                                            );
                                            let deny_msg = Message::Tool {
                                                content: format!("error: {deny_err}"),
                                                tool_call_id: tc.id.clone(),
                                            };
                                            self.store
                                                .append_message(self.session_id, &deny_msg)?;
                                            context.push(deny_msg);
                                            let output = format!("error: {deny_err}");
                                            self.store.record_tool_call(ToolCallRecord {
                                                message_id: msg_id,
                                                id: &tc.id,
                                                tool: &tc.function.name,
                                                args: &tc.function.arguments,
                                                risk: risk.as_deref(),
                                                output: &output,
                                                status: "error",
                                            })?;
                                            cursor += 1;
                                            continue;
                                        }
                                        GuardrailOutcome::Failed(e) => {
                                            bail!("guardrail review failed: {e}");
                                        }
                                    }
                                }
                            }

                            self.renderer.tool_start(
                                Some(&tc.id),
                                &tc.function.name,
                                &args,
                                tool_previews
                                    .entries
                                    .get(cursor)
                                    .map(|preview| preview.rendered)
                                    .unwrap_or(false),
                            )?;
                            let started = Instant::now();

                            let tool_result = if tc.function.name == "bash" {
                                let mut ctx = ToolContext {
                                    config: self.config,
                                    renderer: Some(self.renderer),
                                    state_dir: self.state_dir,
                                };
                                tools::execute_bash_tool(args, &mut ctx).await
                            } else {
                                unknown_tool_result(&tc.function.name)
                            };

                            self.persist_tool_result(
                                msg_id,
                                tc,
                                tool_result,
                                started.elapsed(),
                                &mut context,
                                true,
                            )?;
                            cursor += 1;
                            continue;
                        }

                        let mut end = cursor + 1;
                        while end < tool_calls.len() {
                            let next_args = parse_tool_args(&tool_calls[end]);
                            let next_concurrent = self.concurrent_tool_call_eligible(
                                guardrail.as_ref(),
                                &tool_calls[end],
                                &next_args,
                            );
                            if !next_concurrent {
                                break;
                            }
                            end += 1;
                        }

                        let batch = &tool_calls[cursor..end];
                        for chunk in batch.chunks(bash::MAX_ACTIVE_PROCESS_GROUPS) {
                            self.execute_concurrent_bash_batch(msg_id, chunk, &mut context)
                                .await?;
                            if bash::cancellation_requested() {
                                bail!("turn interrupted");
                            }
                        }
                        cursor = end;
                    }
                }
                FinishReason::Other(reason) => {
                    self.renderer
                        .notice(&format!("[mu] stopped: finish_reason={reason}"))?;
                    break;
                }
            }

            if iteration + 1 >= max_iter {
                self.renderer
                    .notice("[mu] max iterations reached; stopping")?;
                bail!("max iterations reached");
            }
        }

        Ok(TurnResult { usage: total_usage })
    }

    fn load_pending_context(&self) -> Result<Vec<Message>> {
        let checkpoint_message_id = self
            .store
            .pending_turn(self.session_id)?
            .map(|pending| pending.checkpoint_message_id);
        let mut context = self
            .store
            .load_context_messages_until(self.session_id, checkpoint_message_id)?;
        context.insert(
            0,
            Message::System {
                content: self.system_prompt.clone(),
            },
        );
        Ok(context)
    }

    fn persist_tool_result(
        &mut self,
        message_id: i64,
        call: &ToolCall,
        result: Result<ToolResult>,
        elapsed: Duration,
        context: &mut Vec<Message>,
        emit_renderer: bool,
    ) -> Result<()> {
        let (output, status) = match result {
            Ok(result) => {
                if emit_renderer {
                    self.renderer.tool_finished(
                        Some(&call.id),
                        &call.function.name,
                        &result.display,
                        elapsed,
                    )?;
                }
                (result.output, "ok")
            }
            Err(error) => {
                let message = format!("error: {error}");
                if emit_renderer {
                    self.renderer.tool_failed(
                        Some(&call.id),
                        &call.function.name,
                        &error.to_string(),
                        elapsed,
                    )?;
                }
                (message, "error")
            }
        };

        let risk = tool_call_risk(&call.function.arguments);
        let message = Message::Tool {
            content: output.clone(),
            tool_call_id: call.id.clone(),
        };
        self.store.persist_tool_result(
            self.session_id,
            ToolCallRecord {
                message_id,
                id: &call.id,
                tool: &call.function.name,
                args: &call.function.arguments,
                risk: risk.as_deref(),
                output: &output,
                status,
            },
            &output,
        )?;
        context.push(message);
        Ok(())
    }

    fn concurrent_tool_call_eligible(
        &self,
        guardrail: Option<&Guardrail>,
        call: &ToolCall,
        args: &Value,
    ) -> bool {
        if self.renderer.output_format() == OutputFormat::Plain {
            return false;
        }
        let Some(mode) = tools::execution_mode(&call.function.name, args) else {
            return false;
        };
        if mode != ExecutionMode::Concurrent {
            return false;
        }
        !guardrail_review_required(guardrail, call, args)
    }

    async fn execute_concurrent_bash_batch(
        &mut self,
        message_id: i64,
        batch: &[ToolCall],
        context: &mut Vec<Message>,
    ) -> Result<()> {
        let mut executions = Vec::new();
        for call in batch {
            let args = parse_tool_args(call);
            let bash_args = tools::parse_args(&args)?;
            executions.push(ConcurrentBashExecution {
                call,
                args,
                running: Some(bash::start_bash_task(
                    bash_args,
                    self.config,
                    self.state_dir,
                )),
                streamed_len: 0,
                completed: None,
            });
        }

        match self.renderer.output_format() {
            OutputFormat::Terminal => {
                for exec in &mut executions {
                    if let Some(running) = exec.running.as_ref() {
                        for warning in running.warnings() {
                            self.renderer.notice(&format!("[redaction] {warning}"))?;
                        }
                    }
                    self.renderer.tool_start(
                        Some(&exec.call.id),
                        &exec.call.function.name,
                        &exec.args,
                        false,
                    )?;
                    self.stream_running_bash(exec).await?;
                    let (result, elapsed, final_output) = exec
                        .running
                        .take()
                        .expect("running bash present")
                        .finish()
                        .await;
                    self.flush_buffered_bash_output(exec, &final_output)?;
                    self.persist_tool_result(
                        message_id, exec.call, result, elapsed, context, true,
                    )?;
                }
            }
            OutputFormat::Json => {
                for exec in &mut executions {
                    if let Some(running) = exec.running.as_ref() {
                        for warning in running.warnings() {
                            self.renderer.notice(&format!("[redaction] {warning}"))?;
                        }
                    }
                    self.renderer.tool_start(
                        Some(&exec.call.id),
                        &exec.call.function.name,
                        &exec.args,
                        false,
                    )?;
                }

                while executions.iter().any(|exec| exec.completed.is_none()) {
                    let mut progressed = false;
                    for exec in &mut executions {
                        if exec.completed.is_some() {
                            continue;
                        }
                        let (snapshot, finished) = if let Some(running) = exec.running.as_ref() {
                            (running.snapshot_output(), running.is_finished())
                        } else {
                            (String::new(), false)
                        };
                        if self.flush_buffered_bash_output(exec, &snapshot)? {
                            progressed = true;
                        }
                        if finished {
                            let (result, elapsed, final_output) = exec
                                .running
                                .take()
                                .expect("running bash present")
                                .finish()
                                .await;
                            let _ = self.flush_buffered_bash_output(exec, &final_output)?;
                            match result.as_ref() {
                                Ok(result) => self.renderer.tool_finished(
                                    Some(&exec.call.id),
                                    &exec.call.function.name,
                                    &result.display,
                                    elapsed,
                                )?,
                                Err(error) => self.renderer.tool_failed(
                                    Some(&exec.call.id),
                                    &exec.call.function.name,
                                    &error.to_string(),
                                    elapsed,
                                )?,
                            }
                            exec.completed = Some((result, elapsed));
                            progressed = true;
                        }
                    }
                    if executions.iter().any(|exec| exec.completed.is_none()) && !progressed {
                        sleep(Duration::from_millis(25)).await;
                    }
                }

                for exec in executions {
                    let (result, elapsed) = exec.completed.expect("completed bash result");
                    self.persist_tool_result(
                        message_id, exec.call, result, elapsed, context, false,
                    )?;
                }
            }
            OutputFormat::Plain => unreachable!("plain mode stays sequential"),
        }

        Ok(())
    }

    async fn stream_running_bash(&mut self, exec: &mut ConcurrentBashExecution<'_>) -> Result<()> {
        loop {
            let (snapshot, finished) = if let Some(running) = exec.running.as_ref() {
                (running.snapshot_output(), running.is_finished())
            } else {
                (String::new(), false)
            };
            self.flush_buffered_bash_output(exec, &snapshot)?;
            if finished {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    }

    fn flush_buffered_bash_output(
        &mut self,
        exec: &mut ConcurrentBashExecution<'_>,
        snapshot: &str,
    ) -> Result<bool> {
        if snapshot.len() <= exec.streamed_len {
            return Ok(false);
        }
        let next = snapshot[exec.streamed_len..].to_string();
        exec.streamed_len = snapshot.len();
        self.renderer
            .bash_output(Some(&exec.call.id), &exec.call.function.name, &next)?;
        Ok(true)
    }
}

fn parse_tool_args(call: &ToolCall) -> Value {
    serde_json::from_str(&call.function.arguments).unwrap_or(Value::Object(Default::default()))
}

fn handle_tool_call_delta(
    renderer: &mut Renderer,
    previews: &mut StreamingToolPreviews,
    delta: ToolCallDelta,
) -> std::io::Result<()> {
    if delta.index >= previews.entries.len() {
        previews
            .entries
            .resize_with(delta.index + 1, StreamingToolPreview::default);
    }
    let preview = &mut previews.entries[delta.index];
    let was_rendered = preview.rendered;
    if let Some(id) = delta.id {
        preview.id = Some(id);
    }
    if let Some(name) = delta.name {
        preview.name = Some(name);
    }
    preview.arguments.push_str(&delta.arguments_delta);
    if was_rendered {
        return Ok(());
    }

    let mut rendered_any = false;
    while previews.next_to_render < previews.entries.len() {
        let preview = &mut previews.entries[previews.next_to_render];
        if preview.rendered {
            previews.next_to_render += 1;
            continue;
        }
        let Some(name) = preview.name.as_deref() else {
            break;
        };
        let Some(args) = parse_preview_args(&preview.arguments) else {
            break;
        };
        preview.rendered = renderer.tool_call_preview(preview.id.as_deref(), name, &args)?;
        if !preview.rendered {
            break;
        }
        rendered_any = true;
        previews.next_to_render += 1;
    }

    if rendered_any {
        Ok(())
    } else {
        renderer.tool_call_composition_start()
    }
}

fn parse_preview_args(arguments: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str(arguments) {
        return Some(value);
    }

    let title = extract_json_string_field(arguments, "title");
    let risk = extract_json_string_field(arguments, "risk");
    let script = extract_json_string_field(arguments, "script")?;

    let mut map = Map::new();
    if let Some(title) = title {
        map.insert("title".into(), Value::String(title));
    }
    if let Some(risk) = risk {
        map.insert("risk".into(), Value::String(risk));
    }
    map.insert("script".into(), Value::String(script));
    Some(Value::Object(map))
}

fn extract_json_string_field(input: &str, field: &str) -> Option<String> {
    let quoted_field = format!("\"{field}\"");
    let mut search_from = 0;
    while let Some(relative) = input[search_from..].find(&quoted_field) {
        let after_field = search_from + relative + quoted_field.len();
        let rest = &input[after_field..];
        let colon = rest.find(':')?;
        let mut value_start = after_field + colon + 1;
        while matches!(
            input.as_bytes().get(value_start),
            Some(b' ' | b'\n' | b'\r' | b'\t')
        ) {
            value_start += 1;
        }
        if input.as_bytes().get(value_start) != Some(&b'"') {
            search_from = value_start;
            continue;
        }
        return parse_json_string_prefix(&input[value_start + 1..]);
    }
    None
}

fn parse_json_string_prefix(input: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('b') => out.push('\u{0008}'),
                Some('f') => out.push('\u{000c}'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let mut code = String::new();
                    for _ in 0..4 {
                        code.push(chars.next()?);
                    }
                    let value = u16::from_str_radix(&code, 16).ok()?;
                    out.push(char::from_u32(value as u32)?);
                }
                Some(other) => out.push(other),
                None => return Some(out),
            },
            other => out.push(other),
        }
    }
    Some(out)
}

fn tool_call_risk(args: &str) -> Option<String> {
    let value: Value = serde_json::from_str(args).ok()?;
    let risk = value.get("risk")?.as_str()?;
    match risk {
        "readonly" | "reversible" | "destructive" => Some(risk.to_string()),
        _ => None,
    }
}

fn guardrail_review_required(guardrail: Option<&Guardrail>, call: &ToolCall, args: &Value) -> bool {
    if call.function.name != "bash" {
        return false;
    }
    let Some(guardrail) = guardrail else {
        return false;
    };
    let Some(risk) = bash_risk(args) else {
        return false;
    };
    guardrail.should_review(risk.as_str())
}

fn unknown_tool_result(name: &str) -> Result<ToolResult> {
    Err(anyhow::anyhow!(missing_tool_message(name)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::config::{
        CompactionConfig, GuardrailConfig, LimitsConfig, ProviderConfig, RedactionConfig,
        TerminalBellConfig,
    };
    use crate::provider::{FinishReason, ProviderError, StreamResult, Usage, UserContent};

    struct RetryThenStopProvider {
        step: Mutex<usize>,
    }

    #[async_trait(?Send)]
    impl Provider for RetryThenStopProvider {
        async fn stream_chat(
            &self,
            _request: &RequestOptions,
            _messages: &[Message],
            _tools: &[Value],
            _on_event: &mut dyn FnMut(crate::provider::StreamEvent) -> Result<(), ProviderError>,
        ) -> Result<StreamResult, ProviderError> {
            let mut step = self.step.lock().unwrap();
            let current = *step;
            *step += 1;
            match current {
                0 => Err(ProviderError::RateLimit {
                    message: "slow down".into(),
                }),
                1 => Ok(StreamResult {
                    message: Message::Assistant {
                        content: Some("done".into()),
                        tool_calls: None,
                    },
                    finish_reason: FinishReason::Stop,
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 2,
                        ..Usage::default()
                    }),
                }),
                other => panic!("unexpected retry provider step {other}"),
            }
        }
    }

    fn test_config() -> Config {
        Config {
            providers: crate::config::OrderedMap::from_iter([(
                "test".into(),
                ProviderConfig {
                    base_url: "http://localhost".into(),
                    api_key_env: "MU_TEST_KEY".into(),
                    models: crate::config::OrderedMap::from_iter([(
                        "fake-model".into(),
                        crate::config::ModelConfig {
                            context_window: None,
                            supported_efforts: None,
                        },
                    )]),
                },
            )]),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn preview_args_parse_partial_bash_fields() {
        let value = parse_preview_args(
            "{\"title\":\"List files\",\"risk\":\"readonly\",\"script\":\"printf \\\"a\\\\n\\\"",
        )
        .unwrap();

        assert_eq!(value["title"], "List files");
        assert_eq!(value["risk"], "readonly");
        assert_eq!(value["script"], "printf \"a\\n\"");
    }

    #[test]
    fn preview_args_parse_multiple_tool_calls_independently() {
        let first = parse_preview_args(
            "{\"title\":\"First\",\"risk\":\"readonly\",\"script\":\"echo first\"",
        )
        .unwrap();
        let second = parse_preview_args(
            "{\"title\":\"Second\",\"risk\":\"reversible\",\"script\":\"echo second\"",
        )
        .unwrap();

        assert_eq!(first["title"], "First");
        assert_eq!(first["risk"], "readonly");
        assert_eq!(first["script"], "echo first");
        assert_eq!(second["title"], "Second");
        assert_eq!(second["risk"], "reversible");
        assert_eq!(second["script"], "echo second");
    }

    #[test]
    fn streamed_tool_previews_render_in_index_order() {
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let mut previews = StreamingToolPreviews::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut previews,
            ToolCallDelta {
                index: 1,
                id: Some("call_2".into()),
                name: Some("bash".into()),
                arguments_delta:
                    "{\"title\":\"Second\",\"risk\":\"readonly\",\"script\":\"echo second\"".into(),
            },
        )
        .unwrap();

        assert_eq!(previews.next_to_render, 0);
        assert!(!previews.entries[1].rendered);

        handle_tool_call_delta(
            &mut renderer,
            &mut previews,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta:
                    "{\"title\":\"First\",\"risk\":\"readonly\",\"script\":\"echo first\"".into(),
            },
        )
        .unwrap();

        assert_eq!(previews.next_to_render, 2);
        assert!(previews.entries[0].rendered);
        assert!(previews.entries[1].rendered);
    }

    #[test]
    fn rendered_tool_preview_does_not_restart_composition_line() {
        let mut renderer = Renderer::with_format(OutputFormat::Terminal);
        renderer.force_styled_for_test();
        let mut previews = StreamingToolPreviews::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut previews,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta:
                    "{\"title\":\"List\",\"risk\":\"readonly\",\"script\":\"printf 'a'".into(),
            },
        )
        .unwrap();
        assert!(previews.entries[0].rendered);
        assert!(!renderer.has_tool_composition_live_line_for_test());

        handle_tool_call_delta(
            &mut renderer,
            &mut previews,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\\n'\"}".into(),
            },
        )
        .unwrap();

        assert!(previews.entries[0].rendered);
        assert!(previews.entries[0].arguments.ends_with("\\n'\"}"));
        assert!(!renderer.has_tool_composition_live_line_for_test());
    }

    #[tokio::test]
    async fn live_provider_retry_reuses_pending_checkpoint() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp", "test/fake-model").unwrap();
        let config = test_config();
        let request_model = crate::models::resolve_model_ref(&config, "test/fake-model").unwrap();
        store
            .begin_pending_turn(&session.id, &UserContent::Text("retry me".into()))
            .unwrap();
        let provider = Arc::new(RetryThenStopProvider {
            step: Mutex::new(0),
        });
        let mut renderer = Renderer::with_format(OutputFormat::Json);
        let mut agent = AgentLoop {
            config: &config,
            provider,
            store: &store,
            session_id: &session.id,
            request: RequestOptions {
                model: request_model,
            },
            model_context_window: None,
            renderer: &mut renderer,
            state_dir: &tmp,
            system_prompt: "system".into(),
        };

        agent.run_turn().await.unwrap();

        let pending = store.pending_turn(&session.id).unwrap().unwrap();
        assert_eq!(pending.retry_count, 1);
        let messages = store.load_context_messages(&session.id).unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::User { .. }))
                .count(),
            1
        );
        assert!(matches!(
            messages.last(),
            Some(Message::Assistant {
                content: Some(content),
                tool_calls: None,
            }) if content == "done"
        ));
        let _ = std::fs::remove_dir_all(tmp);
    }
}
