# M6b — the desktop shell and the control panel

The approved plan for M6b, written before any code, and checked against the
source rather than against memory of it. Every claim below about what the
gateway does today was read out of `crates/` on the day this was written.

M6a left a control API and no way to see it. M6b is the seeing. What the
reference design asks for, though, is not a client for the API that exists —
about half of it has nothing behind it at all. That is stated first, because a
plan that hides it would produce a panel of convincing numbers that are not
measurements.

## 1. What is already served, and what is not

Read against the eight screens in the reference design.

| Screen | Element | State | Source |
|---|---|---|---|
| Dashboard | model switcher, current-model card | served | `GET /api/v1/models`, `POST /api/v1/models/{id}/load` |
| | tokens/sec, uptime, request counts | served | `GET /api/v1/metrics` |
| | context length | served | `/props`, catalog `context_length` |
| | RAM usage | not exposed | `MemorySnapshot` exists; no endpoint reaches it |
| | CPU usage | absent | `CpuInfo` is static ISA and core detection; nothing samples `/proc/stat` |
| | disk usage | absent | `statvfs` was refused in M6a because it needs `unsafe` |
| | inference live feed | absent | the data exists only as tracing lines in the log file |
| | connected clients | absent | no connection gauge in `metrics.rs` |
| Chat | streaming, per-message tok/s and token counts, stop | served | `/v1/chat/completions`; cancel-on-disconnect proven in M3 |
| | temperature, top-p, context | served | request parameters |
| | conversation list, search, persistence | absent | `paths.conversations_dir()` exists and nothing calls it |
| Models | table, details, import, download, delete, unload | served | the whole `/api/v1/models` and `/api/v1/catalog` surface, with job SSE |
| | RAM estimate column | not exposed | the M1 estimator exists; `CatalogRow` does not carry it |
| | vocab size, hidden size, layers, heads | absent | the GGUF has them; the catalog stores only architecture, params, quantization, context |
| Inference | temperature, top-p, top-k, min-p, seed, all three penalties | served | accepted **and forwarded** — `chat.rs:296-306`, not dropped |
| | context length, CPU threads | served | `LoadBody` into `LoadOptions` |
| | batch size | absent | not in `LoadOptions`, not passed to the engine |
| | save preset | absent | `paths.settings_file()` unused |
| Performance | token totals, cache hit rate, decode and prefill rates | served | metrics |
| | the four charts over time | absent | metrics are cumulative counters; there is no history |
| | hardware panel | not exposed | `CpuInfo` has it; no endpoint |
| | run benchmark | absent | nothing; `benchmarks_dir()` unused |
| API Gateway | status, host, port, key, max clients, LAN toggle, endpoints, recent requests | absent | no endpoint reports the gateway's own configuration |
| Settings | every row | absent | no settings store on either side |
| Logs | level and source filters, search, table | absent | logs go to a file; no read endpoint |

Models is fully backed. Chat is backed except for history. Dashboard is half
backed. Performance, API Gateway, Settings and Logs are mostly facade. M6b is
therefore a backend milestone with a UI on top of it, and is staged that way.

## 2. Four decisions that cut across every screen

### 2.1 Origin: no CORS is added, because none is needed

There is no CORS layer in the gateway and `tower-http` is not in the dependency
graph. Adding one would be the obvious move and it is the wrong one.

In development, Vite's dev server proxies `/api` and `/v1` to the gateway. In
production, the gateway serves the built SPA at `/`. Both are same-origin, so
the question never arises — and a browser on another machine reaches the panel
over the exposed bind M3.5 built, for free.

Static serving is written by hand rather than taken from a crate. It is roughly
eighty lines, and the Prometheus text formatter in `metrics.rs` set the
precedent: a dependency for something that small is weighed, not assumed.

### 2.2 Charts are sampled by the client

The gateway keeps cumulative counters and no time series, and it should stay
that way — a ring buffer in the server is state to own, invalidate and test,
for a line that only one screen reads.

The SPA polls `/api/v1/metrics` on a one-second tick and differences
consecutive samples. A sparkline then means "since you opened this screen",
which is both honest and what a control panel wants.

### 2.3 One event stream, not two

The live feed on the Dashboard and the recent-requests list on the API Gateway
screen are the same data twice. `GET /api/v1/events` serves it once, as SSE,
carrying the per-request record that already goes to the log: id, model, prompt
tokens, finish reason, duration. Same fields, same redactions — the prompt, the
key and the bound address stay out of it, as grep proved for the log in M3.5.

### 2.4 Disk comes from `rustix`

`rustix` gives `statvfs` with no `unsafe` in our crates, no build script, and
no cmake, OpenSSL or libclang. It passes `scripts/check-deps.sh` unchanged and
leaves every `forbid(unsafe_code)` intact, because the unsafe lives upstream.

This also retires a deferral from M6a: the pre-flight disk check before a
download becomes possible, instead of a full disk being discovered from
`ENOSPC` after minutes of transfer.

## 3. Stages

Each stage ends green on `./scripts/check.sh` — fmt, clippy `-D warnings`, the
full suite, the openai-SDK contract suite and the dependency gate — and gets
its own checkpoint commit, as every milestone before it has.

### M6b.1 — the backend seams

New endpoints, each behind `authorize` like everything else under `/api/v1`:

- `GET /api/v1/system` — CPU model and topology, a `/proc/stat` sampler for
  utilization, memory from the existing `MemorySnapshot`, disk from `rustix`.
  Off Linux it returns `UnsupportedPlatform` rather than an empty body, in the
  style `network.rs` already established: "nothing to report" and "I did not
  look" are opposite answers.
- `GET /api/v1/gateway` — bind addresses, whether auth is on, concurrency
  limit, engine health, version. Read-only.
- `GET /api/v1/events` — the request stream of 2.3.
- `GET /api/v1/logs` — filtered read over the log file, with level, source and
  a since-cursor.

Plus: the RAM estimate on each `CatalogRow`, the GGUF detail fields the Models
screen shows, a connection gauge in `metrics.rs`, and static asset serving.

### M6b.2 — persistence

Conversations under `conversations_dir()`, settings under `settings_file()`.
Both were designed in M0 and have never been written to. Atomic writes, as the
catalog does; the same single-writer caveat the catalog carries applies here
and is documented rather than locked around.

### M6b.3 — the SPA

React, TypeScript and Vite. Bindings are typed against the Rust DTOs by hand
and kept honest by the contract tests, not generated. Charts are hand-rolled
SVG: four sparklines and four line charts do not justify a chart library
several times the size of the rest of the bundle.

Driven against a real gateway running a real model throughout. The standard set
in M3 and M5 holds — a screen is not done because its tests pass, it is done
when it has been watched working against an engine.

### M6b.4 — the Electron shell

The shell spawns `hermes serve` as a child and supervises it, owning the key it
generates, and attaches to an already-running gateway instead of starting a
second one when it finds it. That ownership is what makes the port and LAN
settings on the API Gateway screen applyable at all, since both require the
listener to restart.

Packaging joins `packaging/systemd/`, which is currently all there is.

## 4. Deliberately left

Recorded so they are chosen rather than forgotten, in the manner of the M6a
list:

- **Batch size** is not passed to the engine, so the control is not offered.
  Showing a slider that changes nothing is worse than not showing one.
- **Run Benchmark** is deferred whole. `benchmarks_dir()` stays unused.
- **Live host, port and LAN editing** waits for M6b.4, because before the shell
  owns the process there is nothing that can restart the listener. Until then
  those fields are read-only, and say why.
- **The API key is never rendered by the browser build.** The shell may show it
  because the shell generated it; a panel reached over the exposed bind must
  not, and the two builds differ on exactly this point.

## 5. The design pass

The reference design is light frosted glass. `ui-ux-pro-max` recommends a dark
cinematic palette for this product category; the reference wins, and only what
agrees with it is kept — Inter, the glass and blur vocabulary, the easing feel.

### Tokens

| Role | Light | Dark |
|---|---|---|
| Ground | `#E8EEF8` to `#DCE7F5`, warm blush lower-right | `#0B1220` to `#131C2E` |
| Glass panel | `rgba(255,255,255,0.72)` | `rgba(148,163,184,0.10)` |
| Panel border | `rgba(255,255,255,0.90)` | `rgba(255,255,255,0.08)` |
| Panel shadow | `0 8px 32px rgba(15,42,88,0.08)` | `0 8px 32px rgba(0,0,0,0.40)` |
| Nav rail | `rgba(255,255,255,0.55)` | `rgba(148,163,184,0.06)` |
| Text | `#0F172A` | `#F1F5F9` |
| Muted text | `#64748B` | `#94A3B8` |
| Accent | `#2563EB` | `#3B82F6` |
| Success | `#16A34A` on `#DCFCE7` | desaturated counterpart |
| Warning | `#D97706` on `#FEF3C7` | desaturated counterpart |
| Danger | `#DC2626` on `#FEE2E2` | desaturated counterpart |
| Info | `#7C3AED` on `#EDE9FE` | desaturated counterpart |
| Series | `#3B82F6` `#22C55E` `#8B5CF6` `#F59E0B` | unchanged |

### Layout and type

A 240px navigation rail collapsing to 64px. An 8pt spacing rhythm at dashboard
density, 16px gutters, 16 to 20px radii. Inter throughout, with
`font-variant-numeric: tabular-nums` on every metric so a number updating once
a second does not shift the layout under it. Lucide icons at one stroke width;
no emoji anywhere.

### Two constraints the glass imposes

**Blur costs CPU, and this product's CPU is the one generating tokens.** A
dozen simultaneous `backdrop-filter` panels drop frames on exactly the hardware
Hermes is for — this machine decodes at under one token per second. So
`backdrop-filter` is used on the rail, the header and modals only; content
cards take a translucent tint with no blur. The difference is barely visible
and the cost is not.

**Contrast is where translucency quietly fails.** Panels sit at 0.72 alpha and
above rather than the 0.3 that photographs well and reads badly, and the
transparency toggle already present in the reference Settings screen is the
real escape hatch to solid surfaces. Both themes are checked independently;
neither is inferred from the other.

### Motion

150 to 200ms on hover and press, 240ms crossfade between routes, and nothing
choreographed — this is an instrument panel, not a landing page.
`prefers-reduced-motion` is respected, and animation never gates the display of
a number that has already arrived.
