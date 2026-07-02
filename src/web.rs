use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{Mutex, watch};

use crate::cli::WebArgs;
use crate::paths;

mod assets;
mod http;
#[cfg(test)]
mod tests;

use assets::{INDEX_HTML, static_asset};
use http::{
    parse_socket_mode, prepare_socket_path, query_flag, read_request, write_json_response,
    write_response, write_sse_event, write_sse_headers,
};

const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_TURN_EVENT_BUFFER: usize = 512;
const GLOBAL_PROJECT_ID: &str = "__mu_global__";

pub async fn serve(args: WebArgs, launch_cwd: PathBuf) -> Result<()> {
    let mode = parse_socket_mode(&args.socket_mode)?;
    prepare_socket_path(&args.socket)?;
    let listener = UnixListener::bind(&args.socket)
        .map_err(|error| anyhow::anyhow!("binding {}: {error}", args.socket.display()))?;
    std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting socket mode on {}", args.socket.display()))?;

    let launch_project = paths::discover_project(&launch_cwd).map(project_summary);
    let recent_projects = launch_project.iter().cloned().collect();
    let upload_root = paths::runtime_dir().join("web-uploads");
    paths::ensure_dir(&upload_root)?;
    let global_home = global_scope_cwd();

    let state = Arc::new(WebState {
        exe: std::env::current_exe().context("resolving current executable")?,
        launch_cwd,
        launch_project,
        global_home,
        recent_projects: Mutex::new(recent_projects),
        turns: Mutex::new(HashMap::new()),
        upload_root,
    });

    eprintln!("mu web listening on unix://{}", args.socket.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, state).await {
                eprintln!("mu web request failed: {error}");
            }
        });
    }
}

struct WebState {
    exe: PathBuf,
    launch_cwd: PathBuf,
    launch_project: Option<ProjectSummary>,
    global_home: PathBuf,
    recent_projects: Mutex<Vec<ProjectSummary>>,
    turns: Mutex<HashMap<String, Arc<TurnRuntime>>>,
    upload_root: PathBuf,
}

#[derive(Clone, Serialize)]
struct ProjectSummary {
    path: String,
    marker: &'static str,
}

#[derive(Clone, Serialize)]
struct ActiveTurn {
    id: String,
    project: String,
    session_id: Option<String>,
    started_at: String,
    pgid: i32,
}

#[derive(Serialize)]
struct TurnAcceptedResponse {
    turn: ActiveTurn,
}

#[derive(Clone, Default, Serialize)]
struct PlainTurnSnapshot {
    prompt: String,
    assistant_text: String,
    raw_events: Vec<serde_json::Value>,
    stderr: String,
    exit_code: Option<i32>,
}

#[derive(Clone, Serialize)]
struct ActiveTurnView {
    turn: ActiveTurn,
    last_seq: u64,
    completed: bool,
    snapshot: PlainTurnSnapshot,
}

#[derive(Clone)]
struct TurnEventEnvelope {
    seq: u64,
    event: String,
    payload: serde_json::Value,
}

struct TurnRuntime {
    turn: ActiveTurn,
    state: Mutex<TurnRuntimeState>,
    signal: watch::Sender<u64>,
}

struct TurnRuntimeState {
    turn: ActiveTurn,
    events: VecDeque<TurnEventEnvelope>,
    next_seq: u64,
    completed: bool,
    snapshot: PlainTurnSnapshot,
}

#[derive(Clone)]
enum ScopeTarget {
    Global,
    Project(PathBuf),
}

impl ScopeTarget {
    fn key(&self) -> String {
        match self {
            ScopeTarget::Global => GLOBAL_PROJECT_ID.into(),
            ScopeTarget::Project(path) => path.display().to_string(),
        }
    }

    fn current_dir<'a>(&'a self, state: &'a WebState) -> &'a Path {
        match self {
            ScopeTarget::Global => &state.global_home,
            ScopeTarget::Project(path) => path.as_path(),
        }
    }
}

enum ReplayWindow {
    Ready {
        events: Vec<TurnEventEnvelope>,
        completed: bool,
    },
    Reset {
        next_seq: u64,
    },
}

impl TurnRuntime {
    fn new(turn: ActiveTurn, prompt: String) -> Arc<Self> {
        let initial = TurnEventEnvelope {
            seq: 1,
            event: "turn_start".into(),
            payload: json!({ "turn": turn.clone() }),
        };
        let mut snapshot = PlainTurnSnapshot {
            prompt,
            ..PlainTurnSnapshot::default()
        };
        snapshot
            .raw_events
            .push(event_json(initial.seq, &initial.event, &initial.payload));
        let (signal, _) = watch::channel(initial.seq);
        Arc::new(Self {
            turn: turn.clone(),
            state: Mutex::new(TurnRuntimeState {
                turn,
                events: VecDeque::from([initial]),
                next_seq: 1,
                completed: false,
                snapshot,
            }),
            signal,
        })
    }

    async fn push_event(&self, event: impl Into<String>, payload: serde_json::Value) -> u64 {
        let seq = {
            let mut state = self.state.lock().await;
            state.next_seq += 1;
            let seq = state.next_seq;
            let event = event.into();
            apply_snapshot_event(&mut state.snapshot, seq, &event, &payload);
            state.events.push_back(TurnEventEnvelope {
                seq,
                event,
                payload,
            });
            while state.events.len() > MAX_TURN_EVENT_BUFFER {
                state.events.pop_front();
            }
            seq
        };
        let _ = self.signal.send(seq);
        seq
    }

    async fn mark_completed(&self) {
        self.state.lock().await.completed = true;
    }

    async fn is_completed(&self) -> bool {
        self.state.lock().await.completed
    }

    async fn current_view(&self) -> ActiveTurnView {
        let state = self.state.lock().await;
        ActiveTurnView {
            turn: state.turn.clone(),
            last_seq: state.next_seq,
            completed: state.completed,
            snapshot: state.snapshot.clone(),
        }
    }

    async fn replay_after(&self, after: u64) -> ReplayWindow {
        let state = self.state.lock().await;
        if let Some(first) = state.events.front()
            && first.seq > after.saturating_add(1)
        {
            return ReplayWindow::Reset {
                next_seq: first.seq,
            };
        }
        ReplayWindow::Ready {
            events: state
                .events
                .iter()
                .filter(|event| event.seq > after)
                .cloned()
                .collect(),
            completed: state.completed,
        }
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Deserialize)]
struct OpenProjectRequest {
    path: String,
    #[serde(default)]
    create: bool,
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    project: String,
}

#[derive(Deserialize)]
struct TurnRequest {
    project: String,
    #[serde(default)]
    session_id: Option<String>,
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    images: Vec<ImageUpload>,
}

#[derive(Deserialize)]
struct ImageUpload {
    name: String,
    data_url: String,
}

async fn handle_connection<S>(mut stream: S, state: Arc<WebState>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            write_json_response(
                &mut stream,
                400,
                "Bad Request",
                &json!({ "error": error.to_string() }),
            )
            .await?;
            return Ok(());
        }
    };

    let result = route_request(&mut stream, state, request).await;
    if let Err(error) = result {
        if is_client_disconnect(&error) {
            return Ok(());
        }
        write_json_response(
            &mut stream,
            500,
            "Internal Server Error",
            &json!({ "error": error.to_string() }),
        )
        .await?;
    }
    Ok(())
}

fn is_client_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}

async fn route_request(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
) -> Result<()> {
    if request.method == "GET"
        && let Some(asset) = static_asset(request.path.as_str())
    {
        return write_response(
            stream,
            200,
            "OK",
            &[("content-type", asset.content_type)],
            asset.body.as_bytes(),
        )
        .await;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => {
            write_response(
                stream,
                200,
                "OK",
                &[("content-type", "text/html; charset=utf-8")],
                INDEX_HTML.as_bytes(),
            )
            .await
        }
        ("GET", "/api/bootstrap") => {
            let recent = state.recent_projects.lock().await.clone();
            write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "launch_cwd": state.launch_cwd.display().to_string(),
                    "global_home": state.global_home.display().to_string(),
                    "launch_project": state.launch_project,
                    "recent_projects": recent,
                }),
            )
            .await
        }
        ("GET", "/api/project/inspect") => {
            let path = request
                .query
                .get("path")
                .ok_or_else(|| anyhow::anyhow!("missing path"))?;
            let info = inspect_project(path)?;
            write_json_response(stream, 200, "OK", &info).await
        }
        ("POST", "/api/projects/open") => open_project(stream, state, request).await,
        ("GET", "/api/sessions") => {
            let scope = scope_query(&request)?;
            let value = run_json_command(
                &state,
                Some(&scope),
                &["session", "list", "--all-origins", "--json"],
            )
            .await?;
            write_json_response(stream, 200, "OK", &value).await
        }
        ("POST", "/api/sessions") => create_session(stream, state, request).await,
        ("GET", "/api/status") => {
            let scope = scope_query(&request)?;
            let mut args = vec!["status", "--json"];
            if query_flag(&request.query, "include_models") {
                args.push("--include-models");
            }
            if let Some(session) = request.query.get("session") {
                args.push("--session");
                args.push(session);
            }
            let value = run_json_command(&state, Some(&scope), &args).await?;
            write_json_response(stream, 200, "OK", &value).await
        }
        ("GET", "/api/turns/active") => active_turn(stream, state, request).await,
        ("POST", "/api/turns") => create_turn(stream, state, request).await,
        _ => {
            if request.method == "GET"
                && request.path.starts_with("/api/sessions/")
                && request.path.ends_with("/messages")
            {
                let scope = scope_query(&request)?;
                let session = request
                    .path
                    .trim_start_matches("/api/sessions/")
                    .trim_end_matches("/messages")
                    .trim_matches('/');
                let value = run_json_command(
                    &state,
                    Some(&scope),
                    &["session", "transcript", "--session", session, "--json"],
                )
                .await?;
                return write_json_response(stream, 200, "OK", &value).await;
            }
            if request.method == "GET"
                && request.path.starts_with("/api/turns/")
                && request.path.ends_with("/events")
            {
                let id = request
                    .path
                    .trim_start_matches("/api/turns/")
                    .trim_end_matches("/events")
                    .trim_matches('/')
                    .to_string();
                return stream_turn_events(stream, state, request, id).await;
            }
            if request.method == "POST"
                && request.path.starts_with("/api/turns/")
                && request.path.ends_with("/abort")
            {
                let id = request
                    .path
                    .trim_start_matches("/api/turns/")
                    .trim_end_matches("/abort")
                    .trim_matches('/');
                return abort_turn(stream, state, id).await;
            }
            write_json_response(stream, 404, "Not Found", &json!({ "error": "not found" })).await
        }
    }
}

async fn open_project(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
) -> Result<()> {
    let input: OpenProjectRequest = parse_json_body(&request)?;
    let path = resolve_existing_dir(Path::new(&input.path))?;
    let marker = marker_at(&path);
    if marker.is_none() && !input.create {
        return write_json_response(
            stream,
            409,
            "Conflict",
            &json!({
                "error": "project confirmation required",
                "needs_confirmation": true,
                "path": path.display().to_string(),
            }),
        )
        .await;
    }
    if marker.is_none() {
        let value = run_json_command(
            &state,
            None,
            &[
                "project",
                "init",
                "--path",
                path.to_str().unwrap_or_default(),
                "--force",
                "--json",
            ],
        )
        .await?;
        let _ = value;
    }
    let summary = project_summary_from_root(&path)?;
    remember_project(&state, summary.clone()).await;
    write_json_response(stream, 200, "OK", &summary).await
}

async fn create_session(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
) -> Result<()> {
    let input: CreateSessionRequest = parse_json_body(&request)?;
    let scope = parse_scope_target(&input.project)?;
    let value = create_web_session(&state, &scope).await?;
    write_json_response(stream, 200, "OK", &value).await
}

async fn create_turn(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
) -> Result<()> {
    let mut input: TurnRequest = parse_json_body(&request)?;
    if input.prompt.trim().is_empty() {
        return write_json_response(
            stream,
            400,
            "Bad Request",
            &json!({ "error": "empty prompt" }),
        )
        .await;
    }
    let scope = parse_scope_target(&input.project)?;
    if let Some(session_id) = input.session_id.as_deref() {
        if active_session(&state, &scope, session_id).await {
            return write_json_response(
                stream,
                409,
                "Conflict",
                &json!({ "error": "session busy" }),
            )
            .await;
        }
    } else {
        let session = create_web_session(&state, &scope).await?;
        let session_id = session
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("session creation response missing id"))?;
        input.session_id = Some(session_id.to_string());
    }
    if let Some(session_id) = input.session_id.as_deref()
        && active_session(&state, &scope, session_id).await
    {
        return write_json_response(stream, 409, "Conflict", &json!({ "error": "session busy" }))
            .await;
    }
    let turn = launch_turn(state, input).await?;
    write_json_response(stream, 200, "OK", &TurnAcceptedResponse { turn }).await
}

async fn launch_turn(state: Arc<WebState>, input: TurnRequest) -> Result<ActiveTurn> {
    let scope = parse_scope_target(&input.project)?;
    let turn_id = uuid::Uuid::new_v4().to_string();
    let upload_dir = state.upload_root.join(&turn_id);
    let image_paths = save_uploads(&upload_dir, &input.images)?;
    let session_id = input
        .session_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("missing session id for turn launch"))?;

    let mut command = Command::new(&state.exe);
    command
        .arg("--origin")
        .arg("web")
        .arg("--output")
        .arg("json")
        .current_dir(scope.current_dir(&state))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.arg("--session").arg(&session_id);
    if let Some(model) = input.model.as_deref().filter(|value| !value.is_empty()) {
        command.arg("--model").arg(model);
    }
    for image in &image_paths {
        command.arg("--image").arg(image);
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().context("spawning mu turn")?;
    let pid = child.id().unwrap_or_default() as i32;
    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(input.prompt.as_bytes()).await
    {
        let _ = child.start_kill();
        let _ = std::fs::remove_dir_all(&upload_dir);
        return Err(error).context("writing prompt to mu turn stdin");
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing child stderr"))?;
    let turn = ActiveTurn {
        id: turn_id.clone(),
        project: scope.key(),
        session_id: Some(session_id),
        started_at: chrono::Utc::now().to_rfc3339(),
        pgid: pid,
    };
    let runtime = TurnRuntime::new(turn.clone(), input.prompt.clone());
    state.turns.lock().await.insert(turn_id, runtime.clone());
    tokio::spawn(run_turn_task(runtime, child, stdout, stderr, upload_dir));
    Ok(turn)
}

async fn run_turn_task(
    runtime: Arc<TurnRuntime>,
    mut child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    upload_dir: PathBuf,
) {
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text).await;
        text
    });

    let mut stream_error = None;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match parse_child_turn_event(&line) {
                Ok((event, payload)) => {
                    runtime.push_event(event, payload).await;
                }
                Err(error) => {
                    stream_error = Some(error);
                    break;
                }
            },
            Ok(None) => break,
            Err(error) => {
                stream_error = Some(error.into());
                break;
            }
        }
    }

    if let Some(error) = stream_error {
        runtime
            .push_event("error", json!({ "message": error.to_string() }))
            .await;
    }

    match child.wait().await {
        Ok(status) => {
            let stderr_text = stderr_task.await.unwrap_or_default();
            if !stderr_text.trim().is_empty() {
                runtime
                    .push_event(
                        "stderr",
                        json!({ "text": stderr_text.trim_end_matches('\n') }),
                    )
                    .await;
            }
            runtime
                .push_event(
                    "turn_finish",
                    json!({ "exit_code": status.code().unwrap_or(1) }),
                )
                .await;
        }
        Err(error) => {
            runtime
                .push_event("error", json!({ "message": error.to_string() }))
                .await;
            runtime
                .push_event("turn_finish", json!({ "exit_code": 1 }))
                .await;
        }
    }

    runtime.mark_completed().await;
    let _ = std::fs::remove_dir_all(upload_dir);
}

fn parse_child_turn_event(line: &str) -> Result<(String, serde_json::Value)> {
    let event: serde_json::Value =
        serde_json::from_str(line).context("parsing child JSON event")?;
    let event_name = event
        .get("event")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("child JSON event missing event name"))?;
    let payload = event.get("payload").cloned().unwrap_or_else(|| json!({}));
    Ok((event_name.to_string(), payload))
}

async fn active_turn(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
) -> Result<()> {
    let scope = scope_query(&request)?;
    let session_id = request
        .query
        .get("session")
        .ok_or_else(|| anyhow::anyhow!("missing session"))?;
    let runtime = find_active_turn(&state, &scope, session_id).await;
    let view = match runtime {
        Some(runtime) => Some(runtime.current_view().await),
        None => None,
    };
    write_json_response(stream, 200, "OK", &view).await
}

async fn stream_turn_events(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
    id: String,
) -> Result<()> {
    let after = request
        .query
        .get("after")
        .map(|value| value.parse::<u64>().context("invalid after"))
        .transpose()?
        .unwrap_or(0);
    let runtime = state.turns.lock().await.get(&id).cloned();
    let Some(runtime) = runtime else {
        return write_json_response(
            stream,
            404,
            "Not Found",
            &json!({ "error": "turn not found" }),
        )
        .await;
    };

    write_sse_headers(stream).await?;
    let mut last_seq = after;
    let mut changes = runtime.signal.subscribe();
    loop {
        match runtime.replay_after(last_seq).await {
            ReplayWindow::Ready { events, completed } => {
                for event in events {
                    write_sse_event(stream, Some(event.seq), &event.event, &event.payload).await?;
                    last_seq = event.seq;
                }
                if completed {
                    return Ok(());
                }
            }
            ReplayWindow::Reset { next_seq } => {
                write_sse_event(
                    stream,
                    None,
                    "reset",
                    &json!({ "reason": "replay_missed", "next_seq": next_seq }),
                )
                .await?;
                return Ok(());
            }
        }

        if changes.changed().await.is_err() {
            return Ok(());
        }
    }
}

async fn abort_turn(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    id: &str,
) -> Result<()> {
    let runtime = state.turns.lock().await.get(id).cloned();
    let Some(runtime) = runtime else {
        return write_json_response(
            stream,
            404,
            "Not Found",
            &json!({ "error": "turn not found" }),
        )
        .await;
    };
    if runtime.is_completed().await {
        return write_json_response(
            stream,
            404,
            "Not Found",
            &json!({ "error": "turn not found" }),
        )
        .await;
    }
    let result = unsafe { libc::kill(-runtime.turn.pgid, libc::SIGTERM) };
    if result == -1 {
        return write_json_response(
            stream,
            500,
            "Internal Server Error",
            &json!({ "error": std::io::Error::last_os_error().to_string() }),
        )
        .await;
    }
    let runtime_for_kill = runtime.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        if runtime_for_kill.is_completed().await {
            return;
        }
        let _ = unsafe { libc::kill(-runtime_for_kill.turn.pgid, libc::SIGKILL) };
    });
    write_json_response(stream, 200, "OK", &json!({ "ok": true })).await
}

async fn active_session(state: &WebState, scope: &ScopeTarget, session_id: &str) -> bool {
    find_active_turn(state, scope, session_id).await.is_some()
}

async fn remember_project(state: &WebState, summary: ProjectSummary) {
    let mut recent = state.recent_projects.lock().await;
    recent.retain(|project| project.path != summary.path);
    recent.insert(0, summary);
    recent.truncate(20);
}

async fn run_json_command(
    state: &WebState,
    scope: Option<&ScopeTarget>,
    args: &[&str],
) -> Result<serde_json::Value> {
    let mut command = Command::new(&state.exe);
    if let Some(scope) = scope {
        command.current_dir(scope.current_dir(state));
    }
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().await.context("running mu command")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!("{}", if stderr.is_empty() { stdout } else { stderr });
    }
    serde_json::from_slice(&output.stdout).context("parsing mu JSON output")
}

async fn create_web_session(state: &WebState, scope: &ScopeTarget) -> Result<serde_json::Value> {
    run_json_command(
        state,
        Some(scope),
        &["session", "new", "--origin", "web", "--json"],
    )
    .await
}

async fn find_active_turn(
    state: &WebState,
    scope: &ScopeTarget,
    session_id: &str,
) -> Option<Arc<TurnRuntime>> {
    let project = scope.key();
    let turns = state
        .turns
        .lock()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for turn in turns {
        if turn.turn.project != project {
            continue;
        }
        if turn.turn.session_id.as_deref() != Some(session_id) {
            continue;
        }
        if !turn.is_completed().await {
            return Some(turn);
        }
    }
    None
}

fn event_json(seq: u64, event: &str, payload: &serde_json::Value) -> serde_json::Value {
    json!({
        "seq": seq,
        "event": event,
        "payload": payload,
    })
}

fn apply_snapshot_event(
    snapshot: &mut PlainTurnSnapshot,
    seq: u64,
    event: &str,
    payload: &serde_json::Value,
) {
    snapshot.raw_events.push(event_json(seq, event, payload));
    match event {
        "assistant_delta" => {
            if let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) {
                snapshot.assistant_text.push_str(text);
            }
        }
        "stderr" => {
            if let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) {
                if !snapshot.stderr.is_empty() {
                    snapshot.stderr.push('\n');
                }
                snapshot.stderr.push_str(text);
            }
        }
        "turn_finish" => {
            snapshot.exit_code = payload
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .map(|value| value as i32);
        }
        _ => {}
    }
}

fn parse_json_body<T: for<'de> Deserialize<'de>>(request: &HttpRequest) -> Result<T> {
    serde_json::from_slice(&request.body).context("parsing request JSON")
}

fn scope_query(request: &HttpRequest) -> Result<ScopeTarget> {
    let project = request
        .query
        .get("project")
        .ok_or_else(|| anyhow::anyhow!("missing project"))?;
    parse_scope_target(project)
}

fn parse_scope_target(value: &str) -> Result<ScopeTarget> {
    if value == GLOBAL_PROJECT_ID {
        return Ok(ScopeTarget::Global);
    }
    Ok(ScopeTarget::Project(require_project(value)?))
}

fn require_project(path: &str) -> Result<PathBuf> {
    let path = resolve_existing_dir(Path::new(path))?;
    if marker_at(&path).is_none() {
        bail!("not a project: {}", path.display());
    }
    Ok(path)
}

fn inspect_project(path: &str) -> Result<serde_json::Value> {
    let path = resolve_existing_dir(Path::new(path))?;
    let marker = marker_at(&path);
    Ok(json!({
        "path": path.display().to_string(),
        "is_project": marker.is_some(),
        "marker": marker,
        "needs_confirmation": marker.is_none(),
    }))
}

fn project_summary(project: paths::Project) -> ProjectSummary {
    ProjectSummary {
        path: project.root.display().to_string(),
        marker: marker_name(project.marker),
    }
}

fn project_summary_from_root(root: &Path) -> Result<ProjectSummary> {
    let marker =
        marker_at(root).ok_or_else(|| anyhow::anyhow!("not a project: {}", root.display()))?;
    Ok(ProjectSummary {
        path: root.display().to_string(),
        marker,
    })
}

fn marker_at(path: &Path) -> Option<&'static str> {
    if path.join(".mu").is_dir() {
        Some("mu")
    } else if path.join(".git").exists() {
        Some("git")
    } else {
        None
    }
}

fn marker_name(marker: paths::ProjectMarker) -> &'static str {
    match marker {
        paths::ProjectMarker::Mu => "mu",
        paths::ProjectMarker::Git => "git",
    }
}

fn global_scope_cwd() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn resolve_existing_dir(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("resolving directory {}", path.display()))?;
    if !path.is_dir() {
        bail!("not a directory: {}", path.display());
    }
    Ok(path)
}

fn save_uploads(root: &Path, images: &[ImageUpload]) -> Result<Vec<PathBuf>> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    paths::ensure_dir(root)?;
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let (mime, encoded) = image
                .data_url
                .split_once(";base64,")
                .ok_or_else(|| anyhow::anyhow!("invalid data URL for {}", image.name))?;
            let mime = mime.trim_start_matches("data:");
            let ext = match mime {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => bail!("unsupported image attachment type: {mime}"),
            };
            let path = root.join(format!("{index}.{ext}"));
            std::fs::write(&path, base64_decode(encoded)?)
                .with_context(|| format!("writing upload {}", path.display()))?;
            Ok(path)
        })
        .collect()
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut chunk = [0u8; 4];
    let mut chunk_len = 0usize;
    for byte in input.bytes().filter(|b| !b.is_ascii_whitespace()) {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => 64,
            _ => bail!("invalid base64 byte"),
        };
        chunk[chunk_len] = value;
        chunk_len += 1;
        if chunk_len == 4 {
            push_base64_chunk(&mut out, chunk)?;
            chunk_len = 0;
        }
    }
    if chunk_len != 0 {
        bail!("invalid base64 length");
    }
    Ok(out)
}

fn push_base64_chunk(out: &mut Vec<u8>, chunk: [u8; 4]) -> Result<()> {
    if chunk[0] == 64 || chunk[1] == 64 {
        bail!("invalid base64 padding");
    }
    out.push((chunk[0] << 2) | (chunk[1] >> 4));
    if chunk[2] != 64 {
        out.push((chunk[1] << 4) | (chunk[2] >> 2));
    }
    if chunk[3] != 64 {
        out.push((chunk[2] << 6) | chunk[3]);
    }
    Ok(())
}
