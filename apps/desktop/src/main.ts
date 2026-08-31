/**
 * The desktop shell.
 *
 * A window onto a gateway, and a supervisor for one. Everything the window
 * shows is the same panel the gateway serves over HTTP — the shell loads it
 * from `http://127.0.0.1:<port>/` rather than from a file, so it is the same
 * origin as the API and behaves exactly as it does in a browser. One panel, one
 * build, one set of behaviours to reason about.
 */

import { existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  BrowserWindow,
  Menu,
  Tray,
  app,
  dialog,
  ipcMain,
  nativeImage,
  shell,
  type NativeImage,
} from "electron";

import {
  DEFAULT_PORT,
  GatewaySupervisor,
  resolveBinary,
  type GatewayState,
} from "./gateway.ts";
import { inspectSandbox, sandboxFailureText } from "./sandbox.ts";

const here = fileURLToPath(new URL(".", import.meta.url));

const supervisor = new GatewaySupervisor();
let window: BrowserWindow | null = null;
let tray: Tray | null = null;
/** Set when the user really means to quit, rather than close the window. */
let quitting = false;

const port = Number(process.env.HERMES_PORT ?? DEFAULT_PORT);

/**
 * Where the panel's built files are.
 *
 * The gateway serves them; the shell only has to say where they are. In a
 * checkout that is `frontend/dist`, and in a packaged build they ship beside
 * the app.
 */
function panelRoot(): string | undefined {
  const packaged = join(process.resourcesPath ?? "", "panel");
  const checkout = join(here, "..", "..", "..", "frontend", "dist");
  // The first that *exists*, not the first that is a non-empty string.
  // `process.resourcesPath` is set in a checkout too, so a truthiness check
  // would hand the gateway a packaged path that is not there and serve the
  // panel from nowhere.
  for (const candidate of [process.env.HERMES_WEB_ROOT, packaged, checkout]) {
    if (candidate && existsSync(candidate)) return candidate;
  }
  return undefined;
}

/**
 * An icon that ships beside the compiled main process.
 *
 * `scripts-build.mjs` copies these into `dist/`, so the same path resolves in a
 * checkout and inside the packaged asar - `join(here, ...)` is the same
 * directory `preload.cjs` is loaded from above.
 *
 * `createFromPath` is documented to return an empty image, rather than throw,
 * when the file is missing, unreadable or not an image, and both callers accept
 * an empty one. That is the behaviour wanted here: a shell that refused to
 * start because a decoration was absent would be a worse failure than one that
 * starts without it.
 */
function icon(name: string): NativeImage {
  return nativeImage.createFromPath(join(here, name));
}

function repoRoot(): string {
  return join(here, "..", "..", "..");
}

async function createWindow(): Promise<void> {
  window = new BrowserWindow({
    width: 1440,
    height: 900,
    minWidth: 960,
    minHeight: 640,
    show: false,
    title: "Lightweight",
    // Linux and Windows read the window's icon from the process; macOS uses the
    // bundle's and ignores this.
    icon: icon("window.png"),
    backgroundColor: "#eef2fb",
    webPreferences: {
      // CommonJS, and named `.cjs` so Node reads it as such inside a
      // `"type": "module"` package. A sandboxed renderer cannot load an ESM
      // preload, and a CommonJS one is valid whether or not the sandbox is on
      // — so this is the format that does not depend on the flag above.
      preload: join(here, "preload.cjs"),
      // Nothing the panel loads is trusted with Node. It is a web page served
      // over HTTP, and the two flags below are what keep it one.
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  // A window that appears grey and empty while a model loads looks broken.
  window.once("ready-to-show", () => window?.show());

  window.on("close", (event) => {
    // Closing the window leaves the gateway serving and the tray in place,
    // which is what a local service wants: the API keeps answering for the
    // editor plugin or the agent harness that is using it.
    if (!quitting && tray) {
      event.preventDefault();
      window?.hide();
    }
  });

  window.on("closed", () => {
    window = null;
  });

  // Anything that is not the panel opens in the user's browser rather than in
  // a chromeless window with no address bar.
  window.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: "deny" };
  });

  await window.loadURL(`http://127.0.0.1:${port}/`);
}

function showStartupFailure(reason: string): void {
  // To stderr as well as to a dialog. A dialog needs a working display and a
  // running message loop; when the shell dies before either exists — or under a
  // virtual display, or in CI — the dialog is never seen and the process exits
  // silently, which is the least serviceable failure a supervisor can have.
  console.error(`hermes-desktop: the gateway did not start.\n${reason}`);
  void dialog.showMessageBox({
    type: "error",
    title: "Lightweight could not start",
    message: "The gateway did not start.",
    detail: reason,
    buttons: ["Quit"],
  });
}

function updateTray(state: GatewayState): void {
  if (!tray) return;

  const label =
    state.kind === "running"
      ? `Serving on port ${state.port}`
      : state.kind === "attached"
        ? `Attached to port ${state.port}`
        : state.kind === "starting"
          ? "Starting…"
          : state.kind === "failed"
            ? "Not running"
            : "Stopped";

  const menu = Menu.buildFromTemplate([
    { label: `Lightweight — ${label}`, enabled: false },
    { type: "separator" },
    {
      label: "Open panel",
      click: () => {
        if (window) {
          window.show();
          window.focus();
        } else {
          void createWindow();
        }
      },
    },
    {
      // The shell no longer holds a key to copy: keys are the gateway's own,
      // hashed, and are created and shown once in the panel (or with
      // `hermes key create`). The tray points there rather than pretending to
      // have a credential it deliberately never sees.
      label: "Manage API keys\u2026",
      click: () => {
        if (window) {
          window.show();
          window.focus();
        } else {
          void createWindow();
        }
      },
    },
    {
      label: supervisor.ownsProcess()
        ? "Quit Lightweight and stop the gateway"
        : "Quit Lightweight",
      click: () => {
        quitting = true;
        app.quit();
      },
    },
  ]);

  tray.setToolTip(`Lightweight — ${label}`);
  tray.setContextMenu(menu);
}

function createTray(): void {
  // Given at 48px for a tray that draws it at 16-24: the platform scales it
  // down, and a HiDPI display has real pixels to use.
  tray = new Tray(icon("tray.png"));
  updateTray(supervisor.current());
}

async function start(): Promise<void> {
  supervisor.onChange((state) => {
    updateTray(state);
    window?.webContents.send("gateway:state", state);
  });

  let binary: string;
  try {
    binary = resolveBinary({
      override: process.env.HERMES_BIN,
      resourcesPath: process.resourcesPath,
      repoRoot: repoRoot(),
    });
  } catch (cause) {
    showStartupFailure(cause instanceof Error ? cause.message : String(cause));
    app.quit();
    return;
  }

  const state = await supervisor.attachOrStart({
    binary,
    port,
    webRoot: panelRoot(),
    hosts: (process.env.HERMES_HOSTS ?? "")
      .split(",")
      .map((host) => host.trim())
      .filter((host) => host !== ""),
    home: process.env.HERMES_GATEWAY_HOME,
  });

  if (state.kind === "failed") {
    showStartupFailure(state.reason);
    app.quit();
    return;
  }

  createTray();
  await createWindow();
}

// The panel asks for this once, to show what it is attached to.
ipcMain.handle("gateway:current", () => supervisor.current());
ipcMain.handle("gateway:restart", () => supervisor.restart());

// Refuse before anything else, including before the app is ready.
//
// The AppImage launcher adds `--no-sandbox` by itself on a host without
// unprivileged user namespaces, preferring - in its own words - to start
// without sandboxing rather than crash. That trade is not ours to accept
// silently on the user's behalf, so the shell stops here and says what
// happened and what to do instead. `sandbox.ts` explains why the check cannot
// live in the packaging.
const sandbox = inspectSandbox(process.argv, process.platform);
if (!sandbox.sandboxed) {
  const message = sandboxFailureText(sandbox);
  // Both channels: a terminal launch reads stderr, and a launcher click sees
  // only the dialog. `showErrorBox` is one of the few dialogs that works
  // before `whenReady`.
  process.stderr.write(`${message}\n`);
  dialog.showErrorBox("Lightweight cannot start unsandboxed", message);
  // Non-zero: a wrapper or a service manager must be able to tell that this
  // was a refusal rather than a normal quit.
  app.exit(1);
}

app.whenReady().then(start).catch((cause: unknown) => {
  showStartupFailure(cause instanceof Error ? cause.message : String(cause));
  app.quit();
});

app.on("window-all-closed", () => {
  // Deliberately does not quit on any platform. The gateway is a local service
  // and the tray is how it stays reachable after the window is closed.
});

app.on("before-quit", () => {
  quitting = true;
});

app.on("will-quit", (event) => {
  if (!supervisor.ownsProcess()) return;
  // Stop what we started, and only that. Held open until the child is really
  // gone so the engine it supervises is not orphaned.
  event.preventDefault();
  void supervisor.stop().finally(() => app.exit(0));
});

app.on("activate", () => {
  if (BrowserWindow.getAllWindows().length === 0) void createWindow();
});
