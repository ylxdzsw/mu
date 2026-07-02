use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::models::RequestOptions;
use crate::provider::{Message, Provider, ProviderError};
use crate::store::Store;

pub async fn maybe_compact(
    store: &Store,
    config: &Config,
    session_id: &str,
    request: &RequestOptions,
    context_window: Option<u64>,
    provider: &dyn Provider,
    system_prompt: &str,
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
        run_compaction(store, config, session_id, request, provider, system_prompt).await?;
    }
    Ok(())
}

pub async fn run_compaction(
    store: &Store,
    config: &Config,
    session_id: &str,
    request: &RequestOptions,
    provider: &dyn Provider,
    system_prompt: &str,
) -> Result<()> {
    crate::tools::bash::install_signal_forwarder();
    let messages = store.all_messages_for_session(session_id)?;
    let keep = config.compaction.keep_recent_turns;

    // Count user turns from the end
    let mut user_turn_starts: Vec<i64> = Vec::new();
    for msg in messages.iter().rev() {
        if msg.role == "user" {
            user_turn_starts.push(msg.seq);
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

    let to_summarize: Vec<String> = messages
        .iter()
        .filter(|m| m.seq < cut_seq && m.role != "summary")
        .map(|m| {
            let role = match m.role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                "tool" => "tool-result",
                _ => "system",
            };
            if m.content.is_empty() {
                format!("[{role}]: (no text content)")
            } else {
                format!("[{role}]: {}", m.content)
            }
        })
        .collect();

    if to_summarize.is_empty() {
        return Ok(());
    }

    let prior_summary = messages
        .iter()
        .rfind(|m| m.role == "summary")
        .map(|m| m.content.as_str());

    let summarize_prompt = if let Some(prior) = prior_summary {
        format!(
            "Update this conversation summary. Preserve still-true details, remove stale facts.\n\nPrior summary:\n{prior}\n\nNew messages to incorporate:\n{}",
            to_summarize.join("\n---\n")
        )
    } else {
        format!(
            "Summarize this conversation concisely for future context:\n\n{}",
            to_summarize.join("\n---\n")
        )
    };

    let msgs = vec![
        Message::System {
            content: system_prompt.into(),
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
            let has_prior = messages.iter().any(|m| m.role == "summary");
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

pub fn prune_spills(state_dir: &Path) {
    crate::tools::truncate::prune_truncation_spills(state_dir, 7);
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
