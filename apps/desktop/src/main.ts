/**
 * The desktop shell.
 *
 * A window onto a gateway, and a supervisor for one. Everything the window
 * shows is the same panel the gateway serves over HTTP — the shell loads it
 * from `http://127.0.0.1:<port>/` rather than from a file, so it is the same
 * origin as the API and behaves exactly as it does in a browser. One panel, one
 * build, one set of behaviours to reason about.
 *
 * The Router, when the user asks for it, gets the same treatment from its own
 * origin: a second supervisor (`router.ts`) and a second window onto the same
 * bundle as the Router serves it. The two are never joined — see
 * `docs/DESKTOP_ROUTER.md`.
 */

import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  BrowserWindow,
  Menu,
  Tray,
  type MenuItemConstructorOptions,
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
import {
  ROUTER_DEFAULT_PORT,
  RouterSupervisor,
  adminTokenCommand,
  describeRouterState,
  isServing,
  resolveConfigPath,
  writeTemplate,
  type RouterState,
} from "./router.ts";
import { inspectSandbox, sandboxFailureText } from "./sandbox.ts";
import {
  gatewayPanelUrl,
  isExternalWebLink,
  isSameOrigin,
  routerPanelUrl,
  routerWindowPreferences,
} from "./windows.ts";

const here = fileURLToPath(new URL(".", import.meta.url));

const supervisor = new GatewaySupervisor();
let window: BrowserWindow | null = null;
let tray: Tray | null = null;
/** Set when the user really means to quit, rather than close the window. */
let quitting = false;

const port = Number(process.env.HERMES_PORT ?? DEFAULT_PORT);

/** The Router's port: its own, beside the gateway's, never shared with it. */
const routerPort = Number(process.env.HERMES_ROUTER_PORT ?? ROUTER_DEFAULT_PORT);
const routerSupervisor = new RouterSupervisor(routerPort);
let routerWindow: BrowserWindow | null = null;
/** The `hermes` binary, once found; the Router is the same binary's `router` command. */
let hermesBinary: string | null = null;

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

  await window.loadURL(gatewayPanelUrl(port));
}

/**
 * The Router's own window onto its own panel.
 *
 * A separate window rather than a screen in the Gateway's, because the panel
 * calls only the origin it was loaded from: loaded from the Router, it shows
 * Auto Routing and Classifier (with Jev Settings) and talks to the Router's API
 * alone, so the Router's same-origin and loopback checks hold exactly as they
 * do in a browser. No preload — the Router page has no bridge to the shell —
 * and it cannot be navigated off the Router's origin.
 */
async function openRouterWindow(routerOrigin: number): Promise<void> {
  if (routerWindow) {
    routerWindow.show();
    routerWindow.focus();
    return;
  }
  const url = routerPanelUrl(routerOrigin);
  const created = new BrowserWindow({
    width: 1280,
    height: 860,
    minWidth: 960,
    minHeight: 640,
    show: false,
    title: "Lightweight — Router & Jev Settings",
    icon: icon("window.png"),
    backgroundColor: "#eef2fb",
    webPreferences: routerWindowPreferences(),
  });
  routerWindow = created;
  created.once("ready-to-show", () => created.show());
  created.on("closed", () => {
    if (routerWindow === created) routerWindow = null;
  });
  created.webContents.on("will-navigate", (event, target) => {
    if (!isSameOrigin(target, url)) event.preventDefault();
  });
  created.webContents.setWindowOpenHandler(({ url: target }) => {
    if (isExternalWebLink(target)) void shell.openExternal(target);
    return { action: "deny" };
  });
  await created.loadURL(url);
}

/** The Router's configuration file, as `hermes router` itself would find it. */
async function routerConfigPath(binary: string): Promise<string> {
  return resolveConfigPath({ binary }, process.env.HERMES_ROUTER_CONFIG);
}

/**
 * Start a Router — only ever because the user asked.
 *
 * Attaches if a Router is already serving, and otherwise checks the file,
 * starts one on loopback and waits for it. Whatever happens, the gateway is not
 * touched: a Router that cannot start is a Router problem, shown as one.
 */
async function startRouter(openAfter: boolean): Promise<void> {
  if (!hermesBinary) return;
  let configPath: string;
  try {
    configPath = await routerConfigPath(hermesBinary);
  } catch (cause) {
    showRouterProblem("The Router's configuration could not be located.", String(cause instanceof Error ? cause.message : cause));
    return;
  }
  const state = await routerSupervisor.start({
    binary: hermesBinary,
    configPath,
    port: routerPort,
    gatewayPort: port,
    webRoot: panelRoot(),
  });
  if (isServing(state)) {
    if (openAfter) await openRouterWindow(state.port);
  } else {
    await showRouterSetup(state);
  }
}

/** "Router & Jev Settings…": open the Router's window, or say what stands in the way. */
async function routerSettings(): Promise<void> {
  // Look again first: a Router may have started or stopped since launch. A
  // failure's own words are kept for the dialog when nothing has replaced it.
  const before = routerSupervisor.current();
  const state = await routerSupervisor.discover(routerPort);
  if (isServing(state)) {
    await openRouterWindow(state.port);
    return;
  }
  await showRouterSetup(state.kind === "off" && before.kind === "failed" ? before : state);
}

/** Restart this shell's own Router, so saved Jev Settings take effect. */
async function restartRouter(): Promise<void> {
  if (!routerSupervisor.ownsProcess()) return;
  const state = await routerSupervisor.restart();
  if (isServing(state)) routerWindow?.webContents.reload();
  else await showRouterSetup(state);
}

async function stopRouter(): Promise<void> {
  if (!routerSupervisor.ownsProcess()) return;
  routerWindow?.close();
  await routerSupervisor.stop();
}

function showRouterProblem(message: string, detail: string): void {
  // To stderr as well, as for the gateway: a dialog nobody can see is no report.
  console.error(`hermes-desktop: ${message}\n${detail}`);
  void dialog.showMessageBox({ type: "warning", title: "Router", message, detail, buttons: ["OK"] });
}

const ROUTER_EXPLAINED =
  "The Router is a separate service from the Gateway. The Gateway runs models on this " +
  "machine; the Router gives clients stable route names (and Auto) and forwards each " +
  "request to the Gateway or another node its own router.json names. Starting or " +
  "stopping the Router never affects the Gateway.";

/**
 * Say what the Router needs, and offer only the levers that are safe.
 *
 * Nothing here writes `router.json`: Desktop does not invent routes, keys or
 * remote nodes. The one write on offer is an explicit, never-overwriting
 * template beside it.
 */
async function showRouterSetup(state: RouterState): Promise<void> {
  switch (state.kind) {
    case "off":
    case "failed": {
      const { response } = await dialog.showMessageBox({
        type: state.kind === "failed" ? "warning" : "info",
        title: "Router & Jev Settings",
        message: state.kind === "failed" ? "The Router is not running." : `No Router is running on port ${routerPort}.`,
        detail: `${state.kind === "failed" ? `${state.reason}\n\n` : ""}${ROUTER_EXPLAINED}`,
        buttons: ["Start Router", "Cancel"],
        defaultId: 0,
        cancelId: 1,
      });
      if (response === 0) await startRouter(true);
      return;
    }
    case "needs-config": {
      const missing = state.problem === "missing";
      const detail = missing
        ? `${state.detail}\n\nThe Router reads its own configuration, separate from the Gateway's. ` +
          `It lists the nodes and routes you choose, and Desktop does not invent them.\n\n` +
          `“Create template” writes router.template.json beside it — it holds no keys, and ` +
          `nothing loads it. Edit it, save it as router.json, then choose Start Router. ` +
          `docs/ROUTER.md describes every field.`
        : `${state.detail}\n\nFix the file, then choose Start Router. Run ` +
          `\`hermes router validate-config --config "${state.configPath}"\` to check it.`;
      const buttons = missing ? ["Create template", "Show folder", "Cancel"] : ["Show folder", "Cancel"];
      const { response } = await dialog.showMessageBox({
        type: "info",
        title: "Router & Jev Settings",
        message: missing ? "The Router needs a configuration first." : "The Router's configuration was refused.",
        detail,
        buttons,
        cancelId: buttons.length - 1,
      });
      const choice = buttons[response];
      if (choice === "Create template") {
        try {
          const written = await writeTemplate(state.configPath, port, routerPort);
          shell.showItemInFolder(written);
        } catch (cause) {
          showRouterProblem("The template was not written.", cause instanceof Error ? cause.message : String(cause));
        }
      } else if (choice === "Show folder") {
        void shell.openPath(dirname(state.configPath));
      }
      return;
    }
    case "conflict":
      showRouterProblem(`The Router cannot use port ${state.port}.`, state.detail);
      return;
    case "starting":
    case "running":
    case "attached":
      return;
  }
}

async function showAdminTokenHelp(): Promise<void> {
  if (!hermesBinary) return;
  let configPath = "<router.json>";
  try {
    configPath = await routerConfigPath(hermesBinary);
  } catch {
    // The command is still worth showing with a placeholder.
  }
  // The command, never the token: the main process does not read it, and the
  // panel is where it is typed.
  void dialog.showMessageBox({
    type: "info",
    title: "Router admin token",
    message: "Saving Jev Settings needs the Router's admin token.",
    detail:
      `It is minted each time the Router starts, kept only in your user's data ` +
      `directory, and is not the API key agents use. Print it with:\n\n` +
      `${adminTokenCommand(hermesBinary, configPath)}\n\n` +
      `then paste it into the Admin token field. A Router started elsewhere: run the ` +
      `command where it runs, with its own --config.`,
    buttons: ["OK"],
  });
}

/** The Router's entries, shared by the tray and the application menu. */
function routerMenuItems(): MenuItemConstructorOptions[] {
  const state = routerSupervisor.current();
  const owned = routerSupervisor.ownsProcess();
  const serving = isServing(state);
  return [
    { id: "router-status", label: `Router — ${describeRouterState(state)}`, enabled: false },
    { id: "router-settings", label: "Router & Jev Settings\u2026", click: () => void routerSettings() },
    {
      id: "router-start",
      label: "Start Router",
      enabled: !serving && state.kind !== "starting",
      click: () => void startRouter(false),
    },
    {
      id: "router-restart",
      label: "Restart Router (applies saved settings)",
      enabled: owned && state.kind === "running",
      click: () => void restartRouter(),
    },
    { id: "router-stop", label: "Stop Router", enabled: owned, click: () => void stopRouter() },
    { id: "router-admin-token", label: "Router admin token\u2026", click: () => void showAdminTokenHelp() },
  ];
}

/**
 * The menu bar: Electron's standard menus, plus a Router menu.
 *
 * Built from the standard roles so copy, paste, reload and the window menu are
 * what they were under Electron's default menu.
 */
function updateApplicationMenu(): void {
  const template: MenuItemConstructorOptions[] = [
    ...(process.platform === "darwin" ? [{ role: "appMenu" } as MenuItemConstructorOptions] : []),
    { role: "fileMenu" },
    { role: "editMenu" },
    { role: "viewMenu" },
    { label: "Router", submenu: routerMenuItems() },
    { role: "windowMenu" },
  ];
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}

/**
 * Turn a port-conflict failure into the two levers the desktop actually has.
 *
 * The gateway's own stderr already explains the conflict and suggests
 * `--port auto`, but that is a CLI flag: the shell does not pass it, and a
 * persisted "random port each start" would make the panel's own URL unstable.
 * So on a taken port the desktop points at the two explicit, stable levers it
 * *does* have — the `HERMES_PORT` environment variable, and the panel's
 * **Serve on** control, which writes the port into `config/api.json`. Detection
 * is on the gateway's own wording so no separate error taxonomy is needed.
 */
function portConflictGuidance(reason: string): string {
  const looksLikePortConflict =
    /already listening/i.test(reason) || /address (already )?in use/i.test(reason);
  if (!looksLikePortConflict) return "";
  return (
    `\n\nThe port is already taken — often by another local-LLM server, since ` +
    `${DEFAULT_PORT} is also Ollama's default. To move Lightweight off it, set ` +
    `HERMES_PORT to a free port before launching, or change the port in the ` +
    `panel's “Serve on” control (it is saved and reused). Or stop whatever is ` +
    `holding the port.`
  );
}

function showStartupFailure(reason: string): void {
  const detail = `${reason}${portConflictGuidance(reason)}`;
  // To stderr as well as to a dialog. A dialog needs a working display and a
  // running message loop; when the shell dies before either exists — or under a
  // virtual display, or in CI — the dialog is never seen and the process exits
  // silently, which is the least serviceable failure a supervisor can have.
  console.error(`hermes-desktop: the gateway did not start.\n${detail}`);
  void dialog.showMessageBox({
    type: "error",
    title: "Lightweight could not start",
    message: "The gateway did not start.",
    detail,
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
    { type: "separator" },
    ...routerMenuItems(),
    { type: "separator" },
    {
      label: quitLabel(),
      click: () => {
        quitting = true;
        app.quit();
      },
    },
  ]);

  tray.setToolTip(`Lightweight — ${label}`);
  tray.setContextMenu(menu);
}

/** Says what quitting will stop: only what this shell started. */
function quitLabel(): string {
  const gateway = supervisor.ownsProcess();
  const router = routerSupervisor.ownsProcess();
  if (gateway && router) return "Quit Lightweight and stop the Router and gateway";
  if (router) return "Quit Lightweight and stop the Router";
  if (gateway) return "Quit Lightweight and stop the gateway";
  return "Quit Lightweight";
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
  routerSupervisor.onChange(() => {
    // The Router's state is the shell's to show; it is never sent to either
    // page, and the gateway's state is not changed by it.
    updateTray(supervisor.current());
    updateApplicationMenu();
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

  hermesBinary = binary;
  createTray();
  updateApplicationMenu();
  // Look, never start: a Router runs only when the user asks for one.
  void routerSupervisor.discover(routerPort);
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
  if (!supervisor.ownsProcess() && !routerSupervisor.ownsProcess()) return;
  // Stop what we started, and only that. Held open until the child is really
  // gone so the engine it supervises is not orphaned.
  event.preventDefault();
  void stopOwnedChildren().finally(() => app.exit(0));
});

/**
 * The Router first, then the gateway: the Router forwards to the gateway, so
 * it goes before what it depends on. Each supervisor stops only a process it
 * started; an attached Router or gateway is left exactly as it was.
 */
async function stopOwnedChildren(): Promise<void> {
  if (routerSupervisor.ownsProcess()) {
    await routerSupervisor.stop().catch((cause: unknown) => {
      console.error(`hermes-desktop: the Router did not stop cleanly: ${String(cause)}`);
    });
  }
  if (supervisor.ownsProcess()) await supervisor.stop();
}

app.on("activate", () => {
  if (BrowserWindow.getAllWindows().length === 0) void createWindow();
});
