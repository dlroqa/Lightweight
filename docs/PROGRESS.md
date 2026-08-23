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
| **M3.55** The remote leg | **done** | `hermes sysinfo` reports every bindable address; a `--host` name that resolves only to loopback is diagnosed instead of served silently |
| **M3.6** Thinking models | **done** | `reasoning_effort` and `chat_template_kwargs` acted on rather than dropped, engine-neutral `ReasoningControl`, coverage at every layer and against both a reasoning and a non-reasoning model; a real agent harness ran a full session against the gateway |
| **M4** Tool calls, taxonomy, completions | **done** | `tools`/`tool_choice`/`parallel_tool_calls` acted on and counted, a real agent loop closed against a real model, the full section 27 taxonomy as OpenAI bodies *with* their statuses, `/v1/completions` streamed and not |
| M5 | next | scheduler, metrics, queue fairness, per-token timings |
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

The remote leg, after finding that its quietest failure was still there:

- **A name that collapses to loopback is now diagnosed.** `hermes serve --host
  "$(hostname)"` is the obvious way to ask for remote access, and on Debian and
  Ubuntu it serves nobody: `/etc/hosts` maps the hostname to `127.0.1.1` at
  install time, and that entry beats anything the network publishes. Every
  signal read as success — name resolved, bind succeeded, "serving" printed —
  while auth was silently off, because the bind really was local. Reproduced on
  this machine, then fixed: the gateway names the value, the loopback address it
  got, the cause, and the addresses this machine can actually be reached at.
- **It warns rather than refuses.** A name resolving to loopback is unusual, not
  invalid, and refusing would break a working configuration to make a point.
  `--host localhost`, `.localhost` names and literal addresses are never
  second-guessed — asserted in tests, so the warning stays worth reading.
- **`hermes sysinfo` gained a Network section**, which is where the question
  "what do I pass to `--host`?" now gets answered. On this machine it lists the
  LAN address, the overlay address and the overlay's unique-local IPv6 address,
  and `--json` carries them under `reachable_addresses` so a script need not
  parse `ip addr`.
- **The address probe is honest about what it does not know.** It reads
  `/proc/net/fib_trie`'s local table and `/proc/net/if_inet6` — no `unsafe`, so
  the crate keeps its `forbid(unsafe_code)` — filters out loopback, link-local
  and broadcast (a link-local address cannot be bound without a scope id, so
  offering one would swap one confusing failure for another), and returns
  `UnsupportedPlatform` off Linux rather than an empty list, because "nothing to
  reach this machine at" and "I did not look" are opposite answers.
- **A name is not an address, and the network gets the final say.** The machine
  this was found on has `hostname` `hermes` and is `hermes-1` on its overlay,
  where `hermes` is a *different* machine — nothing is listening there. No
  software can detect that for you, which is why `sysinfo` reports addresses.
- Verified by running it: `--host "$(hostname)"` prints the warning and the
  three real addresses; `--host localhost` and `--host 127.0.0.1` print nothing;
  and a real model served over the overlay address answered `/v1/models` 200
  with the key and 401 without, on the same socket.

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

M4, the half of tool calling that was missing:

- **`tools` never reached the engine.** Everything on the way *out* had been
  built and tested since M3 — delta parsing, the client's accumulation order,
  `finish_reason: "tool_calls"`, the non-streamed assembly — but `tools` landed
  in the request's catch-all and was logged as an ignored field. The model was
  never told a tool existed, so it never called one, and no agent loop above
  could start. That is now a typed field carried through an engine-neutral
  `ToolDefinition` and `ToolChoice`.
- **A real agent loop closes.** Against Qwen3-1.7B through the genuine `openai`
  SDK: the model returned `finish_reason: "tool_calls"` with
  `get_weather({"city": "Paris"})`, the tool ran, the result was replayed as a
  `tool` message, and the second turn answered "The weather in Paris is 17°C
  with clear skies." Prefix reuse across the turns: **166 of 222** prompt
  tokens cached. Streamed, the same call arrives as 8 deltas whose concatenated
  arguments parse as JSON.
- **Tool declarations cost prompt tokens, and are now counted.** Measured, not
  assumed: on a tool-capable template `input_tokens` went 9 → 157 when `tools`
  was sent, matching the +148 the real generation reported. `count_prompt_tokens`
  had been sending only `messages`, so the pre-flight overflow check would have
  been short by an entire toolset — thousands of tokens for a real agent — and
  the overflow would have surfaced from the engine in wording no client parses.
  A real-engine test now asserts the counted prompt and the generated prompt are
  the same prompt, on both model types.
- **`/v1/completions` is a different endpoint, not an older spelling.** It
  reaches the engine's own `/v1/completions` with `prompt`, so no chat template
  is applied; the token count goes through `/tokenize` for the same reason.
  Proved by behaviour against a real model rather than by routing: "The capital
  city of France is" came back as " Paris. The capital city of the United States
  is" — a continuation, which a templated request could not have produced.
  Array prompts and `n` expand to one choice each, numbered in prompt order,
  sharing one `usage`.
- **Refused by name rather than ignored.** `logprobs`, `best_of`, `suffix` and a
  pre-tokenized `prompt` each change what the client expects back, so each is a
  400 naming the parameter. So is a tool declaration with no function name, and
  a `tool_choice` naming a function `tools` does not declare — the shape a
  half-finished rename takes.
- **Section 27 now has statuses, not just bodies.** `hermes-api` already proved
  every variant renders a well-formed body; the gateway now pins the status a
  client branches on *before* it reads the body, for all 20 variants, with a
  second test that fails if a new variant is added and not listed.
- **Two errors the engine gets wrong are corrected at the boundary.** The pinned
  build answers **500** to `"tools": "nope"` — a client mistake reported as a
  server fault — and 400 to an unknown `tool_choice` string in its own wording.
  Both are now our own 400s, with a code and a `param`. Relatedly, a body that
  is valid JSON with one unreadable field no longer claims to be "not valid
  JSON": that sent clients hunting for a syntax error that was not there.
- **A gate blind spot, found by the gate.** `check-secrets.sh` used `git grep`,
  which searches *tracked* files only, so a new file was invisible to it until
  the commit that added it — one run too late, and how an address reached the
  M3.55 commit. It now uses `git grep --untracked`, and the address it had
  missed is fixed.

M4 profiled on the running gateway, for M5 planning (2026-08-23):

- **There is no cold-start penalty; that hypothesis was wrong.** A first pass
  blamed a slow tool-call turn on a cold engine. Measured against a genuinely
  pristine boot — `engine ready` in **8931 ms**, first request at `cached_n: 0`
  — cold prefill is **2.25 tok/s** (163 tokens in 72352 ms) against 2.6 tok/s
  warm, and cold decode is **1.59 tok/s**, faster than *every* warm decode
  recorded in the same session. There is no warm-up curve: request 1 performs
  like request 100. The only genuine cold cost is the one-time 8931 ms load.
- **The variance is machine load, not engine state.** The same payload at the
  same cache state (`cached_n: 3`, 43 prompt tokens) took 29392 ms of prefill
  and 25424 ms of decode under load average 4.14-4.87, and 13399 ms / 4327 ms
  on a quiet box — **2.2x** and **5.9x**. Within one session an identical
  15-token cached decode measured 11165, 11196 and 32468 ms: a **2.9x spread on
  identical work**. This box is a 4-core Pentium Silver J5005 (1.5 GHz base)
  where a chat app and the editor server hold roughly 1.5 cores while the engine
  asks for `--threads 4`. Every timing here carries that error bar.
- **Prefix reuse holds cold, incrementally, and across interleaving.** Cold, the
  second identical request returned `prompt_n: 1, cached_n: 162` in 509 ms.
  Across a tool loop the growth is incremental rather than a re-prefill: turn 2,
  with the assistant tool call and the tool result appended, reported
  `prompt_n: 48, cached_n: 159` — only the delta was computed. An unrelated
  conversation run between two turns did **not** evict the first: session A came
  back at `cached_n: 206` in 840 ms, despite the engine running `--parallel 1`.
- **Reasoning is the largest lever on a tool-call turn: 3.8x.** Same prompt,
  both warm, both returning a correct `tool_calls`: default thinking spent
  **113** completion tokens over 135.3 s, and `reasoning_effort: "none"` spent
  **20** over 35.4 s. Qwen3 deliberates ~100 tokens over arguments that are
  `{"city": "Paris"}`. The control already exists from M3.6 (`reasoning_effort`
  to `enable_thinking`); what M5 has to decide is the default on a dispatch turn.
- **The M5 per-turn budget.** One tool-loop turn against a warm prefix costs the
  delta prefill plus the decode — about 48 prompt and 20 completion tokens —
  which is **~26 s on a quiet box** and **~45-50 s under contention**, putting a
  five-turn loop between 2 and 4 minutes. Turn *count* is the cost driver, not
  context length: the cache makes revisiting a long prefix nearly free, so the
  scheduler should prefer fewer, fatter turns. Leave reasoning on and multiply
  by ~3.8.
- **Measured read-only against the live process**, through
  `/v1/chat/completions` with the timings the gateway already returns; nothing
  was restarted, and no source, test or configuration file was changed.

## Test counts

| Suite | Count | Notes |
|---|---:|---|
| Default (`cargo test --workspace`) | 454 | no network, no model downloads |
| openai-SDK contract (`scripts/contract-test.sh`) | 30 | real `openai` package against the gateway over `MockBackend`; imports Hermes' own error parser |
| Real model headers | 3 | needs `scripts/fetch-real-headers.sh`; `HERMES_REQUIRE_REAL_MODELS=1` makes absence a failure |
| Real engine | 9 | needs `HERMES_TEST_MODEL=<path.gguf>`; downloads the pinned engine on first run |

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
