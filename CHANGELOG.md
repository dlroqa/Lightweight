# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

## [0.3.3] - 2026-09-09

A patch release. When Agent Tools cannot connect, the agent server can now be
started from the panel itself — the desktop shell no longer requires a separate
terminal to bring the agent screens to life. Strictly additive: a gateway with
no configurable agent origin, or an agent already running, is left exactly as
before.

### Added

- **Start the Lightagent server from Settings.** A new *Lightagent server* card
  shows the agent server's status (checking, running, stopped, starting, failed
  or not responding) and its address, polling every two seconds, and offers a
  *Start server* button that enables the Agent, Agent Tools and Chat screens
  without leaving the panel. The gateway starts the child at its configured
  `http://127.0.0.1:<port>` or `http://localhost:<port>` upstream, resolving the
  `lightagent` binary beside its own executable, then on `PATH`, with
  `LIGHTAGENT_BIN` as an override. The child inherits the gateway's environment
  (including `LIGHTAGENT_HOME`) and stops when the gateway shuts down; an agent
  already answering is left running. New gateway routes
  `GET /api/v1/agent-server` and `POST /api/v1/agent-server/start` back the
  card, and startup progress and any error the agent returns are surfaced in
  Settings. The README documents the flow, and an end-to-end check exercises the
  start path.

## [0.3.2] - 2026-09-09

A patch release. A gateway that fronts the panel without an agent upstream no
longer answers the agent screens with its own HTML, and when something is
genuinely misconfigured the panel now says what to do about it instead of
surfacing a raw parse error.

### Fixed

- **An unconfigured agent proxy returns a setup error, not the panel's HTML.**
  The `/api/lightagent` namespace is now a real route even when no
  `--agent-upstream` is set, answering with a `503` JSON setup error naming the
  fix rather than falling through to the panel fallback and returning
  `index.html` — the `<!doctype …>` the agent screens choked on. A configured
  gateway proxies exactly as before.
- **Agent Tools explains a bad response and offers Retry.** The panel rejects a
  non-JSON response from the agent API with an actionable message (start
  `lightagent serve`, connect the gateway with `--agent-upstream`) and adds a
  Retry that re-runs the load once the connection is corrected, instead of
  showing `… is not valid JSON`. The end-to-end render check now proves both the
  message and the recovery.

## [0.3.1] - 2026-09-06

A patch release. When the control panel is served by the gateway, its agent
screens — Agent, Tools and Chat — now reach the agent API instead of falling
through to the panel's own HTML, so they work the same way the rest of the panel
already did.

### Fixed

- **The panel's agent screens reach the agent API through the gateway.** The
  agent API (`lightagent serve`) runs on its own server and port, and the gateway
  had no route for its `/api/lightagent` prefix, so those calls fell to the panel
  fallback and came back as `index.html` — which the panel's JSON parse rejected.
  The gateway now reverse-proxies `/api/lightagent/*` to the agent server,
  streaming responses (including the run event stream), so the agent, tools and
  chat screens are same-origin with the rest of the panel and need no CORS — the
  same property `--web-root` gives the control API. Configured by `hermes serve
  --agent-upstream <origin>` (default `http://127.0.0.1:8735`, `off` to disable);
  the cross-origin write guard covers the proxied surface, and a gateway with no
  upstream is unchanged.
- **An intermittent failure in the RAG store tests.** Two tests shared a scratch
  directory keyed only on a timestamp, so under parallel execution one could
  delete the other's directory mid-run; each call now gets a unique directory.
  Test-only — no runtime behaviour changed.

## [0.3.0] - 2026-09-04

The first release to include Lightagent, the agent harness, alongside the
Lightweight inference engine. A minor bump: Lightagent is a large, strictly
additive product surface, and the engine's binaries, tests and dependency policy
are untouched, so nothing existing breaks.

### Added

- **Lightagent, the agent harness.** New crates and a new `lightagent` binary
  serving the agent runtime — runs, sessions, tools, approvals and their event
  stream — with agent screens in the shared control panel, added alongside the
  inference engine without changing it.

## [0.2.1] - 2026-09-01

Public reach and multi-model serving. The gateway can now sit behind a trusted
reverse proxy or Cloudflare Tunnel with `--behind-proxy` — reachable at a real
domain, key-required, and no longer fooled into treating a remote caller as
local — and `hermes fleet` runs up to four models at once as isolated
per-tenant gateways. Per-user API keys and their rate limits now take effect the
moment they change instead of at the next restart, and the desktop icons are
rounded to the macOS squircle with a new violet-feather menu-bar mark.

### Added

- **`--behind-proxy` mode** for putting the gateway behind a trusted reverse
  proxy or Cloudflare Tunnel while it stays bound to loopback. It turns on
  API-key auth (refusing to start without a credential) and trusts the proxy's
  `CF-Connecting-IP` header — only from a loopback peer — so a remote caller is
  identified by its real address rather than passing as local. Set it with the
  flag or `HERMES_BEHIND_PROXY`. A plain loopback gateway is unchanged.
- **`hermes fleet`** runs up to four models at once, one isolated gateway per
  model. Each entry in a small JSON manifest gets its own data root, port and
  keys, so one tenant's traffic can never evict or disturb another's. The
  four-model cap and the manifest checks (duplicate ports/names, missing model
  files, a profile with no key) are enforced before anything launches.
- **A public-domain recipe** in the README: reaching the gateway at
  `https://…/v1` over a Cloudflare Tunnel with `--behind-proxy`, and serving
  several models behind per-hostname routing with `hermes fleet`.

### Changed

- **Per-user API keys and limits now take effect live.** Creating a key,
  changing its rate limit, or revoking it through the control API is honoured on
  the next request instead of at the next restart — the gateway reloads its key
  set from the store on each change. A revoked key stops working immediately.
- **The menu-bar (tray) icon** is now its own transparent mark, keyed from a
  dedicated `icon/tray-source.png`, rather than the plated brand icon.
- **The desktop app icons are rounded** to the macOS "squircle" with a
  transparent margin, so the app sits on the dock like a native one instead of a
  hard-edged square. Generated for every packaged size by `scripts/build-icons.py`.

### Fixed

- **An engine launch that loses the ephemeral-port race is retried.** The
  supervisor hands the engine a loopback port it proved free a moment earlier;
  on a busy machine another process can take it in the gap before the engine
  binds. Such a launch is now retried with a fresh port instead of surfacing the
  crash, as the design always intended. Only that transient case is retried — a
  signal, a timeout, or a genuinely unstartable engine is still reported at once.

## [0.2.0] - 2026-08-31

Remote access: the gateway can now be reached from another machine over any
overlay network, authenticated with named API keys that survive a restart and
can be rate-limited per key. A new **Access & Keys** panel and the `hermes key`
/ `hermes config` commands manage it, and the bind hosts and port persist in
`config/api.json`. The default port moves to **11434** to agree with the desktop
app and the common local-LLM clients — a behaviour change for anyone who relied
on the old `8737`.

### Added

- **Named, hashed API keys.** A gateway can now issue a key per consumer, each
  nameable and revocable on its own. Keys are stored as SHA-256 hashes and a
  display prefix in `config/api-keys.json`; the plaintext is shown once, at
  creation, and never again. Create, list and revoke them with `hermes key`, or
  on the panel's new **Access & Keys** screen. The existing `--api-key` /
  `HERMES_API_KEY` static key still works alongside them.
- **Per-key rate limits.** Each key can carry a per-minute and a per-day
  ceiling, enforced live: a key over its limit gets a `429` with a `Retry-After`.
  Loopback and anonymous callers (the panel, a local script) are never metered.
- **Persisted bind configuration** in `config/api.json` — the hosts and port the
  gateway serves on, read beneath the command-line flags so a typed `--host` or
  `--port` always wins. Edit it with `hermes config`, or the panel's *Serve on*
  control, which lists the machine's reachable addresses tagged with the reserved
  range each falls in (a Tailscale/CGNAT address reads *shared range*).
- **The `lightweight` command**, a second entry point identical to `hermes` that
  prints a feather welcome mark on an interactive terminal. `NO_COLOR` and
  `LIGHTWEIGHT_NO_BANNER` are honoured; `--json` and pipes are never decorated.
- `hermes sysinfo` reports an address's reserved-range scope, in the human output
  and as an `addresses` array under `--json`.
- **`hermes serve --port auto`** (equivalently `--port 0`) binds a kernel-assigned
  free port and prints it — the explicit way past a taken 11434 without moving the
  default. With several `--host` values it binds them all to the one shared port.
  It is a per-run choice and is never written to `api.json`.

### Changed

- **The `address in use` message on the default port is now a signpost.** Because
  11434 is also Ollama's default, `hermes serve` names that likely cause and
  suggests `--port auto`, a different `--port`, or stopping the other process,
  rather than failing with a bare "in use". The desktop shell inherits the same
  guidance and points at its own levers (`HERMES_PORT` or the *Serve on* control).

- **The default port is now 11434** (was 8737), so the CLI, the desktop shell and
  the dev proxy agree and a client assuming the common local-LLM port finds the
  gateway. Anyone who relied on the old default must now pass `--port 8737`.
- The desktop shell no longer mints a fresh API key on every launch — the bug
  that broke a key shared with a remote agent. Keys are the gateway's own, and the
  tray's "Copy API key" is now "Manage API keys…", which opens the panel.
- State-changing control endpoints under `/api/v1` now refuse a request from a
  foreign origin, and creating keys or widening the bind set is refused from a
  non-loopback peer: those take access to the machine running the gateway.

## [0.1.2] - 2026-08-30

### Added

- CPU utilization is now reported on macOS and Windows, so the Performance
  page's CPU Usage tile shows a live figure on every supported platform instead
  of only on Linux. It is read through `host_statistics` on macOS and
  `GetSystemTimes` on Windows, and normalised to the same tick units the panel
  already differences.

### Changed

- Completed the rename to **Lightweight**: the native desktop application — the
  window title, tray, menus, dialogs, and the installer and artifact names —
  now reads "Lightweight" instead of "Hermes", following the panel rename in
  0.1.1.
- Renamed the internal workspace crates from `hermes-*` to `lightweight-*`. The
  `hermes` command and its `HERMES_*` environment variables, the `hermes_*`
  metric names, the `hermes::` log targets, and the existing data directory are
  deliberately unchanged, so nothing that scripts, scrapers, or existing
  installs depend on has moved.

### Fixed

- Long file-path values no longer overflow their cards on the Settings and API
  Gateway pages; they wrap within the card instead.

## [0.1.1] - 2026-08-26

### Changed

- Renamed the desktop shell to **Lightweight**: the sidebar brand name and the
  window title bar now read "Lightweight" instead of "Hermes".

## [0.1.0] - 2026-08-25

### Added

- An OpenAI-compatible, CPU-only inference gateway backed by a supervised
  llama.cpp process.
- GGUF model discovery, verified downloads, imports, and live model switching.
- Conservative RAM admission control with context and KV-cache sizing.
- Benchmarking and machine-scoped calibration with trust checks that reject
  unsafe fits.
- A desktop UI and CLI packages for macOS, Windows, and Linux.

### Known limitations

- Calibration is intentionally deferred for pinned llama.cpp `b10590`:
  `hermes bench --fit` safely refuses every honest fit, so the shipped estimates
  remain conservative by 1.37×–2.85×.

[Unreleased]: https://github.com/dlroqa/Lightweight/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/dlroqa/Lightweight/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/dlroqa/Lightweight/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/dlroqa/Lightweight/releases/tag/v0.1.0
