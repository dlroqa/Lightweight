# Progress

Checkpoint of where the build stands, so work resumes without re-deriving it.
Milestones follow the approved plan (M0-M10); this pass covers M0-M6.

Updated after each milestone, and only ever on green: `./scripts/check.sh` must
pass — fmt, clippy `-D warnings`, the full test suite and the dependency gate —
before a checkpoint is committed.

## Status

| Milestone | State | Delivered |
|---|---|---|
| **M0** Foundations | **done** | workspace, pinned toolchain, dependency policy + gate, error taxonomy, privacy primitives, structured logging, platform data dirs |
| **M1** Metadata, system info, RAM estimation | **done** | GGUF reader, ggml type table, architecture table, CPU/ISA + memory probes, RAM estimator with admission verdicts, `hermes inspect \| estimate \| sysinfo` |
| **M2** Engine acquisition and supervision | **done** | pinned runtime manifest with per-platform digests, download with resume + streamed sha256, archive extraction, `InferenceBackend` trait, supervised `llama-server` child process, crash classification, `hermes serve` |
| **M3** Vertical slice: Hermes talks to the gateway | next | OpenAI DTOs + SSE codec, `/v1/chat/completions`, `/v1/models`, `/health`, `/props`, permissive auth |
| M4 | pending | tool calls, reasoning passthrough, full error contract, `/v1/completions` |
| M5 | pending | scheduler, metrics, cancellation |
| M6 | pending | model manager, Electron shell + SPA |

## Verified by execution, not only by unit tests

- The pinned engine downloads, verifies against its sha256, extracts, and runs
  on this no-AVX CPU. Runtime dispatch selects `libggml-cpu-sse42.so`; measured
  `ggml_backend_score()` is `sse42` 5, `x64` 1, every AVX-and-above variant 0.
- A real LFM2-1.2B Q4_K_M loads in about 5 seconds, engine resident 785 MiB.
- `kill -9` of the engine is reported as a structured error and the supervisor
  shuts down cleanly, leaving no orphan process.
- The RAM estimate came out 1.37x the engine's observed peak — conservative, as
  intended: over-estimating refuses a load the user can override, while
  under-estimating invites the OOM killer.
- With no `--ctx`, `serve` sized the context to the machine and chose the
  model's full 128000 tokens here, rather than a fixed default.

## Test counts

| Suite | Count | Notes |
|---|---:|---|
| Default (`cargo test --workspace`) | 244 | no network, no model downloads |
| Real model headers | 3 | needs `scripts/fetch-real-headers.sh`; `HERMES_REQUIRE_REAL_MODELS=1` makes absence a failure |
| Real engine | 4 | needs `HERMES_TEST_MODEL=<path.gguf>`; downloads the pinned engine on first run |

## Bugs found by running the code, and fixed

Recorded because each was invisible to the type system and to unit tests:

- `hermes sysinfo | head` **panicked** on a broken pipe. `println!` panics on
  EPIPE and Rust ignores SIGPIPE at startup. Reports are now rendered into a
  buffer and written once, with `BrokenPipe` handled in `main`.
- `reqwest::Client::builder().build()` **panics** when no rustls provider is
  installed. Only the installer established that precondition, so driving the
  supervisor directly panicked. Now a shared `tls::ensure_provider()`.
- **Progress reporting could deadlock a model load.** `load` used
  `send().await`; a caller holding a receiver it never drained — a closed UI —
  filled the channel and blocked the load forever. All progress is `try_send`
  now, and a regression test loads with an undrained channel.
- Extraction and re-hashing ran **on the async executor**, stalling every other
  task on a current-thread runtime. Both moved to `spawn_blocking`.
- The engine archive has a top-level `llama-b10590/` directory, so the binary
  landed one level too deep. Extraction now detects and strips a shared prefix.
- The crash report read stderr **before the log pump had run**, so a fast
  startup failure reported an empty tail. Now drained first.
- `.cargo/config.toml` pinned `jobs = 3`, which suited this machine and would
  have throttled every larger one. Removed; `CARGO_BUILD_JOBS` is documented
  for constrained hosts instead.

## Still open

Carried forward from the plan's verify-before-coding checklist. Items 1, 2, 5-9
are resolved; these remain:

- **Timings**: `timings_per_token` is a per-request field, default `false`, and
  responses carry a `timings` object. Wire it up in M5 when metrics land.
- **`cache_prompt`** defaults to `true` in the pinned build — the single largest
  performance lever on slow CPUs. Confirm it stays on once generation exists.
- **macOS and Windows** paths are written but cannot be exercised here: CPU
  topology, memory probing and `read_process_memory` return `None` or an error
  off Linux rather than a guess. Cross-platform work is M10.
- **Calibration**: the estimator's compute and overhead terms are still the
  shipped conservative defaults, so estimates report `Confidence::Coarse`.
  Fitting them from observed peak RSS is M9.

## Next step

M3. Build `hermes-api` (OpenAI DTOs and the SSE codec) and `hermes-gateway`,
serving `/v1/chat/completions`, `/v1/models`, `/health` and `/props` over the
already-supervised engine. The contract to satisfy is recorded in the plan's
"Hermes wire contract" table; the acceptance test is a real Hermes session
streaming a multi-turn conversation with no `EmptyStreamError`.
