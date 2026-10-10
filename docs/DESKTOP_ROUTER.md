# Desktop: Router & Jev Settings

The desktop shell has supervised one process, the Gateway (`hermes serve`).
This document is the design for letting the same shell open the Router's own
panel (Auto Routing, Classifier and Jev Settings), and start a Router when the
user asks. It is written before the code and is the reference for review.

Read [ROUTER.md](ROUTER.md) first for what the Router is. The one-line
version: **the Gateway is the local inference node; the Router is a separate
control-plane process that routes requests to the Gateway or to other nodes.**
Nothing here merges the two. Their HTTP APIs, configuration files,
credentials, ports, ownership and lifecycles stay separate.

## Why a separate sidecar and window, not a Gateway feature

- **Same-origin is the security model.** The panel calls only its own origin.
  Jev Settings writes are admitted only with a loopback `Host`, an `Origin`
  equal to that host, the admin token and `If-Match`. A Router screen inside
  the Gateway window would need a cross-origin call, permissive CORS, a
  reverse proxy in front of both APIs, or disabled web security. Each of those
  weakens a check that exists on purpose. A second `BrowserWindow` pointed at
  the Router's own origin needs none of them.
- **The panel already chooses its screens by origin.** `frontend/src/state/
  backend.tsx` asks `GET /version`. A `lightweight-router-` build gets Auto
  Routing and Classifier; anything else gets the Gateway screens. The Router
  window loads the same bundle from the Router. No new frontend code and no
  second bundle are needed.
- **They fail independently.** A Router with a bad `router.json` must not
  stop the Gateway from serving, and a Gateway restart must not drop the
  Router. Two supervisors, each with its own ownership flag, keep that true.

## 1. Process topology

| | Gateway | Router |
|---|---|---|
| Process | `hermes serve` | `hermes router` |
| Port | `HERMES_PORT`, default 11434 | `HERMES_ROUTER_PORT`, default 11500 |
| Bind (when Desktop starts it) | as today | always `127.0.0.1:<port>` via `--listen` |
| Identified by | `/health` has string `status` and `backend` | `/version` `build` starts `lightweight-router-` |
| Supervisor | `GatewaySupervisor` (`gateway.ts`, unchanged) | `RouterSupervisor` (`router.ts`, new) |
| Started | at launch (attach-or-start, as today) | only on an explicit user action |
| Graceful stop | `SIGTERM`, then `SIGKILL` after 8 s | `SIGINT`, then `SIGKILL` after 8 s |

- **Distinct ports.** If the two configured ports are equal, Desktop refuses
  to start the Router and says why. It does not move either port.
- **Independent probes.** The two identity checks exclude each other. A
  Router's `/health` has no `backend`, so the Gateway probe never attaches to
  a Router. A Gateway's `/version` build is not `lightweight-router-…`, so the
  Router probe never treats a Gateway as a Router.
- **Ownership.** Each supervisor has its own `owned` flag. It is set only when
  that supervisor spawned the child, and cleared when the child exits. `stop`
  and `restart` act only on an owned child. A discovered (attached) Router or
  Gateway is never signalled, on any path, including quit.
- **Why `SIGINT` for the Router.** `hermes router` stops cleanly on Ctrl-C:
  it finishes serving and removes its admin-token file.
  `scripts/render-panel.sh` stops Routers the same way. On Windows,
  `child.kill` ends the process whatever the signal, as it does for the
  Gateway today.
- **No shared generic supervisor.** `GatewaySupervisor` is left as it is.
  The Router differs on almost every axis: how it is identified, the stop
  signal, the configuration preflight, attach not being chained to start, and
  conflict classification. A shared base would need a parameter for each of
  these. That is less clear than two small classes, and refactoring would put
  verified Gateway behavior at risk for no gain.

## 2. UI topology

- The Gateway window, its URL (`http://127.0.0.1:<gateway port>/`), its
  preload bridge (`hermesShell`) and its navigation are unchanged.
- New entry point **Router & Jev Settings…**, in two places:
  - the tray menu;
  - a **Router** menu in the application menu bar. The bar is rebuilt from
    Electron's standard roles (app, File, Edit, View, Window), so the default
    copy/paste/reload items stay.

  The Router menu also shows the Router's status, plus **Start Router**,
  **Restart Router** and **Stop Router** where they apply.
- **The Router window** is a separate `BrowserWindow`, loading
  `http://127.0.0.1:<router port>/`. It opens on Auto Routing, with Classifier
  (including the Jev Settings card) in its sidebar. There is one Router window
  at a time; asking again focuses it.
  - Same `contextIsolation: true`, `sandbox: true`,
    `nodeIntegration: false` as the Gateway window.
  - **No preload.** The Router page gets no `hermesShell` bridge. It cannot
    reach `gateway:restart` or any other IPC.
  - Navigation is pinned to the Router origin. `will-navigate` to any other
    origin is refused, and `window.open` goes to the system browser for
    `http(s)` only.
- The frontend never calls two backends. There is no CORS change, no proxy,
  and `webSecurity` is untouched.

## 3. Configuration ownership

- **Path.** `HERMES_ROUTER_CONFIG` if set. Otherwise Desktop asks the bundled
  binary: `hermes router config-path`, a new, additive, read-only subcommand
  that prints the path `hermes router` itself would read (`router.json` in
  the config directory, honouring `HERMES_GATEWAY_HOME`). Desktop then passes
  that path to the Router with `--config`. The binary stays the only source of
  truth for platform directories, including inside the Flatpak.
- **Preflight, before anything is spawned:**
  1. Missing: state *Needs configuration (missing)*.
  2. Unreadable: *Needs configuration (unreadable)*.
  3. Not a JSON object: *Needs configuration (malformed)*.
  4. `hermes router validate-config --config <path>` fails:
     *Needs configuration (invalid)*, showing the Router's own messages,
     which name environment variables and never their values.
- **Never written by Desktop:** `router.json`, its `.bak`, or the Gateway's
  `config/api.json`. Gateway keys are never read, copied into the Router's
  configuration, or used to infer a Jev key. The Router child inherits the
  shell's environment exactly as a terminal launch would. Desktop adds no
  variables to it.
- **Template (explicit action only).** The *Needs configuration (missing)*
  dialog offers **Create template…**. That writes `router.template.json`
  beside where `router.json` would be, never `router.json` itself:
  - It is written to a temporary file with mode `0600` and then
    hard-linked into place. The link fails if the target exists, so the write
    is atomic and never overwrites anything, including an earlier template.
  - It holds no secrets.
  - Its one node is this Desktop's own Gateway (`http://127.0.0.1:<gateway
    port>`). Its one route uses an obvious placeholder model name.
  - Nothing ever loads it. The user edits it, saves it as `router.json`, and
    runs **Start Router**.

  Desktop never invents a live route, key or remote node.

## 4. Security

- Jev Settings is unchanged. Writes still need the per-start admin token,
  a loopback-only Router, a loopback `Host`, a matching `Origin`, `If-Match`,
  and the OS credential store. Desktop forces a loopback `--listen` for a
  Router it starts, which is exactly the condition under which the Router
  allows admin writes.
- The main process never reads the admin token or a Jev key. It does not put
  them in a URL, a window title, IPC, the clipboard or a log. The setup dialog
  shows the *command* that prints the token
  (`"<bundled hermes>" router admin-token --config "<path>"`), never the token.
- Failure messages quote the last lines of the Router's own output (bounded,
  as for the Gateway). The Router never prints keys or the token.
- Router and Gateway failures are separate states with separate dialogs. A
  Router failure never quits the app or touches the Gateway.

## 5. Lifecycle and failure handling

Router states, shown in the tray and the Router menu:

| State | Meaning | Offered |
|---|---|---|
| `off` | No Router on the port; none started | Start Router |
| `needs-config` | missing / unreadable / malformed / invalid | Create template (if missing), Show config folder |
| `starting` | Desktop spawned it; waiting for `/version` | — |
| `running` | Desktop owns it | Open, Restart, Stop |
| `attached` | A Router was already serving; not ours | Open (restart it where it was started) |
| `conflict` | The port is held by a Gateway or a stranger | Move with `HERMES_ROUTER_PORT`, or free the port |
| `failed` | Spawned, then exited or never answered | Start Router again |

- **At launch**, Desktop probes the Router port once, to show `attached` or
  `off`. It never starts a Router at launch. A persisted auto-start
  preference is deliberately deferred.
- **Start** = preflight, then identify the port, then do one of:
  - attach, if a Router is already there;
  - report `conflict`, if a Gateway or a stranger is there, or nothing
    answers in time;
  - spawn `hermes router --config <path> --listen 127.0.0.1:<port>
    --web-root <panel>` and poll `/version` until it answers as a Router. If
    the child dies first, `failed` quotes its output. An "address in use" in
    that output becomes `conflict`.
- **Restart** (owned only) is how saved Jev Settings take effect. Any open
  Router window reloads when the Router answers again.
- **Gateway start and restart** do not touch the Router supervisor. **Router
  start, stop and restart** do not touch the Gateway supervisor.
- **Quit** stops only owned children: first the Router (it depends on the
  Gateway as a node), then the Gateway. Each gets its grace period. An
  attached process of either kind is left running.

## Tests

- `router.test.ts` (no display): command construction, identification
  (Router, Gateway, stranger, nothing, unresponsive), config preflight
  (missing, unreadable, malformed, invalid, valid), port-equality refusal,
  template no-clobber and mode. Also attach-vs-own, stop and restart
  ownership, port conflict and "exited before serving", run against a
  scripted fake Router binary.
- `windows.test.ts`: the Gateway URL stays the Gateway port, the Router URL
  is the Router port, the Router window's preferences have no preload and keep
  the three security flags, and the navigation guard admits only the Router
  origin.
- `router.integration.test.ts` (real `hermes`, opt-in by presence like the
  existing supervisor test): a real Gateway and a real Router run at once,
  each supervisor's stop leaves the other's process serving, and a second
  supervisor attaches rather than starting a rival.
- `e2e/desktop-router.mjs`, run by `scripts/render-desktop.sh` under `xvfb`
  in a new `desktop` job in `render.yml`:
  - **Start mode:** the real Electron app starts the Gateway, **Start
    Router** starts an independently configured Router (scripted Jev and
    nodes), and **Router & Jev Settings…** opens the Router window. The
    Gateway window still renders and calls only the Gateway. The Router
    window shows Classifier and the Jev Settings card, and both Test
    Connection buttons reach only the Router origin. Quit leaves no child
    running.
  - **Attach mode:** externally started processes are attached, not killed,
    on quit.
  - **No leaks:** the Jev key and the admin token appear in no DOM, browser
    storage, response body, Desktop log or uploaded artifact.

## Deliberate deferrals

- **First-run route configuration.** Desktop does not author routes. The
  template is a starting point the user edits. A guided route editor is
  future work.
- **Auto-start the Router at launch.** Off. A persisted, user-approved
  preference could be added later.
- **Copying the admin token.** Not offered. The token stays a thing the user
  fetches with the shown command, as in a browser.
