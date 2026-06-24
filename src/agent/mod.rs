use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use futures_util::future::join_all;
use serde_json::Value;

use crate::compaction;
use crate::config::Config;
use crate::guardrail::{Guardrail, GuardrailOutcome, bash_risk};
use crate::provider::{FinishReason, Message, Provider, ProviderError, ToolCall};
use crate::renderer::Renderer;
use crate::store::Store;
use crate::tools::{missing_tool_message, ExecutionMode, ToolContext, ToolRegistry, ToolResult};

pub struct TurnResult {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub final_total_tokens: u64,
    pub cost: f64,
}

pub struct AgentLoop<'a> {
    pub config: &'a Config,
    pub provider: Arc<dyn Provider>,
    pub store: &'a Store,
    pub session_id: &'a str,
    pub model: String,
    pub renderer: &'a mut Renderer,
    pub state_dir: &'a Path,
    pub system_prompt: String,
}

impl<'a> AgentLoop<'a> {
    pub async fn run_turn(&mut self, prompt: &str) -> Result<TurnResult> {
        compaction::maybe_compact(
            self.store,
            self.config,
            self.session_id,
            &self.model,
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

        let user_msg = Message::User {
            content: prompt.to_string(),
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
            let mut on_text_delta = |text: String| -> Result<(), ProviderError> {
                self.renderer
                    .assistant_text(&text)
                    .map_err(|e| ProviderError::Other(e.to_string()))
            };
            let result = self
                .provider
                .stream_chat(&self.model, &context, &tools, &mut on_text_delta)
                .await;
            self.renderer.assistant_end()?;

            let stream_result = match result {
                Ok(r) => r,
                Err(ProviderError::ContextLength) if overflow_retries < MAX_OVERFLOW_RETRIES => {
                    overflow_retries += 1;
                    compaction::run_compaction(
                        self.store,
                        self.config,
                        self.session_id,
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
                        let args = parse_tool_args(&tool_calls[cursor]);
                        let concurrent = registry
                            .get(&tool_calls[cursor].function.name)
                            .is_some_and(|tool| {
                                tool.execution_mode(&args) == ExecutionMode::Concurrent
                            });

                        if !concurrent {
                            let tc = &tool_calls[cursor];

                            // Guardrail: review destructive bash calls before execution.
                            // Runs before tool_start so denied commands never print a `$` line.
                            if let Some(g) = guardrail.as_mut() {
                                if tc.function.name == "bash" {
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
                                        )?;
                                        cursor += 1;
                                        continue;
                                    }
                                    if g.should_review(risk.as_deref().unwrap_or("")) {
                                        let args_for_review = args.clone();
                                        let action_json = serde_json::to_string(
                                            &args_for_review,
                                        )
                                        .unwrap_or_default();
                                        let script = args_for_review
                                            .get("script")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        match g.assess(&args_for_review, &context).await {
                                            GuardrailOutcome::Allow(a) => {
                                                self.renderer.guardrail_verdict(
                                                    true,
                                                    &a.risk_level.to_string(),
                                                    &a.user_auth_level.to_string(),
                                                    &a.reason,
                                                    script,
                                                )?;
                                                self.store.record_review(
                                                    self.session_id,
                                                    Some(&tc.id),
                                                    &action_json,
                                                    &a.risk_level.to_string(),
                                                    &a.user_auth_level.to_string(),
                                                    a.outcome(),
                                                    Some(&a.reason),
                                                )?;
                                            }
                                            GuardrailOutcome::Deny(a) => {
                                                self.renderer.guardrail_verdict(
                                                    false,
                                                    &a.risk_level.to_string(),
                                                    &a.user_auth_level.to_string(),
                                                    &a.reason,
                                                    script,
                                                )?;
                                                self.store.record_review(
                                                    self.session_id,
                                                    Some(&tc.id),
                                                    &action_json,
                                                    &a.risk_level.to_string(),
                                                    &a.user_auth_level.to_string(),
                                                    a.outcome(),
                                                    Some(&a.reason),
                                                )?;
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
                                                    a.risk_level, a.user_auth_level, a.reason
                                                );
                                                let deny_msg = Message::Tool {
                                                    content: format!("error: {deny_err}"),
                                                    tool_call_id: tc.id.clone(),
                                                };
                                                self.store.append_message(
                                                    self.session_id,
                                                    &deny_msg,
                                                )?;
                                                context.push(deny_msg);
                                                self.store.record_tool_call(
                                                    msg_id,
                                                    &tc.id,
                                                    &tc.function.name,
                                                    &tc.function.arguments,
                                                    risk.as_deref(),
                                                    &format!("error: {deny_err}"),
                                                    "error",
                                                )?;
                                                cursor += 1;
                                                continue;
                                            }
                                            GuardrailOutcome::Failed(e) => {
                                                bail!("guardrail review failed: {e}");
                                            }
                                        }
                                    }
                                }
                            }

                            self.renderer.tool_start(&tc.function.name, &args)?;
                            let started = Instant::now();

                            let tool_result = if let Some(tool) =
                                registry.get(&tc.function.name)
                            {
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
                            )?;
                            cursor += 1;
                            continue;
                        }

                        let mut end = cursor + 1;
                        while end < tool_calls.len() {
                            let next_args = parse_tool_args(&tool_calls[end]);
                            let next_concurrent = registry
                                .get(&tool_calls[end].function.name)
                                .is_some_and(|tool| {
                                    tool.execution_mode(&next_args) == ExecutionMode::Concurrent
                                });
                            if !next_concurrent {
                                break;
                            }
                            end += 1;
                        }

                        let batch = &tool_calls[cursor..end];
                        let config = self.config;
                        let state_dir = self.state_dir;
                        let executions = batch.iter().map(|tc| {
                            let args = parse_tool_args(tc);
                            let tool = registry
                                .get(&tc.function.name)
                                .expect("concurrent batch contains a registered tool");
                            async move {
                                let started = Instant::now();
                                let mut ctx = ToolContext {
                                    config,
                                    renderer: None,
                                    state_dir,
                                };
                                let result = tool.execute(args, &mut ctx).await;
                                (result, started.elapsed())
                            }
                        });
                        let results = join_all(executions).await;

                        for (tc, (result, elapsed)) in batch.iter().zip(results) {
                            self.persist_tool_result(msg_id, tc, result, elapsed, &mut context)?;
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

        let cost = compute_cost(self.config, &self.model, total_prompt, total_completion);

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
    ) -> Result<()> {
        let (output, status) = match result {
            Ok(result) => {
                self.renderer.tool_finished(&result.display, elapsed)?;
                (result.output, "ok")
            }
            Err(error) => {
                let message = format!("error: {error}");
                self.renderer
                    .tool_failed(&call.function.name, &error.to_string(), elapsed)?;
                (message, "error")
            }
        };

        let risk = tool_call_risk(&call.function.arguments);
        self.store.record_tool_call(
            message_id,
            &call.id,
            &call.function.name,
            &call.function.arguments,
            risk.as_deref(),
            &output,
            status,
        )?;

        let message = Message::Tool {
            content: output,
            tool_call_id: call.id.clone(),
        };
        self.store.append_message(self.session_id, &message)?;
        context.push(message);
        Ok(())
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

fn unknown_tool_result(name: &str) -> Result<ToolResult> {
    Err(anyhow::anyhow!(missing_tool_message(name)))
}

fn compute_cost(config: &Config, model: &str, prompt: u64, completion: u64) -> f64 {
    let Some(model_cfg) = config.models.get(model) else {
        return 0.0;
    };
    let Some(prices) = &model_cfg.price_per_mtok else {
        return 0.0;
    };
    (prompt as f64 / 1_000_000.0) * prices.input + (completion as f64 / 1_000_000.0) * prices.output
}
