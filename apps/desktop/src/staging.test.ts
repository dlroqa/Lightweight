import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const stageScript = fileURLToPath(new URL("../scripts-stage.mjs", import.meta.url));

// Exercise the actual staging script in a disposable package tree. The tiny
// executables answer --version; no compiler or release build is needed.
function fixture(agentVersion: string | null) {
  const root = mkdtempSync(join(tmpdir(), "lightweight-stage-"));
  const app = join(root, "apps", "desktop");
  mkdirSync(app, { recursive: true });
  copyFileSync(stageScript, join(app, "scripts-stage.mjs"));
  writeFileSync(join(app, "package.json"), JSON.stringify({ version: "1.2.3" }));
  for (const [name, version] of [["lightweight", "1.2.3"], ["hermes", "1.2.3"], ["lightagent", agentVersion]]) {
    if (version === null) continue;
    const binary = join(root, "target", "release", name!);
    mkdirSync(dirname(binary), { recursive: true });
    writeFileSync(binary, `#!/bin/sh\nprintf '${name} ${version}\\n'\n`);
    chmodSync(binary, 0o755);
  }
  return { root, app };
}

for (const scenario of [
  { name: "stages both gateway and agent", version: "1.2.3", success: true },
  { name: "refuses a package without the agent", version: null, success: false },
  { name: "refuses a stale agent binary", version: "1.2.2", success: false },
]) {
  test(scenario.name, { skip: process.platform !== "linux" }, () => {
    const { root, app } = fixture(scenario.version);
    try {
      const result = spawnSync(process.execPath, [join(app, "scripts-stage.mjs")], { encoding: "utf8" });
      assert.equal(result.status === 0, scenario.success, result.stdout + result.stderr);
      if (scenario.success) {
        assert.equal(existsSync(join(app, "staging", "bin", "hermes")), false, "never ship the conflicting legacy command");
        for (const name of ["lightweight", "lightagent"]) {
          assert.equal(readFileSync(join(app, "staging", "bin", name), "utf8"),
            readFileSync(join(root, "target", "release", name), "utf8"));
        }
      } else {
        assert.match(result.stderr, /lightagent/);
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
}
