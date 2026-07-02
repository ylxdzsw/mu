import { execFile, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { lstat, mkdir, chmod, readFile, realpath, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import type { IncomingMessage, ServerResponse } from "node:http";

type Scope = { kind: "global" } | { kind: "project"; path: string };

type EventPayload = Record<string, unknown>;

interface ProjectSummary {
  path: string;
  marker: string;
}

interface ProjectInspectOutput {
  project_root?: string;
  discovered_marker?: string;
}

interface SessionSummary {
  id: string;
}

interface UploadImage {
  name: string;
  data_url: string;
}

interface TurnInput {
  prompt?: string;
  project: string;
  session_id?: string;
  model?: string;
  images?: UploadImage[];
}

interface OpenProjectInput {
  path: string;
  create?: boolean;
}

interface TurnRecord {
  id: string;
  project: string;
  session_id: string;
  started_at: string;
  pgid: number;
}

interface TurnEventEnvelope {
  seq: number;
  event: string;
  payload: EventPayload;
}

interface TurnSnapshot {
  prompt: string;
  assistant_text: string;
  raw_events: TurnEventEnvelope[];
  stderr: string;
  exit_code: number | null;
}

interface TurnView {
  turn: TurnRecord;
  last_seq: number;
  completed: boolean;
  snapshot: TurnSnapshot;
}

type TurnReplay =
  | { type: "reset"; nextSeq: number }
  | { type: "ready"; events: TurnEventEnvelope[]; completed: boolean };

interface AppState {
  launchCwd: string;
  launchProject: ProjectSummary | null;
  globalHome: string;
  recentProjects: ProjectSummary[];
  turns: Map<string, TurnRuntime>;
  uploadRoot: string;
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const publicRoot = path.join(__dirname, "public");
const MAX_REQUEST_BYTES = 32 * 1024 * 1024;
const MAX_TURN_EVENT_BUFFER = 512;
const GLOBAL_PROJECT_ID = "__mu_global__";
const SOCKET_MODE = 0o660;
const MU_EXE = "mu";
const BASE_HEADERS = {
  "cache-control": "no-store",
  "content-security-policy":
    "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'",
  "x-content-type-options": "nosniff",
};

class TurnRuntime {
  turn: TurnRecord;
  events: TurnEventEnvelope[];
  nextSeq: number;
  completed: boolean;
  waiters: Set<() => void>;
  snapshot: TurnSnapshot;

  constructor(turn: TurnRecord, prompt: string) {
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

  pushEvent(event: string, payload: EventPayload): void {
    this.nextSeq += 1;
    const envelope = { seq: this.nextSeq, event, payload };
    applySnapshotEvent(this.snapshot, envelope);
    this.events.push(envelope);
    while (this.events.length > MAX_TURN_EVENT_BUFFER) {
      this.events.shift();
    }
    this.notify();
  }

  markCompleted(): void {
    this.completed = true;
    this.notify();
  }

  currentView(): TurnView {
    return {
      turn: this.turn,
      last_seq: this.nextSeq,
      completed: this.completed,
      snapshot: structuredClone(this.snapshot),
    };
  }

  replayAfter(after: number): TurnReplay {
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

  waitForChange(after: number): Promise<void> {
    if (this.nextSeq > after || this.completed) {
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      this.waiters.add(resolve);
    });
  }

  notify(): void {
    for (const resolve of this.waiters) {
      resolve();
    }
    this.waiters.clear();
  }
}

async function main(): Promise<void> {
  const socketPath = socketPathFromArgv(process.argv);
  await prepareSocketPath(socketPath);

  const state = await createState();
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
    server.listen(socketPath, resolve);
  });
  await chmod(socketPath, SOCKET_MODE);
  console.error(`mu web listening on unix://${socketPath}`);

  const shutdown = async () => {
    await new Promise((resolve) => server.close(() => resolve()));
    await rm(socketPath, { force: true }).catch(() => {});
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

function socketPathFromArgv(argv: string[]): string {
  if (argv.length !== 3 || !argv[2]) {
    throw new Error("expected exactly one argument: <socket-path>");
  }
  return argv[2];
}

async function prepareSocketPath(socketPath: string): Promise<void> {
  await mkdir(path.dirname(socketPath), { recursive: true });
  try {
    const metadata = await lstat(socketPath);
    if (!metadata.isSocket()) {
      throw new Error(`refusing to replace non-socket path: ${socketPath}`);
    }
    await rm(socketPath, { force: true });
  } catch (error: any) {
    if (error && error.code === "ENOENT") {
      return;
    }
    throw error;
  }
}

async function createState(): Promise<AppState> {
  const launchCwd = await realpath(process.cwd());
  const launchProject = await discoverProject(launchCwd);
  const uploadRoot = path.join(runtimeDir(), "web-uploads");
  await mkdir(uploadRoot, { recursive: true });
  return {
    launchCwd,
    launchProject,
    globalHome: process.env.HOME || "/tmp",
    recentProjects: launchProject ? [launchProject] : [],
    turns: new Map(),
    uploadRoot,
  };
}

function runtimeDir(): string {
  if (process.env.XDG_RUNTIME_DIR) {
    return path.join(process.env.XDG_RUNTIME_DIR, "mu");
  }
  return path.join(tmpdir(), "mu");
}

async function discoverProject(cwd: string): Promise<ProjectSummary | null> {
  const info = await runJsonCommand<ProjectInspectOutput>(
    { globalHome: process.env.HOME || "/tmp" },
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

async function handleRequest(
  request: IncomingMessage,
  response: ServerResponse,
  state: AppState,
): Promise<void> {
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
    const input = await parseJsonBody<OpenProjectInput>(request);
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
    const input = await parseJsonBody<{ project: string }>(request);
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
    const input = await parseJsonBody<TurnInput>(request);
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

async function serveStatic(response: ServerResponse, pathname: string): Promise<boolean> {
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
  } catch (error: any) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw error;
  }
}

function contentType(filePath: string): string {
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

async function parseJsonBody<T>(request: IncomingMessage): Promise<T> {
  const body = await readBody(request);
  return JSON.parse(body || "{}") as T;
}

async function readBody(request: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
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

function queryFlag(url: URL, key: string): boolean {
  const value = url.searchParams.get(key);
  if (!value) {
    return false;
  }
  return ["1", "true", "yes", "on"].includes(value.trim().toLowerCase());
}

async function scopeQuery(state: AppState, url: URL): Promise<Scope> {
  const project = url.searchParams.get("project");
  if (!project) {
    throw new Error("missing project");
  }
  return parseScopeTarget(state, project);
}

async function parseScopeTarget(state: AppState, value: string): Promise<Scope> {
  if (value === GLOBAL_PROJECT_ID) {
    return { kind: "global" };
  }
  return { kind: "project", path: await requireProject(value) };
}

function scopeKey(scope: Scope): string {
  return scope.kind === "global" ? GLOBAL_PROJECT_ID : scope.path;
}

function currentDirForScope(state: AppState, scope: Scope): string {
  return scope.kind === "global" ? state.globalHome : scope.path;
}

async function resolveExistingDir(target: string): Promise<string> {
  const resolved = await realpath(target);
  const metadata = await stat(resolved);
  if (!metadata.isDirectory()) {
    throw new Error(`not a directory: ${resolved}`);
  }
  return resolved;
}

async function markerAt(target: string): Promise<string | null> {
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

async function requireProject(target: string): Promise<string> {
  const resolved = await resolveExistingDir(target);
  if (!(await markerAt(resolved))) {
    throw new Error(`not a project: ${resolved}`);
  }
  return resolved;
}

async function requireMarker(target: string): Promise<string> {
  const marker = await markerAt(target);
  if (!marker) {
    throw new Error(`not a project: ${target}`);
  }
  return marker;
}

async function inspectProject(target: string): Promise<{
  path: string;
  is_project: boolean;
  marker: string | null;
  needs_confirmation: boolean;
}> {
  const resolved = await resolveExistingDir(target);
  const marker = await markerAt(resolved);
  return {
    path: resolved,
    is_project: Boolean(marker),
    marker,
    needs_confirmation: !marker,
  };
}

function rememberProject(state: AppState, summary: ProjectSummary): void {
  state.recentProjects = [summary, ...state.recentProjects.filter((project) => project.path !== summary.path)];
  state.recentProjects = state.recentProjects.slice(0, 20);
}

async function runJsonCommand<T>(
  state: { globalHome: string } | AppState,
  scope: Scope | null,
  args: string[],
  overrideCwd: string | null = null,
): Promise<T> {
  const cwd = overrideCwd || (scope ? currentDirForScope(state, scope) : undefined);
  const { stdout } = await execFileResult(MU_EXE, args, { cwd });
  try {
    return JSON.parse(stdout) as T;
  } catch (error: any) {
    throw new Error(`parsing mu JSON output: ${error.message}`);
  }
}

function execFileResult(
  file: string,
  args: string[],
  options: { cwd?: string },
): Promise<{ stdout: string; stderr: string }> {
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

async function createWebSession(state: AppState, scope: Scope): Promise<SessionSummary> {
  return runJsonCommand<SessionSummary>(state, scope, ["session", "new", "--origin", "web", "--json"]);
}

async function activeSession(state: AppState, scope: Scope, sessionId: string): Promise<boolean> {
  return Boolean(await findActiveTurn(state, scope, sessionId));
}

async function findActiveTurn(
  state: AppState,
  scope: Scope,
  sessionId: string,
): Promise<TurnRuntime | null> {
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

async function launchTurn(state: AppState, scope: Scope, input: TurnInput): Promise<TurnRecord> {
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

  const child: ChildProcessWithoutNullStreams = spawn(MU_EXE, args, {
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

function onceSpawn(child: ChildProcessWithoutNullStreams): Promise<void> {
  return new Promise((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", reject);
  });
}

async function runTurnTask(
  runtime: TurnRuntime,
  child: ChildProcessWithoutNullStreams,
  uploadDir: string,
): Promise<void> {
  let streamError: Error | null = null;
  const stderrChunks: string[] = [];
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
      const event = JSON.parse(line) as { event?: string; payload?: EventPayload };
      const eventName = event.event;
      if (typeof eventName !== "string") {
        throw new Error("child JSON event missing event name");
      }
      runtime.pushEvent(eventName, event.payload ?? {});
    }
  } catch (error: any) {
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

function onceExit(child: ChildProcessWithoutNullStreams): Promise<number> {
  return new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) => {
      resolve(code ?? 1);
    });
  });
}

async function saveUploads(root: string, images: UploadImage[]): Promise<string[]> {
  if (!images.length) {
    return [];
  }
  await mkdir(root, { recursive: true });
  const paths: string[] = [];
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

function extensionForMime(mime: string): string {
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

async function streamTurnEvents(
  response: ServerResponse,
  state: AppState,
  id: string,
  after: number,
): Promise<void> {
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

function onceResponseClosed(response: ServerResponse): Promise<void> {
  return new Promise((resolve) => {
    response.once("close", resolve);
  });
}

async function writeSseEvent(
  response: ServerResponse,
  id: number | null,
  event: string,
  payload: EventPayload,
): Promise<void> {
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

async function abortTurn(response: ServerResponse, state: AppState, id: string): Promise<void> {
  const runtime = state.turns.get(id);
  if (!runtime || runtime.completed) {
    writeJson(response, 404, { error: "turn not found" });
    return;
  }
  try {
    process.kill(-runtime.turn.pgid, "SIGTERM");
  } catch (error: any) {
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

function applySnapshotEvent(snapshot: TurnSnapshot, envelope: TurnEventEnvelope): void {
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

function writeJson(response: ServerResponse, status: number, value: unknown): void {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, {
    ...BASE_HEADERS,
    "content-length": body.length,
    "content-type": "application/json; charset=utf-8",
    connection: "close",
  });
  response.end(body);
}

main().catch((error: any) => {
  console.error(error.message || String(error));
  process.exit(1);
});
