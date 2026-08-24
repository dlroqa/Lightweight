/**
 * The supervisor against the real gateway binary.
 *
 * Opt-in by presence, like the Rust suite's real-engine tier: if there is no
 * built `hermes` to drive, this says so and skips rather than passing quietly.
 *
 * What it proves is the whole of M6b.4's substance, and none of it can be
 * proven with a fake: that a gateway really starts, really answers, really
 * stops, leaves no orphan behind, and that a second shell attaches to the first
 * one instead of starting a rival.
 */

import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, it } from "node:test";

import { GatewaySupervisor, probe } from "./gateway.ts";

const repoRoot = join(import.meta.dirname, "..", "..", "..");
const binary = join(repoRoot, "target", "debug", "hermes");
const available = existsSync(binary);

/** A port unlikely to collide with anything a developer is running. */
const PORT = 18492;

describe("the supervisor against a real gateway", { skip: !available && `no binary at ${binary}` }, () => {
  let home = "";

  before(async () => {
    home = await mkdtemp(join(tmpdir(), "hermes-shell-"));
  });

  after(async () => {
    if (home) await rm(home, { recursive: true, force: true });
  });

  it("starts a gateway, waits for it to answer, and stops it cleanly", async () => {
    const supervisor = new GatewaySupervisor();
    const state = await supervisor.attachOrStart({ binary, port: PORT, home });

    assert.equal(state.kind, "running", `state was ${JSON.stringify(state)}`);
    assert.equal(supervisor.ownsProcess(), true);
    assert.ok("pid" in state && state.pid > 0);

    // It is genuinely serving, not merely spawned.
    const health = await probe(PORT);
    assert.ok(health, "the gateway did not answer /health");
    assert.equal(typeof health.backend, "string");

    const pid = "pid" in state ? state.pid : -1;
    await supervisor.stop();

    assert.equal(supervisor.current().kind, "stopped");
    assert.equal(supervisor.ownsProcess(), false);
    assert.equal(await probe(PORT), null, "the port is still answering");
    assert.equal(alive(pid), false, "the gateway process is still alive");
  });

  it("a loopback gateway is given no key", async () => {
    // The gateway requires one only for a bind reachable from elsewhere.
    const supervisor = new GatewaySupervisor();
    const state = await supervisor.attachOrStart({ binary, port: PORT, home });
    assert.equal(state.kind, "running");
    assert.equal("apiKey" in state ? state.apiKey : undefined, undefined);
    await supervisor.stop();
  });

  it("a second shell attaches instead of starting a rival", async () => {
    // Two engines on a machine that can barely hold one is the obvious cost.
    // The subtler one: a user who started `hermes serve` themselves has their
    // own flags, model and bind, and replacing that would be the shell
    // overruling them.
    const first = new GatewaySupervisor();
    await first.attachOrStart({ binary, port: PORT, home });
    assert.equal(first.current().kind, "running");

    const second = new GatewaySupervisor();
    const attached = await second.attachOrStart({ binary, port: PORT, home });

    assert.equal(attached.kind, "attached");
    assert.equal(second.ownsProcess(), false);

    // The second shell quitting must leave the first one's gateway serving.
    await second.stop();
    assert.ok(await probe(PORT), "the second shell killed a gateway it did not own");

    await first.stop();
    assert.equal(await probe(PORT), null);
  });
});

/** Whether a pid is still running, without signalling it. */
function alive(pid: number): boolean {
  if (pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}
