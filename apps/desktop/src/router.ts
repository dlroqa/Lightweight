/**
 * Starting, finding and stopping a Router — beside the gateway, never through
 * it.
 *
 * Deliberately free of any `electron` import, for the reason `gateway.ts` is:
 * the decisions worth getting right can then be tested without a display. Read
 * `docs/DESKTOP_ROUTER.md` for the design; the short version is that the
 * Router is a separate process on its own port, with its own configuration and
 * its own credentials, and this file is a second supervisor beside
 * `GatewaySupervisor` rather than a change to it.
 *
 * The gateway's rule holds here unchanged: **only ever stop what this process
 * started.** A Router that was serving before the shell asked is attached, and
 * nothing here ever signals it.
 *
 * Two things differ from the gateway on purpose:
 *
 * - A Router is never started just because the shell opened. `discover` only
 *   looks; `start` is called on an explicit user action.
 * - It is stopped with `SIGINT`, not `SIGTERM`. `hermes router` stops cleanly
 *   on Ctrl-C — finishing what it serves and removing its admin-token file —
 *   and `SIGTERM` would leave that file behind.
 */

import { execFile, spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { constants, promises as fs } from "node:fs";
import { dirname, join } from "node:path";

import { isHermesHealth } from "./gateway.ts";

/** The port `hermes router` uses when its file names none. */
export const ROUTER_DEFAULT_PORT = 11500;

/**
 * How a Router names itself on `GET /version`.
 *
 * The same test the panel makes (`frontend/src/state/backend.tsx`), so the
 * shell and the page it loads can never disagree about what is on a port.
 */
export const ROUTER_BUILD_PREFIX = "lightweight-router-";

/** The file the explicit "Create template" action writes. Never `router.json`. */
export const TEMPLATE_NAME = "router.template.json";

/** How long a probe waits before deciding nothing is answering. */
const PROBE_TIMEOUT_MS = 1500;

/** How long a Router gets to stop politely before it is killed. */
export const ROUTER_SHUTDOWN_GRACE_MS = 8000;

/** How long a started Router gets to begin answering. */
const START_TIMEOUT_MS = 30_000;

/** How long `validate-config` may take. It reads a file and contacts nothing. */
const VALIDATE_TIMEOUT_MS = 30_000;

/** Lines of the Router's own output kept for a failure message. */
const OUTPUT_LINES_KEPT = 12;

/**
 * What is on a port.
 *
 * `stranger` covers anything that is listening but is neither a Router nor a
 * gateway — including something that accepts a connection and never answers.
 */
export type PortHolder = "router" | "gateway" | "stranger" | "nothing";

type Fetched = { body: unknown } | "refused" | "silent" | "odd";

async function fetchJson(port: number, path: string, fetchImpl: typeof fetch): Promise<Fetched> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), PROBE_TIMEOUT_MS);
  try {
    const response = await fetchImpl(`http://127.0.0.1:${port}${path}`, {
      signal: controller.signal,
    });
    if (!response.ok) return { body: null };
    try {
      return { body: await response.json() };
    } catch {
      return { body: null };
    }
  } catch (cause) {
    if (controller.signal.aborted) return "silent";
    return refusedConnection(cause) ? "refused" : "odd";
  } finally {
    clearTimeout(timer);
  }
}

/** Whether a fetch failed because nothing was listening at all. */
function refusedConnection(cause: unknown): boolean {
  const code = (cause as { cause?: { code?: unknown } } | null)?.cause?.code;
  return code === "ECONNREFUSED";
}

/** Whether a `/version` body is a Router's. */
export function isRouterVersion(body: unknown): boolean {
  if (typeof body !== "object" || body === null) return false;
  const build = (body as Record<string, unknown>).build;
  return typeof build === "string" && build.startsWith(ROUTER_BUILD_PREFIX);
}

/** Probes of `/version` that end with no answer before the port is called taken. */
export const INCONCLUSIVE_TRIES = 3;
const INCONCLUSIVE_PAUSE_MS = 250;

/**
 * Say what is listening on `port`.
 *
 * A Router is recognised only by its own `/version`, and a gateway only by the
 * `/health` shape `GatewaySupervisor` already attaches to — the two tests
 * exclude each other, so neither supervisor can ever claim the other's process.
 */
export async function identify(port: number, fetchImpl: typeof fetch = fetch): Promise<PortHolder> {
  // A probe that times out or fails oddly says nothing about the port by
  // itself: at launch the app's own start-up can starve one past its timeout,
  // and a free port was once reported as "in use by another program" that way
  // (the v0.8.1 release run's Intel DMG check). Only a listener that stays
  // that way across every try is a stranger. A refusal or an HTTP answer is
  // decided at once, as before.
  let version = await fetchJson(port, "/version", fetchImpl);
  for (let tries = 1; (version === "silent" || version === "odd") && tries < INCONCLUSIVE_TRIES; tries += 1) {
    await new Promise((resolve) => setTimeout(resolve, INCONCLUSIVE_PAUSE_MS));
    version = await fetchJson(port, "/version", fetchImpl);
  }
  if (version === "refused") return "nothing";
  if (typeof version === "object" && isRouterVersion(version.body)) return "router";
  if (version === "silent" || version === "odd") return "stranger";
  const health = await fetchJson(port, "/health", fetchImpl);
  if (typeof health === "object" && isHermesHealth(health.body)) return "gateway";
  return "stranger";
}

/** How to run the `hermes` binary, and — for tests only — what to put before its arguments. */
export interface RouterBinary {
  binary: string;
  /** Prepended to every argument list. Lets a test run a scripted Router through `node`. */
  binaryPrefix?: string[] | undefined;
}

export interface RouterOptions extends RouterBinary {
  /** The Router's own configuration file, passed as `--config`. */
  configPath: string;
  port: number;
  /** The Gateway's port, so the two can never be configured onto one. */
  gatewayPort?: number | undefined;
  /** The built panel, the same bundle the gateway serves. */
  webRoot?: string | undefined;
  /**
   * Extra environment for the child. Desktop passes none — the Router inherits
   * exactly what a terminal launch would — and tests use it to isolate a run.
   */
  env?: Record<string, string> | undefined;
}

export interface RouterLaunch {
  command: string;
  args: string[];
}

/**
 * The command line for a Router this shell owns.
 *
 * `--listen` is always loopback. That is what makes the panel's settings
 * writable at all — the Router admits admin writes only when every listener
 * is on loopback — and it overrides the file's own `listen` for this run only;
 * the file is not touched. No key is ever on the command line: the Router
 * reads its keys from the variables its file names, or the credential store.
 */
export function planRouterLaunch(options: RouterOptions): RouterLaunch {
  const args = [
    ...(options.binaryPrefix ?? []),
    "router",
    "--config",
    options.configPath,
    "--listen",
    `127.0.0.1:${options.port}`,
  ];
  if (options.webRoot) args.push("--web-root", options.webRoot);
  return { command: options.binary, args };
}

/** The argument list for `hermes router validate-config`. */
export function planValidate(options: RouterBinary & { configPath: string }): RouterLaunch {
  return {
    command: options.binary,
    args: [...(options.binaryPrefix ?? []), "router", "validate-config", "--config", options.configPath],
  };
}

/**
 * The command a person runs to see the running Router's admin token.
 *
 * The shell shows this, never the token: the panel asks for the token, and the
 * main process deliberately never holds one.
 */
export function adminTokenCommand(binary: string, configPath: string): string {
  return `"${binary}" router admin-token --config "${configPath}"`;
}

function run(
  launch: RouterLaunch,
  env: Record<string, string> | undefined,
  timeoutMs: number,
): Promise<{ code: number | null; output: string }> {
  return new Promise((resolve) => {
    execFile(
      launch.command,
      launch.args,
      { env: { ...process.env, ...env }, timeout: timeoutMs, windowsHide: true },
      (error, stdout, stderr) => {
        const code =
          error === null ? 0 : typeof error.code === "number" ? error.code : null;
        const output = `${stderr}\n${stdout}`.trim();
        resolve({ code, output: output === "" && error ? error.message : output });
      },
    );
  });
}

/**
 * Where the Router's configuration is.
 *
 * `HERMES_ROUTER_CONFIG` when set; otherwise the binary is asked
 * (`hermes router config-path`), so the platform's directories — including the
 * Flatpak's — are decided in one place, the place `hermes router` reads them.
 */
export async function resolveConfigPath(
  binary: RouterBinary,
  override: string | undefined,
  env?: Record<string, string>,
): Promise<string> {
  if (override && override.trim() !== "") return override;
  const result = await run(
    { command: binary.binary, args: [...(binary.binaryPrefix ?? []), "router", "config-path"] },
    env,
    VALIDATE_TIMEOUT_MS,
  );
  const path = result.output.split(/\r?\n/).pop()?.trim() ?? "";
  if (result.code !== 0 || path === "") {
    throw new Error(`The Router's configuration path could not be determined.\n${result.output}`);
  }
  return path;
}

export type ConfigProblem = "missing" | "unreadable" | "malformed" | "invalid";

export type ConfigCheck = { ok: true } | { ok: false; problem: ConfigProblem; detail: string };

/**
 * Whether the file is there, readable, and JSON at all.
 *
 * Only the shape is judged here; whether it is a *valid* Router configuration
 * is `hermes router validate-config`'s question. The detail never quotes the
 * file — it holds no secrets by design, but there is no reason to echo it.
 */
export async function inspectConfigFile(path: string): Promise<ConfigCheck> {
  let text: string;
  try {
    text = await fs.readFile(path, "utf8");
  } catch (cause) {
    const code = (cause as NodeJS.ErrnoException).code;
    if (code === "ENOENT") {
      return { ok: false, problem: "missing", detail: `There is no Router configuration at ${path}.` };
    }
    return {
      ok: false,
      problem: "unreadable",
      detail: `The Router configuration at ${path} could not be read (${code ?? "unknown error"}).`,
    };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { ok: false, problem: "malformed", detail: `${path} is not valid JSON.` };
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return { ok: false, problem: "malformed", detail: `${path} is not a JSON object.` };
  }
  return { ok: true };
}

/** Ask the binary whether the file is a configuration it would start with. */
export async function validateConfig(options: RouterOptions): Promise<ConfigCheck> {
  const result = await run(planValidate(options), options.env, VALIDATE_TIMEOUT_MS);
  if (result.code === 0) return { ok: true };
  return { ok: false, problem: "invalid", detail: result.output };
}

/**
 * The first-run template's text.
 *
 * A starting point and nothing more: its one node is this Desktop's own
 * Gateway and its one route names a model that is obviously a placeholder. No
 * key, no remote node, and it is never loaded — the person edits it and saves
 * it as `router.json` themselves.
 */
export function templateText(gatewayPort: number, routerPort: number): string {
  const template = {
    listen: [`127.0.0.1:${routerPort}`],
    nodes: [{ id: "this-gateway", url: `http://127.0.0.1:${gatewayPort}` }],
    routes: [
      {
        name: "Example",
        description: "TEMPLATE - replace with a route of your own",
        deployments: [{ node: "this-gateway", model: "REPLACE-WITH-A-MODEL-THIS-GATEWAY-SERVES" }],
      },
    ],
  };
  return `${JSON.stringify(template, null, 2)}\n`;
}

/**
 * Write the template beside where `router.json` belongs, without ever
 * overwriting anything.
 *
 * Atomic and no-clobber: the text goes to a private temporary file, which is
 * then hard-linked into place. A link fails if the name exists, so an earlier
 * template — or anything else of that name — is left exactly as it was. Where
 * hard links are unavailable, an exclusive copy keeps the no-clobber promise.
 */
export async function writeTemplate(
  configPath: string,
  gatewayPort: number,
  routerPort: number,
): Promise<string> {
  const directory = dirname(configPath);
  await fs.mkdir(directory, { recursive: true, mode: 0o700 });
  const target = join(directory, TEMPLATE_NAME);
  const temporary = join(directory, `.${TEMPLATE_NAME}.${randomBytes(6).toString("hex")}.tmp`);
  await fs.writeFile(temporary, templateText(gatewayPort, routerPort), { mode: 0o600, flag: "wx" });
  try {
    try {
      await fs.link(temporary, target);
    } catch (cause) {
      const code = (cause as NodeJS.ErrnoException).code;
      if (code === "EEXIST") throw cause;
      await fs.copyFile(temporary, target, constants.COPYFILE_EXCL);
    }
  } catch (cause) {
    if ((cause as NodeJS.ErrnoException).code === "EEXIST") {
      throw new Error(`${target} already exists; it was left unchanged.`);
    }
    throw cause;
  } finally {
    await fs.rm(temporary, { force: true });
  }
  return target;
}

export type RouterState =
  | { kind: "off"; port: number }
  | { kind: "needs-config"; configPath: string; problem: ConfigProblem; detail: string }
  | { kind: "starting"; port: number }
  | { kind: "running"; port: number; pid: number }
  | { kind: "attached"; port: number }
  | { kind: "conflict"; port: number; holder: "gateway" | "stranger"; detail: string }
  | { kind: "failed"; reason: string };

/** Whether the Router's panel can be opened in this state. */
export function isServing(state: RouterState): state is Extract<RouterState, { kind: "running" | "attached" }> {
  return state.kind === "running" || state.kind === "attached";
}

/** One short line for the tray and the Router menu. */
export function describeRouterState(state: RouterState): string {
  switch (state.kind) {
    case "off":
      return "Not started";
    case "needs-config":
      return state.problem === "missing" ? "Needs configuration" : `Configuration ${state.problem}`;
    case "starting":
      return "Starting…";
    case "running":
      return `Running on port ${state.port}`;
    case "attached":
      return `Attached to port ${state.port}`;
    case "conflict":
      return `Port ${state.port} is in use by ${state.holder === "gateway" ? "a Gateway" : "another program"}`;
    case "failed":
      return "Unavailable";
  }
}

/** Whether the Router's own words say its port was taken. */
export function looksLikePortConflict(text: string): boolean {
  return /already listening|address (already )?in use|EADDRINUSE|only one usage of each socket address/i.test(text);
}

function conflictDetail(port: number, holder: "gateway" | "stranger"): string {
  return (
    `Port ${port} is already in use by ${holder === "gateway" ? "a Lightweight Gateway" : "another program"}, ` +
    `so no Router was started and nothing on that port was touched. Set HERMES_ROUTER_PORT ` +
    `to a free port before launching, or stop whatever holds ${port}.`
  );
}

/**
 * A Router this shell either started or found.
 */
export class RouterSupervisor {
  private child: ChildProcess | null = null;
  private state: RouterState;
  private readonly listeners = new Set<(state: RouterState) => void>();
  /** True only when this process started the Router. See `GatewaySupervisor.owned`. */
  private owned = false;
  private lastOptions: RouterOptions | null = null;
  private lastOutput: string[] = [];

  constructor(port: number = ROUTER_DEFAULT_PORT) {
    this.state = { kind: "off", port };
  }

  onChange(listener: (state: RouterState) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  current(): RouterState {
    return this.state;
  }

  ownsProcess(): boolean {
    return this.owned;
  }

  private set(state: RouterState) {
    this.state = state;
    for (const listener of this.listeners) listener(state);
  }

  /**
   * Look at the port, and nothing more.
   *
   * Called at launch so the tray can say whether a Router is already serving.
   * It never starts one, and it leaves a Router this shell owns alone.
   */
  async discover(port: number, fetchImpl: typeof fetch = fetch): Promise<RouterState> {
    if (this.owned || this.state.kind === "starting") return this.state;
    const holder = await identify(port, fetchImpl);
    if (holder === "router") this.set({ kind: "attached", port });
    else if (holder === "nothing") this.set({ kind: "off", port });
    else this.set({ kind: "conflict", port, holder, detail: conflictDetail(port, holder) });
    return this.state;
  }

  /**
   * Attach to a Router already serving on the port, or start one — on an
   * explicit request only.
   *
   * The order matters. An existing Router is attached before the file is even
   * read: it was started by someone else with their own file, and Desktop's
   * opinion of Desktop's file is not a reason to refuse to open it. A port held
   * by anything else is reported, never fought over.
   */
  async start(options: RouterOptions, fetchImpl: typeof fetch = fetch): Promise<RouterState> {
    if (this.owned && this.child) return this.state;
    this.lastOptions = options;
    const { port } = options;

    if (options.gatewayPort !== undefined && options.gatewayPort === port) {
      this.set({
        kind: "conflict",
        port,
        holder: "gateway",
        detail:
          `The Router and the Gateway are both configured for port ${port}. They are separate ` +
          `services and need separate ports: set HERMES_ROUTER_PORT (default ${ROUTER_DEFAULT_PORT}) ` +
          `to a different port before launching.`,
      });
      return this.state;
    }

    const holder = await identify(port, fetchImpl);
    if (holder === "router") {
      this.owned = false;
      this.set({ kind: "attached", port });
      return this.state;
    }
    if (holder !== "nothing") {
      this.set({ kind: "conflict", port, holder, detail: conflictDetail(port, holder) });
      return this.state;
    }

    const file = await inspectConfigFile(options.configPath);
    const checked = file.ok ? await validateConfig(options) : file;
    if (!checked.ok) {
      this.set({
        kind: "needs-config",
        configPath: options.configPath,
        problem: checked.problem,
        detail: checked.detail,
      });
      return this.state;
    }

    this.set({ kind: "starting", port });
    const launch = planRouterLaunch(options);
    try {
      const child = spawn(launch.command, launch.args, {
        env: { ...process.env, ...options.env },
        stdio: ["ignore", "pipe", "pipe"],
        // This shell's child, as the gateway is: a Router outliving the window
        // that started it is an orphan nobody can see.
        detached: false,
        windowsHide: true,
      });
      this.child = child;
      this.owned = true;
      this.lastOutput = [];

      const remember = (chunk: Buffer | string) => {
        for (const line of String(chunk).split(/\r?\n/)) {
          const text = line.trim();
          if (text === "") continue;
          this.lastOutput.push(text);
          if (this.lastOutput.length > OUTPUT_LINES_KEPT) this.lastOutput.shift();
        }
      };
      child.stdout?.on("data", remember);
      child.stderr?.on("data", remember);

      child.on("exit", (code, signal) => {
        if (this.child === child) this.child = null;
        this.owned = false;
        if (this.state.kind === "off" || this.state.kind === "starting") return;
        this.set({
          kind: "failed",
          reason:
            signal !== null
              ? `The Router was stopped by ${signal}.${this.explain()}`
              : `The Router exited with code ${code ?? "unknown"}.${this.explain()}`,
        });
      });
      child.on("error", (error) => {
        // Quoted by the start path's failure message, which is the one shown.
        this.lastOutput.push(error.message);
        if (this.child === child) this.child = null;
        this.owned = false;
        if (this.state.kind !== "starting") this.set({ kind: "failed", reason: error.message });
      });

      await this.waitUntilServing(child, port, fetchImpl);
      this.set({ kind: "running", port, pid: child.pid ?? -1 });
    } catch (cause) {
      // Stopped while it was starting: that was asked for, and is not a failure.
      if (this.state.kind !== "starting") return this.state;
      const reason = cause instanceof Error ? cause.message : String(cause);
      if (looksLikePortConflict(reason)) {
        this.set({ kind: "conflict", port, holder: "stranger", detail: `${conflictDetail(port, "stranger")}\n\n${reason}` });
      } else {
        this.set({ kind: "failed", reason });
      }
    }
    return this.state;
  }

  private explain(): string {
    if (this.lastOutput.length === 0) return "";
    return `\n\nIt said:\n${this.lastOutput.join("\n")}`;
  }

  /**
   * Wait for *this* child to answer as a Router, or give up.
   *
   * Polled, and only while the child is alive: a Router that exits — a port
   * taken between the check and the bind, a key its file names that is not
   * set — is reported with its own last words rather than waited out.
   */
  private async waitUntilServing(
    child: ChildProcess,
    port: number,
    fetchImpl: typeof fetch,
    timeoutMs = START_TIMEOUT_MS,
  ): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (this.child !== child || child.exitCode !== null || child.signalCode !== null) {
        // Let the last of its output arrive before quoting it.
        await delay(100);
        throw new Error(`The Router stopped before it began serving.${this.explain()}`);
      }
      if ((await identify(port, fetchImpl)) === "router" && this.child === child) return;
      if (Date.now() > deadline) {
        throw new Error(
          `The Router did not start answering on port ${port} within ${timeoutMs / 1000} seconds.${this.explain()}`,
        );
      }
      await delay(200);
    }
  }

  /**
   * Stop the Router, but only if this shell started it.
   *
   * `SIGINT` first — `hermes router`'s clean stop, which removes its admin
   * token — and `SIGKILL` only after the grace period. An attached Router is
   * never signalled; the shell simply stops showing it as its own.
   */
  async stop(graceMs = ROUTER_SHUTDOWN_GRACE_MS): Promise<void> {
    const child = this.child;
    const owned = this.owned;
    const port = this.lastOptions?.port ?? ("port" in this.state ? this.state.port : ROUTER_DEFAULT_PORT);
    this.set({ kind: "off", port });
    if (!child || !owned) {
      this.child = null;
      return;
    }
    if (child.exitCode === null && child.signalCode === null) {
      const exited = new Promise<boolean>((resolve) => child.once("exit", () => resolve(true)));
      child.kill("SIGINT");
      const stopped = await Promise.race([exited, delay(graceMs).then(() => false)]);
      if (!stopped) {
        child.kill("SIGKILL");
        await Promise.race([exited, delay(graceMs)]);
      }
    }
    this.child = null;
    this.owned = false;
  }

  /**
   * Stop this shell's Router and start it again with the same options.
   *
   * How saved Jev Settings take effect: the Router reads them at start. Only a
   * Router this shell owns is ever restarted — an attached one belongs to
   * whoever started it.
   */
  async restart(fetchImpl: typeof fetch = fetch): Promise<RouterState> {
    if (!this.owned || !this.lastOptions) {
      throw new Error("this Router was not started by this app, so it cannot be restarted here");
    }
    const options = this.lastOptions;
    await this.stop();
    return this.start(options, fetchImpl);
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
