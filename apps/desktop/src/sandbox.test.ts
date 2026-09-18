/**
 * The sandbox refusal.
 *
 * What is being pinned is a *refusal*: the shell must not start when Chromium's
 * sandbox has been turned off behind the user's back. Every assertion below is
 * about the case the AppImage launcher creates on a host without unprivileged
 * user namespaces, which is the case nobody would otherwise see.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { inspectSandbox, sandboxFailureText } from "./sandbox.ts";

describe("the sandbox verdict", () => {
  it("accepts an ordinary Linux launch", () => {
    const verdict = inspectSandbox(["/opt/Hermes/hermes", "%U"], "linux");
    assert.equal(verdict.sandboxed, true);
  });

  it("refuses a Linux launch that disabled the sandbox", () => {
    const verdict = inspectSandbox(["/opt/Hermes/hermes", "--no-sandbox"], "linux");
    assert.equal(verdict.sandboxed, false);
    assert.match(verdict.reason, /--no-sandbox/);
    assert.ok(verdict.remedy.length > 0, "a refusal with no remedy is a dead end");
  });

  it("refuses the assignment form too", () => {
    // Chromium accepts `--no-sandbox=1`; matching only the bare flag would let
    // that through, and the whole point is that nothing gets through quietly.
    const verdict = inspectSandbox(["hermes", "--no-sandbox=1"], "linux");
    assert.equal(verdict.sandboxed, false);
  });

  it("names both ways out, and neither of them is 'ignore this'", () => {
    const text = sandboxFailureText(inspectSandbox(["hermes", "--no-sandbox"], "linux"));
    assert.match(text, /Flatpak/, "the sandboxed alternative must be named");
    assert.match(text, /unprivileged_userns_clone|apparmor_restrict_unprivileged_userns/);
    assert.doesNotMatch(
      text,
      /--no-sandbox is fine|ignore|continue anyway/i,
      "the message must not offer running unsandboxed as an option",
    );
  });

  it("does not judge macOS or Windows", () => {
    // No user-namespace question and no launcher that decides this on the
    // user's behalf, so the flag there came from a person who typed it.
    for (const platform of ["darwin", "win32"]) {
      const verdict = inspectSandbox(["hermes", "--no-sandbox"], platform);
      assert.equal(verdict.sandboxed, true, `${platform} should not be judged`);
    }
  });
});
