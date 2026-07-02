use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;

use super::*;
use crate::models::RequestOptions;
use crate::provider::{FinishReason, StreamResult, Usage};

struct FakeProvider;

#[async_trait(?Send)]
impl Provider for FakeProvider {
    async fn stream_chat(
        &self,
        _request: &RequestOptions,
        _messages: &[Message],
        _tools: &[Value],
        _on_event: &mut dyn FnMut(crate::provider::StreamEvent) -> Result<(), ProviderError>,
    ) -> Result<StreamResult, ProviderError> {
        Ok(StreamResult {
            message: Message::Assistant {
                content: Some("summary".into()),
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
}

fn test_config() -> Config {
    Config {
        providers: HashMap::from([(
            "test".into(),
            crate::config::ProviderConfig {
                base_url: "http://localhost".into(),
                api_key_env: "TEST_KEY".into(),
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
        compaction: crate::config::CompactionConfig {
            fraction: 0.75,
            keep_recent_turns: 2,
        },
        limits: crate::config::LimitsConfig::default(),
        guardrail: crate::config::GuardrailConfig::default(),
        terminal_bell: crate::config::TerminalBellConfig::default(),
        redaction: crate::config::RedactionConfig::default(),
        env: Default::default(),
    }
}

#[tokio::test]
async fn compaction_keeps_only_requested_recent_turns() {
    let tmp = std::env::temp_dir().join(format!("mu-compaction-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let store = Store::open(&tmp.join("mu.db")).unwrap();
    let session = store.create_session("/tmp", "test/fake-model").unwrap();
    let request_model =
        crate::models::resolve_model_ref(&test_config(), "test/fake-model").unwrap();

    for n in 1..=4 {
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: format!("user {n}").into(),
                },
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::Assistant {
                    content: Some(format!("assistant {n}")),
                    tool_calls: None,
                },
            )
            .unwrap();
    }

    run_compaction(
        &store,
        &test_config(),
        &session.id,
        &RequestOptions {
            model: request_model,
        },
        &FakeProvider,
        "system prompt",
    )
    .await
    .unwrap();

    let messages = store.load_context_messages(&session.id).unwrap();
    let visible_users: Vec<String> = messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => Some(content.text()),
            _ => None,
        })
        .collect();

    // The summary is now framed as a leading user message (so the
    // assembled context keeps exactly one leading system message), followed
    // by the kept-verbatim recent turns.
    assert_eq!(
        visible_users,
        vec![
            "[summary of earlier conversation]\nsummary".to_string(),
            "user 3".to_string(),
            "user 4".to_string()
        ]
    );

    let _ = std::fs::remove_dir_all(Path::new(&tmp));
}
