/**
 * The Router supervisor's decisions, tested without a display.
 *
 * Process behaviour — attach versus own, the clean stop, restart, a port taken
 * mid-start — runs against `test-fixtures/fake-router.mjs`, a scripted Router
 * started through `node`, so these run everywhere `npm test` does. The same
 * claims against the real binary are in `router.integration.test.ts`.
 */

import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, stat, writeFile, chmod } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import { createServer as createTcpServer, type Server as TcpServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, it } from "node:test";

import {
  ROUTER_BUILD_PREFIX,
  RouterSupervisor,
  TEMPLATE_NAME,
  adminTokenCommand,
  describeRouterState,
  identify,
  inspectConfigFile,
  isRouterVersion,
  looksLikePortConflict,
  planRouterLaunch,
  planValidate,
  resolveConfigPath,
  templateText,
  writeTemplate,
  type RouterOptions,
} from "./router.ts";

const FAKE = join(import.meta.dirname, "..", "test-fixtures", "fake-router.mjs");
const fakeBinary = { binary: process.execPath, binaryPrefix: [FAKE] };

const json = (body: unknown, status = 200): Response =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const refused = (): never => {
  throw Object.assign(new TypeError("fetch failed"), { cause: { code: "ECONNREFUSED" } });
};

/** A fake fetch answering per path. */
function answering(routes: Record<string, () => Response>): typeof fetch {
  return (async (input: string | URL | Request) => {
    const path = new URL(String(input)).pathname;
    const route = routes[path];
    return route ? route() : new Response("not found", { status: 404 });
  }) as typeof fetch;
}

describe("the Router's command line", () => {
  it("is hermes router with its own config, a loopback listener and the panel", () => {
    const launch = planRouterLaunch({
      binary: "/opt/hermes",
      configPath: "/home/u/.config/x/router.json",
      port: 11500,
      webRoot: "/app/resources/panel",
    });
    assert.equal(launch.command, "/opt/hermes");
    assert.deepEqual(launch.args, [
      "router",
      "--config",
      "/home/u/.config/x/router.json",
      "--listen",
      "127.0.0.1:11500",
      "--web-root",
      "/app/resources/panel",
    ]);
  });

  it("is always loopback, whatever the port", () => {
    const { args } = planRouterLaunch({ binary: "h", configPath: "c", port: 18555 });
    assert.equal(args[args.indexOf("--listen") + 1], "127.0.0.1:18555");
    assert.equal(args.includes("--web-root"), false, "no web root was given, so none is passed");
  });

  it("never carries a key, and never a gateway argument", () => {
    const { args } = planRouterLaunch({ binary: "h", configPath: "c", port: 1, webRoot: "w" });
    for (const forbidden of ["serve", "--port", "--host", "--api-key", "--key"]) {
      assert.equal(args.includes(forbidden), false, `${forbidden} is not a router argument`);
    }
  });

  it("validates with the same file", () => {
    assert.deepEqual(planValidate({ binary: "h", configPath: "/c.json" }).args, [
      "router",
      "validate-config",
      "--config",
      "/c.json",
    ]);
  });

  it("shows the admin-token command, not a token", () => {
    assert.equal(
      adminTokenCommand("/app/bin/hermes", "/cfg/router.json"),
      '"/app/bin/hermes" router admin-token --config "/cfg/router.json"',
    );
  });
});

describe("recognising what is on the Router's port", () => {
  it("knows a Router by its own build name", () => {
    assert.equal(isRouterVersion({ build: `${ROUTER_BUILD_PREFIX}0.8.0` }), true);
    assert.equal(isRouterVersion({ build: "lightweight-0.8.0" }), false);
    assert.equal(isRouterVersion({ version: "0.8.0" }), false);
    assert.equal(isRouterVersion(null), false);
  });

  it("identifies a Router", async () => {
    const fetchImpl = answering({ "/version": () => json({ build: "lightweight-router-0.8.0" }) });
    assert.equal(await identify(11500, fetchImpl), "router");
  });

  it("identifies a Gateway, and does not call it a Router", async () => {
    const fetchImpl = answering({
      "/version": () => json({ version: "0.8.0", build: "lightweight-0.8.0" }),
      "/health": () => json({ status: "ok", backend: "llamacpp-process", model: null }),
    });
    assert.equal(await identify(11500, fetchImpl), "gateway");
  });

  it("calls anything else a stranger", async () => {
    const fetchImpl = answering({ "/health": () => json({ status: "ok" }) });
    assert.equal(await identify(11500, fetchImpl), "stranger");
  });

  it("calls a refused connection nothing", async () => {
    assert.equal(await identify(11500, refused as unknown as typeof fetch), "nothing");
  });

  it("calls a listener that fails oddly a stranger, not nothing", async () => {
    const fetchImpl = (async () => {
      throw new TypeError("other side closed");
    }) as typeof fetch;
    assert.equal(await identify(11500, fetchImpl), "stranger");
  });

  it("calls a listener that never answers a stranger", async () => {
    const silent = createTcpServer(() => {
      // Accept, and say nothing.
    });
    const port = await listenOn(silent);
    try {
      assert.equal(await identify(port), "stranger");
    } finally {
      silent.close();
    }
  });
});

describe("the Router's configuration", () => {
  let dir = "";
  before(async () => {
    dir = await mkdtemp(join(tmpdir(), "hermes-router-config-"));
  });
  after(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  it("is missing when there is no file", async () => {
    const check = await inspectConfigFile(join(dir, "absent.json"));
    assert.deepEqual(check.ok ? null : check.problem, "missing");
  });

  it("is malformed when it is not JSON, or not an object", async () => {
    for (const [name, text] of [
      ["bad.json", "{ nodes: ["],
      ["array.json", "[]"],
      ["string.json", '"router"'],
    ] as const) {
      const path = join(dir, name);
      await writeFile(path, text);
      const check = await inspectConfigFile(path);
      assert.equal(check.ok ? null : check.problem, "malformed", name);
      assert.equal(check.ok ? "" : check.detail.includes(text), false, "the file's text is not echoed");
    }
  });

  it("is unreadable when it cannot be read", { skip: process.platform === "win32" || process.getuid?.() === 0 }, async () => {
    const path = join(dir, "locked.json");
    await writeFile(path, "{}");
    await chmod(path, 0o000);
    try {
      const check = await inspectConfigFile(path);
      assert.equal(check.ok ? null : check.problem, "unreadable");
    } finally {
      await chmod(path, 0o600);
    }
  });

  it("passes the shape check when it is a JSON object", async () => {
    const path = join(dir, "ok.json");
    await writeFile(path, '{"nodes": []}');
    assert.deepEqual(await inspectConfigFile(path), { ok: true });
  });

  it("is found by asking the binary, or by the override", async () => {
    assert.equal(await resolveConfigPath(fakeBinary, "/explicit/router.json"), "/explicit/router.json");
    const asked = await resolveConfigPath(fakeBinary, undefined, {
      FAKE_ROUTER_DEFAULT_CONFIG: "/from/the/binary/router.json",
    });
    assert.equal(asked, "/from/the/binary/router.json");
  });
});

describe("the first-run template", () => {
  let dir = "";
  before(async () => {
    dir = await mkdtemp(join(tmpdir(), "hermes-router-template-"));
  });
  after(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  it("holds no key, no remote node, and an obvious placeholder", () => {
    const text = templateText(11434, 11500);
    const parsed = JSON.parse(text) as {
      listen: string[];
      nodes: { url: string }[];
      api_key_env?: unknown;
    };
    assert.deepEqual(parsed.listen, ["127.0.0.1:11500"]);
    assert.deepEqual(
      parsed.nodes.map((node) => node.url),
      ["http://127.0.0.1:11434"],
      "its only node is this machine's Gateway",
    );
    assert.equal(parsed.api_key_env, undefined);
    assert.match(text, /REPLACE-WITH-A-MODEL/);
    assert.match(text, /TEMPLATE/);
    assert.doesNotMatch(text, /api_key|secret|token|auto_route|jev/i);
  });

  it("is written beside router.json, privately, and never as router.json", async () => {
    const configPath = join(dir, "fresh", "router.json");
    const written = await writeTemplate(configPath, 11434, 11500);
    assert.equal(written, join(dir, "fresh", TEMPLATE_NAME));
    assert.equal(await readFile(written, "utf8"), templateText(11434, 11500));
    await assert.rejects(stat(configPath), "router.json itself was not created");
    if (process.platform !== "win32") {
      assert.equal((await stat(written)).mode & 0o777, 0o600);
    }
  });

  it("never overwrites an existing file", async () => {
    const configPath = join(dir, "kept", "router.json");
    await writeTemplate(configPath, 11434, 11500);
    const target = join(dir, "kept", TEMPLATE_NAME);
    await writeFile(target, "edited by a person");
    await assert.rejects(writeTemplate(configPath, 1, 2), /already exists/);
    assert.equal(await readFile(target, "utf8"), "edited by a person");
  });
});

describe("describing the Router's state", () => {
  it("says running, attached, unavailable and needs configuration distinctly", () => {
    const lines = new Set([
      describeRouterState({ kind: "off", port: 11500 }),
      describeRouterState({ kind: "running", port: 11500, pid: 1 }),
      describeRouterState({ kind: "attached", port: 11500 }),
      describeRouterState({ kind: "failed", reason: "x" }),
      describeRouterState({ kind: "needs-config", configPath: "c", problem: "missing", detail: "d" }),
      describeRouterState({ kind: "conflict", port: 11500, holder: "gateway", detail: "d" }),
    ]);
    assert.equal(lines.size, 6);
  });

  it("recognises the Router's port-conflict wording", () => {
    assert.equal(looksLikePortConflict("Address already in use (os error 98)"), true);
    assert.equal(looksLikePortConflict("the configuration lists no nodes"), false);
  });
});

describe("the Router supervisor", () => {
  let dir = "";
  let validConfig = "";
  let invalidConfig = "";

  before(async () => {
    dir = await mkdtemp(join(tmpdir(), "hermes-router-supervisor-"));
    validConfig = join(dir, "router.json");
    invalidConfig = join(dir, "empty-router.json");
    await writeFile(validConfig, JSON.stringify({ nodes: [{ id: "n", url: "http://127.0.0.1:1" }] }));
    await writeFile(invalidConfig, JSON.stringify({ nodes: [] }));
  });
  after(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  const options = async (overrides: Partial<RouterOptions> = {}): Promise<RouterOptions> => ({
    ...fakeBinary,
    configPath: validConfig,
    port: await freePort(),
    ...overrides,
  });

  it("discovering only looks: nothing is started", async () => {
    const port = await freePort();
    const supervisor = new RouterSupervisor(port);
    const state = await supervisor.discover(port);
    assert.deepEqual(state, { kind: "off", port });
    assert.equal(supervisor.ownsProcess(), false);
    assert.equal(await identify(port), "nothing", "discover started something");
  });

  it("starts a Router it owns, and stops it with SIGINT", async () => {
    const signalFile = join(dir, "signal");
    const opts = await options({ env: { FAKE_ROUTER_SIGNAL_FILE: signalFile } });
    const supervisor = new RouterSupervisor(opts.port);
    const state = await supervisor.start(opts);
    assert.equal(state.kind, "running", JSON.stringify(state));
    assert.equal(supervisor.ownsProcess(), true);
    assert.equal(await identify(opts.port), "router");

    const pid = state.kind === "running" ? state.pid : -1;
    await supervisor.stop();
    assert.deepEqual(supervisor.current(), { kind: "off", port: opts.port });
    assert.equal(supervisor.ownsProcess(), false);
    assert.equal(alive(pid), false, "the Router process is still alive");
    assert.equal(await identify(opts.port), "nothing");
    if (process.platform !== "win32") {
      // Windows has no SIGINT for a child; `kill` ends it whatever is asked.
      assert.equal(await readFile(signalFile, "utf8"), "SIGINT", "the clean stop was not used");
    }
  });

  it("attaches to a Router it did not start, and never stops it", async () => {
    const opts = await options();
    const owner = new RouterSupervisor(opts.port);
    await owner.start(opts);
    assert.equal(owner.current().kind, "running");

    const second = new RouterSupervisor(opts.port);
    assert.deepEqual(await second.discover(opts.port), { kind: "attached", port: opts.port });
    assert.deepEqual(await second.start(opts), { kind: "attached", port: opts.port });
    assert.equal(second.ownsProcess(), false);

    await second.stop();
    assert.equal(await identify(opts.port), "router", "stopping an attached Router killed it");
    await assert.rejects(second.restart(), /not started by this app/);
    assert.equal(await identify(opts.port), "router", "restarting an attached Router touched it");

    await owner.stop();
  });

  it("attaches to an existing Router before judging its own configuration", async () => {
    const opts = await options();
    const owner = new RouterSupervisor(opts.port);
    await owner.start(opts);
    const second = new RouterSupervisor(opts.port);
    const state = await second.start({ ...opts, configPath: join(dir, "absent.json") });
    assert.equal(state.kind, "attached");
    await owner.stop();
  });

  it("restarts only what it owns, on the same port", async () => {
    const opts = await options();
    const supervisor = new RouterSupervisor(opts.port);
    const first = await supervisor.start(opts);
    const restarted = await supervisor.restart();
    assert.equal(restarted.kind, "running");
    assert.notEqual(
      restarted.kind === "running" && first.kind === "running" ? restarted.pid : 0,
      first.kind === "running" ? first.pid : 0,
      "a restart is a new process",
    );
    await supervisor.stop();
  });

  it("reports a missing configuration without starting anything", async () => {
    const opts = await options({ configPath: join(dir, "absent.json") });
    const supervisor = new RouterSupervisor(opts.port);
    const state = await supervisor.start(opts);
    assert.equal(state.kind, "needs-config");
    assert.equal(state.kind === "needs-config" && state.problem, "missing");
    assert.equal(supervisor.ownsProcess(), false);
    assert.equal(await identify(opts.port), "nothing");
  });

  it("reports a malformed configuration without starting anything", async () => {
    const path = join(dir, "malformed.json");
    await writeFile(path, "{ not json");
    const opts = await options({ configPath: path });
    const state = await new RouterSupervisor(opts.port).start(opts);
    assert.equal(state.kind === "needs-config" && state.problem, "malformed");
  });

  it("reports an invalid configuration in the Router's own words", async () => {
    const opts = await options({ configPath: invalidConfig });
    const state = await new RouterSupervisor(opts.port).start(opts);
    assert.equal(state.kind === "needs-config" && state.problem, "invalid");
    assert.match(state.kind === "needs-config" ? state.detail : "", /lists no nodes/);
    assert.equal(await identify(opts.port), "nothing");
  });

  it("does not take a port a Gateway holds, and does not touch the Gateway", async () => {
    const gateway = createServer((request, response) => {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify(
          request.url === "/health"
            ? { status: "ok", backend: "llamacpp-process" }
            : { version: "0.8.0", build: "lightweight-0.8.0" },
        ),
      );
    });
    const port = await listenOn(gateway);
    try {
      const supervisor = new RouterSupervisor(port);
      const state = await supervisor.start(await options({ port }));
      assert.equal(state.kind, "conflict");
      assert.equal(state.kind === "conflict" && state.holder, "gateway");
      assert.equal(supervisor.ownsProcess(), false);
      assert.equal(await identify(port), "gateway", "the Gateway is still serving");
    } finally {
      gateway.close();
    }
  });

  it("does not take a port a stranger holds", async () => {
    const stranger = createServer((_request, response) => {
      response.writeHead(200);
      response.end("hello");
    });
    const port = await listenOn(stranger);
    try {
      const state = await new RouterSupervisor(port).start(await options({ port }));
      assert.equal(state.kind === "conflict" && state.holder, "stranger");
    } finally {
      stranger.close();
    }
  });

  it("refuses a Router port equal to the Gateway's", async () => {
    const opts = await options();
    const state = await new RouterSupervisor(opts.port).start({ ...opts, gatewayPort: opts.port });
    assert.equal(state.kind, "conflict");
    assert.match(state.kind === "conflict" ? state.detail : "", /HERMES_ROUTER_PORT/);
  });

  it("calls a port taken between the check and the bind a conflict", async () => {
    const opts = await options({ env: { FAKE_ROUTER_MODE: "exit" } });
    const supervisor = new RouterSupervisor(opts.port);
    const state = await supervisor.start(opts);
    assert.equal(state.kind, "conflict", JSON.stringify(state));
    assert.match(state.kind === "conflict" ? state.detail : "", /Address already in use/);
    assert.equal(supervisor.ownsProcess(), false);
  });

  it("quotes a Router that died before serving, in its own words", async () => {
    const opts = await options({ env: { FAKE_ROUTER_MODE: "crash" } });
    const supervisor = new RouterSupervisor(opts.port);
    const state = await supervisor.start(opts);
    assert.equal(state.kind, "failed", JSON.stringify(state));
    assert.match(state.kind === "failed" ? state.reason : "", /stopped before it began serving/);
    assert.match(state.kind === "failed" ? state.reason : "", /NODE_KEY it names is not set/);
    assert.equal(supervisor.ownsProcess(), false);
  });

  it("reports a binary that cannot be run, and runs nothing", async () => {
    // The check runs the binary before anything is spawned to serve, so a
    // missing binary is caught there, in the operating system's own words.
    const opts = await options({ binary: join(dir, "no-such-binary"), binaryPrefix: [] });
    const supervisor = new RouterSupervisor(opts.port);
    const state = await supervisor.start(opts);
    assert.equal(state.kind, "needs-config", JSON.stringify(state));
    assert.match(state.kind === "needs-config" ? state.detail : "", /ENOENT/);
    assert.equal(supervisor.ownsProcess(), false);
    assert.equal(await identify(opts.port), "nothing");
  });

  it("gives up on a Router that never answers, and does not leave it running", async () => {
    // Only the fake can be told to hang; the deadline is the supervisor's own.
    const opts = await options({ env: { FAKE_ROUTER_MODE: "hang" } });
    const supervisor = new RouterSupervisor(opts.port);
    const starting = supervisor.start(opts);
    await new Promise((resolve) => setTimeout(resolve, 500));
    assert.equal(supervisor.current().kind, "starting");
    assert.equal(supervisor.ownsProcess(), true);
    await supervisor.stop(2000);
    assert.equal(supervisor.ownsProcess(), false);
    const state = await starting;
    assert.equal(state.kind, "off", "a stop asked for during start is not reported as a failure");
  });
});

async function listenOn(server: Server | TcpServer): Promise<number> {
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("no port");
  return address.port;
}

async function freePort(): Promise<number> {
  const server = createTcpServer();
  const port = await listenOn(server);
  await new Promise((resolve) => server.close(resolve));
  return port;
}

function alive(pid: number): boolean {
  if (pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}
