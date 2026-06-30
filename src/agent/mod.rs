use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::time::sleep;

use crate::cli::OutputFormat;
use crate::compaction;
use crate::config::Config;
use crate::guardrail::{Guardrail, GuardrailOutcome, bash_risk};
use crate::models::RequestOptions;
use crate::provider::{
    FinishReason, Message, Provider, ProviderError, StreamEvent, ToolCall, UserContent,
};
use crate::renderer::Renderer;
use crate::store::{ReviewRecord, Store, ToolCallRecord};
use crate::tools::bash::{self, RunningBash};
use crate::tools::{ExecutionMode, ToolContext, ToolRegistry, ToolResult, missing_tool_message};

pub struct TurnResult {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub final_total_tokens: u64,
    pub cost: f64,
}

struct ConcurrentBashExecution<'a> {
    call: &'a ToolCall,
    args: Value,
    running: Option<RunningBash>,
    streamed_len: usize,
    completed: Option<(Result<ToolResult>, Duration)>,
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
    pub attachments: Vec<crate::provider::ContentPart>,
}

impl<'a> AgentLoop<'a> {
    pub async fn run_turn(&mut self, prompt: &str) -> Result<TurnResult> {
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
            Some(Guardrail::new(self.config, self.provider.clone()))
        } else {
            None
        };

        let mut context = self.store.load_context_messages(self.session_id)?;
        context.insert(
            0,
            Message::System {
                content: self.system_prompt.clone(),
            },
        );

        let user_msg = if self.attachments.is_empty() {
            Message::User {
                content: UserContent::Text(prompt.to_string()),
            }
        } else {
            let mut parts = vec![crate::provider::ContentPart::Text {
                text: prompt.to_string(),
            }];
            parts.extend(self.attachments.clone());
            Message::User {
                content: UserContent::Parts(parts),
            }
        };
        self.store.append_message(self.session_id, &user_msg)?;
        context.push(user_msg);

        let registry = ToolRegistry::new(self.config);
        let tools = registry.definitions();
        let max_iter = self.config.limits.max_iterations;

        let mut total_prompt: u64 = 0;
        let mut total_completion: u64 = 0;
        let mut final_total: u64 = 0;
        let mut overflow_retries: u32 = 0;
        const MAX_OVERFLOW_RETRIES: u32 = 3;

        for iteration in 0..max_iter {
            let mut on_stream_event = |event: StreamEvent| -> Result<(), ProviderError> {
                let result = match event {
                    StreamEvent::TextDelta(text) => self.renderer.assistant_text(&text),
                    StreamEvent::ReasoningStart => self.renderer.reasoning_start(),
                    StreamEvent::ReasoningDelta(text) => self.renderer.reasoning_delta(&text),
                    StreamEvent::ReasoningEnd => self.renderer.reasoning_end(None),
                    StreamEvent::ToolCallStart => self.renderer.tool_call_composition_start(),
                    StreamEvent::Tick => self.renderer.thinking_tick(),
                };
                result.map_err(|e| ProviderError::Other(e.to_string()))
            };
            let result = self
                .provider
                .stream_chat(&self.request, &context, &tools, &mut on_stream_event)
                .await;
            self.renderer.assistant_end()?;
            match &result {
                Ok(stream_result) => {
                    let usage = stream_result
                        .usage
                        .as_ref()
                        .map(|u| (u.prompt_tokens, u.completion_tokens));
                    self.renderer.reasoning_end(usage)?;
                }
                Err(_) => self.renderer.cancel_live_state()?,
            }

            let stream_result = match result {
                Ok(r) => r,
                Err(ProviderError::ContextLength) if overflow_retries < MAX_OVERFLOW_RETRIES => {
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
                    context = self.store.load_context_messages(self.session_id)?;
                    context.insert(
                        0,
                        Message::System {
                            content: self.system_prompt.clone(),
                        },
                    );
                    continue;
                }
                Err(ProviderError::ContextLength) => {
                    bail!("context length exceeded even after compaction");
                }
                Err(e) => bail!("provider error: {e}"),
            };

            if let Some(u) = &stream_result.usage {
                total_prompt += u.prompt_tokens;
                total_completion += u.completion_tokens;
                final_total = u.total_tokens;
            }

            let msg_id = self
                .store
                .append_message(self.session_id, &stream_result.message)?;
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
                            &registry,
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

                            self.renderer
                                .tool_start(Some(&tc.id), &tc.function.name, &args)?;
                            let started = Instant::now();

                            let tool_result = if let Some(tool) = registry.get(&tc.function.name) {
                                let mut ctx = ToolContext {
                                    config: self.config,
                                    renderer: Some(self.renderer),
                                    state_dir: self.state_dir,
                                };
                                tool.execute(args, &mut ctx).await
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
                                &registry,
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

        let cost = compute_cost(
            self.config,
            &self.request.model,
            total_prompt,
            total_completion,
        );

        Ok(TurnResult {
            prompt_tokens: total_prompt,
            completion_tokens: total_completion,
            final_total_tokens: final_total,
            cost,
        })
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
        self.store.record_tool_call(ToolCallRecord {
            message_id,
            id: &call.id,
            tool: &call.function.name,
            args: &call.function.arguments,
            risk: risk.as_deref(),
            output: &output,
            status,
        })?;

        let message = Message::Tool {
            content: output,
            tool_call_id: call.id.clone(),
        };
        self.store.append_message(self.session_id, &message)?;
        context.push(message);
        Ok(())
    }

    fn concurrent_tool_call_eligible(
        &self,
        registry: &ToolRegistry,
        guardrail: Option<&Guardrail>,
        call: &ToolCall,
        args: &Value,
    ) -> bool {
        if self.renderer.output_format() == OutputFormat::Plain {
            return false;
        }
        let Some(tool) = registry.get(&call.function.name) else {
            return false;
        };
        if tool.execution_mode(args) != ExecutionMode::Concurrent {
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
            let bash_args = crate::tools::parse_args(&args)?;
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

fn compute_cost(
    config: &Config,
    model: &crate::models::ResolvedModelRef,
    prompt: u64,
    completion: u64,
) -> f64 {
    let Some(model_cfg) = config.model_config(&model.provider_id, &model.model_id) else {
        return 0.0;
    };
    let Some(prices) = &model_cfg.price_per_mtok else {
        return 0.0;
    };
    (prompt as f64 / 1_000_000.0) * prices.input + (completion as f64 / 1_000_000.0) * prices.output
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
    use crate::provider::{FinishReason, FunctionCall, ProviderError, StreamResult, Usage};

    struct ToolThenStopProvider {
        step: Mutex<usize>,
        cwd: String,
    }

    #[async_trait(?Send)]
    impl Provider for ToolThenStopProvider {
        async fn stream_chat(
            &self,
            _request: &RequestOptions,
            messages: &[Message],
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
                        tool_calls: Some(vec![
                            bash_call(
                                "call-a",
                                "First",
                                "date +%s%N > a-start\nsleep 0.5\ndate +%s%N > a-end\nprintf 'first'",
                                "readonly",
                                Some(&self.cwd),
                            ),
                            bash_call(
                                "call-b",
                                "Second",
                                "date +%s%N > b-start\nsleep 0.5\ndate +%s%N > b-end\nprintf 'second'",
                                "readonly",
                                Some(&self.cwd),
                            ),
                        ]),
                    },
                    finish_reason: FinishReason::ToolCalls,
                    usage: Some(Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                    }),
                }),
                1 => {
                    let tool_ids = messages
                        .iter()
                        .filter_map(|message| match message {
                            Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(tool_ids, vec!["call-a", "call-b"]);
                    Ok(StreamResult {
                        message: Message::Assistant {
                            content: Some("done".into()),
                            tool_calls: None,
                        },
                        finish_reason: FinishReason::Stop,
                        usage: Some(Usage {
                            prompt_tokens: 1,
                            completion_tokens: 1,
                            total_tokens: 2,
                        }),
                    })
                }
                other => panic!("unexpected provider step {other}"),
            }
        }
    }

    fn bash_call(id: &str, title: &str, script: &str, risk: &str, cwd: Option<&str>) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: serde_json::json!({
                    "title": title,
                    "risk": risk,
                    "script": script,
                    "cwd": cwd,
                })
                .to_string(),
            },
        }
    }

    fn test_config() -> Config {
        Config {
            providers: HashMap::from([(
                "test".into(),
                ProviderConfig {
                    base_url: "http://localhost".into(),
                    api_key_env: "MU_TEST_KEY".into(),
                    models: HashMap::from([(
                        "fake-model".into(),
                        crate::config::ModelConfig {
                            context_window: None,
                            price_per_mtok: None,
                            supported_efforts: None,
                        },
                    )]),
                },
            )]),
            default_model: "test/fake-model".into(),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    async fn run_tool_batch(output: OutputFormat, cwd: &Path) -> (Store, String) {
        let tmp = std::env::temp_dir().join(format!("mu-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store
            .create_session(&cwd.display().to_string(), "test/fake-model")
            .unwrap();
        let config = test_config();
        let request_model = crate::models::resolve_model_ref(&config, "test/fake-model").unwrap();
        let provider = Arc::new(ToolThenStopProvider {
            step: Mutex::new(0),
            cwd: cwd.display().to_string(),
        });
        let mut renderer = Renderer::with_format(output);
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
            attachments: Vec::new(),
        };

        agent.run_turn("run both").await.unwrap();
        (store, session.id)
    }

    #[tokio::test]
    async fn readonly_bash_batch_runs_concurrently_and_keeps_tool_results_ordered() {
        let cwd = std::env::temp_dir().join(format!("mu-agent-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let (store, session_id) = run_tool_batch(OutputFormat::Json, &cwd).await;

        let tool_messages = store
            .load_context_messages(&session_id)
            .unwrap()
            .into_iter()
            .filter_map(|message| match message {
                Message::Tool {
                    tool_call_id,
                    content,
                } => Some((tool_call_id, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_messages.len(), 2);
        assert_eq!(tool_messages[0].0, "call-a");
        assert_eq!(tool_messages[1].0, "call-b");
        assert!(tool_messages[0].1.contains("first"));
        assert!(tool_messages[1].1.contains("second"));

        let a_start: u128 = std::fs::read_to_string(cwd.join("a-start"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let a_end: u128 = std::fs::read_to_string(cwd.join("a-end"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let b_start: u128 = std::fs::read_to_string(cwd.join("b-start"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let b_end: u128 = std::fs::read_to_string(cwd.join("b-end"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            a_start < b_end && b_start < a_end,
            "readonly bash batch did not overlap"
        );
        let _ = std::fs::remove_dir_all(cwd);
    }
}
