# Hermes CPU Inference Gateway

A local, **CPU and RAM only** LLM inference platform: an OpenAI-compatible
gateway, GGUF model management, and a desktop UI. No GPU is required anywhere,
and none is used.

The inference engine sits behind a trait boundary so the current llama.cpp
backend can later be replaced by a proprietary Hermes runtime without touching
the UI or the API.

## Status

Milestones 0 and 1 are complete: GGUF metadata reading, CPU and memory probing,
RAM estimation with admission control, and a CLI over all three. The gateway,
the engine supervisor and the UI are not built yet.

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
cargo test --workspace       # 165 tests, no network, no model downloads
./scripts/check.sh           # fmt, clippy -D warnings, tests, dependency gate
```

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
```

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
| `hermes-observability` | Structured logging, rotation, privacy-mode wiring |
| `hermes-cli` | Command-line access to the above |

## Two invariants worth preserving

**Errors are actionable by construction.** Every error type implements
`hermes_core::Actionable`, so adding a failure mode without stating what the
user can do about it does not compile.

**Prompts cannot be logged by accident.** User text is wrapped in
`hermes_core::Private`, which redacts through both `Display` and `Debug` — so
`tracing`'s `?field` capture is safe — and has no `Serialize` impl, so it cannot
be swept into a JSON log line by an enclosing `derive`. Contents come out only
via `.reveal()`, a single greppable token. Privacy Mode is a one-way latch:
once set, prompt logging cannot be re-enabled for the life of the process.
