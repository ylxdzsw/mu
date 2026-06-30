use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::assets::{INDEX_HTML, static_asset};
use super::http::{parse_socket_mode, parse_uri, prepare_socket_path};
use super::*;

fn test_state(launch_cwd: PathBuf) -> Arc<WebState> {
    test_state_with_exe(launch_cwd, std::env::current_exe().unwrap())
}

fn test_state_with_exe(launch_cwd: PathBuf, exe: PathBuf) -> Arc<WebState> {
    let launch_project = paths::discover_project(&launch_cwd).map(project_summary);
    let recent_projects = launch_project.iter().cloned().collect();
    Arc::new(WebState {
        exe,
        launch_cwd,
        launch_project,
        global_home: global_scope_cwd(),
        recent_projects: Mutex::new(recent_projects),
        turns: Mutex::new(HashMap::new()),
        upload_root: std::env::temp_dir()
            .join(format!("mu-web-test-uploads-{}", uuid::Uuid::new_v4())),
    })
}

async fn http_roundtrip(state: Arc<WebState>, request: &str) -> String {
    let (mut client, server) = tokio::io::duplex(1024 * 1024);
    let task = tokio::spawn(handle_connection(server, state));

    client.write_all(request.as_bytes()).await.unwrap();
    client.shutdown().await.unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    task.await.unwrap().unwrap();
    response
}

fn response_body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}

fn response_json(response: &str) -> serde_json::Value {
    serde_json::from_str(response_body(response)).unwrap()
}

#[tokio::test]
async fn serves_bootstrap_over_http_handler() {
    let root = std::env::temp_dir().join(format!("mu-web-bootstrap-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let state = test_state(root.clone());

    let response = http_roundtrip(state, "GET /api/bootstrap HTTP/1.1\r\nhost: mu\r\n\r\n").await;
    let body: serde_json::Value = serde_json::from_str(response_body(&response)).unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(body["launch_cwd"], root.display().to_string());
    assert_eq!(
        body["global_home"],
        global_scope_cwd().display().to_string()
    );
    assert_eq!(body["launch_project"]["path"], root.display().to_string());
    assert_eq!(body["launch_project"]["marker"], "git");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn serves_static_app_shell_over_http_handler() {
    let state = test_state(std::env::temp_dir());

    let response = http_roundtrip(state, "GET / HTTP/1.1\r\nhost: mu\r\n\r\n").await;

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("content-type: text/html; charset=utf-8"));
    assert!(response_body(&response).contains(
        r#"<mu-sidebar id="sidebar" class="sidebar" aria-label="Left panel"></mu-sidebar>"#
    ));
}

#[tokio::test]
async fn serves_split_static_assets_over_http_handler() {
    let state = test_state(std::env::temp_dir());

    let js = http_roundtrip(
        state.clone(),
        "GET /components/mu-sidebar.js HTTP/1.1\r\nhost: mu\r\n\r\n",
    )
    .await;
    let css = http_roundtrip(state, "GET /styles/layout.css HTTP/1.1\r\nhost: mu\r\n\r\n").await;

    assert!(js.starts_with("HTTP/1.1 200 OK"));
    assert!(js.contains("content-type: text/javascript; charset=utf-8"));
    assert!(response_body(&js).contains("customElements.define(\"mu-sidebar\""));

    assert!(css.starts_with("HTTP/1.1 200 OK"));
    assert!(css.contains("content-type: text/css; charset=utf-8"));
    assert!(response_body(&css).contains(".conversation-shell"));
}

#[tokio::test]
async fn launches_turn_then_replays_events_over_sse() {
    let root = std::env::temp_dir().join(format!("mu-web-turn-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let fake = root.join("fake-mu");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"event\":\"assistant_delta\",\"payload\":{\"text\":\"ok\"}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = test_state_with_exe(root.clone(), fake);
    let body = json!({
        "project": root.display().to_string(),
        "session_id": "session-1",
        "prompt": "hello"
    })
    .to_string();
    let request = format!(
        "POST /api/turns HTTP/1.1\r\nhost: mu\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n{}",
        body.len(),
        body
    );

    let launch = http_roundtrip(state.clone(), &request).await;
    let launched = response_json(&launch);
    let turn_id = launched["turn"]["id"].as_str().unwrap();
    let events = http_roundtrip(
        state,
        &format!("GET /api/turns/{turn_id}/events?after=0 HTTP/1.1\r\nhost: mu\r\n\r\n"),
    )
    .await;

    assert!(launch.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(launched["turn"]["session_id"], "session-1");
    assert!(events.starts_with("HTTP/1.1 200 OK"));
    assert!(events.contains("content-type: text/event-stream; charset=utf-8"));
    assert!(events.contains("x-accel-buffering: no"));
    assert!(response_body(&events).contains("event: turn_start"));
    assert!(response_body(&events).contains("event: assistant_delta"));
    assert!(response_body(&events).contains("event: turn_finish"));
    assert!(response_body(&events).contains("id: 1"));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn turn_event_replay_respects_after_cursor() {
    let root = std::env::temp_dir().join(format!("mu-web-turn-after-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let fake = root.join("fake-mu");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"event\":\"assistant_delta\",\"payload\":{\"text\":\"first\"}}'\nprintf '%s\\n' '{\"event\":\"assistant_delta\",\"payload\":{\"text\":\"second\"}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = test_state_with_exe(root.clone(), fake);
    let body = json!({
        "project": root.display().to_string(),
        "session_id": "session-1",
        "prompt": "hello"
    })
    .to_string();
    let launch = http_roundtrip(
        state.clone(),
        &format!(
            "POST /api/turns HTTP/1.1\r\nhost: mu\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n{}",
            body.len(),
            body
        ),
    )
    .await;
    let turn_id = response_json(&launch)["turn"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let replay = http_roundtrip(
        state,
        &format!("GET /api/turns/{turn_id}/events?after=2 HTTP/1.1\r\nhost: mu\r\n\r\n"),
    )
    .await;

    assert!(replay.starts_with("HTTP/1.1 200 OK"));
    assert!(!response_body(&replay).contains("event: turn_start"));
    assert!(!response_body(&replay).contains("\"text\":\"first\""));
    assert!(response_body(&replay).contains("\"text\":\"second\""));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn busy_session_turn_returns_conflict() {
    let root = std::env::temp_dir().join(format!("mu-web-busy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let state = test_state(root.clone());
    state.turns.lock().await.insert(
        "turn-1".into(),
        TurnRuntime::new(
            ActiveTurn {
                id: "turn-1".into(),
                project: root.display().to_string(),
                session_id: Some("session-1".into()),
                started_at: "2026-06-28T00:00:00Z".into(),
                pgid: 1,
            },
            "busy".into(),
        ),
    );
    let body = json!({
        "project": root.display().to_string(),
        "session_id": "session-1",
        "prompt": "hello"
    })
    .to_string();
    let request = format!(
        "POST /api/turns HTTP/1.1\r\nhost: mu\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n{}",
        body.len(),
        body
    );

    let response = http_roundtrip(state, &request).await;

    assert!(response.starts_with("HTTP/1.1 409 Conflict"));
    assert!(response_body(&response).contains("session busy"));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn global_session_creation_uses_home_scope() {
    let root = std::env::temp_dir().join(format!("mu-web-global-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let fake = root.join("fake-mu");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '{\"cwd\":\"%s\",\"ok\":true}\\n' \"$(pwd)\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = test_state_with_exe(root.clone(), fake);
    let body = json!({ "project": GLOBAL_PROJECT_ID }).to_string();
    let request = format!(
        "POST /api/sessions HTTP/1.1\r\nhost: mu\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n{}",
        body.len(),
        body
    );

    let response = http_roundtrip(state, &request).await;
    let payload = response_json(&response);

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(payload["cwd"], global_scope_cwd().display().to_string());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn create_turn_without_session_precreates_web_session_and_exposes_active_snapshot() {
    let root = std::env::temp_dir().join(format!("mu-web-active-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let fake = root.join("fake-mu");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
if [ "$1" = "session" ] && [ "$2" = "new" ]; then
  printf '%s\n' '{"id":"session-new","created_at":"2026-06-30T00:00:00Z","updated_at":"2026-06-30T00:00:00Z","cwd":"/tmp/work","model":"fake-model","effort":null,"title":null,"last_total_tokens":0,"cost_total":0,"origin":"web","archived":false,"message_count":1,"turn_count":0}'
  exit 0
fi
cat >/dev/null
printf '%s\n' '{"event":"assistant_delta","payload":{"text":"live"}}'
sleep 0.2
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = test_state_with_exe(root.clone(), fake);
    let prompt = "hello from web";
    let body = json!({
        "project": root.display().to_string(),
        "prompt": prompt
    })
    .to_string();
    let launch = http_roundtrip(
        state.clone(),
        &format!(
            "POST /api/turns HTTP/1.1\r\nhost: mu\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n{}",
            body.len(),
            body
        ),
    )
    .await;
    let launched = response_json(&launch);
    let active = http_roundtrip(
        state,
        &format!(
            "GET /api/turns/active?project={}&session=session-new HTTP/1.1\r\nhost: mu\r\n\r\n",
            percent_encode_component(&root.display().to_string())
        ),
    )
    .await;
    let payload = response_json(&active);

    assert!(launch.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(launched["turn"]["session_id"], "session-new");
    assert!(active.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(payload["turn"]["session_id"], "session-new");
    assert_eq!(payload["snapshot"]["prompt"], prompt);
    assert!(
        payload["snapshot"]["raw_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "turn_start")
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn malformed_http_request_returns_json_400() {
    let state = test_state(std::env::temp_dir());

    let response = http_roundtrip(
        state,
        "POST /api/projects/open HTTP/1.1\r\ncontent-length: nope\r\n\r\n",
    )
    .await;
    let body: serde_json::Value = serde_json::from_str(response_body(&response)).unwrap();

    assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("invalid content-length")
    );
}

#[test]
fn parses_uri_and_percent_decodes_query() {
    let (path, query) = parse_uri("/api/status?project=%2Ftmp%2Fwork&session=a+b");

    assert_eq!(path, "/api/status");
    assert_eq!(query.get("project").map(String::as_str), Some("/tmp/work"));
    assert_eq!(query.get("session").map(String::as_str), Some("a b"));
}

#[test]
fn parses_octal_socket_modes() {
    assert_eq!(parse_socket_mode("0600").unwrap(), 0o600);
    assert_eq!(parse_socket_mode("660").unwrap(), 0o660);
}

#[test]
fn decodes_base64_image_uploads() {
    let tmp = std::env::temp_dir().join(format!("mu-web-upload-{}", uuid::Uuid::new_v4()));
    let uploads = vec![ImageUpload {
        name: "tiny.png".into(),
        data_url: "data:image/png;base64,aGVsbG8=".into(),
    }];

    let paths = save_uploads(&tmp, &uploads).unwrap();

    assert_eq!(paths.len(), 1);
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"hello");
    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn rejects_regular_file_as_socket_path() {
    let tmp = std::env::temp_dir().join(format!("mu-web-socket-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, "not a socket").unwrap();

    let error = prepare_socket_path(&tmp).unwrap_err().to_string();

    assert!(error.contains("refusing to replace non-socket path"));
    assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "not a socket");
    let _ = std::fs::remove_file(tmp);
}

#[test]
fn static_app_contains_minimal_shell_and_bootstrap_hook() {
    let app_css = static_asset("/app.css").unwrap().body;
    let app_js = static_asset("/app.js").unwrap().body;
    let constants = static_asset("/lib/constants.js").unwrap().body;
    let sidebar = static_asset("/components/mu-sidebar.js").unwrap().body;

    assert!(INDEX_HTML.contains(r#"mu-conversation-view"#));
    assert!(INDEX_HTML.contains(r#"aria-label="Left panel""#));
    assert!(app_css.contains("@import url(\"/styles/layout.css\")"));
    assert!(app_js.contains("components/mu-sidebar.js"));
    assert!(INDEX_HTML.contains(r#"type="module" src="/app.js""#));
    assert!(app_js.contains("/api/bootstrap"));
    assert!(INDEX_HTML.contains(r#"id="project-modal""#));
    assert!(constants.contains(GLOBAL_PROJECT_ID));
    assert!(sidebar.contains("customElements.define(\"mu-sidebar\""));
}

fn percent_encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}
