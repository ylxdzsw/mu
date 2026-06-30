use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
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
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(input.prompt.as_bytes()).await {
            let _ = child.start_kill();
            let _ = std::fs::remove_dir_all(&upload_dir);
            return Err(error).context("writing prompt to mu turn stdin");
        }
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

fn parse_socket_mode(value: &str) -> Result<u32> {
    u32::from_str_radix(value.trim_start_matches('0'), 8)
        .with_context(|| format!("invalid socket mode `{value}`"))
}

fn prepare_socket_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path)
            .with_context(|| format!("removing stale {}", path.display()))?,
        Ok(_) => bail!("refusing to replace non-socket path: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("checking {}", path.display())),
    }
    Ok(())
}

async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let mut temp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut temp).await?;
        if n == 0 {
            bail!("connection closed before request headers");
        }
        buffer.extend_from_slice(&temp[..n]);
        if buffer.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
        if let Some(pos) = find_header_end(&buffer) {
            break pos;
        }
    };

    let (method, uri, content_length) = {
        let header =
            std::str::from_utf8(&buffer[..header_end]).context("request header is not UTF-8")?;
        let mut lines = header.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing request line"))?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let uri = parts.next().unwrap_or_default().to_string();
        let mut content_length = 0usize;
        for line in lines {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().context("invalid content-length")?;
            }
        }
        (method, uri, content_length)
    };

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let n = stream.read(&mut temp).await?;
        if n == 0 {
            bail!("connection closed before request body");
        }
        buffer.extend_from_slice(&temp[..n]);
        if buffer.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
    }
    let (path, query) = parse_uri(&uri);
    Ok(HttpRequest {
        method,
        path,
        query,
        body: buffer[body_start..body_start + content_length].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_uri(uri: &str) -> (String, HashMap<String, String>) {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let mut map = HashMap::new();
    for pair in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(key), percent_decode(value));
    }
    (percent_decode(path), map)
}

fn query_flag(query: &HashMap<String, String>, key: &str) -> bool {
    query.get(key).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((a << 4) | b);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

async fn write_json_response<T: Serialize + ?Sized>(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    reason: &str,
    value: &T,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write_response(
        stream,
        status,
        reason,
        &[("content-type", "application/json; charset=utf-8")],
        &body,
    )
    .await
}

async fn write_response(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("content-security-policy: default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'\r\n");
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_sse_headers(stream: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\nconnection: close\r\ncache-control: no-cache, no-transform\r\nx-accel-buffering: no\r\nx-content-type-options: nosniff\r\n\r\n",
        )
        .await?;
    stream.flush().await?;
    Ok(())
}

async fn write_sse_event<T: Serialize + ?Sized>(
    stream: &mut (impl AsyncWrite + Unpin),
    id: Option<u64>,
    event: &str,
    payload: &T,
) -> Result<()> {
    let data = serde_json::to_string(payload)?;
    let mut frame = String::new();
    if let Some(id) = id {
        frame.push_str("id: ");
        frame.push_str(&id.to_string());
        frame.push('\n');
    }
    frame.push_str("event: ");
    frame.push_str(event);
    frame.push('\n');
    frame.push_str("data: ");
    frame.push_str(&data);
    frame.push_str("\n\n");
    stream.write_all(frame.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_CSS: &str = include_str!("web/app.css");
const APP_JS: &str = include_str!("web/app.js");
const LIB_API_JS: &str = include_str!("web/lib/api.js");
const LIB_CONSTANTS_JS: &str = include_str!("web/lib/constants.js");
const LIB_DOM_JS: &str = include_str!("web/lib/dom.js");
const LIB_ICONS_JS: &str = include_str!("web/lib/icons.js");
const LIB_PROJECTS_JS: &str = include_str!("web/lib/projects.js");
const LIB_STORE_JS: &str = include_str!("web/lib/store.js");
const COMPONENT_COMPOSER_JS: &str = include_str!("web/components/mu-composer.js");
const COMPONENT_CONVERSATION_JS: &str = include_str!("web/components/mu-conversation-view.js");
const COMPONENT_MODAL_JS: &str = include_str!("web/components/mu-project-modal.js");
const COMPONENT_SIDEBAR_JS: &str = include_str!("web/components/mu-sidebar.js");
const STYLE_BASE_CSS: &str = include_str!("web/styles/base.css");
const STYLE_COMPOSER_CSS: &str = include_str!("web/styles/composer.css");
const STYLE_CONVERSATION_CSS: &str = include_str!("web/styles/conversation.css");
const STYLE_LAYOUT_CSS: &str = include_str!("web/styles/layout.css");
const STYLE_MODAL_CSS: &str = include_str!("web/styles/modal.css");
const STYLE_SIDEBAR_CSS: &str = include_str!("web/styles/sidebar.css");
const STYLE_TOKENS_CSS: &str = include_str!("web/styles/tokens.css");

struct StaticAsset {
    path: &'static str,
    content_type: &'static str,
    body: &'static str,
}

const STATIC_ASSETS: &[StaticAsset] = &[
    StaticAsset {
        path: "/app.css",
        content_type: "text/css; charset=utf-8",
        body: APP_CSS,
    },
    StaticAsset {
        path: "/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: APP_JS,
    },
    StaticAsset {
        path: "/lib/api.js",
        content_type: "text/javascript; charset=utf-8",
        body: LIB_API_JS,
    },
    StaticAsset {
        path: "/lib/constants.js",
        content_type: "text/javascript; charset=utf-8",
        body: LIB_CONSTANTS_JS,
    },
    StaticAsset {
        path: "/lib/dom.js",
        content_type: "text/javascript; charset=utf-8",
        body: LIB_DOM_JS,
    },
    StaticAsset {
        path: "/lib/icons.js",
        content_type: "text/javascript; charset=utf-8",
        body: LIB_ICONS_JS,
    },
    StaticAsset {
        path: "/lib/projects.js",
        content_type: "text/javascript; charset=utf-8",
        body: LIB_PROJECTS_JS,
    },
    StaticAsset {
        path: "/lib/store.js",
        content_type: "text/javascript; charset=utf-8",
        body: LIB_STORE_JS,
    },
    StaticAsset {
        path: "/components/mu-composer.js",
        content_type: "text/javascript; charset=utf-8",
        body: COMPONENT_COMPOSER_JS,
    },
    StaticAsset {
        path: "/components/mu-conversation-view.js",
        content_type: "text/javascript; charset=utf-8",
        body: COMPONENT_CONVERSATION_JS,
    },
    StaticAsset {
        path: "/components/mu-project-modal.js",
        content_type: "text/javascript; charset=utf-8",
        body: COMPONENT_MODAL_JS,
    },
    StaticAsset {
        path: "/components/mu-sidebar.js",
        content_type: "text/javascript; charset=utf-8",
        body: COMPONENT_SIDEBAR_JS,
    },
    StaticAsset {
        path: "/styles/base.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_BASE_CSS,
    },
    StaticAsset {
        path: "/styles/composer.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_COMPOSER_CSS,
    },
    StaticAsset {
        path: "/styles/conversation.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_CONVERSATION_CSS,
    },
    StaticAsset {
        path: "/styles/layout.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_LAYOUT_CSS,
    },
    StaticAsset {
        path: "/styles/modal.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_MODAL_CSS,
    },
    StaticAsset {
        path: "/styles/sidebar.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_SIDEBAR_CSS,
    },
    StaticAsset {
        path: "/styles/tokens.css",
        content_type: "text/css; charset=utf-8",
        body: STYLE_TOKENS_CSS,
    },
];

fn static_asset(path: &str) -> Option<&'static StaticAsset> {
    STATIC_ASSETS.iter().find(|asset| asset.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

        let response =
            http_roundtrip(state, "GET /api/bootstrap HTTP/1.1\r\nhost: mu\r\n\r\n").await;
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
        let css =
            http_roundtrip(state, "GET /styles/layout.css HTTP/1.1\r\nhost: mu\r\n\r\n").await;

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
        assert!(INDEX_HTML.contains(r#"mu-conversation-view"#));
        assert!(INDEX_HTML.contains(r#"aria-label="Left panel""#));
        assert!(APP_CSS.contains("@import url(\"/styles/layout.css\")"));
        assert!(APP_JS.contains("components/mu-sidebar.js"));
        assert!(INDEX_HTML.contains(r#"type="module" src="/app.js""#));
        assert!(APP_JS.contains("/api/bootstrap"));
        assert!(INDEX_HTML.contains(r#"id="project-modal""#));
        assert!(LIB_CONSTANTS_JS.contains(GLOBAL_PROJECT_ID));
        assert!(COMPONENT_SIDEBAR_JS.contains("customElements.define(\"mu-sidebar\""));
    }
}
