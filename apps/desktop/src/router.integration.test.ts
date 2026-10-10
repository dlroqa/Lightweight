/**
 * The Router supervisor against the real `hermes` binary, beside a real
 * Gateway.
 *
 * Opt-in by presence, like `supervisor.integration.test.ts`: with no built
 * binary it says so and skips rather than passing quietly.
 *
 * What only the real thing can prove: that a real Router and a real Gateway
 * run at once on their own ports, that each supervisor's stop and restart
 * leaves the other's process serving, that the real `validate-config` refuses
 * a bad file before anything is spawned, and that `config-path` names the file
 * `hermes router` reads.
 */

import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, it } from "node:test";

import { GatewaySupervisor, probe } from "./gateway.ts";
import { RouterSupervisor, identify, resolveConfigPath, type RouterOptions } from "./router.ts";

const repoRoot = join(import.meta.dirname, "..", "..", "..");
const executable = process.platform === "win32" ? "hermes.exe" : "hermes";
const binary = join(repoRoot, "target", "debug", executable);
const available = existsSync(binary);
// Demanded, not hoped for, where the gate builds the binary: a skip that stays
// green is how these suites went unrun on every CI platform.
if (!available && process.env.HERMES_REQUIRE_HERMES_BINARY) {
  throw new Error(`HERMES_REQUIRE_HERMES_BINARY is set, but there is no binary at ${binary}`);
}

/** Ports unlikely to collide with a developer's, and apart from the gateway test's 18492. */
const GATEWAY_PORT = 18493;
const ROUTER_PORT = 18494;

describe("the Router supervisor beside a real Gateway", { skip: !available && `no binary at ${binary}` }, () => {
  let home = "";
  let configPath = "";
  let env: Record<string, string> = {};

  before(async () => {
    home = await mkdtemp(join(tmpdir(), "hermes-shell-router-"));
    configPath = join(home, "router.json");
    // The Router's node is this test's Gateway. Whether the node is healthy
    // does not matter to a Router starting; its routes are reported as
    // unavailable until a model is loaded.
    await writeFile(
      configPath,
      JSON.stringify({
        nodes: [{ id: "local", url: `http://127.0.0.1:${GATEWAY_PORT}` }],
        routes: [{ name: "General", deployments: [{ node: "local", model: "General" }] }],
      }),
    );
    env = {
      // Both processes' data — the Router's admin token included — in a
      // throwaway directory, never the developer's own.
      HERMES_GATEWAY_HOME: home,
      // No credential store prompt on a headless runner (a debug-build switch).
      LIGHTWEIGHT_ROUTER_TEST_SECRET_STORE: "unavailable",
    };
  });

  after(async () => {
    if (home) await rm(home, { recursive: true, force: true });
  });

  const routerOptions = (): RouterOptions => ({
    binary,
    configPath,
    port: ROUTER_PORT,
    gatewayPort: GATEWAY_PORT,
    env,
  });

  it("config-path names router.json in the config directory", async () => {
    const path = await resolveConfigPath({ binary }, undefined, env);
    assert.equal(path, join(home, "config", "router.json"));
  });

  it("refuses an invalid file with the real validate-config, before spawning", async () => {
    const bad = join(home, "bad-router.json");
    await writeFile(bad, JSON.stringify({ nodes: [], routes: [] }));
    const supervisor = new RouterSupervisor(ROUTER_PORT);
    const state = await supervisor.start({ ...routerOptions(), configPath: bad });
    assert.equal(state.kind, "needs-config", JSON.stringify(state));
    assert.equal(state.kind === "needs-config" && state.problem, "invalid");
    assert.match(state.kind === "needs-config" ? state.detail : "", /lists no nodes/);
    assert.equal(supervisor.ownsProcess(), false);
    assert.equal(await identify(ROUTER_PORT), "nothing");
  });

  it("a Gateway and a Router run at once, and neither's lifecycle touches the other", async () => {
    const gateway = new GatewaySupervisor();
    const router = new RouterSupervisor(ROUTER_PORT);
    try {
      const gatewayState = await gateway.attachOrStart({ binary, port: GATEWAY_PORT, home });
      assert.equal(gatewayState.kind, "running", JSON.stringify(gatewayState));

      const routerState = await router.start(routerOptions());
      assert.equal(routerState.kind, "running", JSON.stringify(routerState));
      assert.equal(router.ownsProcess(), true);

      // Each port answers as what it is, and only as that.
      assert.equal(await identify(ROUTER_PORT), "router");
      assert.equal(await identify(GATEWAY_PORT), "gateway");
      assert.equal(await probe(ROUTER_PORT), null, "the Gateway probe claimed the Router");

      // A Router restart leaves the Gateway serving.
      await router.restart();
      assert.equal(router.current().kind, "running");
      assert.ok(await probe(GATEWAY_PORT), "restarting the Router disturbed the Gateway");

      // A Gateway restart leaves the Router serving.
      await gateway.restart();
      assert.equal(gateway.current().kind, "running");
      assert.equal(await identify(ROUTER_PORT), "router", "restarting the Gateway disturbed the Router");

      // Stopping the Router leaves the Gateway; stopping the Gateway leaves nothing.
      const tokens = join(home, "data", "router-admin");
      assert.equal((await readdir(tokens)).length, 1, "a loopback Router this shell started has an admin token");
      await router.stop();
      assert.equal(await identify(ROUTER_PORT), "nothing");
      if (process.platform !== "win32") {
        // The clean stop is why the Router gets SIGINT: it removes its token.
        assert.deepEqual(await readdir(tokens), [], "the Router's admin token outlived it");
      }
      assert.ok(await probe(GATEWAY_PORT), "stopping the Router stopped the Gateway");
    } finally {
      await router.stop();
      await gateway.stop();
    }
    assert.equal(await probe(GATEWAY_PORT), null);
  });

  it("a second shell attaches to a running Router instead of starting a rival", async () => {
    const first = new RouterSupervisor(ROUTER_PORT);
    const second = new RouterSupervisor(ROUTER_PORT);
    try {
      await first.start(routerOptions());
      assert.equal(first.current().kind, "running");

      assert.equal((await second.discover(ROUTER_PORT)).kind, "attached");
      assert.equal((await second.start(routerOptions())).kind, "attached");
      assert.equal(second.ownsProcess(), false);

      await second.stop();
      assert.equal(await identify(ROUTER_PORT), "router", "the second shell killed a Router it did not own");
    } finally {
      await first.stop();
    }
    assert.equal(await identify(ROUTER_PORT), "nothing");
  });
});
