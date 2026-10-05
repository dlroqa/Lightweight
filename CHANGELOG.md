# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- **Router load balancing (R4).** Each route chooses its `strategy`:
  `priority` (the default, unchanged), `round_robin`, or `least_busy`.
  - **Round-robin** takes equal turns over the deployments that are currently
    eligible, in configured order. The per-route cursor advances once per
    request, so failover attempts do not move it.
  - **Least-busy** picks the eligible deployment with the lowest
    `in-flight / advertised concurrency limit`, compared exactly, with ties
    going to configured order. A deployment whose capacity is unknown or zero is
    used only as a fallback.
  - Health eligibility is shared by every policy. The router keeps its own
    in-flight count per deployment, and it is released on every exit path,
    client disconnects included.
  - The admin deployments view shows `active_requests` and `concurrency_limit`.
    Logs carry the policy and how the choice was made. `/metrics` gains decision
    counts by policy and a per-deployment in-flight gauge.
  - An unknown `strategy` is refused at startup. No latency, weights, session
    affinity or capability filtering is involved.
- **Router capability filtering (R5).** Before a route's policy chooses, the
  router drops every available deployment that cannot serve the request in
  hand, using that deployment's own last-probed capabilities.
  - It checks the endpoint (chat or text completion), declared `tools`, a
    `tool_choice` that must be honoured (`required`, a named function, or
    `none` beside tools), a `reasoning_effort` that asks for reasoning, and
    whether the prompt fits the deployment's context.
  - The context rule is the node's own: a prompt must leave room to generate,
    and `max_tokens` is clamped rather than required. The router counts a
    lower bound on prompt tokens, so it never refuses a request a node could
    serve.
  - Priority, round-robin and least-busy are unchanged and see only capable
    deployments. Failover never reaches a deployment that was filtered out.
  - When available deployments exist but none can serve the request, the
    answer is `400 route_capability_mismatch`. It names the route and what was
    missing, never a node. That is distinct from `model_not_found` and
    `route_unavailable`. A request the gateway would refuse as malformed gets
    the gateway's own `400` from the router.
  - A node's `400 context_length_exceeded`, which the router's lower-bound
    estimate could not foresee, moves the request, before anything is sent,
    to a deployment in the plan with a strictly larger context. If there is
    none, the node's error is returned unchanged. Every other `400`, and every
    `500`, still stands.
  - Logs carry each request's requirements and candidate counts before and
    after filtering. `/metrics` gains `router_capability_filtered_total`,
    `router_capability_mismatch_total` and
    `router_context_overflow_failovers_total`. Existing router configurations work
    unchanged.
- **User-defined model aliases.** Give any installed model a short name of your
  choosing (`Coder`, `Fast`, …) when adding it or at any time later, from the
  panel's Models screen, `hermes models alias`, or
  `PATCH /api/v1/models/{id}` (`{"alias": null}` clears it). Renaming applies
  at once without a reload. Aliases persist in the catalog, are never derived
  from the file, and existing catalogs load unchanged with no alias set.
- **Aliases are the public model ids.** `/v1/models`, `/v1/capabilities` and
  every chat and text completion response, streamed chunks included, name an
  aliased model by its alias; an unaliased model is listed by its canonical id
  as before. The alias is a stable name with no `@context` suffix.
- **Canonical ids remain supported.** Requests may name a model by its alias
  (any casing), its canonical id, `default`, or not at all, and the control API
  reports both `id` and `alias`. Aliases and canonical ids share one namespace:
  an alias may not equal any model id, `default` is reserved, duplicates are
  refused rather than renamed, and a pinned or linked model whose id is already
  an alias is refused before it downloads.
- **Lightagent compatibility.** Lightagent discovers the alias from
  `/v1/models` and sends it back as `model` with no Lightagent change; verified
  against a real engine.
- `hermes models alias` changes an alias through a running gateway that serves
  the same profile, rather than editing the catalog file under it.
- **Federated model router (`hermes router`).** A new, separately run
  `lightweight-router` crate puts one OpenAI-compatible endpoint in front of
  several Lightweight gateways.
  - Clients discover and send logical route names (`Coder`, `Fast`).
  - Each route lists node deployments in priority order. The router picks the
    first healthy one, rewrites `model` to that node's own alias, and rewrites
    the response back to the route name, in streamed chunks too. Streams are
    relayed frame by frame, and a client disconnect cancels the node's
    generation.
  - Failover happens only before any response has started: on a refused
    connection, a 502/503/504, or a node that stopped serving the model. A
    stream that fails mid-way ends with an error frame and no `[DONE]`.
  - Unknown routes are `model_not_found`. A known route with nothing healthy is
    `route_unavailable`. `default` resolves only to an explicitly configured
    `default_route`.
  - Node health comes from probing each node's `/v1/capabilities`, with a
    failure threshold.
  - Client and node credentials are separate, and every key is read from an
    environment variable.
  - Read-only `/api/router/v1/{nodes,routes,deployments,health}`, Prometheus
    `/metrics`, and `hermes router validate-config`.
  - `hermes serve` is unchanged. See `docs/ROUTER.md`.

### Fixed

- **`model: "default"` is accepted again.** It was dropped from the gateway in
  the Lightagent split; it once more selects whichever model is resident, on
  both `/v1/chat/completions` and `/v1/completions`.
- **`/v1/capabilities` reports the live concurrency limit.**
  `limits.max_concurrent_requests` was the slot count the gateway started with.
  It is now the scheduler's current count, so it follows a model load that
  resizes it, and the router's least-busy policy picks it up on its next probe.

## [0.4.1] - 2026-09-30

### Added

- **Optional Open WebUI workbench.** A separately deployable companion stack
  documents authenticated multi-gateway routing, private document extraction and
  retrieval, reranking, web search, and an administrator-only terminal profile.
- **Optional Jev model advice.** The `Lightweight Auto` Open WebUI Pipe can
  advise among explicitly allowlisted, prefixed Lightweight routes, with a
  deterministic local fallback for every timeout, invalid answer, or low-confidence
  response.

### Fixed

- The locally served Chat panel now uses its authenticated internal control route
  when a gateway key is configured. Public `/v1` clients still require a Bearer
  credential, including attempts to forge the local-control marker.

## [0.4.0] - 2026-09-29

### Added

- **Public provider capability discovery.** Authenticated clients can now use
  `GET /v1/capabilities` to discover the versioned public inference contract,
  supported OpenAI-compatible endpoints and features, model readiness, served
  context length, and concurrency limit. The endpoint shares `/v1`
  authentication, never loads a model, and intentionally excludes control-plane
  routes, filesystem paths, hardware details, engine internals, jobs, and keys.

### Changed

- **Deterministic paired-provider test support.** The test-only mock gateway can
  consume a startup-provided queue of scripted completions, enabling a
  separate-process public HTTP/SSE integration test without exposing a runtime
  test control endpoint.

## [0.2.4] - 2026-09-18

### Fixed

- The control panel again shows its values when a static gateway API key is
  configured. Its status endpoints — `/api/v1/gateway`, `/api/v1/metrics`, the
  `/api/v1/events` live feed, `/api/v1/system`, and `/api/v1/requests` — were
  being refused on loopback, so the panel (which cannot carry the key, and whose
  `EventSource` cannot send an `Authorization` header at all) was shut out of its
  own status surface. The entire `/api/v1` control surface is now admitted on
  loopback like the rest of the panel; remote requests to it remain
  key-protected, and state-changing routes keep their cross-origin guard.

## [0.2.2] - 2026-09-18

## [0.2.3] - 2026-09-18

### Fixed

- The locally served control panel now continues to reach its own management
  API when a static gateway API key is configured, without exposing that key to
  the browser. Remote API access remains key-protected.

### Changed

- Lightweight is again an exclusive local inference engine. The Lightagent
  runtime, CLI, web interface, gateway proxy, packaging, extensions, and
  release artifacts have been removed. Lightweight continues to provide its
  OpenAI-compatible API, GGUF model management, native inference CLI, and
  inference control panel.

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

[Unreleased]: https://github.com/dlroqa/Lightweight/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/dlroqa/Lightweight/compare/v0.2.4...v0.4.0
[0.2.4]: https://github.com/dlroqa/Lightweight/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/dlroqa/Lightweight/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/dlroqa/Lightweight/compare/v0.2.1...v0.2.2
[0.1.2]: https://github.com/dlroqa/Lightweight/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/dlroqa/Lightweight/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/dlroqa/Lightweight/releases/tag/v0.1.0
