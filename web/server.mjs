import { execFile, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { lstat, mkdir, chmod, readFile, realpath, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const publicRoot = path.join(__dirname, "public");
const MAX_REQUEST_BYTES = 32 * 1024 * 1024;
const MAX_TURN_EVENT_BUFFER = 512;
const GLOBAL_PROJECT_ID = "__mu_global__";
const BASE_HEADERS = {
  "cache-control": "no-store",
  "content-security-policy":
    "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'",
  "x-content-type-options": "nosniff",
};

class TurnRuntime {
  constructor(turn, prompt) {
    this.turn = turn;
    this.events = [];
    this.nextSeq = 0;
    this.completed = false;
    this.waiters = new Set();
    this.snapshot = {
      prompt,
      assistant_text: "",
      raw_events: [],
      stderr: "",
      exit_code: null,
    };
    this.pushEvent("turn_start", { turn });
  }

  pushEvent(event, payload) {
    this.nextSeq += 1;
    const envelope = { seq: this.nextSeq, event, payload };
    applySnapshotEvent(this.snapshot, envelope);
    this.events.push(envelope);
    while (this.events.length > MAX_TURN_EVENT_BUFFER) {
      this.events.shift();
    }
    this.notify();
  }

  markCompleted() {
    this.completed = true;
    this.notify();
  }

  currentView() {
    return {
      turn: this.turn,
      last_seq: this.nextSeq,
      completed: this.completed,
      snapshot: structuredClone(this.snapshot),
    };
  }

  replayAfter(after) {
    const first = this.events[0];
    if (first && first.seq > after + 1) {
      return { type: "reset", nextSeq: first.seq };
    }
    return {
      type: "ready",
      events: this.events.filter((event) => event.seq > after),
      completed: this.completed,
    };
  }

  waitForChange(after) {
    if (this.nextSeq > after || this.completed) {
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      this.waiters.add(resolve);
    });
  }

  notify() {
    for (const resolve of this.waiters) {
      resolve();
    }
    this.waiters.clear();
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const socketMode = parseSocketMode(options.socketMode);
  await prepareSocketPath(options.socket);

  const state = await createState(options);
  const server = createServer((request, response) => {
    handleRequest(request, response, state).catch((error) => {
      if (response.headersSent || response.destroyed) {
        return;
      }
      writeJson(response, 500, { error: error.message });
    });
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.socket, resolve);
  });
  await chmod(options.socket, socketMode);
  console.error(`mu web listening on unix://${options.socket}`);

  const shutdown = async () => {
    await new Promise((resolve) => server.close(() => resolve()));
    await rm(options.socket, { force: true }).catch(() => {});
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

function parseArgs(argv) {
  const values = {
    socket: process.env.MU_WEB_SOCKET || "/run/mu-web/mu-web.sock",
    socketMode: process.env.MU_WEB_SOCKET_MODE || "0600",
    muExe: process.env.MU_WEB_MU_EXE || "mu",
    launchCwd: process.env.MU_WEB_LAUNCH_CWD || process.cwd(),
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = argv[index + 1];
    if (arg === "--socket" && next) {
      values.socket = next;
      index += 1;
      continue;
    }
    if (arg === "--socket-mode" && next) {
      values.socketMode = next;
      index += 1;
      continue;
    }
    if (arg === "--mu-exe" && next) {
      values.muExe = next;
      index += 1;
      continue;
    }
    if (arg === "--launch-cwd" && next) {
      values.launchCwd = next;
      index += 1;
      continue;
    }
    throw new Error(`unknown argument: ${arg}`);
  }
  return values;
}

function parseSocketMode(value) {
  const normalized = value.replace(/^0+/, "") || "0";
  const mode = Number.parseInt(normalized, 8);
  if (!Number.isInteger(mode)) {
    throw new Error(`invalid socket mode \`${value}\``);
  }
  return mode;
}

async function prepareSocketPath(socketPath) {
  await mkdir(path.dirname(socketPath), { recursive: true });
  try {
    const metadata = await lstat(socketPath);
    if (!metadata.isSocket()) {
      throw new Error(`refusing to replace non-socket path: ${socketPath}`);
    }
    await rm(socketPath, { force: true });
  } catch (error) {
    if (error && error.code === "ENOENT") {
      return;
    }
    throw error;
  }
}

async function createState(options) {
  const launchCwd = await realpath(options.launchCwd);
  const launchProject = await discoverProject(options.muExe, launchCwd);
  const uploadRoot = path.join(runtimeDir(), "web-uploads");
  await mkdir(uploadRoot, { recursive: true });
  return {
    muExe: options.muExe,
    launchCwd,
    launchProject,
    globalHome: process.env.HOME || "/tmp",
    recentProjects: launchProject ? [launchProject] : [],
    turns: new Map(),
    uploadRoot,
  };
}

function runtimeDir() {
  if (process.env.XDG_RUNTIME_DIR) {
    return path.join(process.env.XDG_RUNTIME_DIR, "mu");
  }
  return path.join(tmpdir(), "mu");
}

async function discoverProject(muExe, cwd) {
  const info = await runJsonCommand(
    { muExe, globalHome: process.env.HOME || "/tmp" },
    null,
    ["project", "inspect", "--path", cwd, "--json"],
    cwd,
  );
  if (!info.project_root || !info.discovered_marker) {
    return null;
  }
  return {
    path: info.project_root,
    marker: info.discovered_marker,
  };
}

async function handleRequest(request, response, state) {
  const url = new URL(request.url, "http://localhost");

  if (request.method === "GET" && !url.pathname.startsWith("/api/")) {
    const served = await serveStatic(response, url.pathname);
    if (served) {
      return;
    }
  }

  if (request.method === "GET" && url.pathname === "/api/bootstrap") {
    writeJson(response, 200, {
      launch_cwd: state.launchCwd,
      global_home: state.globalHome,
      launch_project: state.launchProject,
      recent_projects: state.recentProjects,
    });
    return;
  }

  if (request.method === "GET" && url.pathname === "/api/project/inspect") {
    const target = url.searchParams.get("path");
    if (!target) {
      writeJson(response, 400, { error: "missing path" });
      return;
    }
    writeJson(response, 200, await inspectProject(target));
    return;
  }

  if (request.method === "POST" && url.pathname === "/api/projects/open") {
    const input = await parseJsonBody(request);
    const root = await resolveExistingDir(input.path);
    const marker = await markerAt(root);
    if (!marker && !input.create) {
      writeJson(response, 409, {
        error: "project confirmation required",
        needs_confirmation: true,
        path: root,
      });
      return;
    }
    if (!marker) {
      await runJsonCommand(state, null, ["project", "init", "--path", root, "--force", "--json"]);
    }
    const summary = {
      path: root,
      marker: await requireMarker(root),
    };
    rememberProject(state, summary);
    writeJson(response, 200, summary);
    return;
  }

  if (request.method === "GET" && url.pathname === "/api/sessions") {
    const scope = await scopeQuery(state, url);
    const value = await runJsonCommand(
      state,
      scope,
      ["session", "list", "--all-origins", "--json"],
    );
    writeJson(response, 200, value);
    return;
  }

  if (request.method === "POST" && url.pathname === "/api/sessions") {
    const input = await parseJsonBody(request);
    const scope = await parseScopeTarget(state, input.project);
    writeJson(response, 200, await createWebSession(state, scope));
    return;
  }

  if (request.method === "GET" && url.pathname === "/api/status") {
    const scope = await scopeQuery(state, url);
    const args = ["status", "--json"];
    if (queryFlag(url, "include_models")) {
      args.push("--include-models");
    }
    if (url.searchParams.get("session")) {
      args.push("--session", url.searchParams.get("session"));
    }
    writeJson(response, 200, await runJsonCommand(state, scope, args));
    return;
  }

  if (request.method === "GET" && url.pathname === "/api/turns/active") {
    const scope = await scopeQuery(state, url);
    const sessionId = url.searchParams.get("session");
    if (!sessionId) {
      writeJson(response, 400, { error: "missing session" });
      return;
    }
    const runtime = await findActiveTurn(state, scope, sessionId);
    writeJson(response, 200, runtime ? runtime.currentView() : null);
    return;
  }

  if (request.method === "POST" && url.pathname === "/api/turns") {
    const input = await parseJsonBody(request);
    if (!input.prompt || !input.prompt.trim()) {
      writeJson(response, 400, { error: "empty prompt" });
      return;
    }
    const scope = await parseScopeTarget(state, input.project);
    if (input.session_id && (await activeSession(state, scope, input.session_id))) {
      writeJson(response, 409, { error: "session busy" });
      return;
    }
    if (!input.session_id) {
      const session = await createWebSession(state, scope);
      input.session_id = session.id;
    }
    if (await activeSession(state, scope, input.session_id)) {
      writeJson(response, 409, { error: "session busy" });
      return;
    }
    writeJson(response, 200, { turn: await launchTurn(state, scope, input) });
    return;
  }

  if (
    request.method === "GET" &&
    url.pathname.startsWith("/api/sessions/") &&
    url.pathname.endsWith("/messages")
  ) {
    const scope = await scopeQuery(state, url);
    const session = url.pathname
      .slice("/api/sessions/".length, -"/messages".length)
      .replace(/^\/+|\/+$/g, "");
    writeJson(
      response,
      200,
      await runJsonCommand(state, scope, ["session", "transcript", "--session", session, "--json"]),
    );
    return;
  }

  if (
    request.method === "GET" &&
    url.pathname.startsWith("/api/turns/") &&
    url.pathname.endsWith("/events")
  ) {
    const id = url.pathname.slice("/api/turns/".length, -"/events".length).replace(/^\/+|\/+$/g, "");
    await streamTurnEvents(response, state, id, Number.parseInt(url.searchParams.get("after") || "0", 10));
    return;
  }

  if (
    request.method === "POST" &&
    url.pathname.startsWith("/api/turns/") &&
    url.pathname.endsWith("/abort")
  ) {
    const id = url.pathname.slice("/api/turns/".length, -"/abort".length).replace(/^\/+|\/+$/g, "");
    await abortTurn(response, state, id);
    return;
  }

  writeJson(response, 404, { error: "not found" });
}

async function serveStatic(response, pathname) {
  const target = pathname === "/" ? "/index.html" : pathname;
  const filePath = path.resolve(publicRoot, `.${target}`);
  if (filePath !== publicRoot && !filePath.startsWith(`${publicRoot}${path.sep}`)) {
    return false;
  }
  try {
    const info = await stat(filePath);
    if (!info.isFile()) {
      return false;
    }
    const body = await readFile(filePath);
    response.writeHead(200, {
      ...BASE_HEADERS,
      "content-length": body.length,
      "content-type": contentType(filePath),
      connection: "close",
    });
    response.end(body);
    return true;
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw error;
  }
}

function contentType(filePath) {
  if (filePath.endsWith(".html")) {
    return "text/html; charset=utf-8";
  }
  if (filePath.endsWith(".css")) {
    return "text/css; charset=utf-8";
  }
  if (filePath.endsWith(".js")) {
    return "text/javascript; charset=utf-8";
  }
  if (filePath.endsWith(".json")) {
    return "application/json; charset=utf-8";
  }
  return "application/octet-stream";
}

async function parseJsonBody(request) {
  const body = await readBody(request);
  return JSON.parse(body || "{}");
}

async function readBody(request) {
  const chunks = [];
  let total = 0;
  for await (const chunk of request) {
    total += chunk.length;
    if (total > MAX_REQUEST_BYTES) {
      throw new Error("request too large");
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function queryFlag(url, key) {
  const value = url.searchParams.get(key);
  if (!value) {
    return false;
  }
  return ["1", "true", "yes", "on"].includes(value.trim().toLowerCase());
}

async function scopeQuery(state, url) {
  const project = url.searchParams.get("project");
  if (!project) {
    throw new Error("missing project");
  }
  return parseScopeTarget(state, project);
}

async function parseScopeTarget(state, value) {
  if (value === GLOBAL_PROJECT_ID) {
    return { kind: "global" };
  }
  return { kind: "project", path: await requireProject(value) };
}

function scopeKey(scope) {
  return scope.kind === "global" ? GLOBAL_PROJECT_ID : scope.path;
}

function currentDirForScope(state, scope) {
  return scope.kind === "global" ? state.globalHome : scope.path;
}

async function resolveExistingDir(target) {
  const resolved = await realpath(target);
  const metadata = await stat(resolved);
  if (!metadata.isDirectory()) {
    throw new Error(`not a directory: ${resolved}`);
  }
  return resolved;
}

async function markerAt(target) {
  try {
    const muStat = await stat(path.join(target, ".mu"));
    if (muStat.isDirectory()) {
      return "mu";
    }
  } catch (_) {}
  try {
    await stat(path.join(target, ".git"));
    return "git";
  } catch (_) {
    return null;
  }
}

async function requireProject(target) {
  const resolved = await resolveExistingDir(target);
  if (!(await markerAt(resolved))) {
    throw new Error(`not a project: ${resolved}`);
  }
  return resolved;
}

async function requireMarker(target) {
  const marker = await markerAt(target);
  if (!marker) {
    throw new Error(`not a project: ${target}`);
  }
  return marker;
}

async function inspectProject(target) {
  const resolved = await resolveExistingDir(target);
  const marker = await markerAt(resolved);
  return {
    path: resolved,
    is_project: Boolean(marker),
    marker,
    needs_confirmation: !marker,
  };
}

function rememberProject(state, summary) {
  state.recentProjects = [summary, ...state.recentProjects.filter((project) => project.path !== summary.path)];
  state.recentProjects = state.recentProjects.slice(0, 20);
}

async function runJsonCommand(state, scope, args, overrideCwd = null) {
  const cwd = overrideCwd || (scope ? currentDirForScope(state, scope) : undefined);
  const { stdout } = await execFileResult(state.muExe, args, { cwd });
  try {
    return JSON.parse(stdout);
  } catch (error) {
    throw new Error(`parsing mu JSON output: ${error.message}`);
  }
}

function execFileResult(file, args, options) {
  return new Promise((resolve, reject) => {
    execFile(file, args, { ...options, maxBuffer: MAX_REQUEST_BYTES }, (error, stdout, stderr) => {
      if (error) {
        const message = (stderr || stdout || error.message).trim();
        reject(new Error(message || error.message));
        return;
      }
      resolve({ stdout, stderr });
    });
  });
}

async function createWebSession(state, scope) {
  return runJsonCommand(state, scope, ["session", "new", "--origin", "web", "--json"]);
}

async function activeSession(state, scope, sessionId) {
  return Boolean(await findActiveTurn(state, scope, sessionId));
}

async function findActiveTurn(state, scope, sessionId) {
  const project = scopeKey(scope);
  for (const runtime of state.turns.values()) {
    if (runtime.turn.project !== project) {
      continue;
    }
    if (runtime.turn.session_id !== sessionId) {
      continue;
    }
    if (!runtime.completed) {
      return runtime;
    }
  }
  return null;
}

async function launchTurn(state, scope, input) {
  const turnId = randomUUID();
  const uploadDir = path.join(state.uploadRoot, turnId);
  const imagePaths = await saveUploads(uploadDir, input.images || []);
  const args = ["--origin", "web", "--output", "json", "--session", input.session_id];
  if (input.model && input.model.trim()) {
    args.push("--model", input.model);
  }
  for (const imagePath of imagePaths) {
    args.push("--image", imagePath);
  }

  const child = spawn(state.muExe, args, {
    cwd: currentDirForScope(state, scope),
    detached: true,
    stdio: ["pipe", "pipe", "pipe"],
  });
  await onceSpawn(child);
  child.stdin.end(input.prompt);

  const turn = {
    id: turnId,
    project: scopeKey(scope),
    session_id: input.session_id,
    started_at: new Date().toISOString(),
    pgid: child.pid || 0,
  };
  const runtime = new TurnRuntime(turn, input.prompt);
  state.turns.set(turnId, runtime);
  runTurnTask(runtime, child, uploadDir);
  return turn;
}

function onceSpawn(child) {
  return new Promise((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", reject);
  });
}

async function runTurnTask(runtime, child, uploadDir) {
  let streamError = null;
  const stderrChunks = [];
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    stderrChunks.push(chunk);
  });

  child.stdout.setEncoding("utf8");
  const lines = readline.createInterface({
    input: child.stdout,
    crlfDelay: Infinity,
  });

  try {
    for await (const line of lines) {
      const event = JSON.parse(line);
      const eventName = event.event;
      if (typeof eventName !== "string") {
        throw new Error("child JSON event missing event name");
      }
      runtime.pushEvent(eventName, event.payload ?? {});
    }
  } catch (error) {
    streamError = error;
  }

  if (streamError) {
    runtime.pushEvent("error", { message: `parsing child JSON event: ${streamError.message}` });
  }

  const exitCode = await onceExit(child);
  const stderrText = stderrChunks.join("").replace(/\n+$/u, "");
  if (stderrText) {
    runtime.pushEvent("stderr", { text: stderrText });
  }
  runtime.pushEvent("turn_finish", { exit_code: exitCode });
  runtime.markCompleted();
  await rm(uploadDir, { recursive: true, force: true }).catch(() => {});
}

function onceExit(child) {
  return new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) => {
      resolve(code ?? 1);
    });
  });
}

async function saveUploads(root, images) {
  if (!images.length) {
    return [];
  }
  await mkdir(root, { recursive: true });
  const paths = [];
  for (let index = 0; index < images.length; index += 1) {
    const image = images[index];
    const [mimePart, encoded] = image.data_url.split(";base64,");
    if (!mimePart || encoded === undefined) {
      throw new Error(`invalid data URL for ${image.name}`);
    }
    const ext = extensionForMime(mimePart.replace(/^data:/, ""));
    const target = path.join(root, `${index}.${ext}`);
    await writeFile(target, Buffer.from(encoded, "base64"));
    paths.push(target);
  }
  return paths;
}

function extensionForMime(mime) {
  switch (mime) {
    case "image/png":
      return "png";
    case "image/jpeg":
      return "jpg";
    case "image/webp":
      return "webp";
    case "image/gif":
      return "gif";
    default:
      throw new Error(`unsupported image attachment type: ${mime}`);
  }
}

async function streamTurnEvents(response, state, id, after) {
  const runtime = state.turns.get(id);
  if (!runtime) {
    writeJson(response, 404, { error: "turn not found" });
    return;
  }

  response.writeHead(200, {
    ...BASE_HEADERS,
    "cache-control": "no-cache, no-transform",
    "content-type": "text/event-stream; charset=utf-8",
    connection: "close",
    "x-accel-buffering": "no",
  });

  let lastSeq = Number.isFinite(after) ? after : 0;
  while (!response.destroyed) {
    const replay = runtime.replayAfter(lastSeq);
    if (replay.type === "reset") {
      await writeSseEvent(response, null, "reset", {
        reason: "replay_missed",
        next_seq: replay.nextSeq,
      });
      response.end();
      return;
    }
    for (const event of replay.events) {
      await writeSseEvent(response, event.seq, event.event, event.payload);
      lastSeq = event.seq;
    }
    if (replay.completed) {
      response.end();
      return;
    }
    await Promise.race([runtime.waitForChange(lastSeq), onceResponseClosed(response)]);
  }
}

function onceResponseClosed(response) {
  return new Promise((resolve) => {
    response.once("close", resolve);
  });
}

async function writeSseEvent(response, id, event, payload) {
  let frame = "";
  if (id !== null) {
    frame += `id: ${id}\n`;
  }
  frame += `event: ${event}\n`;
  frame += `data: ${JSON.stringify(payload)}\n\n`;
  if (!response.write(frame)) {
    await new Promise((resolve) => response.once("drain", resolve));
  }
}

async function abortTurn(response, state, id) {
  const runtime = state.turns.get(id);
  if (!runtime || runtime.completed) {
    writeJson(response, 404, { error: "turn not found" });
    return;
  }
  try {
    process.kill(-runtime.turn.pgid, "SIGTERM");
  } catch (error) {
    writeJson(response, 500, { error: error.message });
    return;
  }
  setTimeout(() => {
    if (!runtime.completed) {
      try {
        process.kill(-runtime.turn.pgid, "SIGKILL");
      } catch (_) {}
    }
  }, 750);
  writeJson(response, 200, { ok: true });
}

function applySnapshotEvent(snapshot, envelope) {
  snapshot.raw_events.push({
    seq: envelope.seq,
    event: envelope.event,
    payload: envelope.payload,
  });
  if (envelope.event === "assistant_delta" && typeof envelope.payload.text === "string") {
    snapshot.assistant_text += envelope.payload.text;
  }
  if (envelope.event === "stderr" && typeof envelope.payload.text === "string") {
    snapshot.stderr = snapshot.stderr ? `${snapshot.stderr}\n${envelope.payload.text}` : envelope.payload.text;
  }
  if (envelope.event === "turn_finish" && Number.isInteger(envelope.payload.exit_code)) {
    snapshot.exit_code = envelope.payload.exit_code;
  }
}

function writeJson(response, status, value) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, {
    ...BASE_HEADERS,
    "content-length": body.length,
    "content-type": "application/json; charset=utf-8",
    connection: "close",
  });
  response.end(body);
}

main().catch((error) => {
  console.error(error.message || String(error));
  process.exit(1);
});
