use super::*;
use crate::provider::{ContentPart, FunctionCall, ImageUrl, ToolCall, Usage};

fn temp_store() -> (Store, std::path::PathBuf) {
    let tmp = std::env::temp_dir().join(format!("mu-store-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    (Store::open(&tmp.join("mu.db")).unwrap(), tmp)
}

#[test]
fn reloads_full_user_content_with_images() {
    let (store, tmp) = temp_store();
    let session = store.create_session("/tmp", "fake-model").unwrap();
    let expected_image_url = "data:image/png;base64,abcd".to_string();

    store
        .append_message(
            &session.id,
            &Message::User {
                content: UserContent::Parts(vec![
                    ContentPart::Text {
                        text: "describe this".to_string(),
                    },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: expected_image_url.clone(),
                        },
                    },
                ]),
            },
        )
        .unwrap();

    let messages = store.load_context_messages(&session.id).unwrap();
    let Message::User {
        content: UserContent::Parts(parts),
    } = &messages[0]
    else {
        panic!("expected user parts");
    };

    assert!(matches!(
        &parts[0],
        ContentPart::Text { text } if text == "describe this"
    ));
    assert!(matches!(
        &parts[1],
        ContentPart::ImageUrl { image_url } if image_url.url == expected_image_url
    ));

    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn reloads_legacy_text_user_content() {
    let (store, tmp) = temp_store();
    let session = store.create_session("/tmp", "fake-model").unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    store
        .conn
        .execute(
            "INSERT INTO message (session_id, role, content, seq, created_at)
             VALUES (?1, 'user', ?2, 0, ?3)",
            params![session.id, "legacy text", now],
        )
        .unwrap();

    let messages = store.load_context_messages(&session.id).unwrap();
    let Message::User {
        content: UserContent::Text(text),
    } = &messages[0]
    else {
        panic!("expected text user content");
    };

    assert_eq!(text, "legacy text");
    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn migrates_legacy_session_columns_and_tracks_turn_usage() {
    let tmp = std::env::temp_dir().join(format!("mu-store-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let db_path = tmp.join("mu.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                effort TEXT,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0,
                cost_total REAL NOT NULL DEFAULT 0
            );
            INSERT INTO session (
                id, created_at, updated_at, cwd, model, effort, title,
                last_total_tokens, cost_total
            ) VALUES (
                'session-1', 'now', 'now', '/tmp', 'old-model', 'high',
                'legacy', 7, 1.23
            );",
        )
        .unwrap();
    }

    let store = Store::open(&db_path).unwrap();
    let columns = store
        .conn
        .prepare("PRAGMA table_info(session)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    assert!(!columns.contains(&"effort".to_string()));
    assert!(!columns.contains(&"cost_total".to_string()));

    let loaded = store.get_session("session-1").unwrap().unwrap();
    assert_eq!(loaded.model, "old-model");
    assert_eq!(loaded.title.as_deref(), Some("legacy"));
    assert_eq!(loaded.last_total_tokens, 7);
    assert_eq!(loaded.origin, SessionOrigin::Cli);
    assert!(!loaded.archived);

    let usage = Usage {
        input_tokens: 12,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        output_tokens: 5,
        reasoning_output_tokens: 4,
        total_tokens: 17,
    };
    store
        .update_session("session-1", &usage, None, "new-model")
        .unwrap();
    let usage_rows = store.turn_usage("session-1").unwrap();

    assert_eq!(usage_rows.len(), 1);
    assert_eq!(usage_rows[0].model, "new-model");
    assert_eq!(usage_rows[0].input_tokens, 12);
    assert_eq!(usage_rows[0].cache_read_input_tokens, 3);
    assert_eq!(usage_rows[0].cache_write_input_tokens, 2);
    assert_eq!(usage_rows[0].output_tokens, 5);
    assert_eq!(usage_rows[0].reasoning_output_tokens, 4);
    assert_eq!(usage_rows[0].total_tokens, 17);

    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn new_sessions_default_to_cli_origin_and_unarchived() {
    let (store, tmp) = temp_store();
    let session = store.create_session("/tmp", "fake-model").unwrap();
    let loaded = store.get_session(&session.id).unwrap().unwrap();

    assert_eq!(loaded.origin, SessionOrigin::Cli);
    assert!(!loaded.archived);
    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn list_sessions_defaults_to_cli_origin_and_skips_archived() {
    let (store, tmp) = temp_store();
    let cli = store.create_session("/tmp", "cli-model").unwrap();
    let web = store
        .create_session_with_origin("/tmp", "web-model", SessionOrigin::Web)
        .unwrap();
    let archived = store.create_session("/tmp", "archived-model").unwrap();
    store.set_session_archived(&archived.id, true).unwrap();

    let sessions = store.list_sessions(20).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].0.id, cli.id);
    assert_eq!(sessions[0].0.origin, SessionOrigin::Cli);
    assert!(!sessions[0].0.archived);

    let web_sessions = store
        .list_sessions_by_origin(SessionOrigin::Web, 20)
        .unwrap();
    assert_eq!(web_sessions.len(), 1);
    assert_eq!(web_sessions[0].0.id, web.id);
    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn all_session_summaries_include_cli_and_web_origins() {
    let (store, tmp) = temp_store();
    let cli = store.create_session("/tmp", "cli-model").unwrap();
    let web = store
        .create_session_with_origin("/tmp", "web-model", SessionOrigin::Web)
        .unwrap();

    let summaries = store.list_all_session_summaries(20).unwrap();
    let ids = summaries
        .iter()
        .map(|summary| (summary.id.as_str(), summary.origin))
        .collect::<Vec<_>>();

    assert!(ids.contains(&(cli.id.as_str(), SessionOrigin::Cli)));
    assert!(ids.contains(&(web.id.as_str(), SessionOrigin::Web)));
    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn begin_pending_turn_tracks_prompt_and_checkpoint() {
    let (store, tmp) = temp_store();
    let session = store.create_session("/tmp", "fake-model").unwrap();

    let prompt_id = store
        .begin_pending_turn(&session.id, &UserContent::Text("hello".into()))
        .unwrap();
    let pending = store.pending_turn(&session.id).unwrap().unwrap();

    assert_eq!(pending.state, PendingState::Running);
    assert_eq!(pending.prompt_message_id, prompt_id);
    assert_eq!(pending.checkpoint_message_id, prompt_id);
    assert_eq!(pending.retry_count, 0);
    let prompt = store
        .prompt_user_content(&session.id, prompt_id)
        .unwrap()
        .unwrap();
    assert_eq!(prompt.text(), "hello");
    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn reconcile_pending_turn_synthesizes_missing_tool_results() {
    let (store, tmp) = temp_store();
    let session = store.create_session("/tmp", "fake-model").unwrap();

    store
        .append_message(
            &session.id,
            &Message::User {
                content: UserContent::Text("seed".into()),
            },
        )
        .unwrap();
    let prompt_id = store
        .begin_pending_turn(&session.id, &UserContent::Text("run".into()))
        .unwrap();
    let assistant_id = store
        .advance_pending_checkpoint_with_message(
            &session.id,
            &Message::Assistant {
                content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "call-a".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments:
                                "{\"title\":\"a\",\"risk\":\"readonly\",\"script\":\"echo a\"}"
                                    .into(),
                        },
                    },
                    ToolCall {
                        id: "call-b".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments:
                                "{\"title\":\"b\",\"risk\":\"readonly\",\"script\":\"echo b\"}"
                                    .into(),
                        },
                    },
                ]),
            },
        )
        .unwrap();
    store
        .persist_tool_result(
            &session.id,
            ToolCallRecord {
                message_id: assistant_id,
                id: "call-a",
                tool: "bash",
                args: "{\"title\":\"a\",\"risk\":\"readonly\",\"script\":\"echo a\"}",
                risk: Some("readonly"),
                output: "a",
                status: "ok",
            },
            "a",
        )
        .unwrap();

    let lock = store.acquire_session_lock(&session.id).unwrap();
    let pending = store
        .reconcile_pending_turn_locked(&lock, &session.id)
        .unwrap()
        .unwrap();
    assert_eq!(pending.state, PendingState::Incomplete);
    assert_eq!(pending.prompt_message_id, prompt_id);
    assert_eq!(
        pending.error_message.as_deref(),
        Some("previous turn was interrupted")
    );

    let tool_messages = store
        .load_context_messages(&session.id)
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
    assert_eq!(tool_messages[0], ("call-a".into(), "a".into()));
    assert_eq!(
        tool_messages[1],
        (
            "call-b".into(),
            "error: interrupted before tool result was completed".into(),
        )
    );
    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn session_lock_contends_across_store_handles_for_same_db() {
    let (store, tmp) = temp_store();
    let session = store.create_session("/tmp", "fake-model").unwrap();
    let second = Store::open(&tmp.join("mu.db")).unwrap();

    let _lock = store.acquire_session_lock(&session.id).unwrap();

    assert!(second.is_session_busy(&session.id));
    let err = second.acquire_session_lock(&session.id).unwrap_err();
    assert!(err.to_string().contains("session busy"));

    let _ = std::fs::remove_dir_all(tmp);
}
