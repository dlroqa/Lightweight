/**
 * Put the CommonJS preload where the main process expects it.
 *
 * `tsc` cannot emit a `.cjs` extension directly, so it is built into a staging
 * directory and moved. The extension matters: this package is `"type":
 * "module"`, so a `.js` preload would be read as ESM and a sandboxed renderer
 * cannot load one.
 */
import { copyFileSync, existsSync, rmSync } from "node:fs";
import { join } from "node:path";

const staged = join("dist", ".preload", "preload.js");
if (!existsSync(staged)) {
  console.error(`the preload was not built: ${staged} is missing`);
  process.exit(1);
}
copyFileSync(staged, join("dist", "preload.cjs"));
rmSync(join("dist", ".preload"), { recursive: true, force: true });
console.log("preload -> dist/preload.cjs");
