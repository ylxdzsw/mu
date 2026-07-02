import { spawn } from "node:child_process";
import { mkdtemp, cp, mkdir, stat, writeFile, rm } from "node:fs/promises";
import http from "node:http";
import { request as httpRequest } from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import type { Server } from "node:http";

export interface Harness {
  baseUrl: string;
  projectDir: string;
  providerRequests: unknown[];
  close(): Promise<void>;
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..", "..", "..");
const webRoot = path.join(repoRoot, "web");
const fixtureProject = path.join(webRoot, "tests", "fixtures", "project");
const fixtureGlobalMu = path.join(webRoot, "tests", "fixtures", "global", ".mu");
const muBinary = path.join(repoRoot, "target", "debug", "mu");
const webServer = path.join(webRoot, "server.ts");
const debug = process.env.MU_WEB_E2E_DEBUG === "1";

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function log(...args: unknown[]): void {
  if (debug) {
    console.log("[mu-web-e2e]", ...args);
  }
}

function onceServerListening(server: Server): Promise<void> {
  return new Promise((resolve, reject) => {
    const onError = (error) => {
      server.off("listening", onListening);
      reject(error);
    };
    const onListening = () => {
      server.off("error", onError);
      resolve();
    };

    server.once("error", onError);
    server.listen(0, "127.0.0.1", onListening);
  });
}

async function waitForSocket(socketPath: string, timeoutMs = 10_000): Promise<void> {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    try {
      const info = await stat(socketPath);
      if (info.isSocket()) {
        return;
      }
    } catch (_) {
      // Retry until the socket appears.
    }
    await delay(100);
  }
  throw new Error(`timed out waiting for socket ${socketPath}`);
}

async function waitForHttp(url: string, timeoutMs = 10_000): Promise<void> {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    try {
      await new Promise((resolve, reject) => {
        const req = http.get(url, (res) => {
          res.resume();
          res.on("end", resolve);
        });
        req.on("error", reject);
      });
      return;
    } catch (_) {
      await delay(100);
    }
  }
  throw new Error(`timed out waiting for ${url}`);
}

function buildStreamingResponse(prompt: string): string[] {
  const text = `Fake response to: ${prompt}`;
  const first = JSON.stringify({
    choices: [{ delta: { content: text }, finish_reason: null }],
  });
  const second = JSON.stringify({
    choices: [{ delta: {}, finish_reason: "stop" }],
    usage: {
      prompt_tokens: 12,
      completion_tokens: 8,
      total_tokens: 20,
    },
  });
  return [first, second, "[DONE]"];
}

async function startFakeProvider(): Promise<{
  requests: unknown[];
  port: number;
  close(): Promise<void>;
}> {
  const requests: unknown[] = [];
  const server = http.createServer((req, res) => {
    if (req.method === "GET" && req.url === "/v1/models") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          object: "list",
          data: [{ id: "fake-model", object: "model" }],
        }),
      );
      return;
    }

    if (req.method === "POST" && req.url === "/v1/chat/completions") {
      let raw = "";
      req.setEncoding("utf8");
      req.on("data", (chunk) => {
        raw += chunk;
      });
      req.on("end", async () => {
        const body = JSON.parse(raw || "{}");
        requests.push(body);
        const prompt =
          body.messages?.filter((message) => message.role === "user").at(-1)?.content || "unknown";
        res.writeHead(200, {
          "content-type": "text/event-stream; charset=utf-8",
          "cache-control": "no-cache",
          connection: "close",
        });
        for (const chunk of buildStreamingResponse(prompt)) {
          res.write(`data: ${chunk}\n\n`);
          await delay(25);
        }
        res.end();
      });
      return;
    }

    res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    res.end(`unexpected ${req.method} ${req.url}`);
  });

  await onceServerListening(server);
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("fake provider did not expose a TCP port");
  }
  return {
    requests,
    port: address.port,
    async close() {
      await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    },
  };
}

async function startSocketProxy(socketPath: string): Promise<{ url: string; close(): Promise<void> }> {
  const server = http.createServer((req, res) => {
    const upstream = httpRequest(
      {
        socketPath,
        path: req.url,
        method: req.method,
        headers: req.headers,
      },
      (upstreamRes) => {
        res.writeHead(upstreamRes.statusCode || 502, upstreamRes.headers);
        upstreamRes.pipe(res);
      },
    );

    upstream.on("error", (error) => {
      res.writeHead(502, { "content-type": "text/plain; charset=utf-8" });
      res.end(error.message);
    });

    req.pipe(upstream);
  });

  await onceServerListening(server);
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("socket proxy did not expose a TCP port");
  }
  return {
    url: `http://127.0.0.1:${address.port}`,
    async close() {
      await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    },
  };
}

function testingEnv({ runRoot, globalDir }: { runRoot: string; globalDir: string }): NodeJS.ProcessEnv {
  return {
    ...process.env,
    HOME: path.join(runRoot, "home"),
    MU_CONFIG_DIR: globalDir,
    MU_WEB_E2E_API_KEY: "mu-web-e2e-secret",
    PATH: `${path.dirname(muBinary)}${path.delimiter}${process.env.PATH || ""}`,
    XDG_RUNTIME_DIR: path.join(runRoot, "runtime"),
  };
}

export async function startHarness(): Promise<Harness> {
  log("creating temp workspace");
  const runRoot = await mkdtemp(path.join(tmpdir(), "mu-web-e2e-"));
  const projectDir = path.join(runRoot, "project");
  const globalDir = path.join(runRoot, "global-mu");
  await cp(fixtureProject, projectDir, { recursive: true });
  await cp(fixtureGlobalMu, globalDir, { recursive: true });
  await mkdir(path.join(runRoot, "home"), { recursive: true });
  await mkdir(path.join(runRoot, "runtime"), { recursive: true });

  log("starting fake provider");
  const provider = await startFakeProvider();
  const env = testingEnv({ runRoot, globalDir });

  const configBody = `{
  "providers": {
    "fake": {
      "base_url": "http://127.0.0.1:${provider.port}/v1",
      "api_key_env": "MU_WEB_E2E_API_KEY",
      "models": {
        "fake-model": {
          "context_window": 128000,
          "supported_efforts": ["low", "high"]
        }
      }
    }
  },
  "default_model": "fake/fake-model"
}
`;
  await writeFile(path.join(projectDir, ".mu", "config.jsonc"), configBody);
  await writeFile(path.join(globalDir, "config.jsonc"), configBody);

  const socketPath = path.join(runRoot, "mu-web.sock");
  log("starting web server", socketPath);
  const mu: ChildProcessWithoutNullStreams = spawn(process.execPath, [webServer, socketPath], {
    cwd: projectDir,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  mu.stderr.on("data", (chunk) => {
    stderr += chunk.toString();
  });

  log("waiting for socket");
  await waitForSocket(socketPath);
  log("starting proxy");
  const proxy = await startSocketProxy(socketPath);
  log("probing http", proxy.url);
  await waitForHttp(proxy.url);
  log("harness ready", proxy.url);

  return {
    baseUrl: proxy.url,
    projectDir,
    providerRequests: provider.requests,
    async close() {
      proxy.close().catch(() => {});
      provider.close().catch(() => {});
      if (!mu.killed) {
        mu.kill("SIGTERM");
        await new Promise((resolve) => mu.once("exit", resolve));
      }
      await rm(runRoot, { recursive: true, force: true });
      if (stderr.includes("request failed")) {
        throw new Error(stderr);
      }
    },
  };
}
