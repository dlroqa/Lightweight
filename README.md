# Hermes CPU Inference Gateway

A local, **CPU and RAM only** LLM inference platform: an OpenAI-compatible
gateway, GGUF model management, and a desktop UI. No GPU is required anywhere,
and none is used.

The inference engine sits behind a trait boundary so the current llama.cpp
backend can later be replaced by a proprietary Hermes runtime without touching
the UI or the API.

## Status

Milestones 0 to 3 are complete. The gateway serves an OpenAI-compatible API
over a supervised llama.cpp child process: streamed and non-streamed chat
completions, `/v1/models`, `/health` and `/props`, with RAM admission control
and GGUF metadata underneath it.

A three-turn streamed conversation through the genuine `openai` Python SDK
against a real model works end to end, and disconnecting mid-stream stops the
engine within milliseconds. Tool calls in a real agent loop and the full error
taxonomy are next. See [docs/PROGRESS.md](docs/PROGRESS.md) for the current
checkpoint.

## Verified constraints

These were measured on the development machine, not assumed. They shape several
design decisions and are worth knowing before changing anything.

| Fact | Consequence |
|---|---|
| The dev CPU (Pentium Silver J5005) has **SSE4.2 only — no AVX, AVX2, FMA or F16C** | The engine must be dispatched at runtime, never built with `-march=native` |
| Official llama.cpp Linux/Windows builds use `GGML_BACKEND_DL=ON GGML_CPU_ALL_VARIANTS=ON` | Stock prebuilt binaries run here via `libggml-cpu-sse42.so`; **no compilation, no cmake, no sudo** |
| Measured `ggml_backend_score()` on this CPU | `sse42` = 5, `x64` = 1, every AVX-and-above variant = 0 |
| `sudo` is unavailable | Nothing may require a system package |

The last of those is why the dependency policy below exists.

## Dependency policy

Enforced by `scripts/check-deps.sh`, which CI runs. Violating it makes the
workspace **unbuildable** on the target machine, and the failure surfaces as an
inscrutable build-script error rather than as "you broke the policy".

- **No cmake.** `rustls` defaults its crypto provider to `aws-lc-rs`, whose
  build requires cmake. Pin `ring` instead: `reqwest` with `rustls-no-provider`,
  plus an explicit `rustls::crypto::ring` provider at startup.
- **No OpenSSL.** `libssl-dev` is absent, so `native-tls` and `openssl-sys` are
  banned. rustls end to end.
- **No libclang**, therefore no `bindgen` and no llama.cpp FFI. The engine is a
  supervised child process instead — which also means a backend crash is
  recoverable rather than fatal.
- **No nightly**, and no crates with a heavy proc-macro compile cost
  (`polars`, `tonic`, `sqlx`). Builds run on 4 cores at 1.5 GHz.

## Build and test

```sh
cargo test --workspace       # 383 tests, no network, no model downloads
./scripts/check.sh           # fmt, clippy, tests, contract suite, dependency + secret gates
./scripts/contract-test.sh   # the real openai SDK against the gateway
```

The contract suite is the highest-value tier: it points the genuine `openai`
Python package at a real gateway backed by a deterministic mock engine and
asserts what the **client** ends up with — content assembled, tool-call
arguments that parse as JSON, `usage` populated from the terminal chunk — rather
than what we intended to send. It also imports Hermes' own
`parse_context_limit_from_error` and checks that our context-overflow message
parses back to the right number.

`HERMES_TEST_MODEL=<path.gguf>` additionally enables the real-engine tests,
which download the pinned engine, load a genuine model, stream a completion and
prove that a dropped stream stops the engine decoding.

`scripts/fetch-real-headers.sh` captures the headers of six real models from
HuggingFace with HTTP range requests — a few megabytes rather than the tens of
gigabytes the full models would cost. The tests that use them skip when they are
absent, so a clean checkout still passes; set `HERMES_REQUIRE_REAL_MODELS=1` to
turn that skip into a failure, which is what CI does.

## CLI

```sh
cargo run -p hermes-cli -- sysinfo
cargo run -p hermes-cli -- inspect model.gguf
cargo run -p hermes-cli -- estimate model.gguf --ctx 8192 --kv-type q8_0

# Acquire the engine, admit the model, load it, and serve the OpenAI API.
# With no --ctx, the largest context that fits this machine is chosen.
cargo run -p hermes-cli -- serve model.gguf --port 8737
```

Then point any OpenAI client at `http://127.0.0.1:8737/v1`:

```sh
curl 127.0.0.1:8737/v1/models
curl -N -H 'Authorization: Bearer no-key-required' \
     -d '{"model":"<id from /v1/models>","messages":[{"role":"user","content":"hi"}],
          "stream":true,"stream_options":{"include_usage":true}}' \
     127.0.0.1:8737/v1/chat/completions
```

Authentication is off for a loopback bind — a client that sends
`Bearer no-key-required`, or no header at all, is accepted — and is **forced on**
the moment any bind is reachable from another machine.

## Remote access

The gateway serves whichever addresses it is told to. It draws exactly one
distinction — **loopback or not** — and knows nothing about interfaces,
networks, or which product assigned an address, so a plain LAN, Tailscale,
Headscale, Netmaker, ZeroTier, Nebula and a hand-rolled WireGuard are all the
same case:

```sh
export HERMES_API_KEY=$(openssl rand -hex 24)   # required off loopback; no default, ever
hermes serve model.gguf --host <address-or-name> --port 8737

# A machine holding several addresses can serve on each of them, with one
# engine and one queue behind them all:
hermes serve model.gguf --host <lan-name> --host <mesh-name> --port 8737
```

`--host` takes an address in either family or a **name**, resolved at startup.
Prefer the name: an overlay network can reissue an address, and a name usually
survives it. Check what the name resolves to first — many systems map the local
hostname to a loopback address in `/etc/hosts`, which serves nobody else.

Without a key, a bind that other machines can reach is refused rather than
silently exposed, and the refusal prints a generated key to use. An
unauthenticated caller is not shut out of everything: `/health` and `/props`
still answer with what a client needs to size a prompt, minus anything about
this machine.

### What address to serve on

This is the one surprising part, and it is a property of the **client**, not of
this gateway. Hermes raises its stream read timeout from 120 s to 1800 s, and
its stale-stream window from 180 s to 900 s, only for hosts it considers local
— which matters enormously when prefill on a CPU can take minutes:

| Reached at | Long-prefill timeouts | Typical source |
|---|---|---|
| `10/8`, `172.16/12`, `192.168/16` | **yes** | LAN, Netmaker, ZeroTier, Nebula, WireGuard |
| `100.64/10` | **yes** | Tailscale, Headscale |
| IPv6 unique-local `fd00::/8` | **yes** | Tailscale v6, ZeroTier 6PLANE |
| a name with no dots | **yes** | any of the above |
| any dotted FQDN, including `.local` and `*.ts.net` | no | mDNS, MagicDNS, TLS proxies |
| a routable public address | no | port forwarding, cloud hosts |

If a deployment has to use an FQDN, set `HERMES_STREAM_READ_TIMEOUT` and
`HERMES_LOCAL_STREAM_STALE_TIMEOUT` on the client rather than hoping.

**Encryption is the network's job, not this gateway's.** WireGuard-based
overlays and ZeroTier already encrypt the hop; on a plain LAN, HTTP is in the
clear, so the API key protects against *use*, not against *reading*. On an
untrusted network the honest answers are an overlay, an SSH tunnel, or a
TLS-terminating proxy — the last two usually meaning an FQDN, with the
consequence in the table above.

### Keeping it running

`hermes serve` in the foreground needs nothing from any platform and is the
portable answer. For a machine that should serve after logout,
`packaging/systemd/` holds a Linux `systemd --user` example whose every value —
model, addresses, key — comes from a file outside the repository. macOS and
Windows service wrappers arrive with cross-platform packaging in M10.

`estimate` exits non-zero when a model would not fit, so scripts can branch on
the verdict without parsing the report. Add `--json` to any command for machine
-readable output.

## Layout

| Crate | Responsibility |
|---|---|
| `hermes-core` | Domain types, the actionable-error contract, privacy primitives. Pure: no I/O |
| `hermes-gguf` | Bounded, panic-free GGUF header reader. Never reads tensor data |
| `hermes-system-info` | CPU topology, ISA detection, memory probing, data directories |
| `hermes-memory` | RAM estimation and admission verdicts |
| `hermes-inference` | The backend abstraction. Contains no engine, by design |
| `hermes-backend-llamacpp` | Acquires and supervises `llama-server`, and translates its SSE into our events |
| `hermes-backend-mock` | A deterministic backend, so the layers above are testable without an engine |
| `hermes-api` | OpenAI request and response types, and the SSE chunk codec |
| `hermes-gateway` | The HTTP surface: routes, auth, streaming, cancellation |
| `hermes-observability` | Structured logging, rotation, privacy-mode wiring |
| `hermes-cli` | Command-line access to the above |

## Designed for the machine it runs on

Limits are derived at runtime, never frozen as constants measured during
development. Thread count comes from detected physical cores; the RAM safety
margin is `max(512 MiB, 15% of available)`, so it grows with the machine; the
ggml CPU variant is chosen by detected instruction set, so one artifact is
optimal on a modern CPU and functional on an old one; and `serve` with no
`--ctx` picks the largest context that still loads safely, which is 8K on a
small laptop and the model's full maximum on a capable machine.

The development machine is deliberately a constrained one — four slow cores, no
AVX, most of its memory already spoken for — but it is one end of the range, not
the design point. Nothing in the repository is tuned to it; `CARGO_BUILD_JOBS`
is documented for constrained hosts rather than pinned in `.cargo/config.toml`.

## Two invariants worth preserving

**Errors are actionable by construction.** Every error type implements
`hermes_core::Actionable`, so adding a failure mode without stating what the
user can do about it does not compile.

**The client's contract is pinned by tests, not by intent.** The chunk order,
the tool-call id-once discipline, the usage chunk's empty `choices`, and the
exact bytes of a stream are all asserted — the last of those by byte-exact
golden transcripts. Where a client behaviour matters, the test runs the client's
own code: the real `openai` package, and Hermes' own error parser.

**Prompts cannot be logged by accident.** User text is wrapped in
`hermes_core::Private`, which redacts through both `Display` and `Debug` — so
`tracing`'s `?field` capture is safe — and has no `Serialize` impl, so it cannot
be swept into a JSON log line by an enclosing `derive`. Contents come out only
via `.reveal()`, a single greppable token. Privacy Mode is a one-way latch:
once set, prompt logging cannot be re-enabled for the life of the process.
