use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::time::sleep;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::compaction;
use crate::config::Config;
use crate::guardrail::Guardrail;
use crate::models::RequestOptions;
use crate::provider::{
    FinishReason, Message, Provider, ProviderError, StreamEvent, ToolCall, ToolCallDelta, Usage,
    approx_tokens,
};
use crate::renderer::Renderer;
use crate::store::{BashResultRecord, ProviderOrigin, Store};
use crate::tools::{BashRisk, ExecutionMode, ToolContext, ToolResult};
use crate::{bash, tools};
use bash::RunningBash;

pub struct TurnResult {
    pub usage: Usage,
    /// Total tokens reported by the *last* model call of the turn — i.e. the
    /// current context size. Distinct from `usage.total_tokens`, which is
    /// cumulative across every call in the turn. Drives the context-fullness
    /// gauge; unlike cumulative turn usage, this is the latest request size.
    pub context_tokens: u64,
    pub final_assistant: Option<String>,
}

#[derive(Default)]
struct TurnAudit {
    current_partial_output: String,
}

impl TurnAudit {
    fn begin_provider_request(&mut self) {
        self.current_partial_output.clear();
    }

    fn abandon_provider_request(&mut self) {
        self.current_partial_output.clear();
    }

    fn commit_provider_response(&mut self) {
        self.current_partial_output.clear();
    }
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
    stdin_displayed_bytes: usize,
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
}

impl<'a> AgentLoop<'a> {
    pub async fn run_turn(&mut self) -> Result<TurnResult> {
        let mut audit = TurnAudit::default();
        self.run_turn_inner(&mut audit).await
    }

    async fn run_turn_inner(&mut self, audit: &mut TurnAudit) -> Result<TurnResult> {
        bash::reset_cancellation_state();
        bash::install_signal_forwarder();
        compaction::maybe_compact(
            self.store,
            self.config,
            self.session_id,
            &self.request,
            self.model_context_window,
            self.provider.as_ref(),
            self.renderer,
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
                        Some(self.renderer),
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
            let (exchange_id, stream_result) = loop {
                audit.begin_provider_request();
                let native_request =
                    self.provider
                        .native_request(&self.request, &context, &tool_definitions)?;
                let recipe = self.store.request_recipe(
                    self.provider.request_format(),
                    &native_request,
                    serde_json::json!({
                        "kind": "agent",
                        "context_through_seq": self.store.current_context_seq(self.session_id)?
                    }),
                    &tool_definitions,
                )?;
                let exchange_id = self.store.start_provider_request(
                    self.session_id,
                    &self.store.current_turn_id(self.session_id)?,
                    "agent",
                    ProviderOrigin {
                        canonical_model_ref: self.request.model.canonical.clone(),
                        provider_id: self.request.model.provider_id.clone(),
                        api: self.provider.api_name().to_string(),
                        endpoint: self.provider.endpoint().to_string(),
                        wire_model: self.request.model.model_id.clone(),
                        effort: self.request.model.effort.clone(),
                    },
                    recipe,
                    None,
                )?;
                let reviews_destructive = guardrail
                    .as_ref()
                    .is_some_and(|guardrail| guardrail.should_review(BashRisk::Destructive));
                let mut on_stream_event = |event: StreamEvent| -> Result<(), ProviderError> {
                    let result = match event {
                        StreamEvent::TextDelta(text) => {
                            audit.current_partial_output.push_str(&text);
                            self.renderer.assistant_text(&text)
                        }
                        StreamEvent::ReasoningStart(visibility) => {
                            self.renderer.reasoning_start(visibility)
                        }
                        StreamEvent::ReasoningDelta(text) => self.renderer.reasoning_delta(&text),
                        StreamEvent::ReasoningSummaryDelta { part_index, text } => {
                            self.renderer.reasoning_summary_delta(part_index, &text)
                        }
                        StreamEvent::ReasoningEnd => self.renderer.reasoning_end(None),
                        StreamEvent::ToolCallDelta(delta) => handle_tool_call_delta(
                            self.renderer,
                            &mut command_headers,
                            reviews_destructive,
                            delta,
                        ),
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
                    Ok(r) => break (exchange_id, r),
                    Err(ProviderError::ContextLength)
                        if overflow_retries < MAX_OVERFLOW_RETRIES =>
                    {
                        self.store.fail_provider_exchange(
                            self.session_id,
                            &exchange_id,
                            "context_length",
                            serde_json::json!({"message":"context length exceeded"}),
                            partial_response(&audit.current_partial_output).as_ref(),
                            None,
                        )?;
                        audit.abandon_provider_request();
                        overflow_retries += 1;
                        compaction::run_compaction(
                            self.store,
                            self.config,
                            self.session_id,
                            &self.request,
                            self.provider.as_ref(),
                            None,
                            Some(self.renderer),
                        )
                        .await?;
                        context = self.load_context()?;
                    }
                    Err(ProviderError::ContextLength) => {
                        self.store.fail_provider_exchange(
                            self.session_id,
                            &exchange_id,
                            "context_length",
                            serde_json::json!({"message":"context length exceeded"}),
                            partial_response(&audit.current_partial_output).as_ref(),
                            None,
                        )?;
                        bail!("context length exceeded even after compaction");
                    }
                    Err(error)
                        if error.retryable_for_live_turn()
                            && live_provider_retries < MAX_LIVE_PROVIDER_RETRIES =>
                    {
                        self.store.fail_provider_exchange(
                            self.session_id,
                            &exchange_id,
                            error.class(),
                            serde_json::json!({"message":error.to_string()}),
                            partial_response(&audit.current_partial_output).as_ref(),
                            None,
                        )?;
                        audit.abandon_provider_request();
                        live_provider_retries += 1;
                        self.renderer.turn_retry(
                            live_provider_retries as u64,
                            MAX_LIVE_PROVIDER_RETRIES as u64,
                            &error.to_string(),
                        )?;
                        context = self.load_context()?;
                    }
                    Err(error) => {
                        self.store.fail_provider_exchange(
                            self.session_id,
                            &exchange_id,
                            error.class(),
                            serde_json::json!({"message":error.to_string()}),
                            partial_response(&audit.current_partial_output).as_ref(),
                            None,
                        )?;
                        bail!("provider error: {error}")
                    }
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

            // Only a provider-declared tool-call completion makes streamed calls
            // executable. A length/content-filter stop can contain an incomplete
            // accumulated call; retain its native response for audit, but do not
            // turn that partial call into semantic history or execution authority.
            let mut accepted_message = stream_result.message.clone();
            if !matches!(stream_result.finish_reason, FinishReason::ToolCalls)
                && let Message::Assistant {
                    tool_calls,
                    native_replay,
                    ..
                } = &mut accepted_message
                && tool_calls.as_ref().is_some_and(|calls| !calls.is_empty())
            {
                *tool_calls = None;
                *native_replay = None;
            }
            let (_message_id, bash_call_ids) = self.store.complete_assistant_exchange(
                self.session_id,
                &exchange_id,
                &accepted_message,
                stream_result.native_response.as_ref(),
                stream_result.usage.as_ref(),
            )?;
            audit.commit_provider_response();
            context.push(accepted_message.clone());

            match stream_result.finish_reason {
                FinishReason::Stop => {
                    if let Message::Assistant { content, .. } = &accepted_message {
                        final_assistant = content.clone();
                    }
                    break;
                }
                FinishReason::ToolCalls => {
                    let tool_calls = match &accepted_message {
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
                            let guardrail_pending =
                                guardrail_review_required(guardrail.as_ref(), tc, &args);

                            let header_already_rendered = finish_command_header(
                                self.renderer,
                                &mut command_headers,
                                cursor,
                                tc,
                                &args,
                                guardrail_pending,
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
                                    self.persist_bash_result(
                                        bash_call_ids[cursor],
                                        tc,
                                        Err(err),
                                        Duration::ZERO,
                                        &mut context,
                                        true,
                                    )?;
                                    cursor += 1;
                                    continue;
                                }
                                if g.should_review(risk.expect("risk checked above")) {
                                    let args_for_review = args.clone();
                                    let command = args_for_review
                                        .get("command")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    self.renderer.guardrail_start()?;
                                    let assessment = match g
                                        .assess(
                                            &args_for_review,
                                            &context,
                                            self.store,
                                            self.session_id,
                                            bash_call_ids[cursor],
                                        )
                                        .await
                                    {
                                        Ok(assessment) => assessment,
                                        Err(error) => {
                                            let message =
                                                format!("guardrail review failed: {error}");
                                            self.persist_bash_result(
                                                bash_call_ids[cursor],
                                                tc,
                                                Err(anyhow::anyhow!(message.clone())),
                                                Duration::ZERO,
                                                &mut context,
                                                false,
                                            )?;
                                            self.renderer.guardrail_failed()?;
                                            bail!("{message}");
                                        }
                                    };
                                    let risk_level = assessment.risk_level.to_string();
                                    let user_auth_level = assessment.user_auth_level.to_string();
                                    if assessment.is_allowed() {
                                        self.renderer.guardrail_verdict(
                                            true,
                                            &risk_level,
                                            &user_auth_level,
                                            &assessment.reason,
                                            command,
                                        )?;
                                    } else {
                                        let deny_err = anyhow::anyhow!(
                                            "guardrail: action rejected — risk_level {} exceeds user_auth_level {} ({}). \
                                             Do not work around this; stop and ask the user to authorize, \
                                             or choose a less destructive approach.",
                                            assessment.risk_level,
                                            assessment.user_auth_level,
                                            assessment.reason
                                        );
                                        self.persist_bash_result(
                                            bash_call_ids[cursor],
                                            tc,
                                            Err(deny_err),
                                            Duration::ZERO,
                                            &mut context,
                                            false,
                                        )?;
                                        self.renderer.guardrail_verdict(
                                            false,
                                            &risk_level,
                                            &user_auth_level,
                                            &assessment.reason,
                                            command,
                                        )?;
                                        self.renderer.guardrail_rejected()?;
                                        if let Some(denials) = g.denial_limit_reached() {
                                            self.renderer.notice(&format!(
                                                "[mu] guardrail: aborting turn — {denials} denials in this turn"
                                            ))?;
                                            bail!("guardrail denial limit reached");
                                        }
                                        cursor += 1;
                                        continue;
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
                                let (manifest, objects_dir) =
                                    self.store.attachment_paths(self.session_id)?;
                                let mut ctx = ToolContext {
                                    config: self.config,
                                    renderer: self.renderer,
                                    attachment_manifest: Some(&manifest),
                                    objects_dir: Some(&objects_dir),
                                    bash_call_id: bash_call_ids[cursor],
                                };
                                bash::execute(args, &mut ctx).await
                            } else {
                                Err(anyhow::anyhow!("unknown tool: {}", tc.function.name))
                            };

                            self.persist_bash_result(
                                bash_call_ids[cursor],
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
                                chunk,
                                &bash_call_ids[cursor
                                    + chunk_offset * bash::MAX_ACTIVE_PROCESS_GROUPS
                                    ..cursor
                                        + chunk_offset * bash::MAX_ACTIVE_PROCESS_GROUPS
                                        + chunk.len()],
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
                    if let Message::Assistant { content, .. } = &accepted_message {
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

    fn persist_bash_result(
        &mut self,
        bash_call_id: i64,
        call: &ToolCall,
        result: Result<ToolResult>,
        elapsed: Duration,
        context: &mut Vec<Message>,
        emit_renderer: bool,
    ) -> Result<()> {
        let (output, attachments, outcome, exit_code) = match result {
            Ok(result) => {
                if emit_renderer {
                    self.renderer.tool_finished(result.exit_code, elapsed)?;
                }
                (
                    result.output,
                    result.attachments,
                    "completed",
                    Some(result.exit_code),
                )
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
                (message, Vec::new(), "error", None)
            }
        };

        let (_, attachments) = self.store.persist_bash_result(
            self.session_id,
            BashResultRecord {
                bash_call_id,
                outcome,
                exit_code,
                duration_ms: Some(elapsed.as_millis().min(u64::MAX as u128) as u64),
            },
            &output,
            &attachments,
        )?;
        context.push(Message::Tool {
            content: output,
            attachments,
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
        batch: &[ToolCall],
        bash_call_ids: &[i64],
        context: &mut Vec<Message>,
        command_headers: &mut StreamingCommandHeaders,
        header_start_index: usize,
    ) -> Result<()> {
        let mut executions = Vec::new();
        let (manifest, objects_dir) = self.store.attachment_paths(self.session_id)?;
        for (call, bash_call_id) in batch.iter().zip(bash_call_ids) {
            let args = parse_tool_args(call);
            let bash_args = tools::parse_args(&args)?;
            executions.push(ConcurrentBashExecution {
                call,
                args,
                running: Some(bash::start_bash_task(
                    bash_args,
                    self.config,
                    Some(&manifest),
                    Some(&objects_dir),
                    *bash_call_id,
                )?),
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
                false,
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
            self.persist_bash_result(
                bash_call_ids[index],
                exec.call,
                result,
                elapsed,
                context,
                true,
            )?;
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

fn partial_response(text: &str) -> Option<Value> {
    (!text.is_empty()).then(|| serde_json::json!({"output_text":text}))
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
    reviews_destructive: bool,
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
                guardrail_pending: reviews_destructive
                    && matches!(
                        string_field_state(&header.arguments, "risk").complete_value(),
                        Some("destructive")
                    ),
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
    guardrail_pending: bool,
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
    header.finish(renderer, args, guardrail_pending)
}

impl StreamingCommandHeader {
    fn finish(
        &mut self,
        renderer: &mut Renderer,
        args: &Value,
        guardrail_pending: bool,
    ) -> std::io::Result<bool> {
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
                guardrail_pending,
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
            guardrail_pending,
        } = update;
        if !self.started {
            self.started = renderer.bash_header_start(tool_call_id)?;
        }

        if renderer.output_format() == crate::cli::OutputFormat::Concise {
            let ready = title.complete_value().is_some() && risk.complete_value().is_some();
            if ready || arguments_complete {
                renderer.concise_tool_ready(
                    title.complete_value(),
                    risk.complete_value(),
                    guardrail_pending,
                )?;
                self.title_line_done = true;
                self.command_line_done = true;
                self.cwd_line_done = true;
                self.stdin_line_done = true;
            }
            return Ok(());
        }

        if renderer.output_format() == crate::cli::OutputFormat::Full {
            return self.update_full(
                renderer,
                FullCommandHeaderUpdate {
                    title,
                    risk,
                    command,
                    cwd,
                    stdin,
                    arguments_complete,
                },
            );
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
                    renderer.bash_header_preview_width(),
                    &mut self.title_displayed_bytes,
                    |text| renderer.bash_header_delta(text),
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
                renderer.bash_header_preview_width(),
                &mut self.command_displayed_bytes,
                |text| renderer.bash_header_delta(text),
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

    fn update_full(
        &mut self,
        renderer: &mut Renderer,
        update: FullCommandHeaderUpdate,
    ) -> std::io::Result<()> {
        let FullCommandHeaderUpdate {
            title,
            risk,
            command,
            cwd,
            stdin,
            arguments_complete,
        } = update;
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
                    renderer.bash_header_preview_width(),
                    &mut self.title_displayed_bytes,
                    |text| renderer.bash_header_delta(text),
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

        let complete_risk = risk.complete_value();
        if self.title_line_done
            && !self.command_started
            && (complete_risk.is_some() || arguments_complete)
        {
            renderer.bash_header_command_start(complete_risk)?;
            self.command_started = true;
        }
        if self.command_started && !self.command_line_done {
            if let Some(value) = command.value() {
                let done = stream_all(
                    value,
                    command.is_complete(),
                    &mut self.command_displayed_bytes,
                    |text| renderer.bash_header_delta(text),
                )?;
                if done {
                    renderer.bash_header_command_end()?;
                    self.command_line_done = true;
                }
            } else if arguments_complete {
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
                StringFieldState::Missing if arguments_complete => self.cwd_line_done = true,
                StringFieldState::Missing | StringFieldState::Partial(_) => {}
            }
        }
        if self.command_line_done && self.cwd_line_done && !self.stdin_line_done {
            if let Some(value) = stdin.value() {
                if !self.stdin_started {
                    renderer.bash_header_stdin_full_start()?;
                    self.stdin_started = true;
                }
                let done = stream_all(
                    value,
                    stdin.is_complete(),
                    &mut self.stdin_displayed_bytes,
                    |text| renderer.bash_header_delta(text),
                )?;
                if done {
                    renderer.bash_header_stdin_full_end()?;
                    self.stdin_line_done = true;
                }
            } else if arguments_complete {
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
    guardrail_pending: bool,
}

struct FullCommandHeaderUpdate {
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
    max_cells: Option<usize>,
    displayed_bytes: &mut usize,
    mut write: impl FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    if let Some(max_cells) = max_cells {
        return stream_first_line_cells(value, complete, max_cells, displayed_bytes, write);
    }

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

fn stream_first_line_cells(
    value: &str,
    complete: bool,
    max_cells: usize,
    displayed_bytes: &mut usize,
    mut write: impl FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    let ellipsis_width = UnicodeWidthStr::width(crate::renderer::ELLIPSIS);
    let body_limit = max_cells.saturating_sub(ellipsis_width);
    let start = (*displayed_bytes).min(value.len());
    let already_width = UnicodeWidthStr::width(&value[..start]);
    let mut out = String::new();
    let mut out_width = 0usize;
    let mut consumed = start;

    for (relative, grapheme) in value[start..].grapheme_indices(true) {
        let absolute = start + relative;
        if grapheme.contains('\n') {
            if max_cells >= ellipsis_width {
                out.push_str(crate::renderer::ELLIPSIS);
            }
            write(&out)?;
            return Ok(true);
        }
        let next_width = UnicodeWidthStr::width(grapheme);
        if already_width
            .saturating_add(out_width)
            .saturating_add(next_width)
            > body_limit
        {
            if max_cells >= ellipsis_width {
                out.push_str(crate::renderer::ELLIPSIS);
            }
            write(&out)?;
            return Ok(true);
        }
        out.push_str(grapheme);
        out_width = out_width.saturating_add(next_width);
        consumed = absolute + grapheme.len();
    }

    *displayed_bytes = consumed;
    write(&out)?;
    Ok(complete)
}

fn stream_all(
    value: &str,
    complete: bool,
    displayed_bytes: &mut usize,
    mut write: impl FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    let start = (*displayed_bytes).min(value.len());
    write(&value[start..])?;
    *displayed_bytes = value.len();
    Ok(complete)
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
    guardrail.should_review(risk)
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

    struct PartialFailureProvider;

    #[async_trait(?Send)]
    impl Provider for PartialFailureProvider {
        async fn stream_chat(
            &self,
            _request: &RequestOptions,
            _messages: &[Message],
            _tools: &[Value],
            on_event: &mut dyn FnMut(crate::provider::StreamEvent) -> Result<(), ProviderError>,
        ) -> Result<StreamResult, ProviderError> {
            on_event(StreamEvent::TextDelta("unfinished answer".into()))?;
            Err(ProviderError::Other("fatal stream failure".into()))
        }
    }

    #[async_trait(?Send)]
    impl Provider for RetryThenStopProvider {
        async fn stream_chat(
            &self,
            _request: &RequestOptions,
            _messages: &[Message],
            _tools: &[Value],
            on_event: &mut dyn FnMut(crate::provider::StreamEvent) -> Result<(), ProviderError>,
        ) -> Result<StreamResult, ProviderError> {
            let mut step = self.step.lock().unwrap();
            let current = *step;
            *step += 1;
            match current {
                0 => {
                    on_event(StreamEvent::TextDelta("discarded partial".into()))?;
                    Err(ProviderError::RateLimit {
                        message: "slow down".into(),
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
                    native_response: None,
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
                        native_response: None,
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
                    native_response: None,
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
            output: Default::default(),
            line_wrapping: true,
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn live_provider_retry_completes_turn() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp").unwrap();
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
        let mut renderer = Renderer::with_format(OutputFormat::Detail);
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
        };

        let result = agent.run_turn().await.unwrap();

        // The transient provider error was retried in-process without adding a
        // second user message, and the session is clean after completion.
        assert_eq!(result.final_assistant.as_deref(), Some("done"));
        assert!(store.is_session_clean(&session.id).unwrap());
        let messages = store.load_context_messages(&session.id).unwrap();
        assert_eq!(
            store
                .audit_events(&session.id)
                .unwrap()
                .iter()
                .filter(|event| event["type"] == "turn_started")
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
        assert!(!messages.iter().any(|message| {
            match message {
                Message::Assistant { content, .. } => content
                    .as_deref()
                    .is_some_and(|content| content.contains("discarded partial")),
                _ => false,
            }
        }));
        let audit = store.audit_events(&session.id).unwrap();
        assert_eq!(
            audit
                .iter()
                .filter(|event| event["type"] == "provider_requested")
                .count(),
            2
        );
        let failed = audit
            .iter()
            .find(|event| event["type"] == "provider_failed")
            .unwrap();
        assert_eq!(failed["error_class"], "rate_limit");
        assert!(failed["partial_response_json"].is_object());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn failed_partial_output_is_audited_but_excluded_from_history() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-partial-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp").unwrap();
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
                    content: UserContent::Text("fail after streaming".into()),
                },
            )
            .unwrap();
        let mut renderer = Renderer::with_format(OutputFormat::Detail);
        let mut agent = AgentLoop {
            config: &config,
            provider: Box::new(PartialFailureProvider),
            store: &store,
            session_id: &session.id,
            request: RequestOptions {
                model: request_model,
            },
            model_context_window: None,
            renderer: &mut renderer,
        };

        let error = match agent.run_turn().await {
            Ok(_) => panic!("expected provider failure"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("provider error"));
        let messages = store.load_context_messages(&session.id).unwrap();
        assert!(matches!(messages.last(), Some(Message::User { .. })));
        assert!(!messages.iter().any(|message| {
            match message {
                Message::Assistant { content, .. } => content
                    .as_deref()
                    .is_some_and(|content| content.contains("unfinished answer")),
                _ => false,
            }
        }));
        let audit = store.audit_events(&session.id).unwrap();
        let failed = audit
            .iter()
            .find(|event| event["type"] == "provider_failed")
            .unwrap();
        assert_eq!(failed["error_class"], "provider");
        assert!(failed["partial_response_json"].is_object());
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
        let session = store.create_session("/tmp").unwrap();
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
        let mut renderer = Renderer::with_format(OutputFormat::Detail);
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
                    native_response: None,
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
                    native_response: None,
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
                    native_response: None,
                }),
            }
        }
    }

    #[tokio::test]
    async fn large_tool_result_triggers_in_loop_compaction() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-proactive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp").unwrap();
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
        let mut renderer = Renderer::with_format(OutputFormat::Detail);
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
                    native_response: None,
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
                    native_response: None,
                }),
            }
        }
    }

    #[tokio::test]
    async fn turn_usage_is_cumulative_but_context_tokens_is_last_call() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp").unwrap();
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
        let mut renderer = Renderer::with_format(OutputFormat::Detail);
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
    /// reason (e.g. `length`) while carrying assistant content and a partially
    /// accumulated tool call.
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
                    tool_calls: Some(vec![ToolCall {
                        id: "truncated".into(),
                        function: crate::provider::FunctionCall {
                            name: "bash".into(),
                            arguments: serde_json::json!({
                                "title": "must not run",
                                "risk": "readonly",
                                "command": "false",
                            })
                            .to_string(),
                        },
                    }]),
                },
                finish_reason: FinishReason::Other("length".into()),
                usage: Some(Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 8,
                    ..Usage::default()
                }),
                native_response: None,
            })
        }
    }

    #[tokio::test]
    async fn captures_final_assistant_on_non_stop_finish() {
        let tmp = std::env::temp_dir().join(format!("mu-agent-length-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp").unwrap();
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
        let mut renderer = Renderer::with_format(OutputFormat::Detail);
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
        };

        let result = agent.run_turn().await.unwrap();

        // A `length` finish still surfaces the streamed assistant text to
        // `--output final`, rather than emitting nothing.
        assert_eq!(result.final_assistant.as_deref(), Some("partial answer"));
        assert!(store.is_session_clean(&session.id).unwrap());
        assert!(matches!(
            store.load_context_messages(&session.id).unwrap().last(),
            Some(Message::Assistant {
                tool_calls: None,
                ..
            })
        ));

        let _ = std::fs::remove_dir_all(tmp);
    }
}
