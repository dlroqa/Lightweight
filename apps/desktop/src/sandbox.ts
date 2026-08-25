/**
 * Whether this process is running with Chromium's sandbox turned off.
 *
 * Electron's own documentation is explicit that `--no-sandbox` disables the
 * sandbox for renderer and helper processes and is for testing only. Two things
 * in the Linux packaging add it on their own:
 *
 * 1. The `.desktop` entry electron-builder generates hard-codes it, unless
 *    `linux.executableArgs` is set to an empty list. It is, in `package.json`.
 * 2. The `AppRun` launcher electron-builder generates adds it *at runtime*
 *    whenever `unshare -Ur true` fails - with a comment saying it prefers the
 *    app to start without sandboxing rather than crash on startup. That script
 *    is regenerated on every build, by both the legacy and the static-runtime
 *    builders, and no configuration option reaches it.
 *
 * So the refusal lives here instead, in code electron-builder does not write.
 * A guard in the packaging can be regenerated away; this one cannot.
 *
 * Deliberately free of any `electron` import, for the same reason `gateway.ts`
 * is: it makes the decision testable without a display or a packaged app.
 */

/** Why the sandbox is off, and what the person in front of it can do. */
export interface SandboxVerdict {
  readonly sandboxed: boolean;
  readonly reason: string;
  readonly remedy: readonly string[];
}

const SANDBOXED: SandboxVerdict = {
  sandboxed: true,
  reason: "",
  remedy: [],
};

/**
 * Inspect the arguments this process was actually launched with.
 *
 * Only Linux is judged. macOS and Windows have no user-namespace question and
 * no launcher that decides this on the user's behalf, so a `--no-sandbox` there
 * came from a person who typed it, and refusing to start would be answering a
 * question nobody asked.
 */
export function inspectSandbox(argv: readonly string[], platform: string): SandboxVerdict {
  if (platform !== "linux") return SANDBOXED;

  const disabled = argv.some(
    (argument) => argument === "--no-sandbox" || argument.startsWith("--no-sandbox="),
  );
  if (!disabled) return SANDBOXED;

  return {
    sandboxed: false,
    reason:
      "Hermes was started with --no-sandbox, which turns off Chromium's " +
      "sandbox for the window and its helper processes.\n\n" +
      "Nothing here added that flag on purpose. The AppImage launcher adds it " +
      "by itself when it cannot create an unprivileged user namespace, which " +
      "is how this build would otherwise run unsandboxed without telling you.",
    remedy: [
      "Install the Flatpak build instead. It is sandboxed by Flatpak itself " +
        "and does not need unprivileged user namespaces.",
      "Or enable unprivileged user namespaces on this machine, then start " +
        "Hermes again:\n" +
        "    sudo sysctl -w kernel.unprivileged_userns_clone=1\n" +
        "  On Debian and Ubuntu the restriction may instead be:\n" +
        "    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0",
      "Running `hermes serve` in a terminal needs none of this: the sandbox " +
        "is a property of the desktop shell, not of the gateway.",
    ],
  };
}

/** The whole message, as one block of text for a dialog or a terminal. */
export function sandboxFailureText(verdict: SandboxVerdict): string {
  return [verdict.reason, "", ...verdict.remedy.map((step) => `• ${step}`)].join("\n");
}
