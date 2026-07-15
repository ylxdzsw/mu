use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::time::sleep;

use crate::compaction;
use crate::config::Config;
use crate::guardrail::{Guardrail, GuardrailOutcome};
use crate::models::RequestOptions;
use crate::provider::{
    FinishReason, Message, Provider, ProviderError, StreamEvent, ToolCall, ToolCallDelta, Usage,
    approx_tokens,
};
use crate::renderer::Renderer;
use crate::store::{ReviewRecord, Store, ToolCallRecord};
use crate::tools::{BashRisk, ExecutionMode, ToolContext, ToolResult};
use crate::{bash, tools};
use bash::RunningBash;

pub struct TurnResult {
    pub usage: Usage,
    /// Total tokens reported by the *last* model call of the turn — i.e. the
    /// current context size. Distinct from `usage.total_tokens`, which is
    /// cumulative across every call in the turn. Drives the context-fullness
    /// gauge and `session.last_total_tokens`.
    pub context_tokens: u64,
    pub final_assistant: Option<String>,
}

struct ConcurrentBashExecution<'a> {
    call: &'a ToolCall,
    args: Value,
    running: Option<RunningBash>,
    streamed_len: usize,
}

#[derive(Default)]
struct StreamingCommandHeader {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    display: CommandHeaderDisplay,
}

#[derive(Default)]
struct StreamingCommandHeaders {
    entries: Vec<StreamingCommandHeader>,
    next_to_render: usize,
}

#[derive(Default)]
struct CommandHeaderDisplay {
    started: bool,
    title_started: bool,
    title_displayed_bytes: usize,
    title_line_done: bool,
    command_started: bool,
    command_displayed_bytes: usize,
    command_line_done: bool,
    cwd_line_done: bool,
    stdin_started: bool,
    stdin_line_done: bool,
}

pub struct AgentLoop<'a> {
    pub config: &'a Config,
    pub provider: Box<dyn Provider>,
    pub store: &'a Store,
    pub session_id: &'a str,
    pub request: RequestOptions,
    pub model_context_window: Option<u64>,
    pub renderer: &'a mut Renderer,
    pub state_dir: &'a Path,
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
        )
        .await?;

        let mut guardrail = if self.config.guardrail.enabled {
            Some(Guardrail::new(self.config, &self.request.model))
        } else {
            None
        };

        let mut context = self.load_context()?;

        let tool_definitions = tools::tool_definitions();
        let max_iter = self.config.limits.max_iterations;

        let mut total_usage = Usage::default();
        let mut context_tokens = 0;
        let mut overflow_retries: u32 = 0;
        let mut live_provider_retries: u32 = 0;
        let mut proactive_compaction_exhausted = false;
        let mut final_assistant = None;
        const MAX_OVERFLOW_RETRIES: u32 = 3;
        const MAX_LIVE_PROVIDER_RETRIES: u32 = 3;

        for iteration in 0..max_iter {
            // Proactive compaction (SPEC §11 Tier 2): `maybe_compact` only runs
            // once before the turn, but a single turn can add many large tool
            // results. Re-check the growing context before each subsequent model
            // call so we compact gracefully instead of waiting for the hard API
            // overflow guard (Tier 3, below). Uses the same context * fraction
            // threshold as the pre-turn check.
            if iteration > 0
                && !proactive_compaction_exhausted
                && let Some(context_window) = self.model_context_window
            {
                let threshold = (context_window as f64 * self.config.compaction.fraction) as u64;
                if approx_context_tokens(&context) > threshold {
                    compaction::run_compaction(
                        self.store,
                        self.config,
                        self.session_id,
                        &self.request,
                        self.provider.as_ref(),
                        None,
                    )
                    .await?;
                    context = self.load_context()?;
                    // If compaction could not get us back under the threshold
                    // (e.g. the retained recent turns alone are huge), stop
                    // retrying proactively this turn to avoid repeated
                    // summarize calls; the reactive guard still covers a true
                    // hard overflow.
                    if approx_context_tokens(&context) > threshold {
                        proactive_compaction_exhausted = true;
                    }
                }
            }

            let mut command_headers = StreamingCommandHeaders::default();
            let stream_result = loop {
                let mut on_stream_event = |event: StreamEvent| -> Result<(), ProviderError> {
                    let result = match event {
                        StreamEvent::TextDelta(text) => self.renderer.assistant_text(&text),
                        StreamEvent::ReasoningStart(visibility) => {
                            self.renderer.reasoning_start(visibility)
                        }
                        StreamEvent::ReasoningDelta(text) => self.renderer.reasoning_delta(&text),
                        StreamEvent::ReasoningSummaryDelta(text) => {
                            self.renderer.reasoning_summary_delta(&text)
                        }
                        StreamEvent::ReasoningEnd => self.renderer.reasoning_end(None),
                        StreamEvent::ToolCallDelta(delta) => {
                            handle_tool_call_delta(self.renderer, &mut command_headers, delta)
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
                            None,
                        )
                        .await?;
                        context = self.load_context()?;
                    }
                    Err(ProviderError::ContextLength) => {
                        bail!("context length exceeded even after compaction");
                    }
                    Err(error)
                        if error.retryable_for_live_turn()
                            && live_provider_retries < MAX_LIVE_PROVIDER_RETRIES =>
                    {
                        live_provider_retries += 1;
                        self.renderer.turn_retry(
                            live_provider_retries as u64,
                            MAX_LIVE_PROVIDER_RETRIES as u64,
                            &error.to_string(),
                        )?;
                        context = self.load_context()?;
                    }
                    Err(e) => bail!("provider error: {e}"),
                }
            };

            if let Some(u) = &stream_result.usage {
                total_usage.input_tokens += u.input_tokens;
                total_usage.cache_read_input_tokens += u.cache_read_input_tokens;
                if let Some(cache_write_tokens) = u.cache_write_input_tokens {
                    *total_usage.cache_write_input_tokens.get_or_insert(0) += cache_write_tokens;
                }
                total_usage.output_tokens += u.output_tokens;
                total_usage.reasoning_output_tokens += u.reasoning_output_tokens;
                total_usage.total_tokens += u.total_tokens;
                context_tokens = u.total_tokens;
            }

            let msg_id = self
                .store
                .append_message(self.session_id, &stream_result.message)?;
            context.push(stream_result.message.clone());

            match stream_result.finish_reason {
                FinishReason::Stop => {
                    if let Message::Assistant { content, .. } = &stream_result.message {
                        final_assistant = content.clone();
                    }
                    break;
                }
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

                            let header_already_rendered = finish_command_header(
                                self.renderer,
                                &mut command_headers,
                                cursor,
                                tc,
                                &args,
                            )?;

                            // Guardrail: review destructive bash calls before execution.
                            // The streamed command header above is the proposed action;
                            // denied commands still never stream execution output.
                            if let Some(g) = guardrail.as_mut()
                                && tc.function.name == "bash"
                            {
                                let risk = BashRisk::from_value(&args);
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
                                if g.should_review(
                                    risk.as_ref().map(|risk| risk.as_str()).unwrap_or(""),
                                ) {
                                    let args_for_review = args.clone();
                                    let action_json =
                                        serde_json::to_string(&args_for_review).unwrap_or_default();
                                    let command = args_for_review
                                        .get("command")
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
                                                command,
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
                                                command,
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
                                                artifacts: Vec::new(),
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
                                                risk: risk.as_ref().map(|risk| risk.as_str()),
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
                                header_already_rendered,
                            )?;
                            let started = Instant::now();

                            let tool_result = if tc.function.name == "bash" {
                                let mut ctx = ToolContext {
                                    config: self.config,
                                    renderer: self.renderer,
                                    state_dir: self.state_dir,
                                };
                                bash::execute(args, &mut ctx).await
                            } else {
                                Err(anyhow::anyhow!("unknown tool: {}", tc.function.name))
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
                        for (chunk_offset, chunk) in
                            batch.chunks(bash::MAX_ACTIVE_PROCESS_GROUPS).enumerate()
                        {
                            self.execute_concurrent_bash_batch(
                                msg_id,
                                chunk,
                                &mut context,
                                &mut command_headers,
                                cursor + chunk_offset * bash::MAX_ACTIVE_PROCESS_GROUPS,
                            )
                            .await?;
                            if bash::cancellation_requested() {
                                bail!("turn interrupted");
                            }
                        }
                        cursor = end;
                    }
                }
                FinishReason::Other(reason) => {
                    if let Message::Assistant { content, .. } = &stream_result.message {
                        final_assistant = content.clone();
                    }
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

        Ok(TurnResult {
            usage: total_usage,
            context_tokens,
            final_assistant,
        })
    }

    /// Load the full completed-message history, including the persisted leading
    /// system prompt.
    /// History is always valid here because the caller normalizes any
    /// interrupted tail (synthesizing missing tool results) before the turn.
    fn load_context(&self) -> Result<Vec<Message>> {
        self.store.load_context_messages(self.session_id)
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
        let (output, artifacts, status) = match result {
            Ok(result) => {
                if emit_renderer {
                    self.renderer.tool_finished(result.exit_code, elapsed)?;
                }
                (result.output, result.artifacts, "ok")
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
                (message, Vec::new(), "error")
            }
        };

        let risk = BashRisk::from_args_json(&call.function.arguments);
        self.store.persist_tool_result(
            self.session_id,
            ToolCallRecord {
                message_id,
                id: &call.id,
                tool: &call.function.name,
                args: &call.function.arguments,
                risk: risk.as_ref().map(|risk| risk.as_str()),
                output: &output,
                status,
            },
            &output,
            &artifacts,
        )?;
        context.push(Message::Tool {
            content: output,
            artifacts,
            tool_call_id: call.id.clone(),
        });
        Ok(())
    }

    fn concurrent_tool_call_eligible(
        &self,
        guardrail: Option<&Guardrail>,
        call: &ToolCall,
        args: &Value,
    ) -> bool {
        if call.function.name != "bash" {
            return false;
        }
        if bash::execution_mode(args) != ExecutionMode::Concurrent {
            return false;
        }
        !guardrail_review_required(guardrail, call, args)
    }

    async fn execute_concurrent_bash_batch(
        &mut self,
        message_id: i64,
        batch: &[ToolCall],
        context: &mut Vec<Message>,
        command_headers: &mut StreamingCommandHeaders,
        header_start_index: usize,
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
            });
        }

        for (index, exec) in executions.iter_mut().enumerate() {
            let header_already_rendered = finish_command_header(
                self.renderer,
                command_headers,
                header_start_index + index,
                exec.call,
                &exec.args,
            )?;
            if let Some(running) = exec.running.as_ref() {
                for warning in running.warnings() {
                    self.renderer.notice(&format!("[redaction] {warning}"))?;
                }
            }
            self.renderer.tool_start(
                Some(&exec.call.id),
                &exec.call.function.name,
                &exec.args,
                header_already_rendered,
            )?;
            self.stream_running_bash(exec).await?;
            let (result, elapsed, final_output) = exec
                .running
                .take()
                .expect("running bash present")
                .finish()
                .await;
            self.flush_buffered_bash_output(exec, &final_output)?;
            self.persist_tool_result(message_id, exec.call, result, elapsed, context, true)?;
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

/// Cheap char/4 estimate of the in-memory context size, mirroring the fallback
/// estimator used elsewhere. Used for the in-loop proactive compaction check so
/// growing tool output is caught before it forces a hard API overflow.
fn approx_context_tokens(context: &[Message]) -> u64 {
    context
        .iter()
        .map(|message| match message {
            Message::System { content } => approx_tokens(content),
            Message::User { content } => approx_tokens(&content.text()),
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                native_replay,
            } => {
                approx_tokens(content.as_deref().unwrap_or(""))
                    + approx_tokens(reasoning_content.as_deref().unwrap_or(""))
                    + tool_calls
                        .as_ref()
                        .map(|calls| {
                            approx_tokens(&serde_json::to_string(calls).unwrap_or_default())
                        })
                        .unwrap_or(0)
                    + native_replay
                        .as_ref()
                        .map(|native| {
                            approx_tokens(&serde_json::to_string(native).unwrap_or_default())
                        })
                        .unwrap_or(0)
            }
            Message::Tool { content, .. } => approx_tokens(content),
        })
        .sum()
}

fn handle_tool_call_delta(
    renderer: &mut Renderer,
    headers: &mut StreamingCommandHeaders,
    delta: ToolCallDelta,
) -> std::io::Result<()> {
    if delta.index >= headers.entries.len() {
        headers
            .entries
            .resize_with(delta.index + 1, StreamingCommandHeader::default);
    }
    let header = &mut headers.entries[delta.index];
    if let Some(id) = delta.id {
        header.id = Some(id);
    }
    if let Some(name) = delta.name {
        header.name = Some(name);
    }
    header.arguments.push_str(&delta.arguments_delta);

    if delta.index == 0 {
        let header = &mut headers.entries[0];
        header.display.update(
            renderer,
            CommandHeaderUpdate {
                tool_call_id: header.id.as_deref(),
                title: string_field_state(&header.arguments, "title"),
                risk: string_field_state(&header.arguments, "risk"),
                command: string_field_state(&header.arguments, "command"),
                cwd: string_field_state(&header.arguments, "cwd"),
                stdin: string_field_state(&header.arguments, "stdin"),
                arguments_complete: arguments_json_complete(&header.arguments),
            },
        )?;
        if header.display.is_done() {
            headers.next_to_render = headers.next_to_render.max(1);
        }
    }

    Ok(())
}

fn finish_command_header(
    renderer: &mut Renderer,
    headers: &mut StreamingCommandHeaders,
    index: usize,
    call: &ToolCall,
    args: &Value,
) -> std::io::Result<bool> {
    if index >= headers.entries.len() {
        headers
            .entries
            .resize_with(index + 1, StreamingCommandHeader::default);
    }
    let header = &mut headers.entries[index];
    if header.id.is_none() {
        header.id = Some(call.id.clone());
    }
    header.finish(renderer, args)
}

impl StreamingCommandHeader {
    fn finish(&mut self, renderer: &mut Renderer, args: &Value) -> std::io::Result<bool> {
        let title = args.get("title").and_then(|value| value.as_str());
        let risk = args.get("risk").and_then(|value| value.as_str());
        let command = args.get("command").and_then(|value| value.as_str());
        let stdin = args.get("stdin").and_then(|value| value.as_str());
        self.display.update(
            renderer,
            CommandHeaderUpdate {
                tool_call_id: self.id.as_deref(),
                title: StringFieldState::from_final(title),
                risk: StringFieldState::from_final(risk),
                command: StringFieldState::from_final(command),
                cwd: StringFieldState::from_final(args.get("cwd").and_then(|value| value.as_str())),
                stdin: StringFieldState::from_final(stdin),
                arguments_complete: true,
            },
        )?;
        Ok(self.display.started)
    }
}

impl CommandHeaderDisplay {
    fn is_done(&self) -> bool {
        self.title_line_done
            && self.command_line_done
            && self.cwd_line_done
            && (!self.stdin_started || self.stdin_line_done)
    }

    fn update(
        &mut self,
        renderer: &mut Renderer,
        update: CommandHeaderUpdate<'_>,
    ) -> std::io::Result<()> {
        let CommandHeaderUpdate {
            tool_call_id,
            title,
            risk,
            command,
            cwd,
            stdin,
            arguments_complete,
        } = update;
        if !self.started {
            self.started = renderer.bash_header_start(tool_call_id)?;
        }

        if !self.title_line_done {
            if let Some(value) = title.value() {
                if !self.title_started {
                    renderer.bash_header_title_start()?;
                    self.title_started = true;
                }
                let done = stream_first_line(
                    value,
                    title.is_complete(),
                    crate::renderer::BASH_TITLE_PREVIEW_BYTES,
                    &mut self.title_displayed_bytes,
                    |text| renderer.bash_header_title_delta(text),
                )?;
                if done {
                    renderer.bash_header_title_end()?;
                    self.title_line_done = true;
                }
            } else if arguments_complete {
                renderer.bash_header_title_start()?;
                renderer.bash_header_title_end()?;
                self.title_started = true;
                self.title_line_done = true;
            }
        }

        let Some(risk) = risk.complete_value() else {
            return Ok(());
        };

        if self.title_line_done && !self.command_started {
            renderer.bash_header_command_start(Some(risk))?;
            self.command_started = true;
        }

        if self.command_started
            && !self.command_line_done
            && let Some(value) = command.value()
        {
            let done = stream_first_line(
                value,
                command.is_complete(),
                crate::renderer::BASH_COMMAND_PREVIEW_BYTES,
                &mut self.command_displayed_bytes,
                |text| renderer.bash_header_command_delta(text),
            )?;
            if done {
                renderer.bash_header_command_end()?;
                self.command_line_done = true;
            }
        }
        if self.command_line_done && !self.cwd_line_done {
            match cwd {
                StringFieldState::Complete(value) => {
                    renderer.bash_header_cwd_line(&value)?;
                    self.cwd_line_done = true;
                }
                StringFieldState::Missing if arguments_complete => {
                    self.cwd_line_done = true;
                }
                StringFieldState::Missing | StringFieldState::Partial(_) => {}
            }
        }
        if self.command_line_done
            && self.cwd_line_done
            && !self.stdin_line_done
            && let Some(value) = stdin.value()
        {
            self.stdin_started = true;
            renderer.bash_header_stdin_summary(value.len(), stdin.is_complete())?;
            if stdin.is_complete() {
                self.stdin_line_done = true;
            }
        }
        Ok(())
    }
}

struct CommandHeaderUpdate<'a> {
    tool_call_id: Option<&'a str>,
    title: StringFieldState,
    risk: StringFieldState,
    command: StringFieldState,
    cwd: StringFieldState,
    stdin: StringFieldState,
    arguments_complete: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum StringFieldState {
    Missing,
    Partial(String),
    Complete(String),
}

impl StringFieldState {
    fn from_final(value: Option<&str>) -> Self {
        value
            .map(|value| Self::Complete(value.to_string()))
            .unwrap_or(Self::Missing)
    }

    fn value(&self) -> Option<&str> {
        match self {
            Self::Missing => None,
            Self::Partial(value) | Self::Complete(value) => Some(value),
        }
    }

    fn complete_value(&self) -> Option<&str> {
        match self {
            Self::Complete(value) => Some(value),
            Self::Missing | Self::Partial(_) => None,
        }
    }

    fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }
}

enum JsonStringParse {
    Complete { value: String, consumed: usize },
    Partial(String),
    Invalid,
}

fn arguments_json_complete(input: &str) -> bool {
    matches!(serde_json::from_str::<Value>(input), Ok(Value::Object(_)))
}

fn string_field_state(input: &str, field: &str) -> StringFieldState {
    let bytes = input.as_bytes();
    let mut pos = skip_ws(input, 0);
    if bytes.get(pos) != Some(&b'{') {
        return StringFieldState::Missing;
    }
    pos += 1;

    loop {
        pos = skip_ws(input, pos);
        match bytes.get(pos) {
            Some(b',') => {
                pos += 1;
                continue;
            }
            Some(b'}') | None => return StringFieldState::Missing,
            Some(b'"') => {}
            Some(_) => return StringFieldState::Missing,
        }

        let JsonStringParse::Complete {
            value: key,
            consumed,
        } = parse_json_string(&input[pos + 1..])
        else {
            return StringFieldState::Missing;
        };
        pos += 1 + consumed;
        pos = skip_ws(input, pos);
        if bytes.get(pos) != Some(&b':') {
            return StringFieldState::Missing;
        }
        pos += 1;
        pos = skip_ws(input, pos);

        if key == field {
            if bytes.get(pos) != Some(&b'"') {
                return StringFieldState::Missing;
            }
            return match parse_json_string(&input[pos + 1..]) {
                JsonStringParse::Complete { value, .. } => StringFieldState::Complete(value),
                JsonStringParse::Partial(value) => StringFieldState::Partial(value),
                JsonStringParse::Invalid => StringFieldState::Missing,
            };
        }

        let Some(next) = skip_json_value(input, pos) else {
            return StringFieldState::Missing;
        };
        pos = next;
    }
}

fn skip_ws(input: &str, mut pos: usize) -> usize {
    while matches!(
        input.as_bytes().get(pos),
        Some(b' ' | b'\n' | b'\r' | b'\t')
    ) {
        pos += 1;
    }
    pos
}

fn skip_json_value(input: &str, pos: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    match bytes.get(pos)? {
        b'"' => match parse_json_string(&input[pos + 1..]) {
            JsonStringParse::Complete { consumed, .. } => Some(pos + 1 + consumed),
            JsonStringParse::Partial(_) | JsonStringParse::Invalid => None,
        },
        b'{' | b'[' => skip_balanced_json(input, pos),
        _ => {
            let mut end = pos;
            while let Some(byte) = bytes.get(end) {
                if matches!(byte, b',' | b'}') {
                    break;
                }
                end += 1;
            }
            (end > pos).then_some(end)
        }
    }
}

fn skip_balanced_json(input: &str, pos: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut cursor = pos;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'"' => match parse_json_string(&input[cursor + 1..]) {
                JsonStringParse::Complete { consumed, .. } => cursor += 1 + consumed,
                JsonStringParse::Partial(_) | JsonStringParse::Invalid => return None,
            },
            b'{' | b'[' => {
                depth += 1;
                cursor += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                cursor += 1;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    None
}

fn parse_json_string(input: &str) -> JsonStringParse {
    let mut out = String::new();
    let mut chars = input.char_indices();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                return JsonStringParse::Complete {
                    value: out,
                    consumed: idx + ch.len_utf8(),
                };
            }
            '\\' => match chars.next() {
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) => out.push('\\'),
                Some((_, '/')) => out.push('/'),
                Some((_, 'b')) => out.push('\u{0008}'),
                Some((_, 'f')) => out.push('\u{000c}'),
                Some((_, 'n')) => out.push('\n'),
                Some((_, 'r')) => out.push('\r'),
                Some((_, 't')) => out.push('\t'),
                Some((_, 'u')) => {
                    let mut code = String::new();
                    for _ in 0..4 {
                        let Some((hex_idx, hex)) = chars.next() else {
                            return JsonStringParse::Partial(out);
                        };
                        code.push(hex);
                        let _ = hex_idx;
                    }
                    let Ok(value) = u16::from_str_radix(&code, 16) else {
                        return JsonStringParse::Invalid;
                    };
                    let Some(ch) = char::from_u32(value as u32) else {
                        return JsonStringParse::Invalid;
                    };
                    out.push(ch);
                }
                Some((_, other)) => out.push(other),
                None => return JsonStringParse::Partial(out),
            },
            other => out.push(other),
        }
    }
    JsonStringParse::Partial(out)
}

fn stream_first_line(
    value: &str,
    complete: bool,
    max_bytes: usize,
    displayed_bytes: &mut usize,
    mut write: impl FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    let body_limit = max_bytes.saturating_sub(crate::renderer::ELLIPSIS.len());
    let start = (*displayed_bytes).min(value.len());
    let mut out = String::new();
    let mut consumed = start;

    for (relative, ch) in value[start..].char_indices() {
        let absolute = start + relative;
        if ch == '\n' {
            out.push_str(crate::renderer::ELLIPSIS);
            write(&out)?;
            return Ok(true);
        }
        let next = absolute + ch.len_utf8();
        if next > body_limit {
            out.push_str(crate::renderer::ELLIPSIS);
            write(&out)?;
            return Ok(true);
        }
        out.push(ch);
        consumed = next;
    }

    *displayed_bytes = consumed;
    write(&out)?;
    if complete {
        return Ok(true);
    }
    if value.len() > body_limit {
        write(crate::renderer::ELLIPSIS)?;
        return Ok(true);
    }
    Ok(false)
}

fn guardrail_review_required(guardrail: Option<&Guardrail>, call: &ToolCall, args: &Value) -> bool {
    if call.function.name != "bash" {
        return false;
    }
    let Some(guardrail) = guardrail else {
        return false;
    };
    let Some(risk) = BashRisk::from_value(args) else {
        return false;
    };
    guardrail.should_review(risk.as_str())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::cli::OutputFormat;
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
                        reasoning_content: None,
                        native_replay: None,
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

    struct TwoReadonlyThenStopProvider {
        step: Mutex<usize>,
        barrier_path: String,
    }

    #[async_trait(?Send)]
    impl Provider for TwoReadonlyThenStopProvider {
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
                0 => {
                    let first_command = format!(
                        "while [ ! -f '{}' ]; do sleep 0.05; done; printf first",
                        self.barrier_path
                    );
                    let second_command = format!("touch '{}'; printf second", self.barrier_path);
                    Ok(StreamResult {
                        message: Message::Assistant {
                            content: None,
                            reasoning_content: None,
                            native_replay: None,
                            tool_calls: Some(vec![
                                ToolCall {
                                    id: "call_first".into(),
                                    function: crate::provider::FunctionCall {
                                        name: "bash".into(),
                                        arguments: serde_json::json!({
                                            "title": "first",
                                            "risk": "readonly",
                                            "command": first_command,
                                            "timeout": 3,
                                        })
                                        .to_string(),
                                    },
                                },
                                ToolCall {
                                    id: "call_second".into(),
                                    function: crate::provider::FunctionCall {
                                        name: "bash".into(),
                                        arguments: serde_json::json!({
                                            "title": "second",
                                            "risk": "readonly",
                                            "command": second_command,
                                            "timeout": 3,
                                        })
                                        .to_string(),
                                    },
                                },
                            ]),
                        },
                        finish_reason: FinishReason::ToolCalls,
                        usage: Some(Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                            total_tokens: 2,
                            ..Usage::default()
                        }),
                    })
                }
                1 => Ok(StreamResult {
                    message: Message::Assistant {
                        content: Some("done".into()),
                        reasoning_content: None,
                        native_replay: None,
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
                other => panic!("unexpected two-tool provider step {other}"),
            }
        }
    }

    fn test_config() -> Config {
        Config {
            providers: crate::config::OrderedMap::from_iter([(
                "test".into(),
                ProviderConfig {
                    endpoint: "http://localhost/chat/completions".into(),
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
    fn string_field_state_distinguishes_partial_and_complete_fields() {
        let partial = r#"{"title":"List files","risk":"readonly","command":"cargo test rende"#;
        assert_eq!(
            string_field_state(partial, "title"),
            StringFieldState::Complete("List files".into())
        );
        assert_eq!(
            string_field_state(partial, "risk"),
            StringFieldState::Complete("readonly".into())
        );
        assert_eq!(
            string_field_state(partial, "command"),
            StringFieldState::Partial("cargo test rende".into())
        );

        let complete = r#"{"command":"cargo test renderer::tests""#;
        assert_eq!(
            string_field_state(complete, "command"),
            StringFieldState::Complete("cargo test renderer::tests".into())
        );
    }

    #[test]
    fn string_field_state_ignores_field_names_inside_values() {
        let args = r#"{"title":"mentions \"command\"","risk":"readonly","command":"echo first""#;

        assert_eq!(
            string_field_state(args, "title"),
            StringFieldState::Complete("mentions \"command\"".into())
        );
        assert_eq!(
            string_field_state(args, "command"),
            StringFieldState::Complete("echo first".into())
        );
    }

    #[test]
    fn streamed_command_headers_defer_later_commands_until_active() {
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let mut headers = StreamingCommandHeaders::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 1,
                id: Some("call_2".into()),
                name: Some("bash".into()),
                arguments_delta: r#"{"title":"Second","risk":"readonly","command":"echo second""#
                    .into(),
            },
        )
        .unwrap();

        assert_eq!(headers.next_to_render, 0);
        assert!(!headers.entries[1].display.started);

        let first_args = serde_json::json!({
            "title": "First",
            "risk": "readonly",
            "command": "echo first",
        });
        let second_args = serde_json::json!({
            "title": "Second",
            "risk": "readonly",
            "command": "echo second",
        });
        let first_call = ToolCall {
            id: "call_1".into(),
            function: crate::provider::FunctionCall {
                name: "bash".into(),
                arguments: first_args.to_string(),
            },
        };
        let second_call = ToolCall {
            id: "call_2".into(),
            function: crate::provider::FunctionCall {
                name: "bash".into(),
                arguments: second_args.to_string(),
            },
        };

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta: r#"{"title":"First","risk":"readonly","command":"echo first""#
                    .into(),
            },
        )
        .unwrap();

        assert_eq!(headers.next_to_render, 0);
        assert!(!headers.entries[0].display.is_done());
        assert!(!headers.entries[0].display.cwd_line_done);
        assert!(!headers.entries[1].display.started);

        assert!(
            finish_command_header(&mut renderer, &mut headers, 0, &first_call, &first_args)
                .unwrap()
        );
        assert!(!headers.entries[1].display.started);
        assert!(
            finish_command_header(&mut renderer, &mut headers, 1, &second_call, &second_args)
                .unwrap()
        );
        assert!(headers.entries[1].display.is_done());
    }

    #[test]
    fn plain_command_header_starts_before_title_arrives() {
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let mut headers = StreamingCommandHeaders::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta: String::new(),
            },
        )
        .unwrap();

        assert!(headers.entries[0].display.started);
        assert!(!headers.entries[0].display.title_started);
        assert!(!headers.entries[0].display.title_line_done);
        assert!(!headers.entries[0].display.command_started);

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta:
                    r#"{"title":"Plain title","risk":"readonly","command":"echo plain"}"#.into(),
            },
        )
        .unwrap();

        assert!(headers.entries[0].display.is_done());
        assert!(headers.entries[0].display.title_started);
    }

    #[test]
    fn streamed_command_header_waits_for_command_completion() {
        let mut renderer = Renderer::with_format(OutputFormat::Terminal);
        renderer.force_styled_for_test();
        let mut headers = StreamingCommandHeaders::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta: r#"{"title":"List","risk":"readonly","command":"printf 'a'"#
                    .into(),
            },
        )
        .unwrap();
        assert!(headers.entries[0].display.started);
        assert!(!headers.entries[0].display.is_done());

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\\n'\"}".into(),
            },
        )
        .unwrap();

        assert!(headers.entries[0].display.is_done());
        assert!(headers.entries[0].arguments.ends_with("\\n'\"}"));
    }

    #[test]
    fn streamed_stdin_summary_waits_for_command_completion() {
        let mut renderer = Renderer::with_format(OutputFormat::Terminal);
        renderer.force_styled_for_test();
        let mut headers = StreamingCommandHeaders::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta:
                    r#"{"title":"Feed stdin","risk":"readonly","stdin":"abc","command":"ca"#.into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.command_started);
        assert!(!display.command_line_done);
        assert!(!display.stdin_started);

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: r#"t"}"#.into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.command_line_done);
        assert!(display.stdin_started);
        assert!(display.stdin_line_done);
        assert!(display.is_done());
    }

    #[test]
    fn streamed_command_header_waits_for_argument_completion_before_missing_cwd_is_decided() {
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let mut headers = StreamingCommandHeaders::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta: r#"{"title":"Maybe cwd","risk":"readonly","command":"echo here""#
                    .into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.command_line_done);
        assert!(!display.cwd_line_done);
        assert!(!display.is_done());

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "}".into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.cwd_line_done);
        assert!(display.is_done());
    }

    #[test]
    fn streamed_stdin_summary_waits_for_cwd_decision() {
        let mut renderer = Renderer::with_format(OutputFormat::Terminal);
        renderer.force_styled_for_test();
        let mut headers = StreamingCommandHeaders::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta:
                    r#"{"title":"Feed stdin","risk":"readonly","command":"cat","stdin":"abc""#
                        .into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.command_line_done);
        assert!(!display.cwd_line_done);
        assert!(!display.stdin_started);
        assert!(!display.is_done());

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: r#","cwd":"./elsewhere"}"#.into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.cwd_line_done);
        assert!(display.stdin_started);
        assert!(display.stdin_line_done);
        assert!(display.is_done());
    }

    #[test]
    fn streamed_stdin_summary_keeps_updating_until_stdin_completes() {
        let mut renderer = Renderer::with_format(OutputFormat::Terminal);
        renderer.force_styled_for_test();
        let mut headers = StreamingCommandHeaders::default();

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                arguments_delta:
                    r#"{"title":"Feed stdin","risk":"readonly","command":"cat","cwd":".","stdin":"abc"#
                        .into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.command_line_done);
        assert!(display.stdin_started);
        assert!(!display.stdin_line_done);
        assert!(!display.is_done());

        handle_tool_call_delta(
            &mut renderer,
            &mut headers,
            ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: r#"def"}"#.into(),
            },
        )
        .unwrap();

        let display = &headers.entries[0].display;
        assert!(display.stdin_line_done);
        assert!(display.is_done());
    }

    #[test]
    fn final_header_shows_empty_stdin_summary() {
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let mut headers = StreamingCommandHeaders::default();
        let args = serde_json::json!({
            "title": "Empty stdin",
            "risk": "readonly",
            "command": "cat",
            "stdin": "",
        });
        let call = ToolCall {
            id: "call_1".into(),
            function: crate::provider::FunctionCall {
                name: "bash".into(),
                arguments: args.to_string(),
            },
        };

        assert!(finish_command_header(&mut renderer, &mut headers, 0, &call, &args).unwrap());
        let display = &headers.entries[0].display;
        assert!(display.stdin_started);
        assert!(display.stdin_line_done);
        assert!(display.is_done());
    }

    #[tokio::test]
    async fn live_provider_retry_completes_turn() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp", "test/fake-model").unwrap();
        let config = test_config();
        let request_model = crate::models::resolve_model_ref(&config, "test/fake-model").unwrap();
        store
            .append_message(
                &session.id,
                &Message::System {
                    content: "system".into(),
                },
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("retry me".into()),
                },
            )
            .unwrap();
        let provider = Box::new(RetryThenStopProvider {
            step: Mutex::new(0),
        });
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
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
        };

        let result = agent.run_turn().await.unwrap();

        // The transient provider error was retried in-process without adding a
        // second user message, and the session is clean after completion.
        assert_eq!(result.final_assistant.as_deref(), Some("done"));
        assert!(store.is_session_clean(&session.id).unwrap());
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
                ..
            }) if content == "done"
        ));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn plain_readonly_bash_batch_executes_concurrently_but_persists_in_order() {
        let tmp = std::env::temp_dir().join(format!(
            "mu-agent-plain-concurrent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp", "test/fake-model").unwrap();
        let config = test_config();
        let request_model = crate::models::resolve_model_ref(&config, "test/fake-model").unwrap();
        store
            .append_message(
                &session.id,
                &Message::System {
                    content: "system".into(),
                },
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("run both".into()),
                },
            )
            .unwrap();
        let provider = Box::new(TwoReadonlyThenStopProvider {
            step: Mutex::new(0),
            barrier_path: tmp.join("second-started").display().to_string(),
        });
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
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
        };

        let result = agent.run_turn().await.unwrap();

        assert_eq!(result.final_assistant.as_deref(), Some("done"));
        let tool_messages: Vec<_> = store
            .load_context_messages(&session.id)
            .unwrap()
            .into_iter()
            .filter_map(|message| match message {
                Message::Tool {
                    content,
                    tool_call_id,
                    ..
                } => Some((tool_call_id, content)),
                _ => None,
            })
            .collect();
        assert_eq!(tool_messages.len(), 2);
        assert_eq!(tool_messages[0].0, "call_first");
        assert!(tool_messages[0].1.contains("first"));
        assert_eq!(tool_messages[1].0, "call_second");
        assert!(tool_messages[1].1.contains("second"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// A provider that grows the context with one large tool result, then
    /// stops. Any summarization request (the compaction call) is answered with
    /// a short summary so the in-loop proactive compaction path can complete.
    struct GrowThenStopProvider {
        turn_step: Mutex<usize>,
    }

    #[async_trait(?Send)]
    impl Provider for GrowThenStopProvider {
        async fn stream_chat(
            &self,
            _request: &RequestOptions,
            messages: &[Message],
            _tools: &[Value],
            _on_event: &mut dyn FnMut(crate::provider::StreamEvent) -> Result<(), ProviderError>,
        ) -> Result<StreamResult, ProviderError> {
            let is_summarize = messages.iter().any(|message| match message {
                Message::User { content } => {
                    let text = content.text();
                    text.contains("Summarize this conversation")
                        || text.contains("Update this conversation summary")
                }
                _ => false,
            });
            if is_summarize {
                return Ok(StreamResult {
                    message: Message::Assistant {
                        content: Some("summary".into()),
                        reasoning_content: None,
                        native_replay: None,
                        tool_calls: None,
                    },
                    finish_reason: FinishReason::Stop,
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 2,
                        ..Usage::default()
                    }),
                });
            }

            let mut step = self.turn_step.lock().unwrap();
            let current = *step;
            *step += 1;
            match current {
                0 => Ok(StreamResult {
                    message: Message::Assistant {
                        content: None,
                        reasoning_content: None,
                        native_replay: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "call_grow".into(),
                            function: crate::provider::FunctionCall {
                                name: "bash".into(),
                                arguments: serde_json::json!({
                                    "title": "grow context",
                                    "risk": "readonly",
                                    "command": "printf 'x%.0s' {1..3000}",
                                })
                                .to_string(),
                            },
                        }]),
                    },
                    finish_reason: FinishReason::ToolCalls,
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                        ..Usage::default()
                    }),
                }),
                _ => Ok(StreamResult {
                    message: Message::Assistant {
                        content: Some("done".into()),
                        reasoning_content: None,
                        native_replay: None,
                        tool_calls: None,
                    },
                    finish_reason: FinishReason::Stop,
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                        ..Usage::default()
                    }),
                }),
            }
        }
    }

    #[tokio::test]
    async fn large_tool_result_triggers_in_loop_compaction() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-proactive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp", "test/fake-model").unwrap();
        let config = test_config();
        let request_model = crate::models::resolve_model_ref(&config, "test/fake-model").unwrap();
        store
            .append_message(
                &session.id,
                &Message::System {
                    content: "system".into(),
                },
            )
            .unwrap();

        // Small prior history so the pre-turn check does NOT compact; the huge
        // tool result produced mid-turn is what should push us over.
        for turn in ["one", "two"] {
            store
                .append_message(
                    &session.id,
                    &Message::User {
                        content: UserContent::Text(format!("turn {turn}")),
                    },
                )
                .unwrap();
            store
                .append_message(
                    &session.id,
                    &Message::Assistant {
                        content: Some(format!("reply {turn}")),
                        reasoning_content: None,
                        native_replay: None,
                        tool_calls: None,
                    },
                )
                .unwrap();
        }
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("turn three".into()),
                },
            )
            .unwrap();

        // No summary exists yet.
        assert!(
            store
                .latest_summary_sequence(&session.id)
                .unwrap()
                .is_none()
        );

        let provider = Box::new(GrowThenStopProvider {
            turn_step: Mutex::new(0),
        });
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let mut agent = AgentLoop {
            config: &config,
            provider,
            store: &store,
            session_id: &session.id,
            request: RequestOptions {
                model: request_model,
            },
            // Tiny window: threshold = 200 * 0.75 = 150 tokens (~600 bytes).
            // The ~3KB tool result blows past it, forcing an in-loop compaction.
            model_context_window: Some(200),
            renderer: &mut renderer,
            state_dir: &tmp,
        };

        agent.run_turn().await.unwrap();

        // Proactive compaction ran mid-turn and produced a summary row.
        assert!(
            store
                .latest_summary_sequence(&session.id)
                .unwrap()
                .is_some()
        );
        // The turn still completed cleanly after compaction.
        let messages = store.load_context_messages(&session.id).unwrap();
        assert!(matches!(
            messages.last(),
            Some(Message::Assistant {
                content: Some(content),
                tool_calls: None,
                ..
            }) if content == "done"
        ));

        let _ = std::fs::remove_dir_all(tmp);
    }

    /// Two model calls in one turn: a `readonly` bash call, then a stop. Each
    /// call reports its own `total_tokens` so the test can distinguish the
    /// cumulative turn total from the last-call context size.
    struct TwoCallUsageProvider {
        step: Mutex<usize>,
    }

    #[async_trait(?Send)]
    impl Provider for TwoCallUsageProvider {
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
                0 => Ok(StreamResult {
                    message: Message::Assistant {
                        content: None,
                        reasoning_content: None,
                        native_replay: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "call_readonly".into(),
                            function: crate::provider::FunctionCall {
                                name: "bash".into(),
                                arguments: serde_json::json!({
                                    "title": "noop",
                                    "risk": "readonly",
                                    "command": "true",
                                })
                                .to_string(),
                            },
                        }]),
                    },
                    finish_reason: FinishReason::ToolCalls,
                    usage: Some(Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                        total_tokens: 120,
                        ..Usage::default()
                    }),
                }),
                _ => Ok(StreamResult {
                    message: Message::Assistant {
                        content: Some("done".into()),
                        reasoning_content: None,
                        native_replay: None,
                        tool_calls: None,
                    },
                    finish_reason: FinishReason::Stop,
                    usage: Some(Usage {
                        input_tokens: 130,
                        output_tokens: 10,
                        total_tokens: 140,
                        ..Usage::default()
                    }),
                }),
            }
        }
    }

    #[tokio::test]
    async fn turn_usage_is_cumulative_but_context_tokens_is_last_call() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp", "test/fake-model").unwrap();
        let config = test_config();
        let request_model = crate::models::resolve_model_ref(&config, "test/fake-model").unwrap();
        store
            .append_message(
                &session.id,
                &Message::System {
                    content: "system".into(),
                },
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("go".into()),
                },
            )
            .unwrap();
        let provider = Box::new(TwoCallUsageProvider {
            step: Mutex::new(0),
        });
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
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
        };

        let result = agent.run_turn().await.unwrap();

        // input/output are summed across both calls; total_tokens is now also
        // cumulative and therefore self-consistent (>= input_tokens).
        assert_eq!(result.usage.input_tokens, 230);
        assert_eq!(result.usage.output_tokens, 30);
        assert_eq!(result.usage.total_tokens, 260);
        assert!(result.usage.total_tokens >= result.usage.input_tokens);
        // context_tokens reflects only the final call — the current context size.
        assert_eq!(result.context_tokens, 140);

        let _ = std::fs::remove_dir_all(tmp);
    }

    /// A single model call that ends on a non-`stop`, non-`tool_calls` finish
    /// reason (e.g. `length`) while still carrying assistant content.
    struct LengthFinishProvider;

    #[async_trait(?Send)]
    impl Provider for LengthFinishProvider {
        async fn stream_chat(
            &self,
            _request: &RequestOptions,
            _messages: &[Message],
            _tools: &[Value],
            _on_event: &mut dyn FnMut(crate::provider::StreamEvent) -> Result<(), ProviderError>,
        ) -> Result<StreamResult, ProviderError> {
            Ok(StreamResult {
                message: Message::Assistant {
                    content: Some("partial answer".into()),
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: None,
                },
                finish_reason: FinishReason::Other("length".into()),
                usage: Some(Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 8,
                    ..Usage::default()
                }),
            })
        }
    }

    #[tokio::test]
    async fn captures_final_assistant_on_non_stop_finish() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-length-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp", "test/fake-model").unwrap();
        let config = test_config();
        let request_model = crate::models::resolve_model_ref(&config, "test/fake-model").unwrap();
        store
            .append_message(
                &session.id,
                &Message::System {
                    content: "system".into(),
                },
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("write a lot".into()),
                },
            )
            .unwrap();
        let provider = Box::new(LengthFinishProvider);
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
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
        };

        let result = agent.run_turn().await.unwrap();

        // A `length` finish still surfaces the streamed assistant text to
        // `--output final`, rather than emitting nothing.
        assert_eq!(result.final_assistant.as_deref(), Some("partial answer"));

        let _ = std::fs::remove_dir_all(tmp);
    }
}
