use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::models::RequestOptions;
use crate::provider::{Message, Provider, ProviderError};
use crate::store::Store;
use crate::{bash, tools};

/// Per-message caps applied only to the *summarization input*, so a very large
/// history (e.g. many big tool outputs) cannot make the compaction request
/// itself overflow. The stored transcript is untouched — this bounds only the
/// text handed to the summarizer.
const MAX_SUMMARY_ENTRY_CHARS: usize = 4000;
const MAX_SUMMARY_TOOL_CHARS: usize = 2000;

/// Clamp a single transcript entry to `max_chars`, keeping a head and tail
/// (errors and results often live at the end) with an elision marker. Operates
/// on `char`s so multi-byte codepoints are never split.
fn clamp_for_summary(content: &str, max_chars: usize) -> String {
    let total = content.chars().count();
    if total <= max_chars {
        return content.to_string();
    }
    let head = max_chars.saturating_sub(max_chars / 4);
    let tail = max_chars.saturating_sub(head);
    let chars: Vec<char> = content.chars().collect();
    let omitted = total.saturating_sub(head + tail);
    let head_str: String = chars[..head].iter().collect();
    let tail_str: String = chars[total - tail..].iter().collect();
    format!("{head_str}\n…[{omitted} chars elided for summary]…\n{tail_str}")
}

pub async fn maybe_compact(
    store: &Store,
    config: &Config,
    session_id: &str,
    request: &RequestOptions,
    context_window: Option<u64>,
    provider: &dyn Provider,
) -> Result<()> {
    let session = store
        .get_session(session_id)?
        .ok_or_else(|| anyhow::anyhow!("session not found"))?;

    let threshold = config.compaction.fraction;

    let tokens = if session.last_total_tokens > 0 {
        session.last_total_tokens
    } else {
        store.estimate_context_tokens(session_id)
    };

    let should_compact = match context_window {
        Some(cw) => (tokens as f64) > (cw as f64 * threshold),
        None => false,
    };

    if should_compact {
        run_compaction(store, config, session_id, request, provider, None).await?;
    }
    Ok(())
}

pub async fn run_compaction(
    store: &Store,
    config: &Config,
    session_id: &str,
    request: &RequestOptions,
    provider: &dyn Provider,
    custom_focus: Option<&str>,
) -> Result<()> {
    bash::install_signal_forwarder();
    let records = store.message_records_from_seq(session_id, 0)?;
    let system_prompt = store.system_prompt(session_id)?;
    let keep = config.compaction.keep_recent_turns;

    // Count user turns from the end
    let mut user_turn_starts: Vec<i64> = Vec::new();
    for rec in records.iter().rev() {
        if rec.role == "user" {
            user_turn_starts.push(rec.seq);
            if user_turn_starts.len() >= keep {
                break;
            }
        }
    }
    user_turn_starts.reverse();

    let cut_seq = if keep == 0 {
        i64::MAX
    } else {
        user_turn_starts.first().copied().unwrap_or(i64::MAX)
    };

    let to_summarize: Vec<String> = records
        .iter()
        .filter(|m| m.seq < cut_seq && m.role != "summary" && m.role != "system")
        .map(|m| {
            let (role, cap) = match m.role.as_str() {
                "user" => ("user", MAX_SUMMARY_ENTRY_CHARS),
                "assistant" => ("assistant", MAX_SUMMARY_ENTRY_CHARS),
                "tool" => ("tool-result", MAX_SUMMARY_TOOL_CHARS),
                _ => ("system", MAX_SUMMARY_ENTRY_CHARS),
            };
            let mut text = if m.content.is_empty() {
                format!("[{role}]: (no text content)")
            } else {
                format!("[{role}]: {}", clamp_for_summary(&m.content, cap))
            };
            // Include toolcall requests so compaction sees what the assistant actually asked for
            if m.role == "assistant"
                && let Some(calls) = crate::store::parse_tool_calls(m.tool_calls_json.as_deref())
            {
                for c in calls {
                    text.push_str(&format!(
                        "\n[toolcall {}]: {}",
                        c.function.name,
                        clamp_for_summary(&c.function.arguments, MAX_SUMMARY_TOOL_CHARS)
                    ));
                }
            }
            text
        })
        .collect();

    if to_summarize.is_empty() {
        return Ok(());
    }

    let prior_summary = records
        .iter()
        .rfind(|m| m.role == "summary")
        .map(|m| m.content.as_str());

    let summarize_prompt =
        build_summarize_prompt(prior_summary, &to_summarize.join("\n---\n"), custom_focus);

    let msgs = vec![
        Message::System {
            content: system_prompt,
        },
        Message::User {
            content: summarize_prompt.into(),
        },
    ];

    let tools: Vec<serde_json::Value> = vec![];
    let mut ignore_event = |_event: crate::provider::StreamEvent| Ok(());
    let result = provider
        .stream_chat(request, &msgs, &tools, &mut ignore_event)
        .await;
    match result {
        Ok(r) => {
            let content = match r.message {
                Message::Assistant { content, .. } => content.unwrap_or_default(),
                _ => String::new(),
            };
            if keep == 0 || cut_seq == i64::MAX {
                store.append_summary(session_id, &content)?;
            } else {
                store.insert_summary_before(session_id, &content, cut_seq)?;
            }
        }
        Err(ProviderError::ContextLength) => {
            // Don't overwrite a prior summary with a failure string — that
            // would silently destroy all earlier context. If we have a prior
            // summary, leave it intact and return; the caller's retry logic
            // will eventually bail with a clear error. If there is no prior
            // summary at all, insert an honest minimal note so the model knows
            // earlier history was lost.
            let has_prior = records.iter().any(|m| m.role == "summary");
            if !has_prior {
                let note = "Earlier conversation history was lost due to context overflow.";
                if keep == 0 || cut_seq == i64::MAX {
                    store.append_summary(session_id, note)?;
                } else {
                    store.insert_summary_before(session_id, note, cut_seq)?;
                }
            }
        }
        Err(e) => {
            return Err(anyhow::anyhow!("compaction failed: {e}"));
        }
    }
    Ok(())
}

fn build_summarize_prompt(
    prior_summary: Option<&str>,
    transcript: &str,
    custom_focus: Option<&str>,
) -> String {
    let mut prompt = if prior_summary.is_some() {
        "Update this conversation summary for future context. Remove stale facts."
    } else {
        "Summarize this conversation concisely for future context."
    }
    .to_string();

    prompt.push_str(
        "\n\nPreserve all important facts needed to continue the work correctly, including requirements, constraints, decisions, current state, unresolved problems, and next steps.",
    );
    if let Some(focus) = custom_focus {
        prompt.push_str(
            "\n\nGive material relevant to the custom focus more of the available detail and summary budget. The focus does not permit omitting other important facts.\n\nCustom focus:\n",
        );
        prompt.push_str(focus);
    }
    if let Some(prior) = prior_summary {
        prompt.push_str("\n\nPrior summary:\n");
        prompt.push_str(prior);
        prompt.push_str("\n\nNew messages to incorporate:\n");
    } else {
        prompt.push_str("\n\nConversation:\n");
    }
    prompt.push_str(transcript);
    prompt
}

pub fn prune_spills(state_dir: &Path) {
    tools::prune_truncation_spills(state_dir, 7);
}

#[cfg(test)]
mod tests {
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
                    reasoning_content: None,
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
    }

    fn test_config() -> Config {
        Config {
            providers: crate::config::OrderedMap::from_iter([(
                "test".into(),
                crate::config::ProviderConfig {
                    base_url: "http://localhost".into(),
                    api_key_env: "TEST_KEY".into(),
                    models: crate::config::OrderedMap::from_iter([(
                        "fake-model".into(),
                        crate::config::ModelConfig {
                            context_window: None,
                            supported_efforts: None,
                            preserved_thinking: None,
                        },
                    )]),
                },
            )]),
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

    #[test]
    fn focused_prompt_preserves_general_context_and_prioritizes_focus() {
        let prompt = build_summarize_prompt(
            Some("Existing decisions."),
            "[user]: New evidence.",
            Some("Focus on auth.\nKeep concrete API shapes.\n"),
        );

        assert!(prompt.contains("Preserve all important facts needed to continue"));
        assert!(prompt.contains(
            "Give material relevant to the custom focus more of the available detail and summary budget"
        ));
        assert!(prompt.contains("Custom focus:\nFocus on auth.\nKeep concrete API shapes.\n"));
        assert!(prompt.contains("Prior summary:\nExisting decisions."));
        assert!(prompt.find("Custom focus:") < prompt.find("Prior summary:"));
        assert!(prompt.contains("New messages to incorporate:\n[user]: New evidence."));
    }

    #[test]
    fn unfocused_prompt_omits_focus_guidance() {
        let prompt = build_summarize_prompt(None, "[user]: Hello.", None);

        assert!(prompt.contains("Preserve all important facts needed to continue"));
        assert!(!prompt.contains("custom focus"));
        assert!(!prompt.contains("Custom focus:"));
        assert!(prompt.contains("Conversation:\n[user]: Hello."));
    }

    #[tokio::test]
    async fn compaction_keeps_only_requested_recent_turns() {
        let tmp = std::env::temp_dir().join(format!("mu-compaction-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("mu.db")).unwrap();
        let session = store.create_session("/tmp", "test/fake-model").unwrap();
        let request_model =
            crate::models::resolve_model_ref(&test_config(), "test/fake-model").unwrap();
        store
            .append_message(
                &session.id,
                &Message::System {
                    content: "system prompt".into(),
                },
            )
            .unwrap();

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
                        reasoning_content: None,
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
            None,
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
}
