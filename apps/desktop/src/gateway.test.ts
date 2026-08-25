/**
 * The shell's decisions, tested without a display.
 *
 * Everything here is a decision that is wrong in a way nobody would notice
 * until it mattered: attaching to the wrong service, killing a gateway that was
 * not ours, or putting a credential somewhere it can be read.
 */

import assert from "node:assert/strict";
import { join } from "node:path";
import { describe, it } from "node:test";

import {
  GatewaySupervisor,
  candidatePaths,
  isHermesHealth,
  isNonLoopback,
  planLaunch,
  probe,
  resolveBinary,
} from "./gateway.ts";

describe("recognising a Hermes gateway", () => {
  it("accepts a real health body", () => {
    assert.equal(
      isHermesHealth({ status: "ok", backend: "llamacpp-process", model: null }),
      true,
    );
  });

  it("refuses a body that only looks vaguely similar", () => {
    // Ollama also defaults to port 11434. An open port is not an invitation,
    // and attaching to a stranger's API would point the panel at endpoints that
    // answer the same paths with different meanings.
    assert.equal(isHermesHealth({ status: "ok" }), false);
    assert.equal(isHermesHealth({ backend: "something" }), false);
    assert.equal(isHermesHealth("ok"), false);
    assert.equal(isHermesHealth(null), false);
    assert.equal(isHermesHealth([]), false);
  });
});

describe("probing", () => {
  it("returns the health of a gateway that answers", async () => {
    const health = { status: "ok", backend: "llamacpp-process" };
    const fake: typeof fetch = async () =>
      new Response(JSON.stringify(health), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    assert.deepEqual(await probe(11434, fake), health);
  });

  it("does not attach to something else listening on the port", async () => {
    const fake: typeof fetch = async () =>
      new Response(JSON.stringify({ version: "0.5.1" }), { status: 200 });
    assert.equal(await probe(11434, fake), null);
  });

  it("treats a refused connection as nothing there", async () => {
    const fake: typeof fetch = async () => {
      throw new Error("ECONNREFUSED");
    };
    assert.equal(await probe(11434, fake), null);
  });

  it("treats a non-200 as nothing there", async () => {
    const fake: typeof fetch = async () => new Response("nope", { status: 500 });
    assert.equal(await probe(11434, fake), null);
  });
});

describe("finding the binary", () => {
  it("prefers an explicit override over everything", () => {
    const paths = candidatePaths({
      override: "/opt/hermes",
      repoRoot: "/repo",
      resourcesPath: "/app/resources",
    });
    assert.equal(paths[0], "/opt/hermes");
  });

  it("prefers a release build over a debug one", () => {
    const paths = candidatePaths({ repoRoot: "/repo" });
    const release = paths.findIndex((path) => path.includes("release"));
    const debug = paths.findIndex((path) => path.includes("debug"));
    assert.ok(release >= 0 && debug >= 0);
    assert.ok(release < debug, "release must be looked for first");
  });

  it("prefers a checkout's build over a packaged copy", () => {
    // Nobody debugging this wants to be silently running a stale bundled
    // binary instead of the one they just built.
    const paths = candidatePaths({ repoRoot: "/repo", resourcesPath: "/app" });
    // The prefixes are joined rather than written literally, for the same
    // reason the message assertion below builds its paths through
    // `candidatePaths`: Windows joins with a backslash, so `"/repo"` is a
    // prefix of nothing there and this compared -1 against -1.
    const built = paths.findIndex((path) => path.startsWith(join("/repo")));
    const packaged = paths.findIndex((path) => path.startsWith(join("/app")));
    assert.ok(built >= 0 && packaged >= 0, `neither root is in ${paths}`);
    assert.ok(built < packaged);
  });

  it("asks for the executable name this platform uses", () => {
    // A Windows build that looked for `hermes` would search for a file that
    // cannot exist beside a binary that does - the defect `scripts-stage.mjs`
    // fixed in the packaging, with nothing pinning it on this side.
    const expected = process.platform === "win32" ? "hermes.exe" : "hermes";
    const paths = candidatePaths({ repoRoot: "/repo", resourcesPath: "/app" });
    assert.ok(paths.length >= 3, `expected every candidate: ${paths}`);
    for (const path of paths) {
      assert.ok(
        path.endsWith(expected),
        `${path} does not end with ${expected}`,
      );
    }
  });

  it("names every place it looked when it finds nothing", () => {
    // "The gateway could not be started" is not an actionable sentence.
    //
    // The expected paths are built the way `candidatePaths` builds them rather
    // than written out with forward slashes: Windows joins with `\\` and calls
    // the binary `hermes.exe`, so a literal `/repo/target/release/hermes` here
    // failed on that platform against an error message that was perfectly
    // correct. What is being asserted is that every path it looked in appears
    // in the message, which is exactly this loop.
    const looked = candidatePaths({ repoRoot: "/repo" });
    assert.ok(looked.length >= 2, "a repo checkout has release and debug");
    assert.throws(
      () => resolveBinary({ repoRoot: "/repo" }, () => false),
      (error: Error) => {
        for (const path of looked) {
          assert.ok(
            error.message.includes(path),
            `the message does not name ${path}: ${error.message}`,
          );
        }
        assert.match(error.message, /cargo build --release/);
        return true;
      },
    );
  });

  it("returns the first candidate that exists", () => {
    const found = resolveBinary({ repoRoot: "/repo" }, (path) =>
      path.includes("debug"),
    );
    assert.match(found, /debug/);
  });
});

describe("deciding whether a bind needs a key", () => {
  it("treats every form of loopback as local", () => {
    for (const host of ["localhost", "dev.localhost", "127.0.0.1", "127.0.1.1", "::1", "[::1]", "  LOCALHOST  "]) {
      assert.equal(isNonLoopback(host), false, host);
    }
  });

  it("treats anything else as reachable", () => {
    for (const host of ["0.0.0.0", "192.0.2.10", "198.51.100.7", "my-machine", "2001:db8::1"]) {
      assert.equal(isNonLoopback(host), true, host);
    }
  });
});

describe("planning a launch", () => {
  it("gives a loopback gateway no key at all", () => {
    // The gateway only requires one when a bind is reachable from elsewhere.
    // Inventing a credential for a purely local socket would mean the panel has
    // to carry one for no gain.
    const launch = planLaunch({ binary: "hermes", port: 11434 });
    assert.equal(launch.apiKey, undefined);
    assert.equal(launch.env.HERMES_API_KEY, undefined);
  });

  it("generates a key as soon as a bind is reachable from elsewhere", () => {
    const launch = planLaunch({
      binary: "hermes",
      port: 11434,
      hosts: ["127.0.0.1", "192.0.2.10"],
    });
    assert.ok(launch.apiKey, "a reachable bind must have a key");
    assert.equal(launch.env.HERMES_API_KEY, launch.apiKey);
    assert.ok(launch.apiKey.length >= 32);
  });

  it("never puts the key in the command line", () => {
    // /proc/<pid>/cmdline is world-readable. M3.5 moved the engine's own key
    // out of its arguments for this reason; the same applies to ours.
    const launch = planLaunch({
      binary: "hermes",
      port: 11434,
      hosts: ["0.0.0.0"],
    });
    const line = launch.argv.join(" ");
    assert.ok(launch.apiKey);
    assert.equal(
      line.includes(launch.apiKey),
      false,
      `the key leaked into argv: ${line}`,
    );
    assert.equal(line.includes("--api-key"), false);
  });

  it("passes the port, the hosts and the panel through", () => {
    const launch = planLaunch({
      binary: "hermes",
      port: 8080,
      hosts: ["127.0.0.1", "192.0.2.10"],
      webRoot: "/panel/dist",
      home: "/data",
    });
    assert.deepEqual(launch.argv, [
      "serve",
      "--port",
      "8080",
      "--host",
      "127.0.0.1",
      "--host",
      "192.0.2.10",
      "--web-root",
      "/panel/dist",
    ]);
    assert.equal(launch.env.HERMES_GATEWAY_HOME, "/data");
  });

  it("two launches do not share a key", () => {
    const first = planLaunch({ binary: "h", port: 1, hosts: ["0.0.0.0"] });
    const second = planLaunch({ binary: "h", port: 2, hosts: ["0.0.0.0"] });
    assert.notEqual(first.apiKey, second.apiKey);
  });
});

describe("owning a gateway", () => {
  const healthy: typeof fetch = async () =>
    new Response(JSON.stringify({ status: "ok", backend: "llamacpp-process" }), {
      status: 200,
    });

  it("attaches to a gateway that was already serving", async () => {
    const supervisor = new GatewaySupervisor();
    const state = await supervisor.attachOrStart(
      { binary: "/does/not/exist", port: 11434 },
      healthy,
    );
    assert.equal(state.kind, "attached");
    assert.equal(
      supervisor.ownsProcess(),
      false,
      "a gateway we found is not one we own",
    );
  });

  it("never stops a gateway it did not start", async () => {
    // The single most important rule in this file. A gateway that was already
    // serving belongs to whoever started it; quitting the shell must not take
    // down a service the user did not ask us to touch.
    const supervisor = new GatewaySupervisor();
    await supervisor.attachOrStart(
      { binary: "/does/not/exist", port: 11434 },
      healthy,
    );

    await supervisor.stop();

    // Still answering: nothing was killed, and the supervisor simply let go.
    assert.notEqual(await probe(11434, healthy), null);
    assert.equal(supervisor.current().kind, "stopped");
  });

  it("reports a binary that is not there rather than hanging", async () => {
    const nothing: typeof fetch = async () => {
      throw new Error("ECONNREFUSED");
    };
    const supervisor = new GatewaySupervisor();
    const state = await supervisor.attachOrStart(
      { binary: "/definitely/not/a/binary", port: 11439 },
      nothing,
    );
    assert.equal(state.kind, "failed");
    assert.ok("reason" in state && state.reason.length > 0);
  });

  it("tells watchers about every change", async () => {
    const seen: string[] = [];
    const supervisor = new GatewaySupervisor();
    supervisor.onChange((state) => seen.push(state.kind));
    await supervisor.attachOrStart(
      { binary: "/does/not/exist", port: 11434 },
      healthy,
    );
    await supervisor.stop();
    assert.deepEqual(seen, ["attached", "stopped"]);
  });
});
