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
| **M3.5** Remote access | **done** | serving any non-loopback address (LAN or overlay), repeatable name-resolving `--host`, key from the environment, metadata redaction for unauthenticated callers, engine key out of `argv`, secrets/address gate |
| **M3.6** Thinking models | **done** | `reasoning_effort` and `chat_template_kwargs` acted on rather than dropped, engine-neutral `ReasoningControl`, coverage at every layer and against both a reasoning and a non-reasoning model; a real agent harness ran a full session against the gateway |
| M4 | next | tool calls end to end in an agent loop, `tools` in the request, the full section 27 taxonomy as OpenAI bodies, `/v1/completions` |
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
  `~/.hermes/hermes-agent/agent/model_metadata.py` — parses back to
  exactly `2048`, the effective context.
- **Disconnecting mid-stream stops the engine**: CPU time consumed by the
  engine in the two seconds after the client went away was **0 ticks**, and the
  next request was served in 0.7 s, so the slot was released rather than leaked.
- `/props` and `/v1/models` agree on the context, and `/v1/models` advertises
  the **effective** 2048 while reporting the model's real 8192 ceiling under
  `hermes.model_max_context_length`.

Remote access, against the same engine serving a real model
(Qwen3-1.7B Q4_K_M) on a loopback **and** a non-loopback bind at once:

- **One engine, several listeners.** `--host` given twice served both addresses
  from one model and one queue.
- **Unauthenticated callers are told less, not refused.** Over the exposed
  bind, `/props` still reports `n_ctx` and `total_slots` — which is what a
  client needs to size a prompt — while omitting the model's filesystem path;
  `/health` answers `ok` without naming the model. With the key, both are
  complete. `/v1/models` and `/v1/chat/completions` are 401 without it.
- **Misconfiguration costs nothing.** Refusing an exposed bind with no key, and
  binding an address this machine does not hold, both fail in about 10 ms —
  they used to cost a full 2 GB model load first, because the model was loaded
  before the networking was settled. Now the addresses are claimed first.
- **The gateway key reaches no log**, and **the engine's key is no longer in
  its command line**: `/proc/<pid>/cmdline` is world-readable, so it now travels
  in `LLAMA_API_KEY` instead. Proven both ways — absent from `argv`, and a
  wrong key on the engine's private port still gets a 401.
- **`reasoning_content` works against a real thinking model.** Qwen3 streams its
  reasoning separately from its content, and the gateway re-emits it as such.
- **A two-turn streamed session over the exposed bind**, through the genuine
  `openai` SDK with the key: 142 and 141 completion tokens, `finish_reason`
  `stop` both times, `cached_tokens` 0 → 23 across the turns, and a wrong key
  refused with 401 on the same socket.
- **The log file exists at last.** `hermes-observability` has been complete
  since M0 and nothing had ever called `init()`, so every `tracing` line in the
  workspace went nowhere; `serve` now installs it. What a session records:
  privacy mode, engine lifecycle, model loaded, `gateway listening` with the
  port, listener count and whether auth is on, and one line per request with
  its id, model and prompt token count. What it does not record, checked by
  grep after a real request: the API key, the bound address, and the prompt.

Thinking models, now covered deliberately rather than by accident:

- `reasoning_effort` and `chat_template_kwargs` are typed request fields, carried
  through as an engine-neutral `ReasoningControl` plus untouched template
  options, and asserted at every layer — parsed in `hermes-api`, sent by the
  llama.cpp adapter, seen by the backend in the gateway suite, and sent by the
  genuine `openai` SDK in the contract suite.
- Against a real thinking model, `reasoning_effort: "none"` produces content and
  **no** reasoning; against a non-thinking model the same request is simply
  content. The real-engine test establishes which kind of model it is running
  before asserting, so both are meaningful.
- The full real-engine tier now passes against **both** model types: 7 tests on
  Qwen3-1.7B (reasoning) and 7 on SmolLM2-135M (not).

## Test counts

| Suite | Count | Notes |
|---|---:|---|
| Default (`cargo test --workspace`) | 367 | no network, no model downloads |
| openai-SDK contract (`scripts/contract-test.sh`) | 14 | real `openai` package against the gateway over `MockBackend`; imports Hermes' own error parser |
| Real model headers | 3 | needs `scripts/fetch-real-headers.sh`; `HERMES_REQUIRE_REAL_MODELS=1` makes absence a failure |
| Real engine | 6 | needs `HERMES_TEST_MODEL=<path.gguf>`; downloads the pinned engine on first run |

Measured on this machine, and recorded as a property of *this* box rather than
of the build: Qwen3-1.7B Q4_K_M decodes at roughly 0.7 tokens per second on
four 1.5 GHz cores without AVX, with the engine resident at 1.70 GiB against a
2.10 GiB estimate. A thinking model spends most of a small token budget inside
its reasoning, so a short reply still takes minutes here. A machine with AVX2
runs the same artifact several times faster; no number here is a product claim.

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

One test was corrected rather than the code, and it is worth stating plainly:
`the_real_engine_streams_a_completion_and_reports_its_tokens` asserted that
content arrived. Run against Qwen3 — a thinking model — it failed while the
engine and the gateway behaved perfectly: the whole 24-token budget went into
`reasoning_content`, which is output, not silence. The assertion now accepts
content **or** reasoning and still fails if neither arrives. The non-thinking
model satisfies it through the same branch it always did.

Found while verifying remote access, and fixed rather than deferred:

- **Engine-side request options were accepted and dropped.** A client could not
  turn a thinking model's reasoning off, because `reasoning_effort` and
  `chat_template_kwargs` landed in the tolerant catch-all and went no further.
  With a small `max_tokens`, Qwen3 spends the whole budget inside its reasoning
  and the client sees a completion with no content — the shape that makes a
  client retry blindly. Both are typed fields now, carried as an engine-neutral
  `ReasoningControl` plus untouched template options, and verified end to end:
  against a real thinking model, `reasoning_effort: "none"` produces content and
  no reasoning at all. `tools` remains M4's work.

**The acceptance test passed: a real agent harness ran a full session against
the gateway.** Run in a throwaway `HERMES_HOME` so the user's own configuration
was never touched, the harness initialized against `/v1/models`, sent a
5,596-token agent system prompt, streamed the reply, and answered correctly —
`pong`, from a 135M model — exiting cleanly after 8 minutes 7 seconds, most of
it prefill on this CPU. No `EmptyStreamError`, no truncated stream, and the
gateway served five requests across the session.

Found by that same run, and left for M5:

- **A harness issues auxiliary requests alongside the main turn.** While a
  5,596-token agent prompt was prefilling, the harness sent a small
  non-streaming request of its own (title generation). With
  `max_concurrent_requests: 1` it queued behind the long generation and the
  harness's own timeout fired: `Auxiliary title generation failed: Request
  timed out.` Nothing was lost and the session continued, but it is precisely
  the case section 22's priority bands exist for — a short request must not sit
  behind a multi-minute one. Queue position events and fairness are M5.
- **A harness may impose a minimum context.** This one refuses any model
  advertising under 64,000 tokens and says so at startup rather than failing
  later. The gateway advertises what it is really serving, which is the right
  behaviour; meeting such a floor is a question of loading a model at 64K, and
  `hermes estimate <model> --ctx 65536` answers whether a machine can before
  anything is loaded. On this box it cannot — Qwen3-1.7B at 32768 is already
  2.47 GiB short, with ranked remedies — which is a property of the hardware,
  not of the design.

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
- **The live harness cutover** — pointing the user's own `~/.hermes/config.yaml`
  at this gateway — has **not** been made and still needs explicit permission;
  that file is protected. It is no longer needed for evidence: the same
  cutover, in a throwaway `HERMES_HOME`, ran a complete agent session (above).
  Its md5 was checked before and after and is unchanged.
- **A second machine has not been driven from here.** Everything on this side
  is verified, including a non-loopback bind with auth; the remaining leg is a
  client on *another* host completing a session, which needs either a command
  run there or a way in. SSH from here is refused by key.

## Next step

M4. Tool calls end to end in a real agent loop (the gateway's delta re-emission
and accumulation are already in place and tested; what M4 adds is `tools` in the
request, the engine's `--jinja` parsers exercised against a tool-capable model,
and `reasoning_content` from a model that produces it), the full section 27
error taxonomy as OpenAI-shaped bodies, and `/v1/completions` with its own
integration test.
