/**
 * Put the `hermes` binary the installer will carry where electron-builder can
 * find it, and refuse to continue if it is the wrong one.
 *
 * Four problems this solves, all of which were live before it existed:
 *
 * 1. **The stale binary.** `extraResources` pointed straight at
 *    `target/release/hermes`, so whatever happened to be there got shipped. A
 *    build predating `--web-root` was packaged once and the shell failed on
 *    first run for a reason that had nothing to do with the shell
 *    (`docs/PROGRESS.md:919-922`). The version is checked against
 *    `package.json` here, so a mismatch is a sentence at build time rather
 *    than a mystery at run time.
 * 2. **The `.exe` suffix.** That same path hard-coded a Unix name, while the
 *    shell's own resolver looks for `bin/hermes.exe` on Windows - so a Windows
 *    package would have carried a binary the app could not find.
 * 3. **The universal macOS binary.** `@electron/universal` merges an arm64 and
 *    an x64 app bundle and *fails* when it finds non-identical binaries inside
 *    them unless they are declared in `mac.x64ArchFiles`. `hermes` differs per
 *    architecture, so both slices are built here and `lipo`-merged into one
 *    file before staging. The staged file is then byte-identical in both
 *    halves, the merge never sees a conflict, and the declaration is not
 *    needed at all.
 * 4. **The target directory.** `cargo build --target <triple>` puts the binary
 *    under `target/<triple>/release`, and `release.yml` builds that way while
 *    `check.yml` does not. Staging from `target/release` alone therefore worked
 *    on every CI run and failed in the release - the one place it mattered.
 */
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";

const repoRoot = join(import.meta.dirname, "..", "..");
const stagingBin = join(import.meta.dirname, "staging", "bin");
const version = JSON.parse(readFileSync(join(import.meta.dirname, "package.json"), "utf8")).version;

/** The two halves of a universal macOS binary. */
const MAC_TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

function run(command, args, options = {}) {
  return execFileSync(command, args, { stdio: "pipe", encoding: "utf8", ...options });
}

function die(message) {
  console.error(`stage: ${message}`);
  process.exit(1);
}

/** Refuse a binary that is not the one this package claims to ship. */
function verify(binary) {
  let reported;
  try {
    reported = run(binary, ["--version"]).trim();
  } catch (cause) {
    die(`${binary} could not be run: ${cause instanceof Error ? cause.message : cause}`);
  }
  // `hermes 0.1.0`
  const found = reported.split(/\s+/).at(-1);
  if (found !== version) {
    die(
      `${binary} reports ${found}, but this package is ${version}.\n` +
        "       Rebuild it: cargo build --release -p lightweight-cli",
    );
  }
  console.log(`stage: ${binary} is ${reported}`);
}

function stageMacUniversal() {
  const slices = [];
  for (const target of MAC_TARGETS) {
    console.log(`stage: building ${target}`);
    run("cargo", ["build", "--release", "-p", "lightweight-cli", "--target", target], {
      cwd: repoRoot,
      stdio: "inherit",
    });
    slices.push(join(repoRoot, "target", target, "release", "hermes"));
  }

  const merged = join(stagingBin, "hermes");
  run("lipo", ["-create", "-output", merged, ...slices]);

  // Asserted rather than assumed: a `lipo` that silently produced a
  // single-architecture file would ship a DMG that runs on one of the two
  // machines it claims to support, and nothing downstream would notice.
  const info = run("lipo", ["-info", merged]).trim();
  for (const architecture of ["arm64", "x86_64"]) {
    if (!info.includes(architecture)) {
      die(`the merged binary is missing the ${architecture} slice: ${info}`);
    }
  }
  console.log(`stage: ${info}`);
  verify(merged);
}

/**
 * Where `cargo build --release` left the binary, in the two places it can be.
 *
 * The triple-qualified path is tried first, so a `--target` build wins over
 * whatever an earlier host build happened to leave behind. `verify()` then has
 * the last word either way: a candidate of the wrong version is a sentence at
 * build time rather than a stale binary inside an installer. This is the same
 * two-candidate resolution `scripts/package-cli.sh` does.
 */
function builtBinary(executable) {
  const candidates = [];
  let triple;
  try {
    triple = run("rustc", ["-vV"]).match(/^host: (.+)$/m)?.[1].trim();
  } catch {
    // No rustc on PATH is not fatal here. The host path below may still exist,
    // and if it does not, the message names the file rather than the toolchain.
  }
  if (triple) {
    candidates.push(join(repoRoot, "target", triple, "release", executable));
  }
  candidates.push(join(repoRoot, "target", "release", executable));
  return { found: candidates.find((candidate) => existsSync(candidate)), candidates };
}

function stageHost() {
  const executable = process.platform === "win32" ? "hermes.exe" : "hermes";
  const { found, candidates } = builtBinary(executable);
  const staged = join(stagingBin, executable);
  if (!found) {
    die(
      `no binary at ${candidates.join(" or ")}\n` +
        "       Build it first: cargo build --release -p lightweight-cli",
    );
  }
  try {
    copyFileSync(found, staged);
  } catch (cause) {
    die(`${found} could not be staged: ${cause instanceof Error ? cause.message : cause}`);
  }
  console.log(`stage: from ${found}`);
  verify(staged);
}

rmSync(join(import.meta.dirname, "staging"), { recursive: true, force: true });
mkdirSync(stagingBin, { recursive: true });

if (process.platform === "darwin") {
  stageMacUniversal();
} else {
  stageHost();
}
