use crate::config::Config;
use crate::provider::{Message, UserContent};
use crate::store::{CompactionMode, CompactionTrigger};

pub const EMERGENCY_OUTPUT_UNAVAILABLE: &str =
    "[Bash output unavailable during emergency compaction.]";

const COMMON_PROMPT: &str = "\
We are replacing the current model context. Produce only a self-contained Markdown \
summary for the next context. Do not wrap it in XML or JSON. Do not address the user \
and do not continue the task yet. Avoid tools unless they are genuinely necessary.

Use these sections, omitting only sections with no useful content:
- `## Purpose and current request`
- `## Constraints and acceptance criteria`
- `## Progress`
- `## Artifacts and current state`
- `## Important findings`
- `## Failed approaches`
- `## Pending work`
- `## Recovery references`

You may add `## Notes` as a catch-all only for important material that fits nowhere \
else.";

pub fn compaction_prompt(mode: CompactionMode, focus: Option<&str>) -> UserContent {
    let mode_prompt = match mode {
        CompactionMode::AwaitUser => {
            "\
This is an out-of-turn checkpoint. Summarize the completed conversation state and open \
threads, then stop. There is no new user prompt in this context."
        }
        CompactionMode::ContinueTurn => {
            "\
This is an in-turn checkpoint that will be continued immediately. Include a final \
`## Immediate next step` section as the last section and make it concrete enough for \
another instance to continue without asking the user to repeat the request."
        }
    };
    let focus = focus
        .map(|focus| format!("\n\nAdditional focus from the user:\n{focus}"))
        .unwrap_or_default();
    format!("{COMMON_PROMPT}\n\n{mode_prompt}{focus}").into()
}

pub fn checkpoint(summary: &str, mode: CompactionMode, epoch: u64) -> String {
    format!(
        "<session_checkpoint mode=\"{}\" epoch=\"{epoch}\">\n{summary}\n</session_checkpoint>",
        mode.as_str()
    )
}

pub fn soft_compaction_threshold(context_window: u64, soft_fraction: f64) -> u64 {
    fraction_threshold(context_window, soft_fraction)
}

pub fn hard_compaction_threshold(
    context_window: u64,
    hard_fraction: f64,
    hard_headroom_tokens: u64,
) -> u64 {
    let fraction_threshold = fraction_threshold(context_window, hard_fraction);
    let fixed_headroom_threshold = context_window.saturating_sub(hard_headroom_tokens);
    fraction_threshold.min(fixed_headroom_threshold)
}

fn fraction_threshold(context_window: u64, fraction: f64) -> u64 {
    let product = context_window as f64 * fraction;
    let integer = product.round();
    let tolerance = f64::EPSILON * product.abs().max(1.0) * 4.0;
    if (product - integer).abs() <= tolerance {
        integer as u64
    } else {
        product.floor() as u64
    }
}

pub fn should_compact(
    config: &Config,
    trigger: CompactionTrigger,
    context_tokens: u64,
    context_window: Option<u64>,
) -> bool {
    if !config.compaction.enabled {
        return false;
    }
    context_window.is_some_and(|window| match trigger {
        CompactionTrigger::Soft => {
            context_tokens > soft_compaction_threshold(window, config.compaction.soft_fraction)
        }
        CompactionTrigger::Hard => {
            context_tokens
                > hard_compaction_threshold(
                    window,
                    config.compaction.hard_fraction,
                    config.compaction.hard_headroom_tokens,
                )
        }
        CompactionTrigger::Emergency | CompactionTrigger::Manual => true,
    })
}

pub fn emergency_projection(
    messages: &[Message],
    headroom_tokens: u64,
) -> (Vec<Message>, Vec<String>) {
    let mut projected = messages.to_vec();
    let mut saved_tokens = 0_u64;
    let mut elided = Vec::new();
    if headroom_tokens == 0 {
        return (projected, elided);
    }
    for message in &mut projected {
        if !matches!(message, Message::Tool { .. }) {
            continue;
        }
        let before = message.approx_tokens();
        let Message::Tool {
            content,
            attachments,
            tool_call_id,
        } = message
        else {
            unreachable!()
        };
        if content == EMERGENCY_OUTPUT_UNAVAILABLE && attachments.is_empty() {
            continue;
        }
        let call_id = tool_call_id.clone();
        *content = EMERGENCY_OUTPUT_UNAVAILABLE.into();
        attachments.clear();
        let after = message.approx_tokens();
        saved_tokens = saved_tokens.saturating_add(before.saturating_sub(after));
        elided.push(call_id);
        if saved_tokens >= headroom_tokens {
            break;
        }
    }
    (projected, elided)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolAttachment;

    #[test]
    fn threshold_math_preserves_fixed_headroom() {
        assert_eq!(soft_compaction_threshold(200_000, 0.70), 140_000);
        assert_eq!(hard_compaction_threshold(200_000, 0.85, 48_000), 152_000);
        assert_eq!(soft_compaction_threshold(353_000, 0.70), 247_100);
        assert_eq!(hard_compaction_threshold(353_000, 0.85, 1), 300_050);
    }

    #[test]
    fn prompts_leave_wrapping_to_mu_and_put_the_continuation_last() {
        let prompt = compaction_prompt(CompactionMode::ContinueTurn, Some("focus here"));
        let text = prompt.text();
        assert!(!text.contains("<session_checkpoint"));
        assert!(text.contains("## Notes"));
        assert!(text.contains("focus here"));
        assert!(
            text.ends_with(
                "another instance to continue without asking the user to repeat the request.\n\nAdditional focus from the user:\nfocus here"
            ),
            "{text}"
        );
        let wrapped = checkpoint(
            "## Immediate next step\nRun tests.",
            CompactionMode::ContinueTurn,
            4,
        );
        assert_eq!(
            wrapped,
            "<session_checkpoint mode=\"continue_turn\" epoch=\"4\">\n## Immediate next step\nRun tests.\n</session_checkpoint>"
        );
    }

    #[test]
    fn emergency_projection_elides_oldest_results_first() {
        let messages = vec![
            Message::Tool {
                content: "a".repeat(8_000),
                attachments: vec![ToolAttachment {
                    attachment: crate::provider::Attachment {
                        filename: "plot.png".into(),
                        media_type: "image/png".into(),
                        data: vec![1, 2, 3],
                    },
                    detail: crate::provider::ImageDetail::Auto,
                    object_sha256: None,
                }],
                tool_call_id: "first".into(),
            },
            Message::Tool {
                content: "b".repeat(8_000),
                attachments: Vec::<ToolAttachment>::new(),
                tool_call_id: "second".into(),
            },
        ];
        let (projected, elided) = emergency_projection(&messages, 10_000);
        assert_eq!(elided, ["first", "second"]);
        assert!(matches!(
            &projected[0],
            Message::Tool { content, attachments, .. }
                if content == EMERGENCY_OUTPUT_UNAVAILABLE && attachments.is_empty()
        ));
    }
}
