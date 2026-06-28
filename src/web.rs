use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::cli::WebArgs;
use crate::paths;

const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

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

    let state = Arc::new(WebState {
        exe: std::env::current_exe().context("resolving current executable")?,
        launch_cwd,
        launch_project,
        recent_projects: Mutex::new(recent_projects),
        active_turns: Mutex::new(HashMap::new()),
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
    recent_projects: Mutex<Vec<ProjectSummary>>,
    active_turns: Mutex<HashMap<String, ActiveTurn>>,
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
    effort: Option<String>,
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

async fn route_request(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
) -> Result<()> {
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
        ("GET", "/app.css") => {
            write_response(
                stream,
                200,
                "OK",
                &[("content-type", "text/css; charset=utf-8")],
                APP_CSS.as_bytes(),
            )
            .await
        }
        ("GET", "/app.js") => {
            write_response(
                stream,
                200,
                "OK",
                &[("content-type", "text/javascript; charset=utf-8")],
                APP_JS.as_bytes(),
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
            let project = project_query(&request)?;
            let value = run_json_command(
                &state,
                Some(&project),
                &["session", "list", "--all-origins", "--json"],
            )
            .await?;
            write_json_response(stream, 200, "OK", &value).await
        }
        ("POST", "/api/sessions") => create_session(stream, state, request).await,
        ("GET", "/api/status") => {
            let project = project_query(&request)?;
            let mut args = vec!["status", "--json"];
            if let Some(session) = request.query.get("session") {
                args.push("--session");
                args.push(session);
            }
            let value = run_json_command(&state, Some(&project), &args).await?;
            write_json_response(stream, 200, "OK", &value).await
        }
        ("GET", "/api/models") => {
            let project = project_query(&request)?;
            let value =
                run_json_command(&state, Some(&project), &["models", "list", "--json"]).await?;
            write_json_response(stream, 200, "OK", &value).await
        }
        ("POST", "/api/turns") => stream_turn(stream, state, request).await,
        _ => {
            if request.method == "GET"
                && request.path.starts_with("/api/sessions/")
                && request.path.ends_with("/messages")
            {
                let project = project_query(&request)?;
                let session = request
                    .path
                    .trim_start_matches("/api/sessions/")
                    .trim_end_matches("/messages")
                    .trim_matches('/');
                let value = run_json_command(
                    &state,
                    Some(&project),
                    &["session", "transcript", "--session", session, "--json"],
                )
                .await?;
                return write_json_response(stream, 200, "OK", &value).await;
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
    let project = require_project(&input.project)?;
    let value = run_json_command(
        &state,
        Some(&project),
        &["session", "new", "--origin", "web", "--json"],
    )
    .await?;
    write_json_response(stream, 200, "OK", &value).await
}

async fn stream_turn(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    request: HttpRequest,
) -> Result<()> {
    let input: TurnRequest = parse_json_body(&request)?;
    if input.prompt.trim().is_empty() {
        return write_json_response(
            stream,
            400,
            "Bad Request",
            &json!({ "error": "empty prompt" }),
        )
        .await;
    }
    let project = require_project(&input.project)?;
    if let Some(session_id) = input.session_id.as_deref()
        && active_session(&state, &project, session_id).await
    {
        return write_json_response(stream, 409, "Conflict", &json!({ "error": "session busy" }))
            .await;
    }

    let turn_id = uuid::Uuid::new_v4().to_string();
    let upload_dir = state.upload_root.join(&turn_id);
    let image_paths = save_uploads(&upload_dir, &input.images)?;

    let mut command = Command::new(&state.exe);
    command
        .arg("--origin")
        .arg("web")
        .arg("--output")
        .arg("json")
        .current_dir(&project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(session_id) = input.session_id.as_deref() {
        command.arg("--session").arg(session_id);
    }
    if let Some(model) = input.model.as_deref().filter(|value| !value.is_empty()) {
        command.arg("--model").arg(model);
    }
    if let Some(effort) = input.effort.as_deref().filter(|value| !value.is_empty()) {
        command.arg("--effort").arg(effort);
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
    let active = ActiveTurn {
        id: turn_id.clone(),
        project: project.display().to_string(),
        session_id: input.session_id.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
        pgid: pid,
    };
    state
        .active_turns
        .lock()
        .await
        .insert(turn_id.clone(), active.clone());

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.prompt.as_bytes()).await?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing child stderr"))?;
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text).await;
        text
    });

    write_stream_headers(stream).await?;
    write_json_line(stream, "turn_start", &json!({ "turn": active })).await?;

    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
    }

    let status = child.wait().await?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !stderr_text.trim().is_empty() {
        write_json_line(
            stream,
            "stderr",
            &json!({ "text": stderr_text.trim_end_matches('\n') }),
        )
        .await?;
    }
    write_json_line(
        stream,
        "turn_finish",
        &json!({ "exit_code": status.code().unwrap_or(1) }),
    )
    .await?;
    state.active_turns.lock().await.remove(&turn_id);
    let _ = std::fs::remove_dir_all(upload_dir);
    Ok(())
}

async fn abort_turn(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    state: Arc<WebState>,
    id: &str,
) -> Result<()> {
    let active = state.active_turns.lock().await.get(id).cloned();
    let Some(active) = active else {
        return write_json_response(
            stream,
            404,
            "Not Found",
            &json!({ "error": "turn not found" }),
        )
        .await;
    };
    let result = unsafe { libc::kill(-active.pgid, libc::SIGTERM) };
    if result == -1 {
        return write_json_response(
            stream,
            500,
            "Internal Server Error",
            &json!({ "error": std::io::Error::last_os_error().to_string() }),
        )
        .await;
    }
    write_json_response(stream, 200, "OK", &json!({ "ok": true })).await
}

async fn active_session(state: &WebState, project: &Path, session_id: &str) -> bool {
    let project = project.display().to_string();
    state
        .active_turns
        .lock()
        .await
        .values()
        .any(|turn| turn.project == project && turn.session_id.as_deref() == Some(session_id))
}

async fn remember_project(state: &WebState, summary: ProjectSummary) {
    let mut recent = state.recent_projects.lock().await;
    recent.retain(|project| project.path != summary.path);
    recent.insert(0, summary);
    recent.truncate(20);
}

async fn run_json_command(
    state: &WebState,
    project: Option<&Path>,
    args: &[&str],
) -> Result<serde_json::Value> {
    let mut command = Command::new(&state.exe);
    if let Some(project) = project {
        command.current_dir(project);
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

fn parse_json_body<T: for<'de> Deserialize<'de>>(request: &HttpRequest) -> Result<T> {
    serde_json::from_slice(&request.body).context("parsing request JSON")
}

fn project_query(request: &HttpRequest) -> Result<PathBuf> {
    let project = request
        .query
        .get("project")
        .ok_or_else(|| anyhow::anyhow!("missing project"))?;
    require_project(project)
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

async fn write_stream_headers(stream: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson; charset=utf-8\r\nconnection: close\r\ncache-control: no-store\r\nx-accel-buffering: no\r\nx-content-type-options: nosniff\r\n\r\n",
        )
        .await?;
    stream.flush().await?;
    Ok(())
}

async fn write_json_line<T: Serialize + ?Sized>(
    stream: &mut (impl AsyncWrite + Unpin),
    event: &str,
    payload: &T,
) -> Result<()> {
    let line = json!({ "event": event, "payload": payload });
    stream
        .write_all(serde_json::to_string(&line)?.as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>mu</title>
  <link rel="stylesheet" href="/app.css">
</head>
<body>
  <div id="app" class="app">
    <nav class="rail">
      <button id="new-session" class="icon-button" title="New session">+</button>
      <button id="open-project" class="icon-button" title="Open project">P</button>
      <button id="theme-toggle" class="icon-button" title="Theme">T</button>
    </nav>
    <aside class="left-pane">
      <header class="pane-header">
        <div class="eyebrow">Project</div>
        <div id="project-name" class="pane-title">No project</div>
      </header>
      <form id="project-form" class="path-form">
        <input id="project-path" class="text-input" name="path" placeholder="/path/to/project" autocomplete="off">
        <button class="button neutral" type="submit">Open</button>
      </form>
      <section class="section">
        <div class="section-title">Recent</div>
        <div id="project-list" class="list"></div>
      </section>
      <section class="section sessions-section">
        <div class="section-title">Sessions</div>
        <div id="session-list" class="list"></div>
      </section>
    </aside>
    <main class="workspace">
      <header class="topbar">
        <div>
          <div id="session-title" class="top-title">Home</div>
          <div id="session-subtitle" class="top-subtitle"></div>
        </div>
        <div id="busy-pill" class="pill hidden">busy</div>
      </header>
      <div id="timeline" class="timeline"></div>
      <form id="composer" class="composer">
        <div id="attachments" class="attachments"></div>
        <textarea id="prompt" class="prompt" rows="3" placeholder="Message mu"></textarea>
        <div class="composer-row">
          <input id="image-input" type="file" accept="image/png,image/jpeg,image/webp,image/gif" multiple hidden>
          <button id="attach" class="button ghost" type="button">Attach</button>
          <select id="model" class="select"></select>
          <select id="effort" class="select">
            <option value="">effort</option>
            <option value="low">low</option>
            <option value="medium">medium</option>
            <option value="high">high</option>
            <option value="xhigh">xhigh</option>
            <option value="max">max</option>
          </select>
          <button id="abort" class="button neutral hidden" type="button">Abort</button>
          <button id="submit" class="button contrast" type="submit">Send</button>
        </div>
      </form>
    </main>
    <aside class="right-pane">
      <header class="pane-header">
        <div class="eyebrow">Status</div>
        <div id="status-model" class="pane-title">-</div>
      </header>
      <div id="status" class="status-grid"></div>
    </aside>
  </div>
  <dialog id="confirm-project" class="dialog">
    <form method="dialog" class="dialog-body">
      <div class="dialog-title">Create project metadata?</div>
      <div class="dialog-copy">mu will create .mu in this directory.</div>
      <div id="confirm-path" class="dialog-path"></div>
      <div class="dialog-actions">
        <button id="confirm-cancel" class="button ghost" value="cancel">Cancel</button>
        <button id="confirm-create" class="button contrast" value="create">Create</button>
      </div>
    </form>
  </dialog>
  <div id="toast" class="toast hidden"></div>
  <script src="/app.js"></script>
</body>
</html>
"#;

const APP_CSS: &str = r#":root {
  color-scheme: light;
  --bg-base: #ffffff;
  --bg-deep: #fafafa;
  --bg-layer-1: #eeeeee;
  --bg-layer-2: #d4d4d4;
  --text-base: #161616;
  --text-muted: #5c5c5c;
  --text-faint: #808080;
  --text-inverse: #ffffff;
  --border-muted: #00000014;
  --border-base: #0000001a;
  --border-strong: #00000033;
  --accent: #3b5cf6;
  --danger: #d92e3c;
  --success: #2eaf5a;
  --warning: #e7af36;
  --overlay-hover: #0000000a;
  --overlay-pressed: #00000014;
  --shadow-button: 0 1px 1.5px #0000001a, 0 0 0 .5px #00000024;
  --shadow-floating: 0 8px 16px #0000000a, 0 4px 8px #00000014, 0 0 0 .5px #0000001f;
  --font: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  --mono: "JetBrainsMono Nerd Font Mono", "SFMono-Regular", Consolas, monospace;
}

[data-theme="dark"] {
  color-scheme: dark;
  --bg-base: #161616;
  --bg-deep: #080808;
  --bg-layer-1: #242424;
  --bg-layer-2: #3a3a3a;
  --text-base: #eeeeee;
  --text-muted: #aeaeae;
  --text-faint: #808080;
  --text-inverse: #161616;
  --border-muted: #ffffff14;
  --border-base: #ffffff1a;
  --border-strong: #ffffff33;
  --overlay-hover: #ffffff0f;
  --overlay-pressed: #ffffff1a;
  --shadow-button: 0 1px 2px #00000066, 0 0 0 .5px #ffffff33;
  --shadow-floating: 0 8px 16px #0000004d, 0 4px 8px #0000004d, 0 0 0 .5px #ffffff29;
}

* {
  box-sizing: border-box;
}

html,
body,
.app {
  width: 100%;
  height: 100%;
  margin: 0;
}

body {
  font-family: var(--font);
  background: var(--bg-base);
  color: var(--text-base);
  font-size: 13px;
  line-height: 1.4;
  letter-spacing: 0;
  -webkit-font-smoothing: antialiased;
  text-rendering: geometricPrecision;
}

button,
input,
textarea,
select {
  font: inherit;
  letter-spacing: 0;
}

.app {
  display: grid;
  grid-template-columns: 64px minmax(220px, 280px) minmax(0, 1fr) minmax(260px, 320px);
  min-width: 0;
  overflow: hidden;
}

.rail {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  padding: 12px;
  border-right: 1px solid var(--border-muted);
  background: var(--bg-base);
}

.icon-button,
.button {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  height: 28px;
  min-width: 28px;
  padding: 0 11px;
  border: 0;
  border-radius: 6px;
  color: var(--text-base);
  background: transparent;
  cursor: pointer;
  font-weight: 530;
  font-size: 13px;
  line-height: 1;
}

.icon-button {
  width: 40px;
  height: 40px;
  padding: 0;
}

.button.neutral {
  background: var(--bg-base);
  box-shadow: var(--shadow-button);
}

.button.contrast {
  color: var(--text-inverse);
  background: linear-gradient(180deg, #ffffff33 0%, #ffffff00 100%), var(--text-base);
  box-shadow: var(--shadow-button);
}

.button.ghost:hover,
.icon-button:hover {
  background: var(--overlay-hover);
}

.button:active,
.icon-button:active {
  background: var(--overlay-pressed);
}

.button:disabled,
.icon-button:disabled,
.select:disabled {
  opacity: .5;
  cursor: not-allowed;
}

.left-pane,
.right-pane {
  min-width: 0;
  background: var(--bg-deep);
  border-right: 1px solid var(--border-muted);
  overflow: hidden;
  display: flex;
  flex-direction: column;
}

.right-pane {
  border-right: 0;
  border-left: 1px solid var(--border-muted);
}

.pane-header,
.topbar {
  min-height: 64px;
  padding: 14px 16px;
  border-bottom: 1px solid var(--border-muted);
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.eyebrow,
.section-title,
.top-subtitle {
  color: var(--text-faint);
  font-size: 11px;
  font-weight: 560;
}

.pane-title,
.top-title {
  color: var(--text-base);
  font-size: 15px;
  font-weight: 620;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.path-form {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: 8px;
  padding: 12px;
  border-bottom: 1px solid var(--border-muted);
}

.text-input,
.select {
  min-width: 0;
  height: 28px;
  border: 1px solid var(--border-base);
  border-radius: 6px;
  color: var(--text-base);
  background: var(--bg-base);
  padding: 0 9px;
  outline: none;
}

.text-input:focus,
.select:focus,
.prompt:focus {
  border-color: var(--accent);
}

.section {
  padding: 12px;
  min-height: 0;
}

.sessions-section {
  flex: 1;
  overflow: hidden;
  display: flex;
  flex-direction: column;
}

.list {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-height: 0;
  overflow: auto;
}

.list-item {
  width: 100%;
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: 8px;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: var(--text-base);
  padding: 8px;
  text-align: left;
  cursor: pointer;
}

.list-item:hover,
.list-item.active {
  background: var(--overlay-hover);
}

.item-title,
.item-subtitle {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.item-subtitle {
  color: var(--text-faint);
  font-size: 11px;
}

.workspace {
  min-width: 0;
  min-height: 0;
  display: grid;
  grid-template-rows: auto minmax(0, 1fr) auto;
  background: var(--bg-base);
}

.pill {
  height: 22px;
  padding: 0 8px;
  display: inline-flex;
  align-items: center;
  border-radius: 999px;
  color: var(--success);
  background: #2eaf5a1f;
  font-size: 11px;
  font-weight: 620;
}

.timeline {
  min-height: 0;
  overflow: auto;
  padding: 18px max(18px, 5vw) 24px;
}

.empty {
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--text-faint);
}

.message {
  max-width: 920px;
  margin: 0 auto 18px;
  display: grid;
  grid-template-columns: 82px minmax(0, 1fr);
  gap: 16px;
}

.role {
  color: var(--text-faint);
  font-size: 11px;
  font-weight: 620;
  text-transform: uppercase;
}

.bubble {
  min-width: 0;
  white-space: pre-wrap;
  word-break: break-word;
}

.tool {
  border: 1px solid var(--border-muted);
  border-radius: 6px;
  background: var(--bg-deep);
  overflow: hidden;
}

.tool-head {
  min-height: 32px;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 10px;
  border-bottom: 1px solid var(--border-muted);
  font-weight: 560;
}

.tool-output {
  margin: 0;
  padding: 10px;
  overflow: auto;
  font-family: var(--mono);
  font-size: 12px;
  line-height: 1.45;
}

.composer {
  margin: 0;
  padding: 12px max(18px, 5vw) 18px;
  border-top: 1px solid var(--border-muted);
  background: linear-gradient(180deg, #ffffff00, var(--bg-base) 18%);
}

.prompt {
  display: block;
  width: 100%;
  min-height: 78px;
  max-height: 220px;
  resize: vertical;
  border: 1px solid var(--border-base);
  border-radius: 6px;
  outline: none;
  padding: 10px;
  color: var(--text-base);
  background: var(--bg-base);
  box-shadow: var(--shadow-button);
}

.composer-row,
.attachments {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 8px;
  min-width: 0;
  flex-wrap: wrap;
}

.attachments:empty {
  display: none;
}

.chip {
  max-width: 220px;
  height: 24px;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 0 8px;
  border-radius: 6px;
  background: var(--bg-layer-1);
  color: var(--text-muted);
  font-size: 12px;
}

.chip button {
  border: 0;
  background: transparent;
  color: inherit;
  cursor: pointer;
}

.status-grid {
  padding: 12px 16px;
  display: grid;
  grid-template-columns: minmax(92px, auto) minmax(0, 1fr);
  gap: 8px 12px;
  overflow: auto;
}

.status-key {
  color: var(--text-faint);
  font-size: 11px;
}

.status-value {
  min-width: 0;
  overflow-wrap: anywhere;
}

.dialog {
  border: 1px solid var(--border-base);
  border-radius: 6px;
  padding: 0;
  color: var(--text-base);
  background: var(--bg-base);
  box-shadow: var(--shadow-floating);
}

.dialog::backdrop {
  background: #00000066;
}

.dialog-body {
  width: min(420px, calc(100vw - 32px));
  padding: 16px;
}

.dialog-title {
  font-weight: 620;
  font-size: 15px;
}

.dialog-copy {
  margin-top: 8px;
  color: var(--text-muted);
}

.dialog-path {
  margin-top: 8px;
  color: var(--text-muted);
  overflow-wrap: anywhere;
}

.dialog-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 16px;
}

.toast {
  position: fixed;
  right: 16px;
  bottom: 16px;
  max-width: min(420px, calc(100vw - 32px));
  padding: 10px 12px;
  border-radius: 6px;
  background: var(--bg-base);
  color: var(--text-base);
  box-shadow: var(--shadow-floating);
}

.hidden {
  display: none !important;
}

@media (max-width: 980px) {
  .app {
    grid-template-columns: 56px minmax(210px, 260px) minmax(0, 1fr);
  }

  .right-pane {
    display: none;
  }
}

@media (max-width: 720px) {
  .app {
    grid-template-columns: 1fr;
    grid-template-rows: auto auto minmax(0, 1fr);
  }

  .rail {
    grid-row: 1;
    flex-direction: row;
    height: 52px;
    border-right: 0;
    border-bottom: 1px solid var(--border-muted);
  }

  .left-pane {
    grid-row: 2;
    max-height: 260px;
    border-right: 0;
    border-bottom: 1px solid var(--border-muted);
  }

  .workspace {
    grid-row: 3;
  }

  .message {
    grid-template-columns: 1fr;
    gap: 4px;
  }
}
"#;

const APP_JS: &str = r#"const state = {
  project: null,
  sessions: [],
  session: null,
  status: null,
  models: [],
  attachments: [],
  activeTurn: null,
};

let promptHistory = JSON.parse(localStorage.getItem("mu-web-prompt-history") || "[]");
let promptHistoryIndex = null;

const el = (id) => document.getElementById(id);
const q = (selector, root = document) => root.querySelector(selector);

function toast(message) {
  const node = el("toast");
  node.textContent = message;
  node.classList.remove("hidden");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.classList.add("hidden"), 4200);
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      "content-type": "application/json",
      ...(options.headers || {}),
    },
  });
  if (!response.ok) {
    let text = await response.text();
    try {
      text = JSON.parse(text).error || text;
    } catch (_) {}
    const error = new Error(text || response.statusText);
    error.status = response.status;
    throw error;
  }
  return response.json();
}

function projectName(path) {
  if (!path) return "No project";
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] || path;
}

function sessionLabel(session) {
  return session.title || session.id.slice(0, 8);
}

function setTheme(theme) {
  document.documentElement.dataset.theme = theme;
  localStorage.setItem("mu-web-theme", theme);
}

function renderProjects(projects) {
  const list = el("project-list");
  list.replaceChildren();
  for (const project of projects) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "list-item";
    if (state.project?.path === project.path) button.classList.add("active");
    button.innerHTML = `<span><span class="item-title"></span><span class="item-subtitle"></span></span><span class="pill">${project.marker}</span>`;
    q(".item-title", button).textContent = projectName(project.path);
    q(".item-subtitle", button).textContent = project.path;
    button.addEventListener("click", () => selectProject(project));
    list.append(button);
  }
}

function renderSessions() {
  const list = el("session-list");
  list.replaceChildren();
  for (const session of state.sessions) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "list-item";
    if (state.session?.id === session.id) button.classList.add("active");
    button.innerHTML = `<span><span class="item-title"></span><span class="item-subtitle"></span></span><span class="item-subtitle"></span>`;
    q(".item-title", button).textContent = sessionLabel(session);
    q(".item-subtitle", button).textContent = session.model || "";
    button.lastElementChild.textContent = session.turn_count ? `${session.turn_count}` : "";
    button.addEventListener("click", () => selectSession(session));
    list.append(button);
  }
}

function renderStatus() {
  const status = state.status || {};
  el("status-model").textContent = status.model_id || "-";
  const rows = [
    ["model", status.model_id],
    ["effort", status.effort || "-"],
    ["session", status.session_id || "-"],
    ["project", status.project_root || state.project?.path || "-"],
    ["context", status.context_percent == null ? "-" : `${status.context_percent.toFixed(1)}%`],
    ["window", status.context_window],
    ["max output", status.max_output_tokens],
    ["reasoning", status.reasoning == null ? "-" : String(status.reasoning)],
    ["metadata", status.model_metadata_source],
    ["effort levels", (status.supported_effort_levels || []).join(", ")],
    ["git", status.git?.branch ? `${status.git.branch}${status.git.dirty ? " dirty" : " clean"}` : "-"],
    ["turns", status.session?.turn_count],
    ["updated", status.session?.updated_at],
    ["cost", status.session?.cost_total ? `$${status.session.cost_total.toFixed(4)}` : "-"],
    ["busy", status.active?.busy ? "yes" : "no"],
  ];
  const grid = el("status");
  grid.replaceChildren();
  for (const [key, value] of rows) {
    const k = document.createElement("div");
    k.className = "status-key";
    k.textContent = key;
    const v = document.createElement("div");
    v.className = "status-value";
    v.textContent = value == null || value === "" ? "-" : String(value);
    grid.append(k, v);
  }
}

function renderTimeline(messages = []) {
  const timeline = el("timeline");
  timeline.replaceChildren();
  const visible = messages.filter((message) => !(message.seq === 0 && message.role === "user" && message.content.startsWith("[environment]")));
  if (!state.project) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "No project selected";
    timeline.append(empty);
    return;
  }
  if (!state.session && visible.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "Recent work will appear here";
    timeline.append(empty);
    return;
  }
  for (const message of visible) appendMessage(message.role, message.content);
  timeline.scrollTop = timeline.scrollHeight;
}

function appendMessage(role, content, className = "") {
  const row = document.createElement("article");
  row.className = `message ${className}`;
  const roleNode = document.createElement("div");
  roleNode.className = "role";
  roleNode.textContent = role;
  const bubble = document.createElement("div");
  bubble.className = "bubble";
  bubble.textContent = content || "";
  row.append(roleNode, bubble);
  el("timeline").append(row);
  el("timeline").scrollTop = el("timeline").scrollHeight;
  return bubble;
}

function appendTool(title) {
  const row = document.createElement("article");
  row.className = "message";
  const roleNode = document.createElement("div");
  roleNode.className = "role";
  roleNode.textContent = "tool";
  const tool = document.createElement("div");
  tool.className = "tool";
  const head = document.createElement("div");
  head.className = "tool-head";
  head.textContent = title || "bash";
  const output = document.createElement("pre");
  output.className = "tool-output";
  tool.append(head, output);
  row.append(roleNode, tool);
  el("timeline").append(row);
  el("timeline").scrollTop = el("timeline").scrollHeight;
  return { head, output };
}

function rememberPrompt(prompt) {
  const value = prompt.trim();
  if (!value) return;
  promptHistory = promptHistory.filter((item) => item !== value);
  promptHistory.unshift(value);
  promptHistory = promptHistory.slice(0, 100);
  localStorage.setItem("mu-web-prompt-history", JSON.stringify(promptHistory));
  promptHistoryIndex = null;
}

function recallPrompt(direction) {
  if (promptHistory.length === 0) return;
  const prompt = el("prompt");
  if (direction < 0) {
    promptHistoryIndex = promptHistoryIndex == null ? 0 : Math.min(promptHistory.length - 1, promptHistoryIndex + 1);
  } else {
    promptHistoryIndex = promptHistoryIndex == null ? null : promptHistoryIndex - 1;
    if (promptHistoryIndex < 0) promptHistoryIndex = null;
  }
  prompt.value = promptHistoryIndex == null ? "" : promptHistory[promptHistoryIndex];
  requestAnimationFrame(() => {
    prompt.selectionStart = prompt.selectionEnd = prompt.value.length;
  });
}

async function selectProject(project) {
  state.project = project;
  state.session = null;
  state.status = null;
  el("project-name").textContent = projectName(project.path);
  el("session-title").textContent = "Home";
  el("session-subtitle").textContent = project.path;
  renderProjects([project, ...JSON.parse(localStorage.getItem("mu-web-projects") || "[]").filter((p) => p.path !== project.path)]);
  rememberLocalProject(project);
  await Promise.all([loadSessions(), loadModels(), loadStatus().catch((error) => toast(error.message))]);
  renderTimeline([]);
}

function rememberLocalProject(project) {
  const projects = JSON.parse(localStorage.getItem("mu-web-projects") || "[]").filter((p) => p.path !== project.path);
  projects.unshift(project);
  localStorage.setItem("mu-web-projects", JSON.stringify(projects.slice(0, 20)));
}

async function loadSessions() {
  if (!state.project) return;
  state.sessions = await api(`/api/sessions?project=${encodeURIComponent(state.project.path)}`);
  renderSessions();
}

async function loadStatus() {
  if (!state.project) return;
  const session = state.session ? `&session=${encodeURIComponent(state.session.id)}` : "";
  state.status = await api(`/api/status?project=${encodeURIComponent(state.project.path)}${session}`);
  el("busy-pill").classList.toggle("hidden", !state.status.active?.busy && !state.activeTurn);
  renderStatus();
}

async function loadModels() {
  if (!state.project) return;
  try {
    const catalog = await api(`/api/models?project=${encodeURIComponent(state.project.path)}`);
    state.models = Object.values(catalog.models || {});
  } catch (_) {
    state.models = [];
  }
  const select = el("model");
  select.replaceChildren(new Option("model", ""));
  for (const model of state.models) {
    select.append(new Option(model.display_name || model.id, model.id));
  }
}

async function selectSession(session) {
  state.session = session;
  el("session-title").textContent = sessionLabel(session);
  el("session-subtitle").textContent = session.id;
  renderSessions();
  const messages = await api(`/api/sessions/${encodeURIComponent(session.id)}/messages?project=${encodeURIComponent(state.project.path)}`);
  renderTimeline(messages);
  await loadStatus().catch((error) => toast(error.message));
}

async function createSession() {
  if (!state.project) {
    toast("Select a project");
    return null;
  }
  const session = await api("/api/sessions", {
    method: "POST",
    body: JSON.stringify({ project: state.project.path }),
  });
  await loadSessions();
  const found = state.sessions.find((item) => item.id === session.id) || session;
  await selectSession(found);
  return found;
}

async function openProject(path, create = false) {
  try {
    const project = await api("/api/projects/open", {
      method: "POST",
      body: JSON.stringify({ path, create }),
    });
    await selectProject(project);
  } catch (error) {
    if (error.status === 409) {
      el("confirm-path").textContent = path;
      const dialog = el("confirm-project");
      dialog.showModal();
      return;
    }
    toast(error.message);
  }
}

async function submitPrompt(event) {
  event.preventDefault();
  if (state.activeTurn) return;
  if (!state.project) {
    toast("Select a project");
    return;
  }
  const prompt = el("prompt").value.trimEnd();
  if (!prompt.trim()) return;
  rememberPrompt(prompt);
  let session = state.session;
  if (!session) session = await createSession();
  if (!session) return;

  appendMessage("user", prompt);
  el("prompt").value = "";
  const body = {
    project: state.project.path,
    session_id: session.id,
    prompt,
    model: el("model").value || undefined,
    effort: el("effort").value || undefined,
    images: state.attachments,
  };
  state.attachments = [];
  renderAttachments();
  await readTurnStream(body);
}

async function readTurnStream(body) {
  el("submit").disabled = true;
  el("abort").classList.remove("hidden");
  el("busy-pill").classList.remove("hidden");
  let assistant = null;
  let tool = null;
  try {
    const response = await fetch("/api/turns", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!response.ok) throw new Error(await response.text());
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let index;
      while ((index = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (!line) continue;
        const event = JSON.parse(line);
        if (event.event === "turn_start") {
          state.activeTurn = event.payload.turn;
        } else if (event.event === "assistant_delta") {
          if (!assistant) assistant = appendMessage("assistant", "");
          assistant.textContent += event.payload.text || "";
        } else if (event.event === "tool_start") {
          const args = event.payload.args || {};
          tool = appendTool(args.title || args.script || event.payload.tool);
        } else if (event.event === "tool_output") {
          if (!tool) tool = appendTool("bash");
          tool.output.textContent += event.payload.text || "";
        } else if (event.event === "tool_finish") {
          if (tool) tool.head.textContent += " done";
        } else if (event.event === "tool_error" || event.event === "error" || event.event === "stderr") {
          appendMessage("system", event.payload.message || event.payload.error || event.payload.text || "");
        } else if (event.event === "turn_finish") {
          state.activeTurn = null;
        }
      }
    }
  } catch (error) {
    toast(error.message);
  } finally {
    state.activeTurn = null;
    el("submit").disabled = false;
    el("abort").classList.add("hidden");
    el("busy-pill").classList.add("hidden");
    await Promise.all([loadSessions(), loadStatus().catch(() => {})]);
    if (state.session) {
      const refreshed = state.sessions.find((item) => item.id === state.session.id);
      if (refreshed) state.session = refreshed;
    }
  }
}

async function abortTurn() {
  if (!state.activeTurn) return;
  try {
    await api(`/api/turns/${encodeURIComponent(state.activeTurn.id)}/abort`, { method: "POST", body: "{}" });
  } catch (error) {
    toast(error.message);
  }
}

function renderAttachments() {
  const root = el("attachments");
  root.replaceChildren();
  state.attachments.forEach((attachment, index) => {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = attachment.name;
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = "x";
    remove.addEventListener("click", () => {
      state.attachments.splice(index, 1);
      renderAttachments();
    });
    chip.append(remove);
    root.append(chip);
  });
}

function attachImages(files) {
  for (const file of files) {
    const reader = new FileReader();
    reader.addEventListener("load", () => {
      state.attachments.push({ name: file.name, data_url: reader.result });
      renderAttachments();
    });
    reader.readAsDataURL(file);
  }
}

async function bootstrap() {
  setTheme(localStorage.getItem("mu-web-theme") || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"));
  const data = await api("/api/bootstrap");
  const local = JSON.parse(localStorage.getItem("mu-web-projects") || "[]");
  const projects = [...(data.recent_projects || []), ...local].filter((project, index, all) => all.findIndex((p) => p.path === project.path) === index);
  renderProjects(projects);
  if (data.launch_project) {
    await selectProject(data.launch_project);
  } else {
    renderTimeline([]);
  }
}

el("project-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const path = el("project-path").value.trim();
  if (path) openProject(path, false);
});
el("open-project").addEventListener("click", () => el("project-path").focus());
el("new-session").addEventListener("click", () => createSession().catch((error) => toast(error.message)));
el("composer").addEventListener("submit", submitPrompt);
el("prompt").addEventListener("keydown", (event) => {
  if (event.key === "ArrowUp" && (event.currentTarget.value === "" || event.currentTarget.selectionStart === 0)) {
    event.preventDefault();
    recallPrompt(-1);
  } else if (event.key === "ArrowDown" && promptHistoryIndex != null) {
    event.preventDefault();
    recallPrompt(1);
  } else if (event.key.length === 1) {
    promptHistoryIndex = null;
  }
});
el("attach").addEventListener("click", () => el("image-input").click());
el("image-input").addEventListener("change", (event) => attachImages(event.target.files || []));
el("abort").addEventListener("click", abortTurn);
el("theme-toggle").addEventListener("click", () => setTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark"));
el("confirm-create").addEventListener("click", (event) => {
  event.preventDefault();
  const path = el("confirm-path").textContent;
  el("confirm-project").close();
  openProject(path, true);
});
el("confirm-cancel").addEventListener("click", () => el("confirm-project").close());

bootstrap().catch((error) => toast(error.message));
"#;

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
            recent_projects: Mutex::new(recent_projects),
            active_turns: Mutex::new(HashMap::new()),
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
        assert!(response_body(&response).contains(r#"<div id="app" class="app">"#));
    }

    #[tokio::test]
    async fn streams_turn_events_from_child_process() {
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

        let response = http_roundtrip(state, &request).await;

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("content-type: application/x-ndjson; charset=utf-8"));
        assert!(response.contains("x-accel-buffering: no"));
        assert!(response_body(&response).contains(r#""event":"turn_start""#));
        assert!(response_body(&response).contains(r#""event":"assistant_delta""#));
        assert!(response_body(&response).contains(r#""event":"turn_finish""#));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn busy_session_turn_returns_conflict() {
        let root = std::env::temp_dir().join(format!("mu-web-busy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let state = test_state(root.clone());
        state.active_turns.lock().await.insert(
            "turn-1".into(),
            ActiveTurn {
                id: "turn-1".into(),
                project: root.display().to_string(),
                session_id: Some("session-1".into()),
                started_at: "2026-06-28T00:00:00Z".into(),
                pgid: 1,
            },
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
    fn static_app_contains_prompt_history_and_project_confirmation_copy() {
        assert!(INDEX_HTML.contains("mu will create .mu in this directory."));
        assert!(APP_JS.contains("mu-web-prompt-history"));
    }
}
