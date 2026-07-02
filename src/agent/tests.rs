use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use super::*;
use crate::config::{
    CompactionConfig, GuardrailConfig, LimitsConfig, ProviderConfig, RedactionConfig,
    TerminalBellConfig,
};
use crate::provider::{
    FinishReason, FunctionCall, ProviderError, StreamResult, Usage, UserContent,
};

struct ToolThenStopProvider {
    step: Mutex<usize>,
    cwd: String,
}

struct RetryThenStopProvider {
    step: Mutex<usize>,
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
                    input_tokens: 1,
                    output_tokens: 1,
                    total_tokens: 2,
                    ..Usage::default()
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
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 2,
                        ..Usage::default()
                    }),
                })
            }
            other => panic!("unexpected provider step {other}"),
        }
    }
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
    };

    store
        .begin_pending_turn(&session.id, &UserContent::Text("run both".into()))
        .unwrap();

    agent.run_turn().await.unwrap();
    (store, session.id)
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
