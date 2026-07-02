use super::*;

#[test]
fn parse_assessment_direct_json() {
    let text = r#"{"risk_level":"high","user_auth_level":"low","reason":"deletes prod data"}"#;
    let a = parse_assessment(text).unwrap();
    assert_eq!(a.risk_level, RiskLevel::High);
    assert_eq!(a.user_auth_level, UserAuthLevel::Low);
    assert!(!a.is_allowed());
}

#[test]
fn parse_assessment_with_prose_wrapper() {
    let text = "Here is my assessment:\n{\"risk_level\":\"low\",\"user_auth_level\":\"unknown\",\"reason\":\"safe\"}\nDone.";
    let a = parse_assessment(text).unwrap();
    assert_eq!(a.risk_level, RiskLevel::Low);
    assert_eq!(a.user_auth_level, UserAuthLevel::Unknown);
    assert!(a.is_allowed());
}

#[test]
fn parse_assessment_missing_fields_fail_closed() {
    let text = r#"{"reason":"some text"}"#;
    let a = parse_assessment(text).unwrap();
    assert_eq!(a.risk_level, RiskLevel::Critical);
    assert_eq!(a.user_auth_level, UserAuthLevel::Unknown);
    assert!(!a.is_allowed());
}

#[test]
fn parse_assessment_invalid_json_fails() {
    let text = "not json at all";
    assert!(parse_assessment(text).is_err());
}

#[test]
fn truncate_text_keeps_prefix_and_suffix() {
    let content = "X".repeat(300);
    let (truncated, did_truncate) = truncate_text(&content, 50);
    assert!(did_truncate);
    assert!(truncated.contains(&format!("<{TRUNCATION_TAG}")));
    assert!(truncated.len() < content.len());
    assert!(truncated.starts_with('X'));
    assert!(truncated.ends_with('X'));
}

#[test]
fn render_transcript_anchors_first_and_last_user() {
    let entries: Vec<TranscriptEntry> = (0..5)
        .map(|i| TranscriptEntry {
            kind: if i % 2 == 0 {
                TranscriptEntryKind::User
            } else {
                TranscriptEntryKind::Assistant
            },
            text: format!("message {i}"),
        })
        .collect();

    let (lines, _) = render_transcript(&entries);
    let first = lines.first().unwrap();
    let last = lines.last().unwrap();
    assert!(first.contains("message 0"));
    assert!(last.contains("message 4"));
}

#[test]
fn build_user_content_includes_transcript_and_action() {
    let context = vec![
        Message::User {
            content: "delete the database".into(),
        },
        Message::Assistant {
            content: Some("I'll run rm".into()),
            tool_calls: None,
        },
    ];
    let action = serde_json::json!({
        "tool": "bash",
        "script": "rm -rf /data",
        "risk": "destructive"
    });

    let content = build_reviewer_user_content(&context, &action);
    assert!(content.contains(">>> TRANSCRIPT START"));
    assert!(content.contains("delete the database"));
    assert!(content.contains(">>> TRANSCRIPT END"));
    assert!(content.contains(">>> APPROVAL REQUEST START"));
    assert!(content.contains("rm -rf /data"));
    assert!(content.contains(">>> APPROVAL REQUEST END"));
}
