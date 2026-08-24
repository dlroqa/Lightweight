/**
 * Put the CommonJS preload - and the icons the shell loads - where the main
 * process expects them.
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

/*
 * The window and tray icons travel with the compiled code.
 *
 * They are generated into `build/`, which is electron-builder's own resources
 * directory: builder reads `build/icons/` from there to make the packaged
 * application's icon, but it excludes that directory from the app itself. An
 * icon the main process reads at run time therefore has to be in `dist/`, or it
 * is present in a checkout and absent from the built application - the worst
 * kind of difference, because only the shipped copy is wrong.
 */
for (const icon of ["tray.png", "window.png"]) {
  const from = join("build", icon);
  if (!existsSync(from)) {
    console.error(`the icon is missing: ${from} - run scripts/build-icons.py`);
    process.exit(1);
  }
  copyFileSync(from, join("dist", icon));
  console.log(`${from} -> dist/${icon}`);
}
