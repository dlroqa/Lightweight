# Progress

Checkpoint of where the build stands, so work resumes without re-deriving it.
Milestones follow the approved plan (M0-M10); this pass covers M0-M6.

Updated after each milestone, and only ever on green: `./scripts/check.sh` must
pass — fmt, clippy `-D warnings`, the full test suite, the openai-SDK contract
suite and the dependency gate — before a checkpoint is committed.

## Status

| Milestone | State | Delivered |
|---|---|---|
| **M0** Foundations | **done** | workspace, pinned toolchain, dependency policy + gate, error taxonomy, privacy primitives, structured logging, platform data dirs |
| **M1** Metadata, system info, RAM estimation | **done** | GGUF reader, ggml type table, architecture table, CPU/ISA + memory probes, RAM estimator with admission verdicts, `hermes inspect \| estimate \| sysinfo` |
| **M2** Engine acquisition and supervision | **done** | pinned runtime manifest with per-platform digests, download with resume + streamed sha256, archive extraction, `InferenceBackend` trait, supervised `llama-server` child process, crash classification, `hermes serve` |
| **M3** Vertical slice: the gateway | **done** | SSE codec, generation events, upstream HTTP/SSE adapter, `MockBackend`, `hermes-api` DTOs, `hermes-gateway` serving `/v1/chat/completions` (streamed and not), `/v1/models`, `/health`, `/props`, `/version`, permissive auth, `Semaphore(1)`, cancellation, openai-SDK contract suite |
| M4 | next | tool calls end to end in an agent loop, `reasoning_content` from a real model, the full section 27 taxonomy as OpenAI bodies, `/v1/completions` |
| M5 | pending | scheduler, metrics, queue fairness, per-token timings |
| M6 | pending | model manager, Electron shell + SPA |

## Verified by execution, not only by unit tests

M0-M2 (still true):

- The pinned engine downloads, verifies against its sha256, extracts, and runs
  on this no-AVX CPU. Runtime dispatch selects `libggml-cpu-sse42.so`; measured
  `ggml_backend_score()` is `sse42` 5, `x64` 1, every AVX-and-above variant 0.
- A real LFM2-1.2B Q4_K_M loads in about 5 seconds, engine resident 785 MiB.
- `kill -9` of the engine is reported as a structured error and the supervisor
  shuts down cleanly, leaving no orphan process.
- The RAM estimate is conservative by design — over-estimating refuses a load
  the user can override, while under-estimating invites the OOM killer.
- With no `--ctx`, `serve` sizes the context to the machine.

M3, against a real engine (`b10590`) running a real model
(SmolLM2-135M-Instruct Q4_K_M) behind `hermes serve`:

- A **streamed completion** arrives in the contracted order: role chunk, 19
  content chunks, finish chunk, usage chunk with `choices: []`, `data: [DONE]`.
- **Prefix cache reuse is observable**: `prompt_tokens_details.cached_tokens`
  went 5 → 26 → 44 across three turns of one conversation. This is the single
  largest performance lever on a CPU, and it is now measurable rather than
  assumed.
- A **three-turn streamed conversation** through the genuine `openai` Python
  SDK completed with content and a usage chunk on every turn and **no
  `EmptyStreamError`**.
- An **overlong prompt** yields a 400 whose message Hermes' own
  `parse_context_limit_from_error` — imported from
  `/home/agent/.hermes/hermes-agent/agent/model_metadata.py` — parses back to
  exactly `2048`, the effective context.
- **Disconnecting mid-stream stops the engine**: CPU time consumed by the
  engine in the two seconds after the client went away was **0 ticks**, and the
  next request was served in 0.7 s, so the slot was released rather than leaked.
- `/props` and `/v1/models` agree on the context, and `/v1/models` advertises
  the **effective** 2048 while reporting the model's real 8192 ceiling under
  `hermes.model_max_context_length`.

## Test counts

| Suite | Count | Notes |
|---|---:|---|
| Default (`cargo test --workspace`) | 367 | no network, no model downloads |
| openai-SDK contract (`scripts/contract-test.sh`) | 14 | real `openai` package against the gateway over `MockBackend`; imports Hermes' own error parser |
| Real model headers | 3 | needs `scripts/fetch-real-headers.sh`; `HERMES_REQUIRE_REAL_MODELS=1` makes absence a failure |
| Real engine | 6 | needs `HERMES_TEST_MODEL=<path.gguf>`; downloads the pinned engine on first run |

## Bugs found by running the code, and fixed

Recorded because each was invisible to the type system and to unit tests.
M0-M2's list is unchanged (broken-pipe panic in `sysinfo`, the missing rustls
provider, the progress-channel deadlock, blocking work on the async executor,
the archive's top-level directory, the crash tail read before the log pump, the
pinned `jobs = 3`).

M3 added one, and one discovery worth the same treatment:

- **Sampling parameters widened.** `SamplingParams` held `f32`, and a client's
  `temperature: 0.2` reached the engine as `0.20000000298023224` once serde
  widened it back to `f64`. These values arrive as JSON numbers and leave as
  JSON numbers, so they are `f64` throughout now. Caught by a test that
  compared the built request body against the literal the client sent.
- **The SDK raises on our terminal error chunk.** A generation that fails after
  headers are sent emits `finish_reason: "error"` plus an `error` object; the
  real `openai` client turns that into `APIError` carrying our message, after
  delivering the content that did arrive. That is the outcome we want — the
  partial answer survives and the failure is unmistakable — but it was
  *assumed* to iterate to a clean end until the contract suite said otherwise.
  The test now asserts what the client actually does.

## Verify-before-coding checklist

Items 1, 2, 4-9 are resolved. What M3 settled, each against the pinned build:

- **Client disconnect aborts generation** (item 2, the whole cancellation
  design depended on it): the server's response reader cancels its tasks in its
  destructor (`tools/server/server-queue.h:218`) and the streaming loop polls
  `should_stop` (`server-context.cpp:4287`). Confirmed by measurement — zero
  CPU ticks after a disconnect.
- **`cache_prompt` defaults to `true`** (item 4): `tools/server/server-task.h:53`
  at `b10590`, and confirmed live by non-zero `cached_tokens` across turns.
- **Timings** (item 3): the engine attaches a `timings` object to the final
  chunk, and the gateway forwards it on the usage chunk. Per-token timings
  (`timings_per_token`) remain an M5 concern, with metrics.

Still open:

- **macOS and Windows** paths are written but cannot be exercised here: CPU
  topology, memory probing and `read_process_memory` return `None` or an error
  off Linux rather than a guess. Cross-platform work is M10.
- **Calibration**: the estimator's compute and overhead terms are still the
  shipped conservative defaults, so estimates report `Confidence::Coarse`.
  Fitting them from observed peak RSS is M9.
- **The Hermes cutover** is a one-line change to `~/.hermes/config.yaml`
  (`base_url: http://127.0.0.1:8737/v1`) and has **not** been made: that file is
  protected and the change needs explicit permission. Everything the cutover
  depends on has been proven against the same client library Hermes uses.

## Next step

M4. Tool calls end to end in a real agent loop (the gateway's delta re-emission
and accumulation are already in place and tested; what M4 adds is `tools` in the
request, the engine's `--jinja` parsers exercised against a tool-capable model,
and `reasoning_content` from a model that produces it), the full section 27
error taxonomy as OpenAI-shaped bodies, and `/v1/completions` with its own
integration test.
