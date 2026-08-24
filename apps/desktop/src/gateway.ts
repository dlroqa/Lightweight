/**
 * Starting, finding and stopping the gateway.
 *
 * Deliberately free of any `electron` import so that the decisions worth
 * getting right — is one already running, is it *ours*, which binary, and how
 * to stop what we started — can be tested without a display.
 *
 * The rule that shapes everything here: **only ever stop what this process
 * started.** A gateway that was already serving when the shell opened belongs
 * to someone else; attaching to it is useful, killing it on quit would be
 * taking down a service the user did not ask us to touch.
 */

import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { join } from "node:path";

/** The port `hermes serve` uses when nothing says otherwise. */
export const DEFAULT_PORT = 11434;

/** How long a probe waits before deciding nothing is there. */
const PROBE_TIMEOUT_MS = 1500;

/** How long a gateway gets to stop politely before it is killed. */
export const SHUTDOWN_GRACE_MS = 8000;

/** Lines of the gateway's own output kept for a failure message. */
const OUTPUT_LINES_KEPT = 12;

export interface HealthReport {
  status: string;
  backend: string;
  model?: string | null;
}

/**
 * Ask whether a Hermes gateway is answering on `port`.
 *
 * `null` covers three different situations that all mean "do not attach":
 * nothing is listening, something is listening but is not a Hermes gateway,
 * and the answer did not arrive in time.
 *
 * The second case is the one worth the extra check. An open port is not an
 * invitation — Ollama also defaults to 11434 — and attaching to a stranger's
 * service would point the panel at an API that answers some of the same paths
 * with different meanings.
 */
export async function probe(
  port: number,
  fetchImpl: typeof fetch = fetch,
): Promise<HealthReport | null> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), PROBE_TIMEOUT_MS);
  try {
    const response = await fetchImpl(`http://127.0.0.1:${port}/health`, {
      signal: controller.signal,
    });
    if (!response.ok) return null;
    const body: unknown = await response.json();
    return isHermesHealth(body) ? body : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Whether a `/health` body is one of ours.
 *
 * Both fields, not either: `status` alone is too common a shape to identify
 * anything, and `backend` is the field that names the engine this gateway
 * supervises.
 */
export function isHermesHealth(body: unknown): body is HealthReport {
  if (typeof body !== "object" || body === null) return false;
  const candidate = body as Record<string, unknown>;
  return (
    typeof candidate.status === "string" && typeof candidate.backend === "string"
  );
}

export interface BinarySearch {
  /** An explicit override, honoured before anything else. */
  override?: string | undefined;
  /** Where a packaged build keeps the binary it ships. */
  resourcesPath?: string | undefined;
  /** The repository root, when running from a checkout. */
  repoRoot?: string | undefined;
}

/**
 * Where the `hermes` binary might be, in the order it should be looked for.
 *
 * Ordered so a developer's build wins over a stale packaged copy, and an
 * explicit override wins over both — the last thing anyone debugging this wants
 * is to be silently running a different binary than the one they just built.
 */
export function candidatePaths(search: BinarySearch): string[] {
  const executable = process.platform === "win32" ? "hermes.exe" : "hermes";
  const candidates: string[] = [];

  if (search.override) candidates.push(search.override);
  if (search.repoRoot) {
    candidates.push(join(search.repoRoot, "target", "release", executable));
    candidates.push(join(search.repoRoot, "target", "debug", executable));
  }
  if (search.resourcesPath) {
    candidates.push(join(search.resourcesPath, "bin", executable));
  }
  return candidates;
}

/**
 * The first candidate that exists.
 *
 * Throws naming every place it looked rather than returning `null`. "The
 * gateway could not be started" is not an actionable sentence; a list of paths
 * is.
 */
export function resolveBinary(
  search: BinarySearch,
  exists: (path: string) => boolean = existsSync,
): string {
  const candidates = candidatePaths(search);
  const found = candidates.find(exists);
  if (found) return found;
  throw new Error(
    `The hermes binary could not be found. Looked in:\n${candidates
      .map((path) => `  ${path}`)
      .join("\n")}\nBuild it with \`cargo build --release\`, or set HERMES_BIN.`,
  );
}

export interface LaunchOptions {
  binary: string;
  port: number;
  /** The built panel to serve at `/`. */
  webRoot?: string | undefined;
  /**
   * Addresses to bind beyond loopback.
   *
   * Empty means loopback only, which is the default and the only case that
   * needs no key.
   */
  hosts?: string[];
  /** Where the gateway keeps its data; passed through the environment. */
  home?: string | undefined;
}

export interface Launch {
  argv: string[];
  env: Record<string, string>;
  /** Present only when a key was required, so the shell can show it once. */
  apiKey?: string;
}

/**
 * The command line and environment for a gateway this shell owns.
 *
 * The key, when there is one, travels in the environment and never in `argv`.
 * `/proc/<pid>/cmdline` is world-readable, which is why M3.5 moved the engine's
 * own key out of its arguments; the same reasoning applies to ours.
 *
 * A loopback-only gateway is given no key at all. That is not laziness: the
 * gateway itself only requires one when a bind is reachable from another
 * machine, and inventing a key for a purely local socket would mean the panel
 * has to carry a credential for no gain.
 */
export function planLaunch(options: LaunchOptions): Launch {
  const argv = ["serve", "--port", String(options.port)];
  for (const host of options.hosts ?? []) {
    argv.push("--host", host);
  }
  if (options.webRoot) {
    argv.push("--web-root", options.webRoot);
  }

  const env: Record<string, string> = {};
  if (options.home) env.HERMES_GATEWAY_HOME = options.home;

  const exposed = (options.hosts ?? []).some(isNonLoopback);
  if (!exposed) return { argv, env };

  const apiKey = randomBytes(32).toString("base64url");
  env.HERMES_API_KEY = apiKey;
  return { argv, env, apiKey };
}

/**
 * Whether a `--host` value would be reachable from another machine.
 *
 * Conservative in the safe direction: anything not recognisably local is
 * treated as exposed, so an unfamiliar value gets a key rather than silently
 * going without one. A hostname that resolves only to loopback will simply have
 * a key it does not need, and the gateway diagnoses that case itself.
 */
export function isNonLoopback(host: string): boolean {
  const value = host.trim().toLowerCase();
  if (value === "") return false;
  if (value === "localhost" || value.endsWith(".localhost")) return false;
  if (value === "127.0.0.1" || value.startsWith("127.")) return false;
  if (value === "::1" || value === "[::1]") return false;
  return true;
}

export type GatewayState =
  | { kind: "stopped" }
  | { kind: "attached"; port: number; health: HealthReport }
  | { kind: "starting"; port: number }
  | { kind: "running"; port: number; pid: number; apiKey?: string }
  | { kind: "failed"; reason: string };

/**
 * A gateway this shell either started or found.
 */
export class GatewaySupervisor {
  private child: ChildProcess | null = null;
  private state: GatewayState = { kind: "stopped" };
  private readonly listeners = new Set<(state: GatewayState) => void>();
  /**
   * True only when this process started the gateway.
   *
   * The single most important flag in this file: it is what stops `quit` from
   * killing a gateway that was already serving before the shell opened.
   */
  private owned = false;
  /**
   * The last thing the gateway said before it stopped.
   *
   * Kept because "the gateway stopped before it began serving" is not an
   * actionable sentence, and the gateway's own message almost always is — an
   * unknown argument, a port already taken, a bind this machine does not hold.
   * Bounded, because a gateway that fails in a loop must not fill memory with
   * its complaints.
   */
  private lastOutput: string[] = [];

  onChange(listener: (state: GatewayState) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  current(): GatewayState {
    return this.state;
  }

  ownsProcess(): boolean {
    return this.owned;
  }

  private set(state: GatewayState) {
    this.state = state;
    for (const listener of this.listeners) listener(state);
  }

  /**
   * Attach to a gateway already serving on `port`, or start one.
   *
   * Attaching is preferred, and not only to avoid a second engine on a machine
   * that can barely hold one: a user who started `hermes serve` in a terminal
   * has a gateway with their own flags, their own model and their own bind, and
   * replacing it with the shell's defaults would be the shell overruling them.
   */
  async attachOrStart(
    options: LaunchOptions,
    fetchImpl: typeof fetch = fetch,
  ): Promise<GatewayState> {
    const existing = await probe(options.port, fetchImpl);
    if (existing) {
      this.owned = false;
      this.set({ kind: "attached", port: options.port, health: existing });
      return this.state;
    }

    this.set({ kind: "starting", port: options.port });
    const launch = planLaunch(options);

    try {
      const child = spawn(options.binary, launch.argv, {
        env: { ...process.env, ...launch.env },
        stdio: ["ignore", "pipe", "pipe"],
        // Not detached: the gateway is this shell's child and should go when it
        // goes. A gateway outliving the window that started it is an orphan
        // nobody can see and nobody can stop.
        detached: false,
      });
      this.child = child;
      this.owned = true;
      this.lastOutput = [];

      const remember = (chunk: Buffer | string) => {
        const text = String(chunk).trim();
        if (text === "") return;
        this.lastOutput.push(text);
        if (this.lastOutput.length > OUTPUT_LINES_KEPT) this.lastOutput.shift();
      };
      child.stdout?.on("data", remember);
      child.stderr?.on("data", remember);

      child.on("exit", (code, signal) => {
        this.child = null;
        this.owned = false;
        if (this.state.kind === "stopped") return;
        this.set({
          kind: "failed",
          reason:
            signal !== null
              ? `The gateway was stopped by ${signal}.`
              : `The gateway exited with code ${code ?? "unknown"}.`,
        });
      });

      child.on("error", (error) => {
        this.child = null;
        this.owned = false;
        this.set({ kind: "failed", reason: error.message });
      });

      await this.waitUntilServing(options.port);
      this.set({
        kind: "running",
        port: options.port,
        pid: child.pid ?? -1,
        ...(launch.apiKey ? { apiKey: launch.apiKey } : {}),
      });
    } catch (cause) {
      this.set({
        kind: "failed",
        reason: cause instanceof Error ? cause.message : String(cause),
      });
    }
    return this.state;
  }

  /**
   * Wait for the gateway to answer, or give up.
   *
   * Polled rather than assumed ready: a first start downloads and verifies the
   * engine, and a model named on the command line is read before the listener
   * is even reached on some paths. Failing fast here would report a healthy
   * gateway as broken.
   */
  /** What the gateway said, when it said anything. */
  private explain(): string {
    if (this.lastOutput.length === 0) return "";
    return `\n\nIt said:\n${this.lastOutput.join("\n")}`;
  }

  private async waitUntilServing(port: number, timeoutMs = 60_000): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (this.child === null) {
        throw new Error(
          `The gateway stopped before it began serving.${this.explain()}`,
        );
      }
      if (await probe(port)) return;
      if (Date.now() > deadline) {
        throw new Error(
          `The gateway did not start answering on port ${port} within ${
            timeoutMs / 1000
          } seconds.${this.explain()}`,
        );
      }
      await delay(250);
    }
  }

  /**
   * Stop the gateway, but only if this shell started it.
   *
   * `SIGTERM` first, because the gateway shuts its engine down cleanly on it
   * and leaving an orphaned `llama-server` behind is exactly the failure the
   * supervisor in `hermes-backend-llamacpp` exists to prevent. `SIGKILL` only
   * after a grace period, and only because a shell that cannot quit is worse
   * than one that leaves a mess.
   */
  async stop(graceMs = SHUTDOWN_GRACE_MS): Promise<void> {
    const child = this.child;
    this.set({ kind: "stopped" });
    if (!child || !this.owned) {
      // Attached, not owned. Leaving it running is the whole point.
      this.child = null;
      return;
    }

    child.kill("SIGTERM");
    const exited = await Promise.race([
      new Promise<boolean>((resolve) => child.once("exit", () => resolve(true))),
      delay(graceMs).then(() => false),
    ]);
    if (!exited) child.kill("SIGKILL");
    this.child = null;
    this.owned = false;
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
