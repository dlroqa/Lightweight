# Progress

Checkpoint of where the build stands, so work resumes without re-deriving it.
Milestones follow the approved plan (M0-M10); this pass covers M0-M10.

Updated after each milestone, and only ever on green: `./scripts/check.sh` must
pass — fmt, clippy `-D warnings`, the full test suite, the openai-SDK contract
suite and the dependency gate — before a checkpoint is committed.

## Status

| Milestone | State | Delivered |
|---|---|---|
| **M0** Foundations | **done** | workspace, pinned toolchain, dependency policy + gate, error taxonomy, privacy primitives, structured logging, platform data dirs |
| **M1** Metadata, system info, RAM estimation | **done** | GGUF reader, ggml type table, architecture table, CPU/ISA + memory probes, RAM estimator with admission verdicts, `hermes inspect \| estimate \| sysinfo` |
| **M2** Engine acquisition and supervision | **done** | pinned runtime manifest with per-platform digests, download with resume + streamed sha256, archive extraction, `InferenceBackend` trait, supervised `llama-server` child process, crash classification, `hermes serve` |
| **M3** Vertical slice: the gateway | **done** | SSE codec, generation events, upstream HTTP/SSE adapter, `MockBackend`, `lightweight-api` DTOs, `lightweight-gateway` serving `/v1/chat/completions` (streamed and not), `/v1/models`, `/health`, `/props`, `/version`, permissive auth, `Semaphore(1)`, cancellation, openai-SDK contract suite |
| **M3.5** Remote access | **done** | serving any non-loopback address (LAN or overlay), repeatable name-resolving `--host`, key from the environment, metadata redaction for unauthenticated callers, engine key out of `argv`, secrets/address gate |
| **M3.55** The remote leg | **done** | `hermes sysinfo` reports every bindable address; a `--host` name that resolves only to loopback is diagnosed instead of served silently |
| **M3.6** Thinking models | **done** | `reasoning_effort` and `chat_template_kwargs` acted on rather than dropped, engine-neutral `ReasoningControl`, coverage at every layer and against both a reasoning and a non-reasoning model; a real agent harness ran a full session against the gateway |
| **M4** Tool calls, taxonomy, completions | **done** | `tools`/`tool_choice`/`parallel_tool_calls` acted on and counted, a real agent loop closed against a real model, the full section 27 taxonomy as OpenAI bodies *with* their statuses, `/v1/completions` streamed and not |
| **M5** Scheduler, metrics, per-token timings | **done** | priority bands classified from measured cost, starvation-bounded fairness, queue position reported to streamed clients, `/metrics` and `/api/v1/metrics`, per-token timings from the engine, `--concurrency` |
| **M6a** Model manager | **done** | `lightweight-download` shared by engine and models, persistent catalog with atomic writes, import + pinned downloads + pasted links with per-model integrity, `hermes models`, scheduler pause/drain, hot swap over `/api/v1`, jobs with SSE progress, `serve` with no model |
| **M6b.1** Backend seams | **done** | `/api/v1/system`, `/api/v1/gateway`, `/api/v1/events`, `/api/v1/logs`, `GET /api/v1/models/{id}`; disk via `rustix` and processor time from `/proc/stat` as probes that say when they could not read; an in-flight gauge that spans the response body; the panel served from the gateway, so no CORS layer exists |
| **M6b.2** Persistence | **done** | `lightweight-store`: conversations and settings under the two M0 directories that had never been written to, owner-only, atomic; `/api/v1/conversations` and `/api/v1/settings` |
| **M6b.3** The panel | **done** | React + TypeScript + Vite in `frontend/`, eight screens on the seams M6b.1 and M6b.2 opened, served same-origin by the gateway; no CORS layer exists anywhere |
| **M6b.4** The desktop shell | **done** | `apps/desktop`: attaches to a gateway already serving or starts one, stops only what it started, tray, key handling, packaging config |
| **M7.1** Say the true number | **done** | the KV arithmetic as one fallible pass, so its two halves cannot disagree about a type this build cannot size; per-variant memory remedies naming `--force` instead of a setting that never existed; `Probed<Estimate>` and `ManagerError::MemoryProbe` so a probe failure says why; `targets::MEMORY` given its call sites; the panel reading `label` rather than a field that does not exist |
| **M7.2** Spend the right budget | **done** | an injectable `MemoryProbe` on `GatewayState`; `Budget` crediting a swap with the `RssAnon` the outgoing engine is about to release; engine RSS and peak in `/api/v1/metrics` and two new Prometheus gauges, read per pull with no sampler; `Verdict::Tight` warned in the log and said on screen, gating nothing |
| **M7.3** Controls that change something | **done** | one `choose_context` for the load path and the detail that disagreed; `last_n_ctx` demoted to history; engine capabilities and load defaults on `/api/v1/gateway`; `?ctx=`/`?kv_type=` pricing on the model detail; context and KV type pickers in the panel, which now follows the load job and shows the refusal it had been hiding |
| **M8.1** Make the CPU visible | **done** | `cpu_percent` retired for `cpu_ticks` read from `/proc/<pid>/stat` and published unconverted; the panel differences them into cores; latency histograms with real Prometheus buckets, existing names and values unchanged; per-band generation and wait counters; the engine's own `/metrics` scraped once per pull, which `--metrics` had promised since M2 |
| **M8.2** The harness that measures | **done** | `lightweight-bench`: three deterministic scenarios over the `InferenceBackend` trait, prompts sized by the engine's own tokenizer, runs saved owner-only to the `benchmarks_dir()` M0 chose and nothing had written to; `hermes bench` with its own engine and a per-bucket reload, `POST /api/v1/benchmarks` against what is resident, Run Benchmark wired in the panel; `--fit` writes a slope and an intercept because peak RSS cannot separate two collinear coefficients |
| **M8.3** The knobs | **done** | `n_ubatch` and `n_batch` reachable and `?ubatch=` priced; `--threads-batch`, `--poll`, `--cache-reuse`, `--load-mode` and CPU affinity reachable and absent by default; a locked-memory pre-flight from `/proc/self/limits`, `VmLck` credited on a swap, and `Tight` refused for a locking load; the panel reads the `thread_choices` served since M6b.1 |
| **M9.1** Engine truth | **done** | `--ctx-size` multiplied by the slot count at the one boundary that knows the engine's convention, so each client gets the window every surface advertises; `--no-kv-unified` and `--cont-batching` stated; the engine's own `/props` read back once it is ready and the recorded parameters reconciled with it; three hardcoded `max_concurrent_requests: 1` capabilities retired |
| **M9.2** Clients, not only requests | **done** | `PeerKey` from the connection — observed, never claimed, never logged or labelled; a fair-queuing round in the sort key that degenerates to today's order for one caller; `Ticket::position` an `Option`; `timed_out` and `abandoned` made to mean what they say; admission one locked decision; the benchmark's discarded permit |
| **M9.3** The number, and the clients | **done** | `--concurrency auto` derived from cores and memory, with the divisor set by a sweep rather than chosen; the scheduler's capacity re-derived per load; `/api/v1/requests` and a Running Now card, because a running request was a bare `usize`; `hermes bench --parallel` with a concurrent scenario; two genuinely concurrent clients in the contract suite |
| **M10a** Cross-platform | **done** | `lightweight-sys`, the workspace's only platform FFI, with one `SAFETY:` note per call and nothing contributed to a Linux build, so `lightweight-system-info` keeps its `forbid(unsafe_code)`; `check.yml` running `scripts/check.sh` itself on four runners; the Linux artifact no longer disabling its own Chromium sandbox; one artifact per platform and a Flatpak that builds; per-process memory on the two platforms without `/proc`, and `PeakKind` so a macOS footprint is not mistaken for a resident-set peak |
| **M10b** Calibration | **done** | `HermesPaths::calibration_file()`; the six estimator sites taking a calibrated model when a trustworthy fit describes the load and the shipped defaults when not; `Confidence::Measured` reachable, said by `hermes estimate` and shown in the panel; `/api/v1/gateway` reporting the calibration outcome; trust rules informed by measurement — three distinct batch sizes, and a line accounting for 95% of the spread — and the finding that on this machine no fit earns them, because the shipped compute shape is wrong for this engine rather than merely mis-set |

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
- **The log file exists at last.** `lightweight-observability` has been complete
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
  options, and asserted at every layer — parsed in `lightweight-api`, sent by the
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
- **Section 27 now has statuses, not just bodies.** `lightweight-api` already proved
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

M5, verified against a real engine (`b10590`) running SmolLM2-135M at a
2048-token context, driven with the real client's own numbers:

- **The acceptance run's failure no longer happens.** Three requests, one slot:
  an agent turn (`max_tokens` 65536, as `agent/run_agent.py:1673` sends), a
  second turn arriving one second later, and a title generation (`max_tokens`
  64, as `agent/title_generator.py:408` sends) arriving one second after
  *that*. Finish order was **A → C → B**: the title request overtook the turn
  that had queued ahead of it and finished at 30.6 s instead of after B at
  52.4 s. `overtakes` in `/metrics` counted exactly 1.
- **A queued request is answered immediately.** Both queued requests had their
  response headers in **10 and 20 milliseconds**, and received
  `: queued position=1 waited=15s` / `: queued position=0 waited=15s` while
  they waited. Before this, a queued client received nothing at all — no
  headers, no bytes — until the request ahead of it finished.
- **The band is decided from measurements, and the ceilings were wrong.** The
  first live run put both the "long" and the "short" request in the interactive
  band and served them first-come-first-served, correctly by the rules and
  uselessly in practice: at a 2048-token context the output ceiling computed to
  exactly 64 tokens, and the real client's title generation asks for exactly
  64. The one request the band exists for classified correctly by a single
  token, and would have classified wrongly on any smaller window. The floors
  are now twice the observed value and there is a test named after the two real
  requests, at the smallest context this project has served.
- **An abandoned generation reports what it cost.** A client that walked away
  after 20 seconds contributed **116 completion tokens over 18.8 s** to the
  counters, where the old behaviour was to report nothing at all — the chunk
  carrying the cost is the one such a request never receives. It is counted as
  `cancelled`, not as an error: closing a laptop lid is a normal act.
- **The numbers agree with each other.** After the three-request run:
  3 requests ok, 3 generations, 3 `stop`, 112 prompt tokens of which 48 cached
  and 64 actually prefilled, `queued` 2, `admitted_immediately` 1, queue wait
  max 29.5 s, time to first token max 31.6 s, prefill 8.30 tok/s and decode
  4.56 tok/s. A warm cached request measured 13.08 tok/s decode on the same box
  minutes later — the spread this machine is known for, now visible rather than
  inferred.
- **A scrape carries no text.** Asserted in the suite and checked live: no
  prompt, no completion, no `/mock/model.gguf`, no model path. The model *id*
  appears, because `/v1/models` already advertises it.

M6a, the model manager, verified against a real engine and the real network on
2026-08-23:

- **A model downloads and verifies against a digest recorded beforehand.**
  SmolLM2-135M Q4_K_M, 100.6 MiB in 5.1 s (19.8 MiB/s), sha256 matching the
  manifest entry that `scripts/record-model-digests.sh` read from HuggingFace's
  tree API. Cancelled at **62,251,949 bytes** and re-run, the transfer resumed
  and the completed file still verified — the resume path the engine installer
  has had since M2, now proven on something large enough for it to matter.
- **A digest that does not match is refused and the bytes are discarded**, with
  nothing left where a later resume could inherit it. A link that returns a
  valid file which is not a GGUF (`huggingface.co/robots.txt`) is deleted and
  reported rather than registered as a model.
- **How much was promised about a file is recorded per model, and never rounded
  up.** A pinned entry is `verified (pinned digest)`; the *same file* fetched by
  pasting its HuggingFace link is `verified (published digest)`, read from the
  LFS metadata; a link elsewhere with no digest is `recorded, not verified` in
  those words. An import is `imported from this machine` and checks nothing,
  because there is nothing to check it against.
- **An import references the file where it is.** A 1.19 GiB Qwen3 was hashed in
  place in about 4 s and registered; nothing was copied, and removing it from
  the catalog left the user's file alone. Importing the same file twice returns
  the model already installed rather than a second entry, matched by digest.
- **The gateway starts with no model and is told what to load.**
  `/v1/chat/completions` answers 503 `no_model_loaded`,
  `POST /api/v1/models/{id}/load` returns a job, and the job's SSE stream
  reports `starting_engine → loading_weights → ready → succeeded → [DONE]`.
- **A hot swap works and re-derives the band ceilings.** smollm2@2k → qwen3@8k
  on a serving gateway took **25 s**, `/v1/models` and `/props` both moved to
  the new context, and `hermes_band_ceiling_tokens` went 512/128 → 1024/256.
  Inherited ceilings would have been the M5 bug in a new place, so they are now
  exposed in `/metrics` rather than being invisible policy.
- **Nothing is preempted, and the swap waits.** A 160-token generation was
  running when a load was requested three seconds in. It finished with **all
  160 completion tokens** and `finish_reason: length` — not truncated, not
  aborted — and the swap completed **one second after it**, having waited about
  116 s. A request arriving during a swap queues and is served afterwards
  rather than being refused.
- **Deleting the loaded model is refused** with `model_in_use` and a remedy;
  after unloading it is allowed, and the *imported* file is left on disk because
  it was never ours. Unloading twice is not an error.
- **A record outlives its file.** With the weights moved away the model reads
  `missing` rather than disappearing, and asking to load it names the id and the
  path it expected rather than failing inside the engine.

Found by running M6a, and fixed:

- **A job's progress stream was 1,010 SSE frames for a 16 MB download.** The
  transfer reports per 16 KB chunk, and every update became a broadcast send and
  a frame per watcher — around 65,000 of them for a 1 GB model, on a box whose
  CPU is the scarce resource. Throttled to one update per whole percent plus
  every stage change: the same load now emits **4 frames**.
- **A URL with no path made the host the model's file name.** Caught by its own
  test before it ever ran: `https://example.com` produced a model file called
  `example.com`.
- **The cancellation test could measure a stranger's process.** `engine_pid`
  matched the *first* `llama-server` in `/proc`, so with another gateway serving
  on this machine — or with `cargo test --workspace` running this file's tests
  in parallel — it read an unrelated engine's CPU time. It now matches on the
  engine's own ephemeral port, which is unique per launch. This is a test that
  could pass for the wrong reason, which is worse than one that fails.
- **And that test's real bar was wrong.** With the right process measured, a
  cancelled generation costs a short teardown tail and then exactly nothing: on
  this box it goes idle **1000-2000 ms** after the disconnect, against a decode
  cost of 210-254 ticks per second. The old assertion allowed 2 ticks in a fixed
  window starting 500 ms after the disconnect, which fails whenever the tail
  runs long and proves no more than the new one. The test now polls until the
  engine reports an idle half-second, asserts it stays idle, and prints both
  numbers. The M3 claim stands in substance — a disconnected client stops
  costing CPU — with the honest shape: it stops within a second or two, rather
  than instantly.

Found by reviewing M6a against the project's own standard — no guesswork, no
assumptions, no workarounds — rather than by running it:

- **The catalog lock was held for the length of a download.** `install` locked
  the store and then ran the transfer inside it, so `GET /api/v1/models` waited
  for the whole thing — the listing a UI refreshes while watching the very
  download it started. The installer is now three phases: `plan` and `fetch`
  take no catalog at all, and the lock is taken twice for microseconds, to check
  for an existing copy and to commit the result. Pinned by a test that lists the
  catalog throughout a real 100 MB download: **103 listings, slowest 47 µs**.
  The test was checked against the defect it exists for — reintroduce the lock
  and it fails on a five-second timeout.
- **A test of mine downloaded 100 MB from the network inside `cargo test`.** It
  raced two installs to prove they exclude each other; when the first failed
  fast, the second went to HuggingFace. The default suite promises no network
  and no model downloads, and it had quietly stopped being true. The guard is
  now tested by taking the lock directly, and the promise is **verified rather
  than assumed**: the whole suite passes with outbound HTTP blocked
  (`HTTPS_PROXY=127.0.0.1:1`, loopback exempted, 562 tests).
- **A failed `rename` fell back to copying the file.** Any error, not just a
  cross-filesystem one — so a permissions problem became a second, misleading
  error about copying and hid the first. It now falls back only on `EXDEV`, the
  way the download layer already matches `ENOSPC`, and the error names the
  verified file it left behind so it can be moved by hand rather than fetched
  again.
- **An unresolvable path was silently accepted.** `canonicalize().unwrap_or(path)`
  on import would store a relative path, and the model would go missing later
  for a reason nobody would connect to the import. It is an error now.
- **"No catalog attached" was reported as "busy".** A client retrying a busy
  that will never clear is the cost of confusing a transient condition with a
  permanent one; `no_model_catalog` is its own error.
- **Two places decided whether a file was ours to delete**, and two places
  answering the same question is two answers waiting to disagree. `remove` now
  returns what it actually did, including whether the delete succeeded, and the
  route reports that.
- **Two copies of "is this a GGUF?"** — the catalog's reader and a second one in
  the gateway's load path. There is one now.
- **And a third copy of "is this file ours to delete?"**, found only by grepping
  for it after claiming the duplication was fixed: the CLI had its own. The
  predicate now lives on the record, where the data is, so the CLI and the
  control API cannot disagree about whose file it is.
- **A progress pump that panicked disappeared silently.** `let _ = pump.await`
  discards a `JoinError`, which turns a panic in a background task into "the
  progress bar stopped" with nothing anywhere to explain it. It is logged now,
  and still never fails the operation it was reporting on.
- **The gate's own "no network" step could reach the network.** With the opt-in
  variables set, `cargo test --workspace` picked up the real-engine and
  model-download tests too — running nine engines at default parallelism on a
  four-core box, downloading the same 100 MB twice, and quietly contradicting
  the step's own description. `check.sh` now unsets both for that step.
- **The resume test could stop testing resumption.** It cancelled after three
  seconds, which on a fast link is after the download has finished; it then
  printed a skip and passed. It cancels after a quarter of the bytes now, so
  there is always a partial file to resume from.

## The icon

One artwork, `icon/source.png`, and one script that cuts everything from it:
`scripts/build-icons.py`. The script measures where the mark actually sits -
the render frames it small and high in a large field of plate - and re-cuts the
canvas so the mark fills 78% of it, because an icon framed for a poster is a
coloured square with a smudge in it at 32px.

What it writes is committed, so no build step and nothing at run time depends
on Pillow. Replacing `icon/source.png` and re-running the script is the whole
of "change the icon":

| Output | Size | Read by |
|---|---:|---|
| `apps/desktop/build/icons/<n>x<n>.png` | 16-1024 | electron-builder, as an icon *set*: the AppImage installs each size where the desktop looks for it, and macOS and Windows convert the largest |
| `apps/desktop/build/window.png` | 256 | the shell's `BrowserWindow`, via `dist/` |
| `apps/desktop/build/tray.png` | 48 | the shell's tray, via `dist/` |
| `frontend/public/icon.png` | 256 | the browser tab, and the panel's rail |
| `frontend/public/apple-touch-icon.png` | 180 | a home screen |
| `frontend/public/favicon.ico` | 16/32/48 | the reflex request for `/favicon.ico`, so it is not a 404 on every load |

Three decisions worth keeping:

- **The runtime icons travel in `dist/`, not `build/`.** `build/` is
  electron-builder's own resources directory: it reads `icons/` from there to
  make the package's icon, and excludes that directory from the app itself -
  `getMainFileMatchers` adds `!<buildResources>{,/**/*}` to every file pattern.
  An icon loaded from `build/` at run time would be present in a checkout and
  missing from the AppImage - the worst kind of difference, because only the
  shipped copy is wrong. `scripts-build.mjs` copies them into `dist/` and fails
  the build if they are not there.
- **The rail chip's ring is an `outline`, not an inset `box-shadow`.** The
  first version used the shadow, which is what the rest of the panel uses, and
  it drew nothing: an inset shadow is painted below the element's content, and
  the content of an `img` is an opaque image covering the whole box. Checked in
  a browser against a deliberately garish test ring rather than reasoned about
  - `outline` with a negative `outline-offset` draws above the image and still
  follows `border-radius`.
- **There is no transparent version of the mark.** It was tried, with alpha
  taken from each pixel's distance to the plate colour. The feather's vane is a
  dark teal about as far from the plate as the film grain is, so keying it out
  deletes half the mark and leaves a quill and a bolt. The artwork is drawn *on*
  its plate and keeps it everywhere, which is why the panel's rail chip is the
  application icon itself rather than a recolourable glyph - and why that chip
  is the one surface in the panel that looks identical in light and dark.

## Test counts

| Suite | Count | Notes |
|---|---:|---|
| Default (`cargo test --workspace`) | 905 | no network, no model downloads — checked with outbound HTTP blocked |
| openai-SDK contract (`scripts/contract-test.sh`) | 45 | real `openai` package against the gateway over `MockBackend`; imports Hermes' own error parser; two clients driven at once from two threads |
| Real model headers | 3 | needs `scripts/fetch-real-headers.sh`; `HERMES_REQUIRE_REAL_MODELS=1` makes absence a failure |
| Real engine | 10 | needs `HERMES_TEST_MODEL=<path.gguf>`; downloads the pinned engine on first run |
| Model downloads | 8 | needs `HERMES_TEST_NETWORK=1`; fetches a real 100 MB model from HuggingFace |

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

Found by that same run, and **fixed in M5**:

- **A harness issues auxiliary requests alongside the main turn.** While a
  5,596-token agent prompt was prefilling, the harness sent a small
  non-streaming request of its own (title generation). With
  `max_concurrent_requests: 1` it queued behind the long generation and the
  harness's own timeout fired: `Auxiliary title generation failed: Request
  timed out.` Nothing was lost and the session continued, but it is precisely
  the case section 22's priority bands exist for — a short request must not sit
  behind a multi-minute one. It now does not: reproduced with the same numbers
  above, the short request overtakes and the queued one is told where it
  stands.
- **A harness may impose a minimum context.** This one refuses any model
  advertising under 64,000 tokens and says so at startup rather than failing
  later. The gateway advertises what it is really serving, which is the right
  behaviour; meeting such a floor is a question of loading a model at 64K, and
  `hermes estimate <model> --ctx 65536` answers whether a machine can before
  anything is loaded. On this box it cannot — Qwen3-1.7B at 32768 is already
  2.47 GiB short, with ranked remedies — which is a property of the hardware,
  not of the design.

Found by running M5 against a real engine, and fixed:

- **The per-token token count latched on its first reading.** With
  `timings_per_token` each timing supersedes the last, and the gateway kept the
  first one it saw — `if completion_tokens == 0` looked like a sensible guard
  and was not. An eight-second abandoned generation reported **1** token
  instead of the twenty it had produced, which is a worse answer than reporting
  none, because it looks like data. Caught by comparing the counter against the
  wall clock on a live cancel, then pinned by a unit test that drops the stream
  half way.
- **The band ceilings were derived without checking them against the client.**
  See the M5 evidence above: correct by construction, useless at the context
  this box serves. Fractions of a window are a good shape for a limit and a bad
  source of a floor.

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
  chunk, and the gateway forwards it on the usage chunk. **Per-token timings
  are now on**: `timings_per_token` is sent with every generation (the symbol
  is present in `libllama-server-impl.so` at the pinned build), so the timing
  object arrives on every chunk and a generation that never reaches its final
  chunk still reports what it cost.

Still open:

- **macOS and Windows** paths are written but cannot be exercised here: CPU
  topology, memory probing and `read_process_memory` return `None` or an error
  off Linux rather than a guess. Cross-platform work is M10.
- **Calibration**: the estimator's compute and overhead terms are still the
  shipped conservative defaults, so estimates report `Confidence::Coarse`.
  Fitting them from observed peak RSS is M10.
- **The live harness cutover** — pointing the user's own `~/.hermes/config.yaml`
  at this gateway — has **not** been made and still needs explicit permission;
  that file is protected. It is no longer needed for evidence: the same
  cutover, in a throwaway `HERMES_HOME`, ran a complete agent session (above).
  Its md5 was checked before and after and is unchanged.
- **A second machine has not been driven from here.** Everything on this side
  is verified, including a non-loopback bind with auth; the remaining leg is a
  client on *another* host completing a session, which needs either a command
  run there or a way in. SSH from here is refused by key.

M6b.1, against a real gateway serving no model on this machine:

- **The panel can describe the machine at last.** `/api/v1/system` reports this
  box as it is — an Intel Pentium Silver J5005, four physical cores, `sse42` and
  no AVX family, `expected_ggml_variant` `sse42`, which is exactly what the
  engine was measured choosing in M0-M2. Nothing about it is hardcoded.
- **Free space distinguishes the budget from the total.** On this machine the
  models filesystem has 23.2 GB free but only 16.8 GB *available* — the ext4
  root reserve is the difference. A download sized against the free count would
  be sized against gigabytes an unprivileged process cannot spend. `statvfs`
  reaches us through `rustix`, so no crate lost `forbid(unsafe_code)` to get it,
  and `scripts/check-deps.sh` still passes: rustix's build script only
  re-invokes `rustc` to probe cfgs and declares no build-dependencies, and on
  Linux its `linux_raw` backend pulls in no libc.
- **Both filesystems a download touches are reported.** The first pass measured
  the models directory alone and its doc comment called that "where a download
  lands", which is wrong: bytes accumulate in `downloads_dir()` under the
  **cache** root and are moved into `models_dir()` under the **data** root.
  `hermes_catalog::install` has fallen back to a copy on `EXDEV` since M6a, so
  the code already knew those can be different filesystems. Both are now
  reported, with `same_filesystem` derived from the device id rather than from
  `statvfs`'s `f_fsid`, which is documented upstream as meaningless.
- **Processor load is published as counters, not a percentage.** `/proc/stat`
  gives monotonic totals and a rate needs two readings of them, so the endpoint
  hands over `total` and `idle` ticks and the caller differences consecutive
  polls — the same discipline `/api/v1/metrics` already imposes on the charts.
  No background sampler was added: it would have had to invent a sampling
  interval, and its first reading would be either absent or a lie.
- **Every probe says whether it was read.** `cpu_times`, `memory` and `disk`
  each carry `state: read` or `state: unavailable` with the taxonomy's own
  error code. A section that returned `0%` where the honest answer is "no probe
  on this platform yet" would report a saturated machine as idle, which is the
  one reading an operator must never be given wrongly.
- **The service can describe itself.** `/api/v1/gateway` reports the addresses
  actually bound — read from the sockets, so a `--port 0` request reports the
  port the kernel chose — plus whether a key is required, the concurrency, the
  live queue and the engine's health. The addresses are read **once** and shared
  with the startup summary, rather than each asking the sockets separately.
- **The key is absent by assertion, not by convention.** `/api/v1/gateway`
  reports *that* a key is required and never the key; a test greps the whole
  body for it, so a later field cannot quietly reintroduce what M3.5 kept out of
  the log and out of the engine's argv.
- **Both new endpoints are behind the key**, asserted directly: they are host
  inventory, and on a bind reachable from elsewhere that is not public.
- **The probes cannot wedge the gateway.** `statvfs` on a network mount that has
  gone away blocks for the mount's timeout, and a panel polling once a second
  would hold every worker on a four-core box. They run under `spawn_blocking`,
  as every other blocking read in this workspace does.
- **No response body is built with `json!` over a path any more.** That macro
  resolves to `to_value(..).unwrap()`, `PathBuf` fails to serialize when a path
  is not valid UTF-8 — legal on Linux — and the release profile sets
  `panic = "abort"`, so one request could have taken the process down. Both the
  new gateway description and `GET /api/v1/models`, which has carried a
  `PathBuf` this way since M6a, are now typed and serialized through
  `axum::Json`, where the same failure is a 500.

M6b.1, second pass — the feed, the log and the gauge:

- **One event stream, published where nothing can miss it.** `/api/v1/events`
  is fed from `Metrics::record_generation`, which is the single point every
  generation passes through. That matters for the case a publisher on the happy
  path would drop: a client that walks away mid-stream. Asserted directly — the
  test abandons a streamed completion and reads the event back, with a null
  `finish_reason`, because ending because the client left is a deliberate act
  and not an error.
- **The feed carries what the log carries and no more.** The same test greps the
  frame for the prompt it sent and requires its absence.
- **`/api/v1/logs` reads the file that has existed since M3.5 and never been
  readable.** Bounded on both sides: records stream through a ring buffer of
  exactly the requested size, so memory is bounded by the answer rather than by
  the file, and only the two newest rotated files are opened. A half-written
  final line — routine while a record is being appended — costs only itself.
  An unknown `level` is a 400 rather than a filter that silently matches
  everything and lets someone believe they are looking at errors only.
- **The in-flight gauge spans the response body, not the handler.** A handler
  returns as soon as the response *head* is ready; on a streamed completion the
  body then runs for as long as the generation does. The guard is moved into
  the body, so the gauge is truthful for the two minutes this gateway is
  busiest. Proved with a slow mock: the head arrives, the body is left unread,
  and `/api/v1/metrics` still reports one in flight.
- **It counts requests, not clients, and says so.** Counting connections would
  mean owning the accept loop, which `axum::serve` owns; keep-alive means one
  client holds one connection across many requests either way. The status and
  monitoring surface is excluded from the count, because the panel polls it
  every second and holds `/api/v1/events` open permanently — counting those
  would pin the gauge at one on an idle gateway and add one to every reading of
  it, including the reading being taken by the request doing the asking.

M6b.1, third pass — the model detail and the panel's own files:

- **`GET /api/v1/models/{id}` reads the file; the list still does not.** The
  Models screen wants the shape of the network — layers, heads, KV heads, vocab
  size — and a RAM estimate. Both need the GGUF header, and putting them on the
  list would mean a header read per row on every poll, or copying the fields
  into the catalog and migrating every catalog already on disk. The detail
  endpoint pays for it once, when someone selects a model.
- **The estimate is for the context a load would actually choose** — the
  model's last context if it has one, otherwise the largest this machine can
  safely give it, by the same call `load` makes. An estimate for any other
  context would be a number no button on the screen produces. Asserted, along
  with the four terms summing to the total.
- **The list and the detail cannot disagree.** Both build their row through one
  constructor, and a test compares the two answers field by field. Two copies of
  "is this loaded, is its file there, was its digest checked?" is how a list and
  a detail view come to contradict each other.
- **A model whose file is gone is described without being read.** State
  `missing`, no header, no estimate — rather than an I/O error or a verdict
  about a file that could not be opened.
- **The panel is served by the gateway that answers its calls, so no CORS layer
  exists.** A cross-origin policy is a decision about who may call this gateway;
  writing one to solve a question about where a file is served from would be
  answering the wrong question. Same-origin in production by serving `/` from
  `--web-root`, and same-origin in development because Vite proxies to here.
- **A file can never shadow an endpoint.** The static handler is a `fallback`,
  so every route is matched first — checked over a real socket, because it is a
  property of the router rather than of any handler.
- **Nothing outside the web root is reachable.** Path resolution is a whitelist:
  every component must be an ordinary name, so `..` is refused rather than
  resolved-then-checked, and so is the `.` that pads a traversal past a check
  that only looks for `..`. Verified against a live gateway as well as in tests.
- **A missing asset is a 404, not the document.** Client-side routes get
  `index.html` so deep links work; a path whose last segment has an extension
  does not, because answering a missing script with HTML produces an
  unexplained syntax error instead of the 404 that says what happened.
- **`index.html` is never cached and hashed assets always are.** The document
  names the assets, so a stale copy points a browser at scripts a redeploy has
  already removed.

M6b.2, against a real gateway on this machine:

- **The two M0 directories are finally written to.** `conversations_dir()` and
  `settings_file()` were chosen in the first milestone and nothing had ever
  called them. `lightweight-store` is a crate of its own rather than part of
  `lightweight-catalog`: the two look alike — a directory, atomic writes, a JSON
  document — and differ in the way that matters, which is that a model can be
  downloaded again and a conversation cannot.
- **What the user typed is owner-only.** Conversation files come out `0600` and
  their directory `0700`, checked on a live gateway as well as in tests. The
  log has redacted prompts by default since M0; writing the same words to a
  world-readable file would have made that redaction decorative. The mode is set
  when the temp file is *created*, not after it is written, because a file that
  is briefly readable while megabytes go into it has been readable for as long
  as it takes to read.
- **Conversation ids are generated and never accepted.** An id becomes a file
  name, so taking one from a request means taking a path from a request. They
  are 128 random bits as hex, and the only shape the store will read back;
  anything else is `malformed_conversation_id` rather than a 404, because "you
  asked for something that cannot exist here" and "it is gone" are different
  answers.
- **Listing bounds its work, not just its output.** Directory entries carry a
  modification time, so the newest are chosen before any file is opened — a
  sidebar showing twenty conversations does not parse four hundred. The order
  shown is then re-sorted on the *recorded* time, because a restore from backup
  rewrites every mtime at once and the order the user remembers is in the file.
- **One damaged conversation costs one conversation.** A file that will not
  parse is skipped in a listing rather than failing it; the alternative turns a
  problem with one chat into the appearance of having lost all of them.
- **Settings have a typed half and an opaque half.** `gateway` is typed and
  every field in it is acted on — `keep_history` gates writes, `default_n_ctx`
  is consulted on the load path when a request names no context. A setting
  stored but never read is the same mistake as a control on screen that changes
  nothing. `ui` is passed through untouched, so the panel can remember a new
  preference without a change here.
- **An older build does not delete a newer one's settings.** Unknown top-level
  keys are preserved across a write, so running an older gateway after a newer
  panel does not silently discard configuration.
- **A corrupt settings file is an error, not a silent reset** — except on the
  load path, which falls back to defaults deliberately: a bad settings file must
  not be able to stop a model from loading, and the endpoint that exists to show
  settings reports the corruption plainly.
- **Turning history off refuses writes and still allows reads.** Conversations
  saved before the setting changed are still the user's; hiding them would leave
  no way to look at them or delete them.

M6b.3, against a real gateway on this machine:

- **The panel is served by the gateway it talks to.** Built to `frontend/dist`
  and handed to `hermes serve --web-root`; in development Vite proxies `/api`,
  `/v1` and the status paths to the same gateway. Both were exercised: every
  endpoint the panel calls answers `200` on the panel's own origin, in both
  modes. **No CORS layer exists anywhere in the workspace**, which was the point.
- **The types were written from captured responses, not from the Rust.** Every
  endpoint was called on a running gateway and its shape recorded before a line
  of the client was written. That is why `model: null` and every `Probed`
  section are in the types: they are the ordinary state at first start, and a
  panel that assumed otherwise would break on the machine it was made for.
- **A probe that could not read says so on screen.** `cpu_times`, `memory` and
  `disk` each carry their own outcome, and the panel renders "not measured"
  rather than a zero. The CPU tile shows nothing for its first second and says
  `measuring…`, because a rate needs two readings — the client differences the
  counters exactly as `/api/v1/system` was built to require.
- **No web font is fetched.** The first draft linked Google Fonts. That is a
  network dependency in a product built for machines that may have none, so it
  was removed: the panel prefers Inter where it is installed and falls back to
  the platform's own UI face. Nothing is downloaded to render the panel.
- **The dev proxy was missing and the gate did not notice.** `vite.config.ts`
  was lost to a failed `cd` in the scaffolding step. The production build still
  worked — Vite compiles JSX without a config — so nothing was red, but
  `npm run dev` could not have reached the gateway at all. Found by checking the
  file list against what should exist rather than by trusting a green build.
- **Blur is used on the rail and modals only.** Content cards take a translucent
  tint with no `backdrop-filter`, because a dozen blurred panels drop frames on
  exactly the hardware this product is for. The transparency toggle in Settings
  makes every surface solid, which is the escape hatch when text over the
  background is hard to read.
- **Controls with nothing behind them are absent, not disabled.** The reference
  design's Batch Size slider is not drawn: the gateway never passes a batch size
  to the engine. The API Gateway screen's host, port and key are read-only and
  say why, from the gateway's own `restart_required` list.
- **`./scripts/check.sh` now type-checks and builds the panel**, and skips with
  a reason when `frontend/node_modules` is absent, like every other optional
  tier.

**Not verified: how it looks.** Chromium and Firefox both fail to render on this
box — the headless renderer is killed with 216 MB free — so the panel's
appearance has been checked only by construction and against the reference
design, never by looking at it. That is the one claim in this file with no run
behind it.

M6b.4, run against a real gateway on this machine:

- **The supervisor is a plain module with no `electron` import**, so the four
  decisions worth getting right can be tested without a display: is one already
  running, is it *ours*, which binary, and how to stop what we started. Twenty‑five
  tests, three of which drive the real `hermes` binary.
- **It attaches rather than competing.** A gateway already serving is attached
  to, not replaced. Two engines on a machine that can barely hold one is the
  obvious cost; the subtler one is that a user who ran `hermes serve` themselves
  has their own flags, model and bind, and overriding that would be the shell
  overruling them.
- **It only ever stops what it started**, and that is asserted directly: a
  second supervisor attaches to the first one's gateway, is told to stop, and
  the gateway is still answering afterwards. Proven with real processes — start,
  serve, stop, and a `kill(pid, 0)` check that no orphan is left.
- **An open port is not an invitation.** The probe requires a `/health` body
  carrying both `status` and `backend` before attaching. Ollama also defaults to
  11434, and attaching to it would point the panel at an API answering some of
  the same paths with different meanings.
- **A loopback gateway is given no key at all**, because the gateway requires
  one only for a bind reachable from elsewhere. When a key is needed it is 32
  random bytes passed in `HERMES_API_KEY` and never in `argv` — the same reason
  M3.5 moved the engine's key out of its command line, applied to ours. A test
  greps the built command line for the key.
- **The window loads the panel over HTTP from the gateway**, not from a file, so
  it is the same origin as the API and behaves exactly as it does in a browser.
  One build of the panel, one set of behaviours.
- **Closing the window leaves the gateway serving**, with the tray to bring it
  back. A local service should keep answering the editor plugin or agent harness
  using it after its window is closed.

Three real defects, all found by running it rather than by building it:

- **A startup failure was invisible.** The shell reported failures only through
  a dialog, which needs a working display and a running message loop; when it
  died before either existed it exited silently with status 0. Failures now go
  to stderr as well.
- **A failure said nothing useful.** "The gateway stopped before it began
  serving" is exactly the unactionable sentence the error taxonomy exists to
  prevent. The child's output is now captured and quoted, which turned that same
  failure into `error: unexpected argument '--web-root' found` — the real cause,
  a stale `target/release/hermes` from before M6b.1.
- **The panel root was chosen by truthiness, not existence.**
  `process.resourcesPath` is set in a checkout too, so the packaged path was
  returned and the gateway was handed a directory that was not there.

One diagnosis was **wrong and had to be undone**: `import { app } from
"electron"` appeared to fail at run time, and a default-import "fix" was written
for it. The real cause was `ELECTRON_RUN_AS_NODE=1` in this environment, which
makes the Electron binary run as plain Node. Probed both forms inside a real
Electron main process, confirmed named imports work, and reverted the change and
its false comment.

Electron is pinned at **43.4.1**. The version first reached for, 33, carried
thirty‑four advisories; upgrading rather than accepting them leaves `npm audit`
at zero.

Packaging, closed afterwards on 2026-08-24:

- **`npm run package` called a tool that was not a dependency.** `electron-builder`
  was named in the script and never installed, so the one command that claimed
  to package the app could only ever fail. Added, and `npm audit` stays at zero.
- **An AppImage now builds and carries what it should**: `bin/hermes` (the
  release binary, current), `panel/` (the built SPA) and the app itself. 132 MB,
  executable, verified by extracting it and running the packaged binary.
- **The two build outputs were colliding.** `tsc` writes to `dist/` and
  electron-builder was writing its installers there too, so a package run buried
  the compiled main process under its own output — and `files: ["dist/**/*"]`
  would then have swept the installer back into the next package. Installers now
  go to `release/`.
- **The window was not associated with its desktop entry.** `desktopName` at the
  package root plus `linux.syncDesktopName` fixes it; the first attempt put
  `desktopName` inside `build.linux`, which electron-builder rejects, and the
  schema in `app-builder-lib` settled where each belongs.

**Still not done: the application icon.** The AppImage ships with Electron's
default. A brand asset has not been drawn, and inventing one was not the right
call to make here.

## Every tier, run

On 2026-08-24 the three opt-in gates were all exercised rather than left
skipped, so nothing in the suite is unproven:

| Tier | Result |
|---|---|
| Default (`cargo test --workspace`) | 644 passed |
| Real models (`HERMES_REQUIRE_REAL_MODELS`) | 3 passed |
| **Real engine** (`HERMES_TEST_MODEL`, SmolLM2-135M) | **9 passed** |
| **Model downloads** (`HERMES_TEST_NETWORK`) | **6 passed** |
| **Gateway downloads** (`HERMES_TEST_NETWORK`) | **2 passed** |
| openai contract suite | 30 passed |
| Desktop shell | 25 passed |

The smallest model was used for the real-engine tier deliberately: it proves the
same path and costs a fraction of the memory on a machine that has little.

A stale `target/release/hermes` from before M6b.1 was also rebuilt. It was a
live trap — the desktop shell prefers a release build, and that one predated
`--web-root`, so the shell failed on first run for a reason that had nothing to
do with the shell.

## M7, against a real engine

On 2026-08-24 every claim M7 makes was checked against `llama-server` running
SmolLM2-135M and Qwen3-1.7B, not only against the mock.

- **The block geometry is real, not rounded.** The same model at 8192 context
  estimates a 180.0 MiB KV cache with `f16` and 95.6 MiB with `q8_0` — exactly
  34/64. Rounding q8_0 to "one byte per element" would have said 90 MiB.
- **`RssAnon` is the right credit, and the margin is not small.** A resident
  SmolLM2 engine reported `rss` 181 MiB against `anon_rss` 67 MiB: the ~99 MiB
  of weights are mmapped and file-backed, already inside `MemAvailable`.
  Crediting the whole resident set on a swap would have double-counted them.
- **The credit reaches the verdict.** Swapping Qwen3 in over SmolLM2 logged
  `reclaimable = 67.2 MiB` on `hermes::memory` and admitted the load as
  `TIGHT`, with the warning that verdict now carries.
- **A refusal says what to do about it.** `qwen3-1.7b-q4_k_m` at 32768 is
  refused with four remedies carrying the numbers to apply them: reduce the
  context to 1084 tokens, quantize the KV cache to q8_0 saving 1.64 GiB, choose
  a model under 1.67 GiB, or free 3.38 GiB. **That list was empty before M7.**
  `BackendError::InsufficientMemory` has no remedy arm and the load path threw
  the estimate away one line before building the error, so the single most
  important refusal in the product arrived with nothing actionable attached —
  and the panel never displayed it at all, because it stopped at the 202.
- **The gauges appear only once measured.** `hermes_engine_resident_bytes` and
  `hermes_engine_peak_resident_bytes` are absent from `/metrics` with no engine
  and present with one.
- **The detail prices what the caller is weighing.** `?ctx=1024` against
  `?ctx=2048` doubles the KV cache exactly; `?kv_type=q8_0` reproduces the
  34/64 identity through HTTP.

Tiers run for this milestone: default (`cargo test --workspace`), the openai
contract suite, the frontend typecheck and build, the desktop shell's 25 tests,
the dependency gate and the secrets gate — all via `./scripts/check.sh` — plus
the real-engine tier (`HERMES_TEST_MODEL`, 9 passed, 115 s).

## M8, against a real engine

On 2026-08-25 every claim M8 makes was checked against `llama-server` running
SmolLM2-135M and Qwen3-1.7B, not only against the mock.

- **The engine's processor time is real and it reaches the scrape.**
  `hermes_engine_cpu_ticks_total{mode="user"} 3531` and `{mode="system"} 25`
  against a `/proc` reading taken independently over the same generation. The
  engine kept **3.21 of 4 cores** busy, derived as a ratio of two tick counts
  with no `USER_HZ` guessed anywhere.
- **The gateway costs nothing while the engine works.** Charged **2 ticks
  against the engine's 3266** — 0.06% — during one generation. That is the
  measurement that decided *not* to cap the gateway's tokio worker threads:
  idle workers park on epoll rather than spinning, so the oversubscription that
  looked real on a 4-core box is not.
- **The engine publishes less than the plan assumed, and the fixture says so.**
  b10590 has **no `llamacpp:kv_cache_usage_ratio`**, though older llama.cpp
  builds do and the plan was written expecting it. Every counter is therefore
  `Option`, a missing series reads as absent rather than zero, and the captured
  body is committed as a fixture with a test asserting it carries no path and
  no label. `hermes_engine_max_sequence_tokens 83` against a 2048 context is
  the signal that survived: the cheapest evidence a model holds a window nobody
  is using.
- **The physical batch earns its control.** At 256 prompt tokens: **ubatch 512
  prefills at 22 t/s against 11-15 t/s at 128**, with 3.9 of 4 cores busy
  against 2.4. Measured before the control was added, not after.
- **Prefix reuse, measured at last.** A fully cached prompt reached its first
  token in **90 ms against 10-16 s cold** — the single largest performance
  feature on this hardware, observed in passing since M3 and now a scenario.
- **Locking weights is refused when it cannot work, and works when it can.**
  This machine's `Max locked memory` is 969.7 MiB. Qwen3's 1.19 GiB of weights
  is refused **before any engine is launched**, while the memory estimate says
  `SAFE` — a distinction `InsufficientMemory` could not have expressed.
  SmolLM2 loads with `--load-mode mlock` reaching the engine's argv and
  `VmLck: 101244 kB` genuinely locked, against `VmLck: 0` for a mapped engine.
- **A benchmark taken through the gateway carries no path, no prompt and no
  hostname.** Checked against a real saved run, not only against a fixture.
- **The estimator over-predicts, and now by a recorded amount.** SmolLM2 at
  2048: predicted 285.1 MiB against an observed peak of 207.3 MiB, of which
  143.9 MiB is the exactly-computed half. The fit from a two-bucket sweep:
  10802 bytes per ubatch token, 51.4 MiB fixed. Nothing reads it yet.

Tiers run for this milestone: default (`cargo test --workspace`), the openai
contract suite, the frontend typecheck and build, the desktop shell's tests,
the dependency gate and the secrets gate — all via `./scripts/check.sh` — plus
the real-engine work above by hand.

**One flake observed and not papered over.**
`a_slow_engine_is_waited_for_rather_than_abandoned`
(`lightweight-backend-llamacpp/tests/supervision.rs`) failed once under a full
`cargo test --workspace` and passed on every isolated and repeated run. It
asserts a wall-clock uptime of at least 400 ms against a fake engine, which is
timing-sensitive on four contended cores. The assertion is not weakened to make
it green: it encodes the behaviour that matters, and the flakiness is the
machine, recorded here so the next person to see it knows it has been seen.

## M9, verified by execution on 2026-08-24

**`--concurrency` had never worked, and the engine said so when it was finally
asked.** Launched with `--ctx-size 4096 --parallel 2`, the pinned build reports
`n_slots = 2, n_ctx_slot = 2048, kv_unified = 'false'`: `-c` is the total and
the engine divides it. So every deployment that had raised the slot count was
handing each client a fraction of the window `/props`, `/v1/models`, the
overflow check, `clamp_max_tokens` and the band ceilings all advertised, while
the estimator priced the caches at N times what the engine allocated. It was
found by reading `--help` and `strings libllama.so` rather than by a failure,
because nothing in the tree had ever built the engine's arguments at more than
one slot.

- **Four slots, each with the whole window.** A real engine loaded at
  `n_ctx: 1024, n_parallel: 4` reports `default_generation_settings.n_ctx` of
  **1024** and `total_slots` of **4** — the multiplication checked against the
  engine's own answer rather than against our reading of its `--help`.
- **Two clients served at once, and the engine confirms they were batched.**
  Two real streamed requests against SmolLM2-135M at `--concurrency 2`: both
  ran, `/api/v1/requests` listed both with their bands and prompt counts, and
  `hermes_engine_busy_slots_per_decode` read **1.267** — above one, so the
  engine served them in shared decode steps rather than in turn. The sweep
  measured 1.30 for the same shape.
- **The advertised window and the engine's window agree**, checked live on the
  same gateway: `/props` reports 1024 with `total_slots` 2, and
  `hermes.engine.default_generation_settings.n_ctx` — which had no caller in
  three milestones — reports 1024 beside it.
- **`auto` resolves out loud.** On this four-core box `hermes serve` prints
  `requests 1 at a time (fitted to 4 cores)`, and `/api/v1/gateway` reports
  `requested: null` beside a live capacity of 1. The number this machine has
  always used, for the first time because a measurement supports it.
- **The sweep that set the rule.** `hermes bench --parallel 1,2,4` at 1024
  tokens per client: aggregate decode 3.95 → ~4.8 → ~4.9 t/s while per-client
  decode fell 3.95 → 2.39 → 1.23, and peak RSS rose 184 → 222 → 283 MiB. A
  single generation already kept 3.0-3.8 of four cores busy, so a second slot
  takes a core rather than finding one. `CORES_PER_SLOT` is four because of
  that column, and the run id is in the constant's doc comment.
- **Two clients are told apart by the connection, over real sockets.** The
  test binds one client to a second loopback address, and asserts the newcomer
  is placed behind *one* of the busy client's requests rather than behind both.
  Checked against the defect it exists for: with peer identity removed it fails
  three times out of three, while the single-client ordering test beside it
  still passes.
- **A flake in that test was fixed rather than retried.** Its first version
  read one client's queue notice before the other's, and reading a notice
  consumes the response — which disconnects that client and reorders the queue
  it was measuring. The reader borrows the response now, and skips the first
  notice, so the position it reports was computed after every request in the
  test had arrived.

Found while building M9, and fixed:

- **A benchmark could run with no slot.** `benchmark.rs` bound the permit as
  `let _permit = ...await;` — an `Option` — so a queue timeout produced exactly
  the interleaved measurement the comment above it says it prevents, silently.
  It is a refusal now, naming the wait it gave up after, and pinned by a test
  that holds the only slot and asserts the job fails `server_busy` with nothing
  measured. Checked against the defect it exists for: restore the discarded
  `Option` and it fails.
- **Two queue counters described overlapping sets.** A non-streamed timeout
  incremented `timed_out` *and* `abandoned`; a streamed one incremented only
  `abandoned`. Both numbers move on `/metrics` as a result of this fix; no name
  and no label changed.
- **Admission was two locks with a gap.** A slot released between "is one free?"
  and "put me in the queue" found an empty queue and went idle while a request
  was on its way into it — at capacity one, a client waiting out the whole queue
  timeout in front of a gateway doing nothing.

M10a, verified by the matrix rather than by this machine:

- **Four runners run `scripts/check.sh` itself**, not a reimplementation of it,
  so a tier that is skipped locally is skipped visibly there too. All six jobs
  — `linux-x64`, `macos-arm64`, `macos-x64`, `windows-x64`, `linux artifacts`
  and `flatpak` — pass on the branch head.
- **Two platform breaks were found by the matrix and not by review**, which is
  why `check.sh` now cross-checks `lightweight-sys` for the three non-host targets on
  any machine that has them installed: the crate holds every `unsafe` block in
  the workspace and has no C dependencies, so a typo in a Windows arm need not
  wait for CI.
- **macOS needed a different number, not a bigger probe.** It publishes no
  per-process maximum resident set, and the lifetime maximum it does publish is
  a *footprint*, which excludes the clean file-backed pages a mapped model's
  weights live in. Subtracting weights from a footprint underflows, the sample
  is discarded, and `--fit` reported a run with no fits and no reason. `PeakKind`
  now travels with every sample, defaulted on read so runs already on disk parse
  as the resident-set peaks they were, and `Prediction::exact_within` subtracts
  only what that kind of peak contains.

The release path, proven end to end on 2026-08-26 (`release.yml` run
`32926802994`, dispatched from master at `d434394`). Everything below was found
by running the release, never by reading it, and each fix was only ever provable
by the next full run — `check.yml` does not build or install anything:

- **`gh release create dist/*` was handed a directory.** `upload-artifact`
  roots each artifact at the least common ancestor of its `path:` entries, so the
  three build jobs' artifacts arrived as *directories* and the flatpak's as flat
  files. A flatten step (PR #5) now makes `dist` one directory of files whatever
  shape they arrived in, and a guard counts what is actually there: on this run
  all six filename patterns matched (`*.tar.gz` twice), `files 7` equalled
  `SHA256SUMS 7 lines`, and the build-provenance step reported **`Attestation
  created for 7 subjects`**. The two green-but-empty failures it replaced — an
  empty `SHA256SUMS` and an attestation covering the flatpak alone — cannot ship
  again; a release missing a platform stops at the guard.
- **The Windows smoke test ran a file its install loop never waited for**
  (PR #6). It gated readiness on the top-level `Hermes.exe` launcher, then with
  no second wait ran `resources/bin/hermes.exe` — a file NSIS extracts
  separately and in no guaranteed order. One release run passed and the next
  failed on the same bytes; that was the race. The loop now waits for the file
  the run assertion needs, and this run printed `ok the installed hermes runs`
  where the prior one had `FAIL` in the same millisecond as the launcher
  appearing.
- **The draft is complete.** `dry-run-32926802994` carried **8 assets** — the
  seven artifacts (two CLI `.tar.gz`, one CLI `.zip`, `.dmg`, `.exe`,
  `.AppImage`, `.flatpak`) plus `SHA256SUMS` — and the whole release job was
  green. Earlier NSIS fixes (the silent `/S` switch Git Bash rewrote, PR #4; the
  installer that never returns because it launches the app, PR #3) are the
  reason the Windows job reaches this point at all.

M10b's three measurement questions, answered on 2026-08-25 against
SmolLM2-135M Q4_K_M on llama.cpp `b10590`, at `--prompt-tokens 256 --repeat 2`
(runs `18cf2bfb07298264`, `18cf2c104b6c6db3`, `18cf2c1c92bd4be4`):

- **Peak RSS is a ratchet, and it had been fitted as if it were noise.** Within
  one bucket every sample is read from one engine process, and `peak_rss` is a
  high-water mark, so the readings only climb: 39.95 → 57.43 MiB across a
  bucket's six samples. The spread was **17.5 MiB in all eight buckets swept**,
  within 0.1 MiB of the same value at four batch sizes and three contexts, and
  it resets on each reload. `bench.rs` already knew this at bucket granularity —
  it reloads between buckets for exactly this reason — and the fit did not know
  it at sample granularity. Regressing the raw samples therefore fitted the
  order they were taken in: R² **0.10**, on data whose slope is stable to 1.4%.
- **1. Is the residual affine in `n_ubatch`? No.** Comparing like with like, the
  segment slopes across 64 → 128 → 256 → 512 are **32,600, then 26,378, then
  2,949 bytes per ubatch** — an elevenfold disagreement, most of the growth at
  the small end and almost none at the large. A straight line through the four
  peaks scores **0.79**. Per M10-PLAN section 4.1, that makes the trust rules
  the place that has to say so, and they now do.
- **2. How wrong are the shipped defaults? Conservative, increasingly so with
  batch size.** Predicted engine-side memory against the measured peak residual:
  **1.37×** at ubatch 64, 1.57× at 128, 1.95× at 256, **2.85×** at 512. The
  cause is structural rather than a mis-set coefficient: `compute_bytes` scales
  the logits term by `n_ubatch`, which is 196,608 bytes per ubatch for this
  vocabulary and **fifteen times the entire measured slope** of ~13,100. The
  engine appears to size that buffer by the number of tokens it is asked for,
  which during prefill is one.
- **3. Does the residual move with `n_ctx`? No.** At a fixed ubatch of 512, the
  residual across contexts of 1024, 2048 and 4096 was **45.40, 45.88 and 45.52
  MiB** at matched positions — 0.48 MiB, under 1%, across a fourfold range —
  while the KV cache it excludes went 22.5 → 45 → 90 MiB. The context's cost is
  the exact half, which is where the estimator already puts it. `n_parallel` is
  nearly as flat: 45.60, 45.66, 46.80 MiB at 1, 2 and 4 slots (M9's run
  `18ceefe0aa03aba1`). **The bucket key is not widened on this evidence** — the
  plan reserves that for a measurement that sets out to test it, and this one
  did not — but the finding is recorded for the pass that does.

What those numbers decided, and what they refused:

- **The trust rules are two.** A fit must rest on at least three *distinct*
  batch sizes, and its line must account for at least 95% of the spread across
  them. The count is checked first because two batch sizes always score a
  perfect 1.0 — a line through two points passes through both — so the order is
  what makes the second rule a rule. The two rules stand on different footing
  and are labelled as such: the count follows from what a line through two
  points can mean, while **95% is measurement-informed conservative policy
  rather than a derived number**. What the sweep establishes is that 0.79 must
  fail; any bar above it would have done that, and the margin above is chosen
  for the asymmetry of the risk, not calculated.
- **The counting defect was real and had shipped.** The old rule counted
  observations, and this machine's stored fit rested on **twelve observations at
  two batch sizes**. Twelve is comfortably more than three. It is refused now,
  by a test written against the fit that fooled it.
- **The fit is taken at each bucket's peak rather than through its climb**,
  which is the ratchet finding applied: the peak a configuration reached is both
  the independent observation and the quantity an estimator has to cover. Every
  observation is still recorded, so a later pass can fit them differently
  without re-running anything.
- **On this machine, calibration correctly refuses, and the shipped guesses
  stand.** Proven end to end rather than argued, on run `18cf2ce44809bc3a`:
  `hermes bench --ubatch 64,128,256,512 --fit` recorded `12965 bytes per ubatch
  token, 57.7 MiB fixed, R² 0.77` and said of it, in the same output, `not used:
  the fitted line accounts for 77% of the spread across batch sizes, below the
  95% a measurement must reach`. `hermes estimate` against that same
  `calibration.json` then reported `SAFE (uncalibrated; compute and overhead are
  estimates)` — the two surfaces agreeing, where before one of them insisted
  nothing read the file at all. A second sweep reproducing the first to within
  2% on the slope is also the answer to how repeatable any of this is.
  This is the contingency M10-PLAN section 4.1 named: it said that if the points
  do not lie near a line, the fit format is describing something it cannot
  describe and the trust rules are the place that has to say so. They said so.
  Section 4.2 is *not* the clause that covers this — it allowed for "the
  defaults are already close", and at 1.37× to 2.85× these defaults plainly are
  not. What makes the refusal the right outcome rather than a near miss is the
  direction of the error: an over-estimate refuses a load the user can force, an
  under-estimate invites the OOM killer. **On this machine M10b's value is
  precisely that it prevents a false `Confidence::Measured`.**
- **`SlopeBelowLogits` was written to catch a run that measured the wrong
  thing, and here it fires on runs that measured correctly.** It stays, because
  its effect is right even where its stated reason was not, and the reason is
  corrected where the guard is. Fixing the *shape* of the compute term on one
  machine's evidence would be the confident wrong number this design exists to
  refuse; it is recorded as a deferral below.
- **A fit is now matched on the engine variant it was actually taken against.**
  Found by audit rather than by a failure: `EngineFingerprint::matches` compared
  the backend and the build and skipped `ggml_variant`, which it had been
  recording all along. The machine fingerprint pins the ISA features the variant
  is derived from, so it was *usually* implied — and "usually implied" is not
  what a type whose whole purpose is refusing a mismatch should rest on. The
  mapping from ISA features to a dispatched variant belongs to the build that
  does the mapping, so a later build shipping a different set of variant
  libraries would have reused a fit taken against code that is not the code
  running. Both sides are read from `CpuInfo::detect()` at runtime, never from
  whatever compiled the binary. Checked against the defect it exists for:
  restore the two-field comparison and the test fails.
- **Three user-facing claims that nothing reads a calibration are gone.** They
  were true when written and had been false since M10b.2: the `--fit` argument's
  own help, the sentence `hermes bench --fit` printed after every fit, and
  `fit.rs`'s module doc. In their place `hermes bench` now reports, per fit,
  whether the next load will use it — which is the question a person actually
  has, and the one whose wrong answer hid all of this.

## User-defined model aliases (feature/user-model-aliases)

Done and green locally: fmt, clippy `-D warnings`, `cargo test --workspace`
(905 passed), the contract suite (43 passed, 2 skipped for the absent agent
parser, as before), the panel build, and the version, dependency and secrets
gates.

- `InstalledModel.alias` (`#[serde(default)]`), validated and resolved in one
  place: `lightweight_catalog::alias` (`validate_alias`, `ModelSelector`,
  `same_name`) and `CatalogStore::{resolve, by_alias, set_alias}`. Lookup order
  is `default`/empty → alias (case-insensitive) → canonical id.
- The gateway's `ResidentModel` carries the alias; `public_id()` is what
  `/v1/models`, `/v1/capabilities` and every response name the model by.
  Metrics, `/health` and the control API keep the canonical id.
- `PATCH /api/v1/models/{id}`; `alias` on import and download, checked before
  the job starts; load, detail and delete accept an alias.
- `model: "default"` restored (removed in `9bb2569`), and guarded by its own
  matrix test (`default` and omitted `model`, chat and text completions, with
  and without an alias) because Lightagent depends on it.
- One namespace: `CatalogStore::check_alias` refuses an alias equal to any id;
  `ensure_id_unaliased` refuses a fixed (pinned or link) id equal to an alias,
  before the transfer and again at commit; generated import ids step around
  aliases case-insensitively.
- `hermes models alias` probes `127.0.0.1:<configured or default port>`; a
  gateway reporting this profile's data directory takes the change over
  `PATCH`, one that cannot be identified refuses the write, and anything that
  is not a Lightweight gateway (the default port is Ollama's too) is ignored.
- Smoke-tested for real on 2026-10-04: SmolLM2-135M on a real engine under an
  isolated profile, Lightagent `7d95232` in an isolated home configured with
  `default`. Through a logging proxy: Lightagent listed `Coder`, sent
  `model: "Coder"`, and every response chunk said `Coder`. Lightagent sends the
  advertised id rather than the literal `default` when one model is listed;
  the literal `default` and an omitted model were checked against the same
  engine directly.
- Not done, deliberately: alias history, a fleet-manifest `alias` field, and
  passing the alias to llama.cpp's `--alias` (the gateway never forwards the
  engine's model name, so it would change nothing a client sees).

## Federated model router, R0-R3 (feature/federated-model-router)

Green locally on 2026-10-05:

- `cargo fmt --check` and `cargo clippy --workspace --all-targets -D warnings`
  are clean.
- `cargo test --workspace`: **991 passed, 0 failed**. 71 of those are new:
  48 router unit tests and 23 router integration tests. The other 920 ran
  unchanged.
- The openai-SDK contract suite: 47 passed, 2 skipped, as before.
- The version, dependency and secrets gates pass.
- One pre-existing timing race was seen once under full-workspace parallel load:
  `supervision::a_segfaulting_engine_is_classified_as_a_crash` reported
  `engine_start_timeout`. It passed 3 times out of 3 in isolation and in the
  `--no-fail-fast` rerun, and the router does not touch that crate. It was left
  unchanged.

What was built:

- New crate `lightweight-router`, run as `hermes router [--config] [--listen]`
  and `hermes router validate-config`. `hermes serve` is untouched.
- **Domain:** `NodeId`, `DeploymentId` (`node/model`), `RouteName` (alias
  rules), `NodeAuth`/`Secret` (redacted `Debug`), `Deployment`, `Route`,
  `RoutePolicy::Priority`, `NodeHealth`, `DeploymentHealth`/`UnavailableReason`,
  `CapabilitySet`, `RoutingDecision`/`RoutingReason`, `RoutingFailure`, and a
  `Topology` built only by validation.
- **Configuration:** JSON, with unknown keys refused, keys taken only from
  environment variables, and every problem reported at once.
- **Health:** one `GET /v1/capabilities` probe per node per interval (default
  5 s), a failure threshold (default 2), and an `unknown` state that is not
  eligible for traffic.
- **Routing and proxying:** priority selection, pre-response failover on
  connect, timeout, 502/503/504 and stale `model_not_found`. `model` is
  rewritten both ways, and SSE is relayed frame by frame.
- **Supporting pieces:** request ids, the read-only `/api/router/v1/*`,
  Prometheus `/metrics`, and the log target `hermes::router`.
- Full description: `docs/ROUTER.md`.

Verified by execution, against two real gateways (`target/debug/hermes serve`)
each running SmolLM2-135M on the real engine. Each was in an isolated scratch
profile, with node-local aliases `QwenCoder` (node A) and `CoderBackup`
(node B), behind the real `hermes router` binary:

- `/v1/models` listed only `Coder` and `Fast`, at context 2048.
- A streamed `Coder` request was answered by node A, with every chunk saying
  `Coder`. `model: "default"` on `/v1/completions` was answered by node B as
  `Fast`.
- Failover, step by step:
  1. Both nodes healthy: `primary_healthy` to node A.
  2. Node A stopped by its PID. The very next request got a refused connection
     and went to node B (`primary_failed_fallback`, `failover_count=1`).
  3. After the health probes, requests went straight to node B
     (`primary_unavailable_fallback`).
  4. Node A restarted on the same port. One probe later, requests were back on
     node A (`primary_healthy`). The client always saw `Coder`.
- A client disconnecting after 2 s of a 1500-token stream: node A's
  `finish_reasons.cancelled` went 0 → 1, `running` returned to 0, and the
  router's active-request count returned to 0.
- **Lightagent `7d95232`, unmodified,** in an isolated `LIGHTAGENT_HOME` with
  `--base-url` set to the router: `lightagent models` printed `Coder` and
  `Fast`, a chat with `Coder` streamed an answer, and its status bar showed
  `Coder`. The router logged both requests to `node-a/QwenCoder`.
- The user's own gateway on port 11434 was not touched. The scratch processes
  were stopped by their recorded PIDs.

Final review pass (2026-10-05). R4 not started. `./scripts/check.sh` passes in
full:

- workspace tests: 1001 passed, 0 failed. The router has 81 of them (53 unit,
  28 integration);
- real-model header tests: 3;
- the contract suite: 47 passed, 2 skipped;
- the panel and desktop builds;
- the cross-target `lightweight-sys` checks;
- the version, dependency and secrets gates.

- **Lightagent's runtime panel.** The only live consumer is the provider panel,
  which reads `/api/v1/gateway` for `reasoning_content` and `/api/v1/models` for
  the runtime catalog. The router serves neither, proxied or imitated. Imitating
  `/api/v1/gateway` would mean inventing an engine `device`, and would open a
  path to model placement through the router. The full table and the reasoning
  are in `docs/ROUTER.md`.
- **Per-deployment state.** Each deployment's capabilities, context and
  concurrency limit are now filed and kept separately (`HealthBook::deployment`),
  and shown in `/api/router/v1/deployments`.
- **Route context.** The route's public context, features and limit come from
  `select::summarize` over exactly the set `select::plan` would try. Before this,
  `/v1/models` context counted every deployment last seen serving, including
  unhealthy ones.
- **New coverage.** Tests now cover:
  - an identity-leak audit across every client surface and refusal;
  - a single-deployment stale `model_not_found` answered as `route_unavailable`
    without the alias;
  - a 500 that is not retried;
  - an `unknown` node at startup that gets no traffic until a probe sees it,
    then serves without a restart;
  - an immediate failover asserting that one failure is recorded and the
    request did not wait for a probe.
- **Cleanup audit.** The integration tests spawn no processes and write no
  files. Killing the test binary with SIGINT or SIGKILL mid-run left no process
  or listener behind. The disk growth was `target/debug/incremental`.
- **Real re-smoke.** Unmodified Lightagent `7d95232` went through the router to
  primary node-a. Node-a was then killed, and the next new Lightagent request
  failed over in the same request to node-b (`primary_failed_fallback`,
  `failover_count=1`). Lightagent showed `Coder` both times.

Deliberately not built (see the `docs/ROUTER.md` roadmap):

- strategies other than priority, and session affinity;
- capability filtering, placement and warm standby;
- `Auto` routing and mixture-of-agents;
- latency/TTFT histograms;
- proxying the node control plane;
- consensus or external state stores.

## Router load balancing, R4 (feature/router-load-balancing)

`./scripts/check.sh` passed in full on 2026-10-05:

- workspace tests: **1025 passed, 0 failed**. The router has 105 of them
  (68 unit, 37 integration), up from 81;
- real-model header tests: 3;
- the contract suite: 47 passed, 2 skipped;
- the panel and desktop builds;
- the version, dependency and secrets gates.

What was built:

- **New policies.** `RoutePolicy::{RoundRobin, LeastBusy}` (`round_robin`,
  `least_busy`) sit beside `Priority`, which is unchanged: every R3 test passes
  as written.
- **Selection order.** `select::eligible` is the one eligibility step. The pure
  functions `order_priority`, `order_round_robin` and `order_least_busy` order
  what is left. `Selector` holds per-route state: an atomic cursor, and a lock
  that least-busy holds while it chooses and reserves.
- **In-flight counts.** `load::LoadBook` counts in-flight work per deployment
  with RAII `Lease`s. The proxy walks the plan and carries the lease. It
  contains no policy logic.
- **Least-busy rule.** Least-busy compares `active × other_limit` in `u128`.
  Known positive capacity ranks before unknown or zero capacity, and ties go to
  configured order. The limit is the node's scheduler slot count
  (`--concurrency`, as confirmed by the engine's `n_parallel`).
- **Mutation check.** Replacing the normalized comparison with raw counts fails
  4 tests.

Verified by execution against two real gateways running SmolLM2-135M. Node A
ran with `--concurrency 2` and node B with `--concurrency 1`, both behind
`hermes router`:

- **Round-robin:** four requests went node-a, node-b, node-a, node-b
  (cursor 0 to 3), and every response said `Coder`.
- **Lightagent `7d95232`, unmodified:** two chats went to node-a and then
  node-b (cursor 4, then 5), and its status bar showed `Coder`.
- **Least-busy:**
  - Two long streams went to node-a (a tie at 0/2 against 0/1, broken by
    configured order) and node-b (node-a now at 1/2).
  - A third request went to node-a with `active_before=1` and
    `concurrency_limit=2`, because 50% is less than 100%.
  - The in-flight counts read 1/1 during the streams and 0/0 afterwards.
- **Disconnect:** a client disconnecting mid-stream released the slot to 0,
  and node-a's `cancelled` went from 0 to 1.

Observed, and documented in `docs/ROUTER.md` rather than changed here:

- a freshly started `hermes serve <file>` briefly advertises its canonical id
  before adopting its alias;
- two routes sharing one deployment can make one slightly uneven simultaneous
  choice (a future refinement; no global lock).

Fixed before merge: `/v1/capabilities` reported the startup slot count
(`GatewayConfig::max_concurrent_requests`) rather than the scheduler's live
count, so least-busy could divide by a stale limit after a hot swap. It now
reads `Scheduler::capacity()`, the value `load_model` resizes and
`/api/v1/gateway` already reported. Covered by
`the_capabilities_report_the_slots_the_running_engine_was_given` (a real load
through the manager, 4 to 2) and
`least_busy_follows_a_node_whose_scheduler_was_resized_after_the_next_probe`
(a real gateway resized, the router's next probe adopting 2, and the third
request going to B at 25% rather than A at 50%). Both fail without the fix.
After the fix, `./scripts/check.sh` passed again: 1027 workspace tests passed,
0 failed (the router now has 106), and the contract suite 47 passed, 2 skipped.

R4 merged as `357b415` (PR #32).

## Router capability filtering, R5 (feature/router-capability-filtering)

`./scripts/check.sh` passed in full on 2026-10-05:

- workspace tests: **1063 passed, 0 failed**. The router has 142 of them
  (94 unit, 48 integration), up from 106;
- the contract suite: 47 passed, 2 skipped;
- the panel and desktop builds, and the version, dependency and secrets gates.

What was built:

- **Pipeline.** `select::eligible` → `capability::filter` → the route's policy.
  `Selector::plan_request` runs all three. `Selector::plan` now means "nothing
  required", so every R4 caller and test is unchanged.
- **Requirements** (`requirements.rs`). Each request is read once, with the
  gateway's own `ChatCompletionRequest::to_generation_request` and
  `CompletionRequest::expand`, so a request the gateway would refuse is refused
  by the router with the gateway's exact 400. It records the endpoint, whether
  tools are declared (`[]` is none), how `tool_choice` must be honoured,
  whether `reasoning_effort` asks for an effort (`"none"` does not), and a
  lower bound on the prompt's tokens.
- **Filter** (`capability.rs`). Each deployment's own last observation gets a
  yes or a no. Reasons are kept, all of them, as `CapabilityGap`. Nothing is
  reordered or ranked. An unobserved deployment is never assumed capable.
- **Context rule.** It matches the node: refuse only `prompt_tokens >= n_ctx`.
  `max_tokens` is clamped by the node, so it is logged and not required.
  Filtering on prompt plus budget would have refused Hermes' default 65536
  everywhere.
- **Errors.** `400 route_capability_mismatch` names the route and what was
  missing, never a node. It is distinct from `model_not_found` and
  `route_unavailable`. If only an *unavailable* deployment could have served
  the request, the answer is `route_unavailable`.
- **Observability.** Requirement fields and `eligible_before`/`eligible_after`
  go on the routed log line. Two metrics are added:
  `router_capability_filtered_total{route,reason}` and
  `router_capability_mismatch_total{route}`.
- **Mutation check.** With the filter bypassed, 9 unit and all 10 new
  integration tests fail, and the 38 existing integration tests still pass.

Measured before it was trusted, as the bound's input. Through a real node
running SmolLM2-135M:

- Text ran from 0.98 to 4.87 bytes a token, always above the bound's 6.
- Eight tool declarations cost **31** prompt tokens in all, because that
  template drops tools. So the bound counts only message text (`d33251b`).
- The table is in `docs/ROUTER.md`.

Verified by execution, with two real gateways running SmolLM2-135M: node A at
`--ctx 8192` aliased `BigWindow`, and node B at `--ctx 2048` aliased
`SmallWindow`. They sat behind the real `hermes router` on a round-robin
`Coder`:

- `/v1/models` listed `Coder` at 2048, the conservative summary.
- Ordinary and tool requests alternated A, B, A, B (`filtered=""`). A streamed
  answer carried only `Coder`.
- Two 13.5 KB prompts (bound 2251, real 2731 tokens) both went to node A, at
  cursors 7 and 8 (`eligible_after=1`, `filtered="context_too_small=1"`).
- A 60 KB prompt was refused `400 route_capability_mismatch` with no node hit.
  `tool_choice: "required"` with no tools got the gateway's
  `400 invalid_tool_choice` from the router.
- Real gateways advertise every feature flag as `true`, so tool filtering was
  shown with a scripted third node advertising `tools: false`, placed first in
  a priority `Coder`:
  - a request without tools went to it;
  - **unmodified Lightagent `7d95232`**, in an isolated `LIGHTAGENT_HOME`,
    chatted with `Coder`. Its request carried its tools, the router passed over
    the scripted node (`tools_unsupported`), node A answered
    (`primary_unavailable_fallback`), and Lightagent's status bar showed
    `Coder`.
- The user's own gateway on 11434 was not touched. The scratch processes were
  stopped by their recorded PIDs, and the Lightagent tree is unchanged.

### Hardening: context-overflow failover

The lower bound can let a prompt through to a deployment too small for it by
the node's own count. That node answers `400 context_length_exceeded` (code
from `BackendError::ContextOverflow`, `invalid_request_error`, `param`
`messages`/`prompt`, sent before any stream starts).

- **Detection.** The router reads every `400` body whole. Only that
  structured `error.code` moves the request: to the next deployment in the
  same plan whose observed context is strictly larger than every one that has
  overflowed.
- **Everything else is unchanged.** Every other `400` and every `500` still
  stands, and nothing is retried once relayed.
- **No re-planning.** There is no second round-robin step and no new
  least-busy choice. The overflowed deployment's slot is returned first.
- **No larger deployment.** The node's error is returned unchanged.
- **Observability.** Reason `context_overflow_failover`, metric
  `router_context_overflow_failovers_total{route}`.

Tests: 7 new integration tests. They cover recovery (plain and streamed);
equal and smaller contexts not tried (and 8K, 8K, 32K going straight to 32K);
a tools-filtered 32K deployment never reached; other 400s, a free-text
"context length" 400 and a 500 standing; a post-commit in-band overflow not
failed over; round-robin's cursor moving once; and least-busy's slot moving
and returning.

- **Mutation checks.** Detection off fails 5 of them. Retrying regardless of
  context fails the equal-context test and the R3 test
  `a_client_error_from_the_node_stands_and_is_not_retried_elsewhere`, which
  passes unmodified (equal contexts).
- **Validation.** `./scripts/check.sh` passed: 1070 workspace tests, 0
  failed. The router has 149 (94 unit, 55 integration). The contract suite was
  47 passed, 2 skipped.

## Router session affinity and observability, R6 (feature/router-session-observability)

Built on `master` after v0.5.0 (`7c177d0`); v0.5.0 is untouched. R7 not
started. Nothing here changes routing for a request without a session, or for
any request while affinity is off (the default).

- **Session id source.** Lightagent `7d95232` sends no session, conversation or
  request id (its body is `model`, `messages`, `stream`, `stream_options`,
  `tools`, `temperature`, `max_tokens`; its only header of note is the bearer
  key). So the router reads an explicit, configurable header,
  `X-Lightweight-Session`, and Lightagent was not modified. No session is ever
  inferred from an address, a key, a user agent or the prompt.
- **Affinity** (`affinity.rs`, `Selector::plan_with_affinity`): key = route +
  keyed hash of the id (raw id never stored; 8-hex fingerprint in views);
  value = deployment, created, last used. In memory, idle TTL (default 1800 s),
  `max_entries` (default 10 000; expired first, then LRU), lazy expiry plus a
  sweep task. Preferred only after health and the capability filter; a hit
  takes no policy turn (no round-robin cursor draw, no least-busy comparison)
  and the rest of the plan is the policy's failover order. Settles only on a
  committed 2xx: first commit establishes (a racing first request does not
  overwrite), a ruled-out or failed sticky deployment is reassigned with a
  reason. No bounce-back on recovery.
- **Observability** — measured, never read by routing: TTFT (receipt → first
  relayed content/reasoning/tool-call delta; streams only), upstream TTFT,
  request, planning (µs buckets), per-attempt response-head and committed
  upstream-body histograms; affinity counters/gauges; `actual / estimated`
  prompt-token ratio and signed error histograms from `usage.prompt_tokens`.
  `RoutingTrace` per request in a bounded ring (`/api/router/v1/traces`);
  `/api/router/v1/sessions`. Connect time is not measured (no client hook).
- **Node request ids** (`lightweight_gateway::request_id`): the gateway logs a
  forwarded `X-Request-Id` on its accepted/queued/admitted/refused/failed
  lines and a new closing `request finished` line (outcome, ttft, total, queue
  wait, tokens), and echoes it. It never invents one. The router reuses the
  same validity rule.
- **Bug found by a test, fixed before commit:** a thread-scoped log subscriber
  in the shared test binary missed router lines intermittently (tracing's
  per-callsite interest cache races across test threads). The log-correlation
  test now has its own binary (`tests/request_correlation.rs`) with a global
  subscriber; 5/5 and 6/6 repeat runs green.

Verified by execution against two real gateways (`target/debug/hermes serve`,
SmolLM2-135M, `--ctx 2048 --concurrency 2`), each in its own scratch
`XDG_DATA_HOME` with catalog aliases `QwenCoder` (A, :18501) and `CoderBackup`
(B, :18502), behind the real `hermes router` on :18500 with a round-robin
`Coder` and affinity on:

- One session, four streamed turns: all on A (`session_affinity`), while
  sessionless requests rotated A, B, A — hits drew no cursor value. TTFT fell
  from 1420 ms to about 66 ms on repeats, but node A was equally warm for a
  sessionless repeat of the same prompt: that is llama.cpp's slot prompt
  cache, not something affinity can claim.
- A stopped by its PID: the next turn tried A (connection refused), failed over
  to B in the same request, and the session moved (`sticky_failed`). A
  restarted; with both nodes healthy (`available 2`) three more turns stayed on
  B while sessionless traffic reached A again.
- Failover between two live, logging nodes under one id: A's alias was renamed
  through A's own scratch control API, so A answered `404 model_not_found`. A's
  `gateway.log` has `request refused request_id="smoke-failover-1" status=404`,
  B's has `generating` and `request finished` with the same id, and so does
  the router's stderr. The alias was restored.
- `request_id="smoke-s1-1"` appears in the client's response header, the
  router's `routed` / `request finished` lines and node A's `generating` /
  `request finished` lines (node TTFT 1414 ms against the router's 1420 ms).
- A client leaving a 1500-token stream after 4 s: router trace `cancelled`
  (status 200), node A `request finished outcome="cancelled"` under the same
  id, router active requests back to 0.
- Estimator, first real numbers: one-line prompts estimated 4 and counted 36
  (template markup dominates; the ratio ladder was widened to 32 because of
  it); a Lightagent turn with tools estimated 124 and counted 160.
- **Unmodified Lightagent `7d95232`** in an isolated `LIGHTAGENT_HOME`:
  `lightagent models` printed `Coder`, a chat streamed an answer under a
  router-generated `rtr-…` id that node B logged, with no session (plain
  round-robin). Its tree is unchanged.
- The user's own gateway on 11434 was not touched; aliases were set by editing
  the scratch catalogs so no alias command probed it. The scratch processes
  were stopped by their recorded PIDs.
- **Validation.** `./scripts/check.sh` passed in full: 1118 workspace tests, 0
  failed (1070 at R5). The router has 192 (121 unit, 70 surface integration, 1
  log-correlation binary); the gateway adds 2. Contract suite 47 passed, 2
  skipped. Cross-platform proof is the PR's CI.

- **`routing_ms` kept its R5 meaning** (review correction). R6 had moved its
  start to the request's receipt, folding body parsing in. It starts again
  after the body is parsed; TTFT and the request duration still start at
  receipt. Planning that fails still records its time (an unknown route under
  `_unknown`/`none`). Proven with test-only pauses before and inside the
  planning window; with the R6 boundary put back, the new test fails
  (`routing_ms 301.75 includes the pre-planning pause`).

**Next:** review of this branch. R7 (placement / warm standby) is not started.

## Router placement and warm standby, R7 (feature/router-placement-controller)

R6 was merged first (PR #34, merge commit `dace9df`) and master validated:
`./scripts/check.sh` 1120 tests, 0 failed; all eight CI jobs green (the
three Linux jobs needed re-running after a GitHub Actions incident in which
no hosted runner picked them up — no step had run). A post-merge smoke test
(Lightagent → router → two scratch nodes) passed. R7 is branched from that
master. R8 not started.

What the nodes actually offer, read before any code: `POST
/api/v1/models/{id}/load` (empty body accepted) starts a **job** and answers
`202 {"job": n}`; `GET /api/v1/jobs/{n}` reports `status.state`
`running|succeeded|failed|cancelled`, a failure carrying the structured
`error.code`; `GET /api/v1/models` lists the catalog with `state`
`loaded|available|missing`. A load runs the node's own admission
(`insufficient_memory`), and on a node already serving a model it is a hot
swap (pause, drain, replace) — one model per gateway. The `/api/v1` surface
takes the node's ordinary key (keys are unscoped), and a client sending no
`Origin` passes its cross-origin guard. So R7 loads only onto healthy
**empty** nodes and never swaps.

- **Domain/config:** `routes[].placement {min_ready, warm_standby,
  allowed_nodes}` (`RoutePlacement`), top-level `placement {interval_secs,
  load_timeout_secs, backoff_secs, backoff_max_secs}`. Unreachable targets,
  unknown or duplicate allowed nodes, and bad timings are refused at startup.
- **Planner** (`placement.rs`, pure): deployment states ready / loading /
  empty / occupied / unavailable; ready counted over every deployment of the
  route; one load per node, never more than the shortfall, configured order.
- **Controller** (`controller.rs`): the loop (interval, a finished load, or the
  reconcile endpoint), loads as owned tasks: catalog check → load job → job
  polled → node probed until available by the request path's rule. Failure
  reasons from the node's codes; doubling bounded backoff.
- **Request path untouched.** `proxy.rs`, `select.rs` and the policies are not
  changed by R7.
- **Bug found by a test:** a real node with no alias advertises
  `<id>@<ctx>`, so a deployment named by bare catalog id never becomes
  available (the pre-existing R0 rule). The controller reports it as
  `load_timeout` / `not_ready` rather than retrying blindly; the tests name the
  alias, as real deployments do. The rule itself is unchanged.
- **Mutation check:** with backoff disabled, the admission test saw 33 load
  attempts instead of 1.

Verified by execution against three real gateways (`target/debug/hermes
serve`, SmolLM2-135M), each in its own scratch `XDG_DATA_HOME`: node A started
with the model (`QwenCoder`), nodes B (`CoderBackup`) and C (`CoderStandby`)
started **empty** with the model installed, behind the real `hermes router`
with a priority `Coder`, `min_ready 1, warm_standby 1`, all three allowed:

- At start the controller saw 1 of 2 ready and loaded **B** through B's control
  API: ready, confirmed by probe, in 1540 ms. C was left empty.
- A stopped by its PID: the next request tried A, failed over and was answered
  by B in the same request (2.14 s, spent on A's attempt while it shut down; no
  load was involved). The controller then loaded **C** — C's own log shows its
  `admission verdict` (SAFE, 687.9 MiB) and `model loaded` — and counted it
  ready after 6723 ms. The route was `satisfied` again, with A `unavailable`.
- **Unmodified Lightagent `7d95232`** listed `Coder` and streamed a chat
  (answered by B, no session); placement is invisible to it. Its tree is
  unchanged.
- The user's own gateway on 11434 was not touched. Aliases were written into
  the scratch catalogs directly. Processes were stopped by recorded PIDs.
- **Validation.** `./scripts/check.sh` passed in full: 1146 workspace tests, 0
  failed (1120 on master). The router has 220 (136 unit, 72 surface, 11
  placement, 1 log-correlation). Contract suite 47 passed, 2 skipped.

**Next:** review of this branch. R8 (rule-based `Auto` route) is not started.

## Router rule-based `Auto`, R8 (feature/router-auto-routing)

R7 was merged first (PR #35, head `321a163` re-verified unchanged; merge
commit `b1c7bc6`) and master validated: `./scripts/check.sh` 1146 tests, 0
failed, contract suite 47 passed, 2 skipped; CI run 37384572874 (all seven
`check` jobs, Flatpak, Linux artifacts, render icons) and 37384572855 (render
panel) green. Post-merge placement smoke on real gateways: node A serving,
node B empty with the model installed, `Coder` `min_ready 1, warm_standby 1`
— the controller loaded B through its control API and counted it ready only
after the probe confirmed it (1539 ms); with A stopped, the next request
failed over to B in the same request (`primary_failed_fallback`) and the one
after went straight to B (`primary_unavailable_fallback`). `v0.5.0` (tag
`e0f2baf` → `7c177d0`, eight assets) unchanged. R7 is frozen; R8 is branched
from that master.

What master offered, read before any code: route resolution
(`Topology::resolve`, `alias::ModelSelector`) ran *before* requirements were
read (`requirements::extract`, R5); requirements carry endpoint, tools,
`ToolChoiceRequirement`, reasoning and the bytes/6 prompt bound; affinity is
keyed by the resolved `RouteName`; the response is rewritten to `route.name`
(`sse::FrameRewriter`, `rewrite_body_measuring`); metric labels are only
configured names or fixed reasons. So `Auto` needed exactly one change in the
request path: for an `Auto` request, read the requirements first and let the
rules name the route — everything after that already keys on the route.

- **Domain/config** (`auto_route.rs`): `auto_route {enabled, fallback_route,
  rules[{name, when, route}]}`, off unless `enabled`. `AutoCondition`
  (`endpoint`, `requires_tools`, `tool_choice`, `requires_reasoning`,
  `min/max_prompt_tokens`) is ANDed; `false` means absent; first match wins;
  no match is the fallback. Named `fallback_route`, not `default_route`, which
  already means `model: "default"`. Validation (on or off) refuses unknown,
  reserved or `Auto` targets, a route named `Auto`, duplicate or label-unsafe
  rule names (`_fallback` reserved), condition-free, zero-threshold and
  unsatisfiable rules, and more than 64 rules. No section: no change at all,
  and a legacy route called `Auto` still works.
- **Request path** (`proxy::resolve_auto`): requirements read once and reused
  for capability filtering; a request the gateway would refuse is refused
  before any route is chosen (counted under the fixed `Auto` label). The chosen
  route goes through the unchanged pipeline; no other route is ever tried.
- **Identity:** `response.model` is the resolved route in bodies, every stream
  chunk, the usage chunk, tool answers and the route's own errors. `/v1/models`
  lists `Auto` with no context fields; `/v1/capabilities` adds
  `auto {id, router_resolved, routes}` and does not touch `routes` or the
  top-level features.
- **Observability:** trace `requested_route` / `auto_rule` / `auto_fallback`
  beside `route`; the same on the `routed` and `request finished` lines; one
  `auto route resolved` line with the traits (no prompt);
  `router_auto_route_decisions_total{rule,route}`,
  `router_auto_route_fallback_total{route}`; `GET /api/router/v1/auto`.
- **Mutation check:** making no rule ever match fails 10 of the 14 integration
  tests, ignoring `enabled` fails 1, dropping `requested_route` fails 3.

Verified by execution against two real gateways (`target/debug/hermes serve`,
SmolLM2-135M, scratch `XDG_DATA_HOME`s): `General → node-a/QwenCoder`,
`Coder → node-b/CoderBackup`, rule `tools → Coder`, fallback `General`:

- `/v1/models` listed `General, Coder, Auto`; `validate-config` printed
  `auto  Auto  on -> tools -> Coder, otherwise General`.
- `smoke-r8-ordinary` and `smoke-r8-stream` (`Auto`, no tools) resolved by
  `_fallback` to `General` and appear only in node A's `gateway.log`;
  `smoke-r8-tools` (`Auto`, one tool) resolved by `tools` to `Coder` and
  appears only in node B's. Every stream chunk said `General`; no alias
  reached the client. A direct `Coder` request traced `requested_route=Coder`.
- **Unmodified Lightagent `7d95232`** (isolated `LIGHTAGENT_HOME`, the scratch
  profile's model set to `Auto`) listed `General, Coder, Auto` and streamed a
  chat. Lightagent sends its tool set on every turn, so its request traced
  `requested_route=Auto, route=Coder, auto_rule=tools` (estimate 123, node 160)
  and reached node B; a tool-less Lightagent request is not possible, so the
  ordinary path was shown with the plain requests above. Its tree is unchanged.
- The user's gateway on 11434 was not touched; processes were stopped by PID.
- **Validation.** `./scripts/check.sh` passed in full: 1184 workspace tests, 0
  failed (1146 on master). The router has 258 (159 unit, 72 surface, 14 Auto
  integration, 12 placement, 1 log-correlation). Contract suite 47 passed, 2
  skipped. Cross-platform proof is the PR's CI.

**Next:** review of this branch. R9 (learned/adaptive routing, mixture of
agents) is not started.

## Router content-aware classification, R9.1 (feature/router-content-classification)

R8 was merged first (PR #36, head `e27c201` re-verified unchanged; merge
commit `036bad0`) and master validated: `./scripts/check.sh` 1184 tests, 0
failed, contract suite 47 passed, 2 skipped; CI run 37390160690 (seven `check`
jobs, Flatpak, Linux artifacts, render icons) and 37390160581 (render panel)
green. Post-merge Auto smoke on real gateways: placement loaded an empty B
(1532 ms, probe-confirmed); ordinary Auto → `General`, tool Auto → `Coder`
round-robin B/A/B, a session settling on one Coder deployment (miss, hit,
hit) with its affinity under `Coder`; `response.model` the resolved route
every time. `v0.5.0` unchanged. R8 is frozen. R9 is split into R9.1–R9.4, each
gated by review; the umbrella branch `feature/router-adaptive-orchestration`
and this branch both start at `036bad0`.

What master offered, read before any code: `proxy::resolve_auto` reads the R5
requirements and runs `AutoRoute::decide`; requirements carry no message text
(by design); the gateway has no `response_format` or grammar option, so a
classifier's output cannot be constrained at decode time — it must be parsed
strictly and checked against the candidates. So the classifier is a
configured route, called through the router's own pipeline, and its answer is
a recommendation the router validates.

- **Domain/config** (`classifier.rs`, `auto_route.rs`):
  `auto_route.classifier {route, routes, fallback_route, min_confidence,
  timeout_ms, max_input_chars}`; rules gain `"classify": true` (exactly one of
  it and `route`; may have an empty `when`); `routes[].description`. A
  classifying rule's `route` is the classifier's fallback, so `decide` stays
  pure and every R8 type and test is unchanged.
- **Request path:** `resolve_auto` (now async) classifies only when the
  matching rule asks; `proxy::forward_nested` (boxed, `nested = true`) sends the
  classification through the pipeline; a nested request never classifies.
  Classifier time is added to `planning_started`, so `routing_ms` is unchanged
  in meaning.
- **Observability:** trace `classifier {route, outcome, chosen_route,
  confidence, duration_ms, request_id, input_truncated}`, one `auto route
  classified` line, three metrics, `/api/router/v1/auto` classifier block.
- **Mutation check:** never classifying fails 6 of the 8 integration tests,
  ignoring the threshold fails 1, counting classifier time in `routing_ms`
  fails 1.

Verified by execution against three real gateways (scratch `XDG_DATA_HOME`s):
A and B SmolLM2-135M (`General`/`Research` → A, `Coder` → B), C **Qwen3-1.7B**
Q4_K_M at `--ctx 2048` behind `RouterClassifier`; candidates General, Coder,
Research with descriptions; `min_confidence 0.5`, `timeout_ms 120000`; rules
`forced-tools → Coder`, then `semantic` (classify). Every request declared tools:

| Prompt | Resolved | Confidence | Classification |
|---|---|---|---|
| "Hello, how are you?" | General | 0.9 | 43.9 s (first, cold) |
| "Write a Rust async TCP server using tokio." | Coder | 0.9 | 13.8 s |
| "Compare today's current GPU announcements from NVIDIA and AMD." | **General** (intended Research) | 0.8 | 17.0 s |
| "Search the web for current CUDA benchmarks." | Research | 0.8 | 14.7 s |
| "What are the latest news headlines about AI chips this week?" | Research | 0.9 | 13.4 s |
| "Hello, how are you?" + `tool_choice: required` | Coder (rule, no classification) | — | — |

`routing_ms` stayed 0.26–0.37 ms throughout. Node C's log has only the five
`-classify` ids; A has the General/Research requests, B the Coder ones. On
this box classification costs 13–44 s, so the original 1.5 s default would
have made every classification time out and fall back. **Hardening:**
`timeout_ms` is now required whenever a classifier section exists (still
bounded 1–120 000 ms, never unlimited); configurations without a classifier
need nothing. A test proves a timeout cancels the classification upstream
(the scripted classifier never finishes its answer, the nested trace reads
`cancelled`) and the request continues at the fallback with `routing_ms`
unaffected. **Unmodified Lightagent
`7d95232`** (scratch profile model `Auto`) listed `General, Coder, Research,
RouterClassifier, Auto`; both of its turns declared its tool set, and "Hi
there! How is your day going?" resolved to `General` (0.9) while "Write a Rust
function that reverses a string." resolved to `Coder` (1.0), about 13 s of
classification each. Its tree is unchanged; the gateway on 11434 untouched.

- **Validation.** `./scripts/check.sh` passed in full: 1203 workspace tests, 0
  failed (1184 on master). The router has 277 (170 unit, 72 surface, 14 Auto,
  8 classification, 12 placement, 1 log-correlation). Contract suite 47
  passed, 2 skipped. Cross-platform proof is the PR's CI.

**Next:** review of this branch. R9.2 (adaptive route scoring) is not started
and needs explicit approval.

## Router classifier providers and Jev, R9.1a (feature/router-classifier-providers)

The R9 plan gained a phase before scoring: R9.1 → **R9.1a** (classifier
provider abstraction + TypeSafe Jev) → R9.2 → R9.3 → R9.4. Stacked on R9.1
(PR #37, not yet merged), so it is reviewable on its own.

**Stage 0 audit — there was no partial R9.2 work.** No branch, commit, stash,
worktree or uncommitted change anywhere (local or `origin`) contained scoring
code; the umbrella `feature/router-adaptive-orchestration` had 0 commits beyond
master `036bad0`. The only uncommitted work was the interrupted R9.1 timeout
hardening, which was finished and committed to PR #37 first (`e654fd0`:
`timeout_ms` required). Nothing was kept, moved, deferred or removed, because
nothing existed.

What TypeSafe documents (read from docs.typesafe.ai before any code):
`POST /v1/systemone` takes `state`, `model` and a map of typed `questions`; a
Choice has `instructions` and `criteria` (option → description, ≤ 255); its
answer has `choice`, `probabilities` and `confidence` (TypeSafe's own
certainty, `(n·p_max − 1)/(n − 1)` for a Choice); `GET /v1/models` returns
`{models: [{name, description, release_date}]}` listing aliases only — a pinned
versioned id is accepted unlisted; errors are `401`, `422`, `429`, `529`.

- **Structure:** `classifier/{mod.rs, lightweight.rs, jev.rs}`. A closed
  `ClassifierProvider` enum (the crate's own idiom, as `RoutePolicy`), one
  provider-neutral `Classification`, timeout and threshold applied once in
  `classify()` for every provider.
- **Jev:** one Choice whose criteria are exactly the candidates; exact-name
  match; its `confidence` thresholded; `401`/`403` → `auth_error`,
  `429`/`529` → `rate_limited`, other statuses → `provider_error`, transport →
  `connection_error` / `timeout`; bounded body reads; no retries; error bodies
  never kept; no request id sent out.
- **Settings:** `provider` (default `lightweight`), per-provider blocks, R9.1
  flat keys = the `lightweight` block; Jev key via the existing secret reader,
  demanded only when active; https off loopback; `model` and `timeout_ms`
  required.
- **Operator view:** `/api/router/v1/auto` shows both blocks (active/standby),
  `api_key_configured`, last success/failure/check; a background start-up check
  and `POST /api/router/v1/classifier/check` (`GET /v1/models`). The router has
  no settings UI (the frontend and desktop shell never call `/api/router`), so
  this is the operator surface.
- **Mutation check:** 429/529 → provider_error fails 3 tests; not trimming
  `base_url` fails 9; case-insensitive choice matching fails 1.

Verified by execution: **real Jev smoke test skipped — `TYPESAFE_API_KEY` not
configured** on this machine. A keyless `GET https://api.typesafe.ai/v1/models`
(no content sent) answered `403` over verified TLS — the documented table says
`401`; both map to `auth_error`. The Lightweight provider was re-run through
the new configuration shape with Qwen3-1.7B on node C: with `timeout_ms
120000`, "Write a Rust async TCP server using tokio." → `Coder` (0.9, 46.6 s
cold) and "Hello, how are you?" → `General` (0.9, 12.7 s), `routing_ms` 0.31;
with `timeout_ms 2000`, the classification timed out at 2001 ms, its nested
trace reads `cancelled`, node C's own log records `cancelled` for
`r91a-short-classify`, and the request was answered by `General` with
`routing_ms` 0.3.

- **Validation.** `./scripts/check.sh` passed in full: 1223 workspace tests, 0
  failed (1203 at R9.1). The router has 297 (181 unit, 72 surface, 14 Auto,
  9 classification, 8 Jev, 12 placement, 1 log-correlation). Contract suite 47
  passed, 2 skipped. Cross-platform proof is the PR's CI.

**Next:** review of R9.1 (PR #37) and this branch. R9.2 is parked — nothing of
it exists — and needs explicit approval.

## Router classifier UI (feature/router-classifier-ui)

R9.1 (PR #37 → `5627ffa`) and R9.1a (PR #38, retargeted to master and updated
by a merge from master that changed no file → `58688cd`) are merged; master
was validated after each (check.sh 1205 then 1223 tests, contract 47/2, all
eight CI jobs green: runs 37401555264/37401555100 and 37406630315/37406630244).
That backend is frozen; this branch builds on it. R9.2 does not exist.

**What the existing UI was, read before building:** the panel (`frontend/`,
React + Vite, `HashRouter`) is served by `hermes serve --web-root` and calls only
its own gateway's `/api/v1/*`, same-origin by design (no CORS, no base URL).
Nine screens; forms are `.field`/`.input` with inline notices, errors are
`ApiError` + `ErrorState`, polling is `usePoll`. The router is a separate
process whose `/api/router/v1/*` the panel could not reach, and it has no
config write path — `router.json` is read once at start.

**Decisions (the user's):** the router serves the panel itself, and saving is
a validated snippet plus a proposal, not a new write API.

- **Backend (additive, opt-in):** `hermes router --web-root <dir>`;
  `lightweight_gateway::web::serve_root` shared so the router uses the same
  path whitelist and cache rules; unknown `/api` or `/v1` paths stay JSON
  `not_found`; panel files need no key, the API keeps its own;
  `GET /api/router/v1/routes` gains `description`. Nothing in the classifier
  contract changed.
- **Panel:** `GET /version` (`build` prefix `lightweight-router-`) chooses the
  router's sections, Auto Routing and Classifier; a gateway's panel is unchanged.
  Classifier = provider status + Test Connection (real
  `POST /classifier/check`), a draft seeded from `/auto` with a provider
  selector and only that provider's fields, the Jev privacy notice,
  include-user-text explained both ways, candidates, descriptions, fallback,
  and the canonical `auto_route.classifier` snippet (never the R9.1 shorthand,
  never a key). Field rules in `classifierModel.ts` mirror the router's.
- **Not built:** Test Classification (no backend endpoint exists; adding one is
  a backend change), a model *list* from discovery (the check endpoint returns
  only `model_listed`), and any write of `router.json`.

**Proposed separately — the smallest safe config write:** `PUT
/api/router/v1/classifier` on a loopback-only router (refused when any listener
is exposed), body = exactly the `auto_route.classifier` section, validated by
the same `classifier::validate` against the running routes, written atomically
(temp file + rename, keeping a `.bak`) to the file the router loaded, answering
`restart_required: true`; never accepting a key, only `api_key_env`. Route
descriptions would be a second, equally narrow endpoint.

Verified by execution: `scripts/render-panel.sh` locally (scratch ports — the
user's own gateway is on 11434): the gateway panel's 10 checks unchanged, and
47 router-panel checks against a real `lightweight router --web-root` with Jev
pointed at a scripted TypeSafe (`e2e/mock-jev.mjs`): Test Connection → real
router → `Connected`; every other status rendered in words; validation;
provider switching; the per-run key absent from DOM, storage and all 29
response bodies.

- **Validation.** `./scripts/check.sh`: 1228 workspace tests (+5 router
  `tests/panel.rs`), 26 frontend unit tests (`npm run test`, run by the build),
  contract 47/2. `.gitignore` gains `/e2e/node_modules` and `/e2e/screens`: a
  local `npm ci --prefix e2e` otherwise tripped the secrets gate.

**Next:** review of this branch (not merged). R9.2 needs explicit approval.

**Merged and frozen.** PR #39 → `c493ce7` (head `b8a29c3`, unchanged since
review). Master validated: check.sh 1228 tests, frontend 26, desktop 29,
contract 47/2; CI run 37413886272 (all seven check jobs) and render panel
37413886286 green. Post-merge smoke against a real `lightweight router
--web-root`: gateway-only paths redirect to Auto Routing; Auto Routing and
Classifier load; provider switching; Jev settings, candidates, fallback and
descriptions render; Test Connection reached the router and returned a real
`model_not_listed` for the pinned `jev-1.13.0`, shown with the pinned-version
caveat; snippet, `validate-config` and restart guidance visible; the per-run
key in neither the DOM nor any of 16 responses. R9.1 + R9.1a + the classifier
UI are frozen.

## R9.2 adaptive route scoring — design only

Design in [R9_2_ADAPTIVE_ROUTE_SCORING.md](R9_2_ADAPTIVE_ROUTE_SCORING.md); no
code. Findings that shaped it: classifiers return one verdict (route +
confidence), never a distribution; Jev's documented `probabilities` are
parsed and discarded today; every metric is cumulative since start and
`router_requests_total` records at response head (a broken stream stays `ok`),
so history needs its own bounded, decayed per-route record taken at
`Tracker::finish`. Proposed slice 1: classifier verdict + threshold anchor
(R9.1 restated, so neutral weights reproduce R9.1 exactly) + operator priors
+ gated, shrunk, time-decayed success history; off by default; deterministic
tie-break; no latency, context-fit, persistence or UI yet.

**Next:** review of the design. Implementation (`feature/router-route-scoring`)
needs explicit approval.

## Next step

M10 is complete, and with it the approved plan M0-M10. Stated exactly:
**calibration is measured, wired, observable, and safely refused when its model
is invalid. On `b10590` the shipped compute shape prevents any honest fit from
being applied; correcting that shape is successor work.**

**What M10 leaves behind is a design problem, not a pending measurement.** The
calibration path is built, wired into all six estimator sites, and enforcing
rules that numbers rather than arguments put there. What it uncovered is that
`Estimator::compute_bytes` has the wrong *shape* for this engine, not merely
mis-set coefficients: it scales the logits term by `n_ubatch`, and the entire
measured slope is a fifteenth of that one term. A milestone that only ran
another sweep would not touch this. Closing it takes three things, in order:

- **A model correction.** Logits are the largest term in the shipped compute
  model and the one the measurement contradicts. Whatever replaces it is a
  change to the estimator's structure, and it needs its own argument about what
  the engine actually allocates — not a coefficient fitted to make the number
  come out.
- **Validation against a second engine build**, to tell an llama.cpp convention
  from a general one. Every number in this milestone comes from `b10590`.
- **Validation on a second machine**, which is what the whole file format exists
  for. A fit is machine-scoped by design and `Calibration::find` refuses a
  mismatch, so nothing about the shape can be separated from this Pentium Silver
  until something else has measured it.

Until all three are done the shipped guesses stand and over-estimate by 1.37× to
2.85×, which is the direction `ComputeModel`'s own doc comment says to err.
Correcting the shape on this machine's evidence alone would be calibrating
against one machine, one engine and one model — the confident wrong number this
design was built to refuse.

**What the product is today, said plainly.** Flexible across machines by safe
fallback and local self-calibration: it estimates conservatively everywhere, it
can measure the machine it is actually running on, and it refuses its own
measurement when the model behind it does not hold. It is *not* universally
adaptive optimization, and no part of this milestone should be read as claiming
that. The successor work has to stay engine-shape-aware — the defect it exists
to fix is a structural one in the compute term, not a coefficient — and it has
to be validated on several **runtime** machines. Every fingerprint in this
design is read where the engine runs, never from whichever machine compiled the
build, and the successor's evidence must come from the same place.

The deferrals recorded through M7 stand, with two closed by this milestone:
`ResourceSnapshot.cpu_percent` is resolved — it became a counter — and Run
Benchmark is no longer deferred. `benchmarks/` is no longer empty: it holds the
workload definitions, deliberately not results.

M8 adds its own, each chosen rather than forgotten:

- **`--numa` is not exposed.** It needs a second NUMA node to mean anything,
  wrong settings degrade silently, and llama.cpp requires it to agree with how
  the process was launched. Nothing on this machine can exercise it, and a knob
  validated nowhere is a knob that breaks somewhere else.
- **`--threads-batch` and `--poll` ship with the engine's defaults.** Both have
  a plausible better setting on *some* machine and none that can be established
  on one with four cores and no SMT. Reachable, measurable and unchanged is the
  honest position.
- **`--cache-reuse` stays off.** It is reachable and the harness can measure
  what it buys; enabling it by default trades output fidelity through KV
  shifting, which no estimate judges — the same argument M7 used against a
  stored default KV cache type.
- **A gateway-taken benchmark and a CLI-taken one share a fit key only because
  `BackendCapabilities` now states the engine build.** Before that they could
  never have been compared, which was found by running it rather than by
  reading it.
- **The fit separates a slope from an intercept and nothing finer.** Two of the
  estimator's coefficients are collinear in peak RSS. A later pass that wants
  them apart needs a second observable, not more samples.

M10 adds three, each with the measurement that would close it:

- **The compute term's shape is not corrected.** Logits are scaled by
  `n_ubatch` and the engine does not appear to allocate them that way — the
  whole measured slope is a fifteenth of that one term. Correcting it needs a
  second engine build to tell a convention from a coincidence. Until then
  `SlopeBelowLogits` refuses every fit on llama.cpp `b10590`, which is a refusal
  in the safe direction rather than a wrong number.
- **The bucket key is not widened, though the residual is flat in `n_ctx`.**
  0.48 MiB across a fourfold context range is a strong hint that a fit could
  travel between contexts, and M10-PLAN section 4.3 says only a measurement that
  sets out to test that may widen the key. This one did not: it varied context
  at one batch size on one model. The narrow key stands.
- **95% is policy informed by one machine, not a number that machine derived.**
  The sweep proves 0.79 must fail; it does not single out 0.95 from any other
  bar above 0.79. The margin is chosen for the asymmetry — passing buys
  permission to budget *less* memory than the shipped guess — and its doc
  comment says exactly this rather than implying a derivation. A second machine
  scoring between 0.79 and 0.95 lands in the range nothing has evidence about,
  and is the reason to revisit it with that machine's data.

## Router adaptive route scoring, R9.2 slice 1 (feature/router-route-scoring)

Branched from validated `master` (`c493ce7`), with the design commit of the
docs-only PR #40 cherry-picked so the design doc it updates is on the branch
(a no-op if #40 merges first). R9.1, R9.1a and the classifier UI are untouched;
R9.3 and R9.4 are not started.

**The invariant.** Scoring ranks logical routes only. It is one pure
function, `scoring::decide`, called at exactly one place
(`proxy::resolve_auto`). It runs only for a classification and only while
`auto_route.adaptive_scoring.enabled`, and its whole output is one route
name. `select.rs` and the deployment pipeline are unchanged.

**The seven locked decisions, as built** (full table:
`docs/R9_2_ADAPTIVE_ROUTE_SCORING.md` section 0):

- **Hard threshold boundary.** R9.1's `outcome` is the input. A
  `low_confidence` verdict is `below_threshold`: the fallback is the decision,
  and the rejected route is in the trace but never contends.
- **Contenders.** Only an accepted verdict contends, against the classifier
  fallback, whose signal is the explicit `scoring::classifier_baseline`
  (= `min_confidence`). Other candidates have no signal and are not scored.
  Ties go to the verdict.
- **Score.** `Wc·signal + Wp·prior + Wh·history`, with defaults 1/0/0, which
  reproduce R9.1 exactly.
- **Dominance bound.** The influence radius `(Wp + 2·Wh)/Wc` must be
  `< (1 − min_confidence)/2` for every configured provider block (active and
  standby), so the upper half of the accepted range is never overturned.
  This replaces the design's `Wp + 2·Ws < Wc·min_conf`, which bounded only
  unnamed candidates.
- **One weight set.** There are no provider weights; the provider is trace
  metadata only.
- **History.** One record per configured route, in memory. Decayed
  successes and failures (half-life 1 h by default, configurable, documented
  as provisional), gated to neutral below `min_samples` 20, shrunk
  `(s + k/2)/(n + k)` with `k = shrinkage_samples` (default = `min_samples`),
  `h = 2ŝ − 1`.
- **Recording.** At `Tracker::finish`, so a stream counts when it ends. All
  routes and all traffic count except nested classification requests.
  `ok` is success; `server_error` and `interrupted` were scored failures
  (**superseded by the hardening below: now observed, never scored**);
  `route_unavailable` and all-502/503/504 refusals count as `unavailable`;
  `route_capability_mismatch` as `mismatch`; client errors and cancellations
  as `neutral`. Only success and failure are scored.
- **Reset.** `POST /api/router/v1/adaptive-scoring/reset` (router key):
  everything, or `{"route": …}`. It touches history only.
- **Observability.** A `scoring` trace block, `adaptive_scoring` in
  `GET /auto`, the `validate-config` summary line, and five low-cardinality
  metric families.

**Verified by execution.**

- *Mutation testing* (scratch script, each mutation reverted from git): 12 of
  12 deliberate breaks were caught. They were: low-confidence verdict allowed
  to contend (5 tests failed); dominance bound not validated (3); no
  shrinkage (2); no `min_samples` gate (10); `route_unavailable` scored as a
  failure (3); all-503 refusal scored as a failure (1); mismatch scored as a
  failure (1); an off section still scoring (1); ties to the fallback (4);
  nested requests counted (1); no decay (5); a non-candidate allowed to
  contend (12).
- *Real smoke*, run on the real `hermes router` binary. General was a real
  `hermes serve` with SmolLM2-135M; Coder, Research and the classifier were
  scripted stdlib-Python nodes answering `PICK <route> <confidence>`. Weights
  were prior 0.05 and history 0.06 (radius 0.170), with prior General 0.2.
  History was fed by 30 direct General requests (real generations, 30/30
  `ok`) and 30 direct Coder requests (scripted 500s). Results:
  - `PICK Coder 0.95` went to **Coder**: 0.914 against General's 0.696.
  - `PICK Coder 0.70` went to **General**, answered by the real model: 0.666
    against 0.696, `overrode: true`.
  - `PICK Coder 0.40` went to **General** with `below_threshold`,
    `rejected_route: Coder` and no candidates.
  - The admin view showed General n 31.8 / signal +0.614 and Coder n 30.9 /
    signal −0.568.
  - A reset without the key got 401. Resetting `coder` cleared only Coder;
    a reset-all cleared every route. The counters stayed monotonic and the
    gauges went to 0.
  - All smoke processes were stopped by PID. The user's gateway on 11434 was
    untouched.
- *Jev.* No `TYPESAFE_API_KEY` exists on this box, so real Jev was not
  called. Provider parity is proven against a scripted TypeSafe server:
  identical winners and reasons for both providers. The scripted server
  sends a contradicting `probabilities` map, which is ignored.

**Deliberately not built:** latency, context-fit or availability scoring; Jev
per-option probabilities and provider calibration; persistence; exploration
or learned weights; cross-route fallback (R9.3); MoA (R9.4); UI; and route
capability declarations.

- **Validation.** `./scripts/check.sh`: 1286 workspace tests (from 1228) (router
  360: 226 unit including 45 scoring, plus 13 `tests/route_scoring.rs`);
  contract 47/2. The first run hit the known `lightweight-backend-llamacpp` supervision load flake (`an_illegal_instruction_…`: `engine_start_timeout`), in a crate this branch does not touch; it passed 3/3 in isolation, and the full rerun was green.

**Next:** review of this branch (not merged). The UI follow-up and R9.3 each
need explicit approval.

### Pre-merge hardening: observation is not scoring

Review found the gap: a committed 500 counted as a route failure. But 500 is
never retried, so it is one deployment's answer. Its sibling deployment
might have answered, and the router cannot tell route quality from deployment
quality. The same holds for `interrupted`: a relay body error, or one node's
stream ending without finishing.

- **Now:** only `ok` is scored. `server_error` and `interrupted` join
  `unavailable` (including the all-502/503/504 refusal), `mismatch` and
  `neutral` as observed-only counters, shown in the admin view and in
  `router_route_history_observations_total`.
- `effective_samples` counts scored successes only, so 1 success plus 19
  server errors is 1 sample and stays gated. `history = n/(n+k)` is in
  [0, 1).
- The radius bound is unchanged (it keeps `2·W_history` and is now
  conservative). The threshold boundary, baseline, priors, min samples,
  shrinkage and decay are unchanged.
- Consequence, documented: history now measures recent successful volume, so
  a busier route accrues more positive history within the bounds.
- No proxy, retry, policy, health, affinity or placement code changed.
- **Tests:** 7 new (4 unit, 3 end to end), several updated. Covered: one 500
  leaves the signal unchanged and is counted; 10 000 500s stay neutral;
  interrupted streams stay neutral; 1 success + 19 server errors stays gated;
  a round-robin route with one always-500 deployment answers 21 × 500 (single
  attempt, no failover) and 21 × 200, and its signal equals 21 successes'
  21/41.
- **Mutations:** 7 of 7 caught: 500 scored as a failure (9 tests failed),
  interrupted scored as a failure (6), 500 counted toward samples (11),
  plus re-checks of the hard boundary (5), the dominance bound (3), the gate
  (14), and unavailable counted as a sample (5).
- **Real smoke**, on the real `hermes router` binary with a `round_robin`
  Coder of a broken and a healthy scripted deployment: 42 requests gave
  21 × 500 (each 1 attempt on `broken`) and 21 × 200. History showed
  `server_error` 21, `effective_samples` 21.00 and `value` 0.5122 (= 21/41).
- **Validation:** `./scripts/check.sh` green on the first run: 1293 workspace tests (from 1286; router 367), contract 47/2.

### Final pre-merge hardening: history is observational only

Review found what scoring only successes leaves: a measure of **successful
traffic volume**, not route quality. A route picked more often succeeds more
often, wins more borderline decisions, and draws more traffic. Popularity
must never masquerade as quality, so in slice 1 history does not choose
routes at all.

- **Score:** `W_classifier·signal + W_prior·prior` only. The trace says
  `history_active: false` and `history_signal: 0`; `total_score =
  classifier_signal + prior_signal`.
- **Validation:** `weights.history` must be 0. The key is kept for a later
  phase; any other value is refused with "weights.history must be 0:
  adaptive route history is observational only in this release (R9.2
  slice 1). Its observations measure successful traffic volume, not
  route-attributable quality, so they must not choose a route".
- **Radius:** `W_prior / W_classifier`, with no phantom `2·W_history` term;
  still `< (1 − min_confidence)/2` per provider.
- **History kept as telemetry.** Recorded, decayed, resettable, and shown as
  observations (`effective_samples`, `successes`, `min_samples_reached`,
  outcome counters) in traces, `GET /auto` (`history_mode:
  "observational"`, `history_affects_scoring: false`) and
  `router_route_history_observations_total` /
  `router_route_history_effective_samples`. The quality-looking
  `router_route_history_signal` gauge and the `value`/`gated`/`success_rate`
  fields were removed before release. The `n/(n+k)` estimator stays in code
  and tests as the foundation for a future route-attributable quality signal.
- **Tests:** router 372 (from 367). Added or rewritten:
  - zero history weight valid; 0.0001, 0.01, 0.06 and 1.0 refused, and also
    while off and when negative;
  - the radius counts only the prior; a decision moves by exactly the radius;
  - 10 vs 10 and 10 vs 10 000 successes (both directions) give identical
    winners and scores over a 351-step sweep, and the telemetry still shows
    10 and 10 000;
  - decay halves the observations and never changes a winner;
  - an exact-threshold decision and a tie are unaffected by history;
  - no observed outcome moves the score;
  - the trace shows history inactive;
  - end to end: 100 vs 5 successes and the reverse give the same winners;
    the admin view shows the observational mode and no quality fields; no
    signal gauge.
- **Mutations:** 5 of 5 caught:
  - non-zero history weight accepted (2 tests failed);
  - history reconnected to the score (12, including both 10-vs-10 000 tests);
  - low-confidence verdict contends (5);
  - the radius regaining a history term (17);
  - the trace claiming history active (2).
- **Real smoke** on the real `hermes router` binary (scripted General, Coder
  and classifier nodes; prior 0.1, priors General 1.0):
  - `validate-config` refused `history: 0.01` with the message above and
    accepted `0`.
  - With General at 100 successes and Coder at 5, Coder verdicts 0.65, 0.70
    and 0.74 went to General, and 0.76, 0.80 and 0.95 went to Coder.
  - After a reset with popularity reversed (Coder 100, General 5), the
    winners were identical.
  - The traces showed `history_signal 0` and totals 0.95 against 0.75; the
    admin view showed the counts with `history_affects_scoring: false`.
  - The processes were stopped by PID.
- **Validation:** locally, fmt, workspace clippy and the router and CLI
  tests. Build, render and the full `check.sh` ran on GitHub Actions, per the
  user's instruction, against `f670109`, and were green:
  - check run 37460150610: Linux x64, Windows x64, macOS x64, macOS arm64,
    Flatpak, Linux artifacts and render icons;
  - render panel run 37460150606.

  Linux ran 1298 workspace tests (from 1293) with 0 failed; Windows ran
  1268, and macOS x64 and arm64 1273 each (platform-gated tests). The
  contract suite was 47 passed, 2 skipped.

### R9.2 slice 1 MERGED and FROZEN

PR #41 was merged as `ef4f868` (reviewed head `f3d16aa`, merge commit, head
pinned; the tree is identical to the reviewed head). PR #40, design only, was
closed as superseded: its design commit is in #41.

Master was validated:
- local `check.sh` green, with 1298 workspace tests and contract 47/2;
- Actions check run 37470607756 (Linux x64, Windows x64, macOS x64, macOS
  arm64, Flatpak, Linux artifacts, render icons) and render panel run
  37470607758, all green;
- no release workflow ran.

Master smoke on the real `hermes router` binary passed all seven checks:
1. `weights.history: 0.01` was refused with the observational-history error.
2. `0` was accepted.
3. A borderline sweep gave General below 0.75 and Coder from 0.76.
4. The same winners held with General at 120 successes and Coder at 5, and
   with that popularity reversed.
5. Coder at 0.40, 0.60 and 0.649 stayed rejected (`below_threshold`, no
   candidates), despite a maximum Coder prior and 150 Coder successes.
6. The admin view showed `history_mode: observational`,
   `history_affects_scoring: false` and `weights.history: 0`, with the
   observations visible.
7. Reset of one route and of all routes worked and needed the key (401
   without it). Routes, sessions, placement and the `Auto` configuration
   were byte-identical before and after (hashes compared), and the Coder
   session's affinity survived.

**Frozen definition:** the R9.1 classifier result feeds the classifier
signal plus a bounded operator prior, which choose the logical-route winner.
Route history is observational telemetry only: `weights.history` must be 0,
and a below-threshold verdict is never resurrected. Any change to R9.2 now
needs explicit approval. R9.3 is at the design stage only.

## Router cross-route fallback, R9.3 (DESIGN ONLY: docs/r9.3-cross-route-fallback-design)

`docs/R9_3_CROSS_ROUTE_FALLBACK.md`. There is no code, configuration,
metric, trace field or UI. The facts were read from `master` (`ef4f868`):

- **Qualifying outcomes are all uncommitted terminal paths of
  `route_request`:** plan-time `RouteUnavailable`, plan-time
  `CapabilityMismatch`, every attempt failing without an answer
  (`route_unavailable`), and every deployment refusing with 502/503/504 (the
  last node's refusal is returned).
- **Commit is the response head** (`Attempt::Committed`), for streams and
  bodies alike.
- **No request deadline exists**: there is only `connect_timeout` (5 s).
- **Nodes never execute tools**: the only `Command::new` is the engine
  supervisor. So side-effect commit cannot precede response commit.
- **Affinity is keyed by route and session.**

**Recommended first slice** (`feature/router-cross-route-fallback`):
- `auto_route.cross_route_fallback`: flat, ordered, non-transitive lists,
  at most 3 entries;
- `Auto`-resolved requests only; never explicit, `default` or nested;
- triggers `route_unavailable`, `route_exhausted` and
  `route_capability_mismatch`, only after same-route failover;
- configuration order only, with no reclassification, re-scoring, history,
  latency, 500 or mid-stream trigger;
- the response names the serving route, and the trace keeps the initial and
  final routes.

**Final design (approved decisions, section 0 of the doc):**
1. keep `route_exhausted` as a distinct reason;
2. R8 rule routes under `Auto` are eligible, with R5 requirements never
   weakened;
3. an exhausted chain returns the final attempted route's existing error;
4. context overflow is deferred, with no fallback;
5. one shared list per route;
6. no shared deadline: deferred, and the latency risk is documented;
7. `router_requests_total` counts once per request;
8. explicit routes never fall back;
9. side-effect safety is a hard future constraint;
10. the list is selected once and non-transitive (validate the graph, never
    traverse it);
11. `MAX_FALLBACK_ROUTES = 3`, fixed;
12. `model` names the final serving route.

The remaining questions are marked DEFERRED. R9.3.1 is the implementation
slice.

### R9.3 design MERGED and FROZEN

PR #42 was merged as `41d0750` (head `4f26ee7`, docs only: `PROGRESS.md`,
`R9_3_CROSS_ROUTE_FALLBACK.md`, `ROUTER.md`). Master was validated:
- local `check.sh` green, with 1298 tests and contract 47/2;
- Actions check run 37508409588 (all 7 jobs) and render panel run
  37508409592, green;
- no release workflow ran.

**Frozen R9.3.1 definition:**
- `Auto`-selected routes only; never explicit routes;
- one flat ordered list, selected once from the initial route, never
  transitive at runtime (the union graph is still validated acyclic);
- triggers `route_unavailable`, `route_exhausted` and
  `route_capability_mismatch`, only after same-route failover is exhausted,
  and only before response commit (no server-side side-effect commit exists
  today);
- no 500, context-overflow or latency fallback; no classifier re-entry, R9.2
  re-scoring, history influence or placement action;
- `MAX_FALLBACK_ROUTES = 3`;
- `model` names the final serving route;
- an exhausted chain returns the final attempted route's existing error;
- `router_requests_total` counts once per request.

Implementation proceeds on `feature/router-cross-route-fallback`.

## Router cross-route fallback, R9.3.1 (feature/router-cross-route-fallback)

Branched from validated, design-frozen master `41d0750`. It implements the
frozen R9.3.1 definition with no deviation. R9.4 is not started.

- **Configuration** (`fallback.rs`): `auto_route.cross_route_fallback`, a
  map from route to a flat ordered list. Refused at load:
  - unknown, `Auto`, `default`, reserved, empty or classifier-route sources
    and targets;
  - a source `Auto` can never resolve to;
  - an empty list, or more than `MAX_FALLBACK_ROUTES` = 3 entries (a
    constant);
  - a self-reference, or a duplicate (compared ignoring case);
  - a cycle anywhere in the union of lists (DFS). The graph is validated,
    never traversed.
- **Refactor** (`proxy.rs`): the per-route part of `route_request` moved
  verbatim into `attempt_route`. Its uncommitted exits return a
  `RouteFailure` (the route's own error, outcome, observation and fallback
  reason) that the caller concludes exactly as before. All 380 router tests
  passed on the refactor alone, before any fallback logic existed.
- **Fallback loop:**
  - Only for an original `model: "Auto"` (never explicit, `default` or
    nested requests). The initial route's list is copied once and frozen,
    and a fallback route's own list is never read.
  - `FallbackReason` has exactly three variants: `RouteUnavailable` (plan
    refusal, or every attempt failed with no answer), `RouteExhausted`
    (every planned deployment tried, ending on a 502/503/504 refusal, with
    no context overflow on the way) and `RouteCapabilityMismatch`.
  - A committed answer ends the request. A committed attempt consumes the
    request's tracker, so a switch after commit cannot even be written.
  - `Tracker::retarget` resets the per-route trace fields for the next
    route and keeps the per-request ones.
  - Each route left is observed in R9.2 history; `router_requests_total`
    counts once, under the final route.
- **Observability:** a `cross_route_fallback` trace block, `route` on each
  deployment attempt, two metric families, `GET /auto` state, and two log
  lines.
- **Tests:** 8 configuration unit tests and 22 end-to-end
  (`tests/cross_route_fallback.rs`), covering every case in the brief.
- **Mutations:** 12 of 12 caught:

  | # | Mutation | Tests failed |
  |---|---|---|
  | 1 | cycle detection removed | 1 |
  | 2 | committed answers treated as route failures | 1 |
  | 3 | fallback on the first deployment's 503 | 3 |
  | 4 | explicit-route fallback | 1 |
  | 5 | transitive lists | 4 |
  | 6 | classifier re-run | 3 |
  | 7 | a second scoring decision | 1 |
  | 8 | list bound removed | 1 |
  | 9 | 500 as a trigger | 1 |
  | 10 | initial route's affinity reused | 1 |
  | 11 | each route counted in `router_requests_total` | 2 |
  | 12 | initial route reported as `model` | 10 |

- **Real smoke** on the real `hermes router` binary (5 topologies, scripted
  stdlib-Python nodes and classifier), all six passing:
  - A: Auto → Coder (down) → General, `model: General`.
  - B: Coder/A down, Coder/B healthy → `answer from CoderB`, no fallback.
  - C: both Coder deployments refused 503 (each hit once) →
    `route_exhausted` → General.
  - D: an explicit Coder request got its own 503; General was not hit.
  - E: a committed stream broke in-band (`upstream_stream_interrupted`) and
    no second route was tried.
  - F: with `Coder: [General, Reasoning]` and `General: [Research]`, Coder
    then General (down) then Reasoning; Research was hit 0 times.

  Each client request was counted once. A first smoke attempt raced: the
  routers took their one-time probe before the Python nodes were listening,
  and everything showed unavailable. The routers were restarted after the
  nodes were up and the run repeated. Processes were stopped by PID.

**Known limits** (documented): there is no shared end-to-end deadline
(deferred), no explicit-route opt-in, no context-overflow trigger, and no UI.

**Next:** review of this branch (not merged). A UI follow-up and any R9.3.x
or R9.4 work need explicit approval.

### R9.3.1 MERGED and FROZEN

PR #43 was merged as `cf3380b`. The reviewed head was `3c4352e`; before
merging I added `ea8b14e`, a documentation-only fix stating that
`router_requests_total` counts each request once under its **final**
logical route, which a test now asserts. All 8 jobs were green on
`ea8b14e`, and the merged tree is identical to it.

Master was validated:
- local `check.sh` green, with 1328 tests and contract 47/2;
- Actions check run 37556974060 (all 7 jobs) and render panel run
  37556974202, green;
- no release workflow ran.

Real-router smoke A–I on the master binary all passed:
- A: Auto → Coder (down) → General, `model: General`, with trace
  initial/final.
- B: Coder/A down, Coder/B healthy → CoderB, no fallback.
- C: both deployments refused 503 → `route_exhausted` (a distinct reason
  and metric) → General.
- D: explicit Coder got its own 503; General was not hit.
- E: a 500 was returned as is.
- F: a committed stream broke in-band.
- G: Coder → General → Reasoning; Research was hit 0 times.
- H: `model` named the final route.
- I: one `router_requests_total` count under the final route, with
  `router_cross_route_fallback_total` showing the transition.

**Frozen:** an `Auto`-selected route goes through normal same-route
routing. On a qualifying pre-commit route-level failure
(`route_unavailable`, `route_exhausted` or `route_capability_mismatch`), the
request follows the initial route's flat list, and each fallback route uses
normal deployment routing again. Frozen exclusions: explicit routes, 500,
context overflow, post-commit failures, classifier retry, R9.2 re-scoring,
history, latency, placement actions, transitive traversal, and MoA.

## Router cross-route fallback UI (feature/router-cross-route-fallback-ui)

Branched from validated master `cf3380b`, where R9.3.1 is frozen. The panel's
existing **Auto Routing** screen gains three cards, built only from existing
pieces: `Card`, `Pill`, `Row`, `Empty`, `Loading`, the notice and table
styles, the classifier screen's `Field` (now exported), and `CodeBlock` with
its Copy button. There is no new screen, app or backend endpoint, and no
config write API. Traces are read from the existing
`GET /api/router/v1/traces`.

- **`fallbackModel.ts`** (pure, no React). It holds:
  - the triggers, exclusions and `MAX_FALLBACK_ROUTES`;
  - context from the running router: routes, routes `Auto` reaches, and the
    classifier's route;
  - the draft and its validation, mirroring `fallback.rs`, including a cycle
    finder that names the cycle;
  - the canonical snippet;
  - transition and exhaustion totals;
  - fallback traces, with deployment attempts grouped by route so
    same-route failover stays visible as distinct.
- **`CrossRouteFallback.tsx`:** the summary, draft and recent-fallback
  cards. The draft is seeded once from the router, and polling never
  overwrites typing.
- **Tests:**
  - `fallbackModel.test.ts` adds 27 unit tests (frontend total 53, from 26).
  - `e2e/render-router.mjs` adds 31 checks against a real router; the render
    harness's router now has `cross_route_fallback: {"Coder": ["General"]}`,
    and a real exhausted Auto request feeds the counters and the trace.
  - The checks cover the card, scope, bound, triggers, explicit warning,
    exclusions, chain, non-transitive and same-versus-cross help, counts,
    exhaustion, identity and `router_requests_total` help, the trace,
    snippet, Copy (read back from the clipboard), the `validate-config` and
    restart steps, and refusals for a two-route cycle, unknown, classifier,
    Auto, self, duplicate and over-3.
  - Gateway and classifier checks are unchanged.
- **Real UI smoke:** local `scripts/render-panel.sh` (gateway on 18434, the
  user's 11434 untouched) passed 89 checks with 0 failures. The summary and
  trace cards were also captured and reviewed.

### Finalization: a successful fallback rendered live

The render covered only an exhausted list, so before merging, `4415a58`
added a successful one. Two scripted nodes (`e2e/mock-node.mjs`) join the
render router as second Coder and General deployments. They start with no
model, so the exhausted scenario runs exactly as before. The render then
loads them, and a real `Auto` tools request goes Coder (`coder-b` 503,
`route_exhausted`) → General (`general-b` 200, `model: "General"`).

The real panel shows:
- requested Auto, initial Coder, final General, and *Served by General*;
- no exhausted marker;
- the steps Coder then General, with Coder's reason;
- each route's same-route attempt under its own route;
- both reasons on the Coder → General counter, and no new exhausted count.

Lock checks were added for exactly three triggers, the explicit-route and
post-start exclusions, and the identity and `router_requests_total` help.
The render now passes 110 checks (gateway 10, router 100; router was 79).
Frontend unit tests: 53. Nothing under `crates/` changed.

### R9.3 UI MERGED and FROZEN

PR #44 was merged as `49ce10d` (head `4415a58`, merge commit,
`--match-head-commit`). On `4415a58`, Actions check run 37565148063 (all 7
jobs) and render panel run 37565148008 were green.

Master was validated:
- local `cargo fmt --check` and `cargo clippy --workspace --all-targets -D
  warnings` clean;
- Actions check run 37565790532 (`check.sh` on Linux, Windows, macOS x64 and
  arm64, plus Flatpak, Linux artifacts and render icons) green;
- render panel run 37565790560 green, at 110/110;
- no Rust changed since `cf3380b`;
- no release workflow ran.

**Router-panel smoke on master** was the render panel run above: the real
router binary serving the real panel in a real browser. Every item passed:
- the card loads, with the Coder → General chain, max 3, the three triggers
  and the four exclusions (explicit route, 500, context overflow, post-start
  stream);
- the non-transitive help, same-route versus cross-route, `model` help and
  `router_requests_total` help are shown;
- the snippet is generated and Copy is read back canonical from the
  clipboard; a cycle is blocked and named, and every other refusal works;
- the `validate-config` and restart-required steps are shown;
- the counters render, and both the successful and the exhausted live traces
  render;
- the classifier screen passes all its checks, the gateway panel's nine
  screens are unchanged, and the TypeSafe key leaks nowhere.

**Frozen R9.3 UI contract.** On the existing router panel, **Auto Routing →
Cross-Route Fallback** shows:
- Auto-only scope, max 3 fallback routes (at most 4 logical-route attempts),
  the three approved triggers and the explicit exclusions;
- the non-transitive rule, and same-route failover kept distinct from
  cross-route fallback;
- the configured lists, transition counters by reason, and exhausted counts;
- successful and exhausted trace paths;
- `response.model` = final serving route, and `router_requests_total` once
  per request under the final route;
- the validated snippet workflow: draft → frontend validation → canonical
  `cross_route_fallback` snippet → Copy → paste into `router.json` →
  `hermes router validate-config` → restart.

There is no backend config mutation API.

**Operational state:** the R9.3.1 backend and the R9.3 UI are operationally
complete. R9.3.2 (shared request budget) is design only, on
`design/router-shared-request-budget`. R9.4 is not started.

## Router shared request budget, R9.3.2 (DESIGN ONLY: design/router-shared-request-budget)

Branched from validated master `49ce10d`. The only addition is
`docs/R9_3_2_SHARED_REQUEST_BUDGET.md`. There is no runtime code, config,
metric, trace field, admin field or UI.

The timeout inventory was verified in code. On the request path today:
- a 5 s connect timeout per attempt, which resets on every deployment and
  route;
- the classifier `timeout_ms` (required, 1–120000), applied once;
- the node's 600 s queue wait.

The upstream response head, body/stream reads and inference are unbounded,
and there is no overall deadline. A Lightweight node commits a **streamed**
request's head immediately but a **non-streamed** one only after the whole
generation, so a pre-commit budget bounds a non-streamed generation.

Recommended design:
- an opt-in `request.pre_commit_budget_ms` (1000–3600000, 0 refused, no
  default value);
- one monotonic deadline created at `received` in `forward_as` (after the
  body is read, before parse, classification and routing), passed by value,
  and inherited by the nested classifier;
- applies to all client requests, and covers R9.1, R9.2, planning, every
  same-route attempt and every cross-route fallback with no reset;
- caps each wait at min(own limit, remaining);
- stops governing at the response commit;
- `504 request_budget_exhausted`, only when the budget stopped further
  work; otherwise the existing error stands;
- cancellation stays `cancelled`;
- never a routing signal, an R9.2 input or a history observation.

### Design hardening (PR #45, before merge)

The design was restructured into the 43 required sections, with a
frozen-invariants table (I1–I17) at the top. The approved refinements are
now explicit:

- **Causal exhaustion (section 7).** The 504 is returned only when a wait
  was cut before its operation completed, or a start check refused a step
  the frozen rules would have started. Example A (General's
  `route_unavailable` at 29.9 s) stays `route_unavailable`. Example B (still
  waiting for the head at 30 s) is a 504. Nothing is rewritten after the
  fact.
- **Deterministic race (section 8).** Every bounded wait is `timeout_at`.
  Verified in the locked tokio 1.53.1: `Timeout::poll` polls the operation
  before the delay, so a ready operation always wins. An unbiased `select!`
  is forbidden. Ties at a start check count as expired, and ties between
  the classifier and the budget go to the budget. Fake-time tests repeat
  each case 1 000 times.
- **One absolute deadline (section 9).** `RequestBudget { deadline:
  tokio::time::Instant, … }`. Remaining time is derived with
  `saturating_duration_since`; no `remaining_ms` is passed between stages.
- **Pre-commit naming and scope (sections 12–13).** The streamed versus
  non-streamed asymmetry is accepted. A post-commit stream or lifetime
  deadline is a separate future feature.
- **Start check before new work (section 19).** It covers six start points.
  An unstarted attempt takes no lease, no transition, no failover count and
  no trace attempt. `next_unattempted_route` is recorded only for a refused
  route.
- **Timeouts and the budget.** Local limits stay as ceilings, at min(own,
  remaining) (sections 20–22). An unbounded head wait and the node queue get
  the remainder. A budget cut never marks node health.
- **Classifier timeout versus the budget (sections 14–15).**
  - The nested classifier inherits the deadline and its start checks, and
    records no budget metric.
  - On a budget expiry the classifier outcome is `request_budget_exhausted`,
    never `timeout`, and there is no R9.1 fallback.
- **Explicit routes (section 23).** The budget applies; cross-route fallback
  still does not.
- **R9.2 neutrality (section 16).** A cut route gets the `Neutral`
  observation (already what `Observation::of_outcome` returns for an unknown
  outcome). A refused route gets no observation. A completed qualifying
  failure keeps its normal observation.
- **`router_requests_total` (section 34).**
  - Counted once, with a new outcome value `request_budget_exhausted`,
    following the `unavailable` precedent; the HELP text is unchanged.
  - The label is the terminal attempted route.
  - Before any attempt, the label follows existing conventions: `Auto` for
    an Auto request (the `resolve_auto` pre-routing label), or the named
    route for an explicit one. No route is invented, and `_unknown` is never
    used.
- **Upstream cancellation (section 26).** Measuring non-streamed
  cancellation is an acceptance requirement. Waiting for the full
  generation fails the slice.
- **Config and tests.**
  - Absent = disabled, `0` = invalid, bounds 1 000 – 3 600 000 (sections
    28–30).
  - Test plan B1–B45, mutation plan M1–M27, and acceptance criteria in
    section 41.

**Next:** merge PR #45 after a green Actions matrix, validate master, then
freeze. Implementation (R9.3.2 slice 1, section 43) needs explicit approval.
R9.4 is not started.

### R9.3.2 design MERGED and FROZEN

PR #45 was merged as `790bbc1` (merge commit, `--match-head-commit`).
- Previous head: `8465a69`. Reviewed final head: `8d6c26a`.
- The final diff was docs only: this file and
  `docs/R9_3_2_SHARED_REQUEST_BUDGET.md`.
- On `8d6c26a`, Actions check run 37599670526 (all 7 jobs) and render panel
  run 37599670492 were green.

Master `790bbc1` was validated:
- Actions check run 37600945837 was green: `check.sh`, including the secrets
  gate, on Linux x64, Windows x64, macOS x64 and macOS arm64, plus Flatpak,
  Linux artifacts and render icons.
- Render panel run 37600945880 was green.
- Local `cargo fmt --check` was clean, and no Rust changed since `49ce10d`.
- No release workflow ran.

**R9.3.2 design = frozen.** The strongest invariants:
- one client request = one absolute monotonic deadline;
- the budget is pre-commit only, and belongs to the client request;
- the budget is causal, not merely clock-based;
- deadline/error precedence is deterministic (`timeout_at` polls the
  operation first; no unbiased `select!`);
- the budget is checked before starting new work;
- existing per-operation limits remain, capped by the remaining budget;
- the classifier/provider timeout stays distinct from the overall deadline;
- the budget applies to explicit routes too, and explicit routes still do not
  cross-route fallback;
- budget expiry is neutral to R9.2 history;
- same-route attempts share one budget, and cross-route attempts share one
  budget;
- the node queue and response-head wait are bounded by the remaining budget;
- post-commit behaviour is unchanged;
- the streaming/non-streaming asymmetry is intentional;
- `router_requests_total` remains one per request: the terminal attempted
  route when one exists, and no invented route when none does;
- upstream non-streamed cancellation must be measured during
  implementation;
- config absent = disabled, and zero = invalid;
- R9.4 remains untouched.

**Next:** `feature/router-shared-request-budget` is created from validated
master after this record merges. There is no implementation until explicit
approval. R9.4 is not started.

### R9.3.2 metric decision FROZEN: `request_budget_exhausted` outcome

This was approved after the design merged and is recorded in docs only.
There is no runtime change.

`router_requests_total` gains a distinct terminal outcome,
`request_budget_exhausted`. It is never folded into `server_error`:

| Outcome | Meaning |
|---|---|
| `server_error` | an actual server or internal failure |
| `unavailable` | a route or deployment availability failure |
| `request_budget_exhausted` | the configured pre-commit budget was the causal terminal condition |

- **Causal only.** A completed route result is never relabelled because the
  clock passed the deadline. Example A (General `route_unavailable` at
  29.9 s, then the budget expires at 30.0 s) keeps the route's own outcome.
  Example B (General uncommitted, waiting for its head at the deadline)
  counts `request_budget_exhausted`.
- **One count per client request.** It is never counted per deployment
  attempt, same-route retry, cross-route fallback or classifier attempt.
- **Terminal attempted route.** `Auto → Coder → General` with General cut
  counts `route="General"`, never Coder or Auto.
- **Before any route attempt.** The label is `Auto` for an `Auto` request,
  or the client-named route for an explicit one (`model: "Coder"` counts
  `route="Coder"`). A route is never invented.
- **R9.2 history stays neutral.** Exhaustion is unscored, never a
  route-quality failure, never a route-choice input and never a
  fallback-order input.

Recorded in:
- `docs/R9_3_2_SHARED_REQUEST_BUDGET.md`: section 34 (frozen block, outcome
  table, examples), invariant I14, appendix row 14, B30 extended, new
  mutation M28 ("counted as `server_error`");
- `docs/ROUTER.md`: a metrics note marked "not implemented and not emitted
  by this version", and an R9.3.2 roadmap row (design frozen, not
  implemented).

CHANGELOG is unchanged, because it records shipped behaviour only.

**Next:** release v0.6.0 from validated master, with R9.3.2 runtime still
paused. `feature/router-shared-request-budget` stays untouched. R9.4 is not
started.

## Router shared request budget, R9.3.2 slice 1 (feature/router-shared-request-budget)

**Baseline.** master = `v0.6.0` = `a8f0c89`. The branch was at `49935b1`
with **zero** unique commits (`git log origin/master..branch` empty; branch an
ancestor of master) and was fast-forwarded to `a8f0c89`; its tree matched
master's before any change.

**Built** exactly to the frozen design (`docs/R9_3_2_SHARED_REQUEST_BUDGET.md`,
section 44 records the readings of points it left open):
- `budget.rs`: `RequestBudget` (one absolute `tokio::time::Instant`, `Copy`,
  no setter), `bound` = `timeout_at` (operation polled first), `Stage`,
  `BudgetTrace`.
- Config `request.pre_commit_budget_ms` (absent = off; 0, null, <1000,
  >3 600 000 refused); `validate-config` prints it and warns when a classifier
  timeout ≥ the budget.
- `proxy.rs`: made once in `forward_as` at `received`; inherited by
  `forward_nested`; start checks before classification, the initial route,
  each further deployment (before its failover is counted) and each fallback
  route (before its transition is counted); each attempt's connect + head +
  pre-decision body read capped; commit snapshot; causal `504`.
- Classifier: wait = min(provider, deadline); tie → request budget; nested
  `504` recognised by a response-extension marker; new outcome
  `request_budget_exhausted`, not a provider failure.
- Metrics (only when configured), trace block, `router_requests_total`
  outcome, admin `GET /api/router/v1/request-budget`. No UI.

**Tests.** 22 new unit tests (budget 8 incl. 1 000-run race and a source
check that no deciding module reads the budget and no unbiased `select!`
races it; config 9; classifier 5 under paused time), 38 scripted-node
integration tests (`tests/request_budget.rs`, named by design id, incl. a
Linux-only hanging-connect pair), 2 real-gateway tests
(`tests/request_budget_gateway.rs`: 600 s node queue cut at 1 s; non-streamed
cancellation), 3 frontend model tests (B44 tolerance). Every existing test
unchanged and green.

**Mutations M1–M28** (M25 split a/b), applied one at a time, all **caught**.
M3 (fresh nested deadline) first survived: the parent's cap masked it. Added
`b42_the_nested_request_ends_on_the_parents_deadline` (the nested trace must
end by its own inherited timer, not `cancelled`), which catches it.

**Upstream cancellation measured.** Mock engine via real gateway: the
generation stopped before the client even read its 504. Real CPU (Qwen3-1.7B,
budget 5 s): 504 at 5.004 s; engine idle within ~1 s; the slot free for the
next request. Best outcome of design section 26.

**Real-router smoke** (`hermes router` binary, scripted Python nodes):
A, B, C (504 at 2.003 s, not 3.0), D (2.006 s, General), F, G (incl. the
validate-config warning), H (stream relayed 3 s past a 1 s budget), I, J,
K, L passed. **E** (Coder's failure completing exactly as the deadline
passes) could not be placed on the binary: 6/6 SIGSTOP-across-the-deadline
runs ended as a cut Coder attempt (504, `same_route_attempt`, General 0 hits,
no transition), which is correct but not the refused-transition state. That
state is proven deterministically in-process by `b11` with the test-only
`after_attempt_ms` hook.

**Next:** GitHub Actions full matrix on the draft PR; STOP for review. No
release; R9.4 not started.

### R9.3.2 slice 1 review (PR #49 at `48b2f62`)

Decisions approved and frozen in review (docs only; no runtime change):
- **Disabled budget emits no budget metric samples.** Absent key = feature
  off = no `router_request_budget_*` series at all; `0` stays invalid. The
  design's section 33 "0 when disabled" was corrected to match.
- **SMOKE E accepted on two layers:** "The exact refused-fallback start
  condition must be proven deterministically in integration tests. The real
  binary must prove that an exhausted request budget cannot permit the next
  fallback route to execute or be counted." Layer 1: `b11` (General 0 hits,
  `next_unattempted_route: "General"`, no `Coder→General` transition, 504).
  Layer 2: the binary runs (504 `request_budget_exhausted`, General never
  attempted, no transition counted, nothing past the deadline). No timing
  hacks added.
- **Envelope:** `504`, `type: "server_error"`, `code:
  "request_budget_exhausted"` is the workspace's 5xx convention; the metric
  outcome stays `request_budget_exhausted` and history stays `neutral`.
- Connect-hang tests Linux-only (platform reason in design section 44);
  frozen R9.3 card "Served by" wording on a fallback cut deferred.

Actions evidence for `48b2f62` (check 37627845811, render 37627846102, both
attempt 1, no reruns): on Linux x64, Windows x64, macOS x64 and arm64 the
contract suite logged `47 passed, 2 skipped`, the secrets gate `ok no
credentials, home paths or machine addresses in tracked files`, frontend
`# pass 56 / # fail 0`, desktop `# pass 26 / # fail 0`, then `All checks
passed.`; `request_budget` 38 (Linux) / 36 (others) and
`request_budget_gateway` 2 passed. Flatpak "Flatpak checks passed.", Linux
artifacts "Artifact checks passed.", icons "icons ok", render panel 110
`[ok]`, 0 failed. No release workflow run since v0.6.0's own; no new tag or
release.

### R9.3.2 slice 1 MERGED and FROZEN

PR #49 (final head `dc020a4`) merged with a merge commit as **`eefa5b2`**,
the validated master head. v0.6.0 (`a8f0c89`) is unchanged and remains the
latest release; slice 1 is not in any release yet.

**Frozen behaviour:**
- One request-owned pre-commit deadline: a single absolute monotonic
  `tokio::time::Instant` per client request, made once at `received` (after
  the body is read, before parsing/routing; body-read time excluded), never
  reset, passed by value; no `remaining_ms` is propagated.
- Shared by classification (nested request inherits it), R9.2, same-route
  failover and cross-route fallback; checked before every new attempt;
  existing limits remain ceilings, effective wait = min(own limit, remaining);
  response-head wait and the node queue bounded externally.
- Deterministic race: `timeout_at`, operation polled first, so a ready
  result wins; no unbiased `select!` on the request path.
- Causal `504`, `type: "server_error"`, `code: "request_budget_exhausted"`,
  only for a cut wait or a refused start; completed outcomes never
  rewritten.
- `router_requests_total` counts it once as its own outcome
  `request_budget_exhausted` (never `server_error`) under the terminal
  attempted route, or `Auto` / the named route before any attempt; no
  invented route; no transition counted for an unstarted fallback.
- Disabled (key absent) = no budget metric samples at all; `0` invalid.
- Neutral to R9.2 history (cut = `neutral`, refused = unobserved), to node
  health and to placement. Applies to explicit routes, which still never
  cross-route fall back. Pre-commit only: nothing after the response head
  changes. No config-write API. No R9.4 behaviour.

**Master validation (`eefa5b2`, push):** check
[37674569075](https://github.com/dlroqa/Lightweight/actions/runs/37674569075)
and render panel
[37674569235](https://github.com/dlroqa/Lightweight/actions/runs/37674569235),
both attempt 1, no reruns. Linux x64, Windows x64, macOS x64, macOS arm64:
contract suite `47 passed, 2 skipped`; secrets gate `ok no credentials, home
paths or machine addresses in tracked files`; frontend `# pass 56 / # fail
0`; desktop `# pass 26 / # fail 0`; `All checks passed.`; budget tests 0
failed. Flatpak "Flatpak checks passed.", Linux artifacts "Artifact checks
passed.", render icons "icons ok", render panel 110 `[ok]`, 0 failed.

**Post-merge real-router smoke** (`hermes router` built from `eefa5b2`,
scripted nodes): 12/12 — explicit success; explicit 504 with no General
fallback (1.505 s); same-route sharing (cut at 2.006 s, not 3.0); cross-route
sharing (General cut at 2.005 s); exhausted budget blocks the next fallback
(504, General 0 hits, no transition); provider timeout first → R9.1 fallback;
budget first during classification → 504, no route attempted; stream relayed
3 s past a 1 s budget; a completed `server_busy` and a `route_unavailable`
stay the answer past the deadline; node stays healthy; `router_requests_total`
once under General.

**Deferred (not part of this freeze):** a budget-cut request may render in
the frozen R9.3 card as "Served by <route>" although the client got `504
request_budget_exhausted` (its trace keeps `exhausted: false`). To be handled
later in a separate, narrow UI follow-up; the wording is not decided here.
Also deferred: the budget UI card, a post-commit stream deadline, node
deadline forwarding, client-supplied deadlines.

**Next:** nothing started. R9.4 not started; no release.

## R9.3.2 budget wording follow-up (feature/router-request-budget-ui-wording)

From frozen master `91d2fb2`. **Presentation only:** no Rust, no trace,
metric, config or API change; R9.3.2 slice 1 stays frozen.

**The mismatch.** The R9.3 card (`CrossRouteFallback.tsx`, `TraceSteps`)
chose its pill from `cross_route_fallback.exhausted` alone: `Exhausted`, or
else always `Served by {final_route}`. A budget-ended request keeps
`exhausted: false` by design (time ran out, not the list), so a 504 read
"Served by General" — and a refused next fallback read "Served by Coder".

**The fix.** `traceVerdict()` (`fallbackModel.ts`) reads existing fields in
this order: `outcome == "request_budget_exhausted"` or
`request_budget.exhausted` → budget (`next_unattempted_route` → "Budget
expired before attempting X"; `stage == "classifier"` → "Budget expired
during classification"; otherwise "Budget expired while attempting
{final route}"; never `Auto` as an attempted route); then an exhausted list →
"Exhausted"; then "Served by X". The card adds a line saying the client got
`504 request_budget_exhausted` before any response started, and the cut
step's reason reads "Budget expired". No budget configuration UI, no
post-commit deadline.

**Tests.** 12 model tests (frontend 68 passing); render: a second router
with `pre_commit_budget_ms: 1500` and scripted nodes (`mock-node.mjs` gains
`hang` and `:loaded`) renders a live budget cut on General and a live
explicit cut, plus the refused-fallback and classifier shapes from fixtures
matching the router's own tests (126 render checks locally, 0 failed; the
original 110 unchanged).

**Known, not changed (pre-existing R9.3 behaviour, no budget involved):** a
chain that ends on a context overflow on a fallback route also keeps
`exhausted: false`, so the card still reads "Served by" for it.

R9.4 not started; no release.

### R9.3.2 budget wording follow-up MERGED and FROZEN

PR #51 (final head `6e3b082`) merged with a merge commit as **`a17dab2`**,
the validated master head. Presentation only: no file under `crates/`,
no Cargo, packaging or workflow change; R9.3.2 slice 1 backend stays frozen
as recorded above. v0.6.0 (`a8f0c89`) is unchanged and remains the latest
release; neither slice 1 nor this follow-up is in any release yet.

**Frozen behaviour (R9.3 *Recent cross-route fallbacks* card):**
- Precedence: request-budget terminal state, then an exhausted fallback
  list, then served. A budget-terminal trace never reads "Served by".
- Success still reads "Served by <route>".
- A budget cut on an attempted route reads "Budget expired while attempting
  <route>"; the cut step reads "Budget expired (request_budget_exhausted)"
  and the card says the client got `504 request_budget_exhausted` before any
  response started.
- A next fallback refused by the start check (`next_unattempted_route`)
  reads "Budget expired before attempting <route>" and is not listed as a
  step: it is never shown as attempted or served.
- Classifier-stage exhaustion invents no route (model verdict "Budget
  expired during classification"; it never appears in the card).
- Explicit-route exhaustion (verdict "Budget expired while attempting
  <route>") implies no fallback and never appears in the card.
- Normal failures (exhausted list, no budget) and a stream committed before
  the deadline keep their existing wording; a trace without a
  `request_budget` block reads exactly as before.
- No request-budget configuration UI, no post-commit deadline, no trace,
  metric, config, API or routing change.

**Master validation (`a17dab2`, push):** check
[37724692646](https://github.com/dlroqa/Lightweight/actions/runs/37724692646)
and render panel
[37724692555](https://github.com/dlroqa/Lightweight/actions/runs/37724692555),
both attempt 1, no reruns. Linux x64, Windows x64, macOS x64, macOS arm64:
contract suite `47 passed, 2 skipped`; secrets gate `ok no credentials, home
paths or machine addresses in tracked files`; frontend `# pass 68 / # fail
0`; desktop `# pass 26 / # fail 0`; `All checks passed.`. Flatpak "Flatpak
checks passed.", Linux artifacts "Artifact checks passed.", render icons
"icons ok", render panel 126 `[ok]`, 0 failed.

**Post-merge UI smoke** (`hermes router` and `frontend/dist` built from
`a17dab2`, scripted nodes, headless Chromium on the card): 21/21 — Auto →
Coder (503) → General (200) "Served by General"; General cut at 1.509 s →
504, "Budget expired while attempting General", nothing served; refused
next fallback (fixture, router b11 shape) "Budget expired before attempting
General", steps `[Coder]` only; classifier exhaustion (fixture, b08 shape)
no card row; explicit Coder 504 at 1.509 s, trace has no fallback block,
verdict "Budget expired while attempting Coder"; no-budget router "Served by
General" with no `request_budget` block; Coder 503 → General 503 "Exhausted";
stream relayed 3.5 s past a 1.5 s budget, outcome `ok`, "Served by General".
Screenshots of the CI render and the smoke inspected by eye.

**Known deferred UI issue (not fixed, out of scope):** a fallback flow
ending in context overflow may still render "Served by <route>" because the
existing R9.3 fallback trace keeps `exhausted: false` for that terminal
condition. Observed live in the smoke: Coder 503 → General `400
context_length_exceeded`, client got 400, trace `outcome: client_error`,
card "Served by General". This is separate from the R9.3.2 request-budget
wording fix and requires its own follow-up; its wording is not decided here,
and no context-overflow routing, trace or capability behaviour changes.

**Next:** nothing started. Separate decision on the context-overflow
follow-up, then release readiness. R9.4 not started; no release.

## R9.3 context-overflow wording follow-up (feature/router-context-overflow-ui-wording)

From frozen master `357a61b`. **Presentation only:** no Rust, no trace,
metric, config, API or routing change; R9.3.1 fallback semantics, the R9.3.2
slice 1 backend and the request-budget UI wording stay frozen.

**The mismatch.** Auto → Coder (503) → General, General answering `400
context_length_exceeded`: the client got 400, the trace reads `outcome:
client_error` with `cross_route_fallback.exhausted: false` (the chain
stopped; the list did not run out), and the card read "Served by General"
because anything not exhausted fell through to served.

**The trace already says it.** `conclude_chain` records the last route
attempt as `failed` / `context_length_exceeded` only when the chain ended
with no fallback reason and that route's own `context_overflow` was set,
which the router sets only from the node's structured `error.code`. Without
a fallback block, the last deployment attempt reads `context_overflow` and
the request `client_error`. No field was missing.

**The fix.** `traceVerdict()` gains one verdict, between the budget and the
exhausted list: "Context limit exceeded while attempting {route}", the
route being the one whose attempt overflowed (General, never Coder or
Auto). The card adds a line: the client got that route's own `400
context_length_exceeded`; no response was served. Precedence: request
budget, context overflow, exhausted list, served. Any other `client_error`
is not read as an overflow; an overflow a larger deployment then answered
is served.

**Tests.** 9 model tests (frontend 77 passing, was 68). Render: two more
routers sharing the budget router's refusing Coder (`mock-node.mjs` gains
`overflow` and `stream`): a live overflow on General, a live explicit
overflow (never in the card) and a live stream relayed past a 1500 ms
budget ("Served by General"). 142 render checks locally, 0 failed (the
original 126 unchanged). Local tip: `render-panel.sh` builds the frontend
only when `frontend/dist` is missing, so rebuild it after a frontend change.

**Truthfulness hardening (same PR, before merge).** The root cause was wider
than context overflow: anything not `exhausted` fell through to "Served
by". "Served by <route>" now needs positive evidence, the trace's `outcome:
"ok"` (`Outcome::of_status` 2xx/3xx; a stream is `ok` only when it ran to
its end, else `interrupted`). Every other ending that is not a budget,
context-overflow or exhausted verdict reads, without guessing a cause,
"Request ended while attempting <route>" (or "Request ended without a
successful response" if no route left `Auto`), and the card adds the
outcome and status. Precedence: request budget, context overflow, exhausted
list, served, neutral. 13 more model tests (frontend 90), including a guard
that fails if `!exhausted` alone ever reads as served again (reintroducing
that rule fails 9 tests); render adds two routers whose General commits a
plain 400 and a 500 (`exhausted: false`, final route General): 156 checks
locally, 0 failed.

**Step badges (same PR, before merge).** A route step's badge was green
whenever the step had `committed` (answered), so General's step in a chain
ending on its own 400 or 500 read "answered (400)" in green under a
non-success verdict. `stepTone()` (`fallbackModel.ts`) makes it green only
for a committed step on a request whose `outcome` is `"ok"` (only the final
step can commit, and the request's outcome is that answer's own result,
a stream counting only when it ran to its end); every other step (failed,
budget-cut, context overflow, a committed 4xx/5xx, interrupted, cancelled)
is a warning. The text ("answered (400)") is unchanged. 10 more model tests
(frontend 100), including a guard that fails if a committed step alone is
styled as success again (reintroducing that rule fails 6 tests); render
checks each live card's step tones (165 checks locally, 0 failed).

R9.4 not started; no release.

### Presentation truthfulness MERGED and FROZEN

PR #53 (final head `65e5299`: `2fd45c1` context overflow, `891d42a`
"Served by" needs success, `65e5299` step tones) merged with a merge commit
as **`a454a47`**, the validated master head. Frontend, render harness and
docs only: no file under `crates/`, no Cargo, packaging or workflow change.
v0.6.0 (`a8f0c89`) is unchanged and remains the latest release; none of
R9.3.2 slice 1, the budget wording or this work is in a release yet.

**Frozen behaviour (R9.3 *Recent cross-route fallbacks* card):**
- "Served by <route>" requires the trace's `outcome: "ok"`
  (`Outcome::of_status` 2xx/3xx; a stream only once it ran to its end).
  `exhausted: false`, a final route, an attempt or a committed response is
  not success.
- Verdict precedence: request budget, context overflow ("Context limit
  exceeded while attempting <route>"), exhausted list ("Exhausted"),
  served, then the neutral "Request ended while attempting <route>" (with
  its outcome and status; no cause guessed).
- A step's badge is green only for a committed step on a request whose
  `outcome` is `"ok"`; a failed, budget-cut, context-overflow, 4xx/5xx,
  interrupted, cancelled or unavailable step is a warning. Step text is
  unchanged ("answered (400)").
- Budget wording ("Budget expired while attempting / before attempting
  <route>", "Budget expired during classification") and committed-stream
  success unchanged; traces without a `request_budget` block read as
  before, except that a known failure no longer reads served.
- Guards: a test fails if `!exhausted` alone reads served (the old rule
  fails 9 tests) and one if a committed step alone is green (the old rule
  fails 6).
- No backend, trace schema, metric, config, API or routing change; R9.3.1
  and R9.3.2 stay frozen.

**Master validation (`a454a47`, push):** check
[37775745576](https://github.com/dlroqa/Lightweight/actions/runs/37775745576)
and render panel
[37775745564](https://github.com/dlroqa/Lightweight/actions/runs/37775745564),
both attempt 1, no reruns. Linux x64, Windows x64, macOS x64, macOS arm64:
contract suite `47 passed, 2 skipped`; secrets gate `ok no credentials, home
paths or machine addresses in tracked files`; frontend `# pass 100 / # fail
0`; desktop `# pass 26 / # fail 0`; `All checks passed.`. Flatpak "Flatpak
checks passed.", Linux artifacts "Artifact checks passed.", render icons
"icons ok", render panel 165 `[ok]`, 0 failed.

**Post-merge UI smoke** (`hermes router` and `frontend/dist` built from
`a454a47`, scripted nodes, headless Chromium; step tones read from
`data-step-tone`): 12/12 — Coder 503 → General 200 "Served by General",
Coder amber / General green; General 400 and 500 "Request ended while
attempting General", both steps amber; General `400 context_length_exceeded`
"Context limit exceeded while attempting General", amber; General cut by a
1500 ms budget at 1.512 s "Budget expired while attempting General", amber;
Coder 503 → General 503 "Exhausted", amber; a stream relayed 3.5 s past a
1500 ms budget "Served by General", General green; fixtures for
interrupted, cancelled, unavailable and a legacy `server_error` read
neutrally with no green step, a legacy `ok` reads served. Screenshots of all
eight live/fixture states inspected by eye: only the 200 and the completed
stream are green.

**Deferred (not part of this freeze):** the card lists only requests that
changed route (explicit and same-route requests never appear in it), and
the same-route attempt detail is plain text with no tone.

**Next:** nothing started. Release-readiness review (likely v0.7.0) is the
next separate task. R9.4 not started; no release.

## Release v0.7.0 (release/v0.7.0)

**Scope audited.** `v0.6.0` (`a8f0c89`) .. `63b4993`: PRs #49–#54, 18
commits, 26 files. Runtime only from #49 (R9.3.2 slice 1: `budget.rs`,
`proxy.rs`, `config.rs`, `metrics.rs`, `api.rs`, classifier, trace, CLI
`validate-config` output, a mock-backend test accessor). Frontend only from
#51 and #53 (`fallbackModel.ts`, `CrossRouteFallback.tsx`, trace types).
The rest is tests, render harness and docs. Nothing in `crates/` changed after
`eefa5b2`, and nothing in `frontend/`, `e2e/` or `scripts/` after `a454a47`.
No workflow, packaging or manifest change, and no R9.4 code.

**Release preparation.** PR #55 (`release/v0.7.0`, head `76cbce3`,
`release: prepare v0.7.0`). It touches the same seven files as v0.6.0's
`3345c4f`: `Cargo.toml`, `Cargo.lock` (18 workspace crates,
`cargo update --workspace --offline`), both `package.json` files and both
lines in each `package-lock.json`, plus `CHANGELOG.md` `[0.7.0] -
2026-10-08`. The changelog has a summary, compatibility and upgrade notes,
**Not in this release**, Added (R9.3.2) and Fixed (trace truthfulness, #51
and #53). PR check
[37804525792](https://github.com/dlroqa/Lightweight/actions/runs/37804525792)
and render
[37804525778](https://github.com/dlroqa/Lightweight/actions/runs/37804525778)
both passed on attempt 1. On all four platforms: contract `47 passed, 2
skipped`, secrets `ok no credentials, home paths or machine addresses in
tracked files`, frontend `# pass 100 / # fail 0`, desktop `# pass 26 /
# fail 0`, and `workspace 0.7.0`. Render panel 165 `[ok]`, 0 failed. The
AppImage and the installed Flatpak each ran their packaged `hermes 0.7.0`.

**Release commit.** Merge **`e2eb732`** (tree identical to `76cbce3`).
Master check
[37808116200](https://github.com/dlroqa/Lightweight/actions/runs/37808116200)
and render
[37808116194](https://github.com/dlroqa/Lightweight/actions/runs/37808116194)
are green with the same results. **One rerun:** on attempt 1, macOS x64
failed with "The hosted runner lost communication with the server" during
`check.sh`. The step never completed and no log was uploaded, so this was
infrastructure, not a test. That job alone was rerun and passed on attempt 2
(contract 47/2, secrets ok, frontend 100/0, desktop 26/0).

**Release.** Before tagging, no `v0.7.0` tag or release existed and no
workflow was running. Annotated tag `v0.7.0` (object `3ad1dab`, `Release
v0.7.0`) is on `e2eb732`. Release run
[37815809575](https://github.com/dlroqa/Lightweight/actions/runs/37815809575)
passed on attempt 1: the Flatpak, linux-x64, macos-universal, windows-x64,
the Intel half of the DMG, and the draft job. Each build ran what it
produced, and each reports `hermes 0.7.0` / "version 0.7.0" (the Windows
installer included). Provenance attestation covers 7 subjects
([54031582](https://github.com/dlroqa/Lightweight/attestations/54031582),
Rekor), and `gh attestation verify` passed for all 7. No `workflow_dispatch`
dry run was used, so there is no `dry-run-*` draft. Published from the
inspected draft with `gh release edit --draft=false --latest`.

**Published state.**
<https://github.com/dlroqa/Lightweight/releases/tag/v0.7.0>, Latest, not a
prerelease. It has 8 assets: the mac-universal DMG, the Windows x64
installer, the Linux Flatpak and AppImage, the three `hermes` CLI archives
(aarch64-apple-darwin, x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu),
and `SHA256SUMS`. All return HTTP 200, as do both source archives.
`sha256sum -c SHA256SUMS` on the downloaded assets passed 7/7, and the
GitHub asset digests equal `SHA256SUMS` 7/7. The notes are the CHANGELOG
`[0.7.0]` section, unchanged from the inspected draft. v0.6.0 (`f43a6fb` ->
`a8f0c89`, 8 assets, digests and publish time) is unchanged. There are no
draft releases and no extra tags.

**Contents.** R9.3.2 shared pre-commit request budget, plus the frozen
budget, context-overflow and "Served by" / step-tone presentation
truthfulness. Not included: R9.4 / MoA, a post-commit or lifetime deadline,
a client deadline, a budget settings UI, and explicit-route or transitive
fallback. Known panel limits are unchanged: the cross-route card lists only
requests that changed route, and same-route attempt detail has no tone.

**Next:** nothing started. v0.7.0 is immutable. Any later fix ships as a new
version. R9.4 stays untouched until it is separately approved.

## Jev Settings in the router panel (post-v0.7.0, branch `feature/router-jev-settings`)

**Audit first (2026-10-08).** Master was still `47d821d`. The audit found
no credential store in the workspace (the Jev key came only from
`TYPESAFE_API_KEY`), no admin boundary (one shared client key, turned off on
a keyless loopback router; the panel sent no `Authorization`), no
Origin/Host guard on the router, and no reload (`router.json` read once).
Work stopped there for approval. The approved decisions were: (A) OS
credential store via `keyring`, with the environment winning and no file
fallback; (B) a separate local-only admin capability; (C) no new Flatpak
permission.

**Built.**
- `secret_store.rs`: `keyring-core` 1.0 with a per-target store crate (Apple
  Keychain, Windows Credential Manager, zbus Secret Service with RustCrypto).
  It is pure Rust, so the dependency policy holds. Flatpak reports no store.
  Platform errors are reduced to fixed sentences, never bytes. A debug-build
  test switch, `LIGHTWEIGHT_ROUTER_TEST_SECRET_STORE=memory|unavailable`,
  exists for the render and is never compiled into a release.
- Config: `validate_with_store` and `load_with_store`. Only the Jev key falls
  back to the store, and only when the environment has none. Jev reports a
  `key_source`. `validate` is unchanged for its 45 callers.
- `admin.rs`: a 32-byte token minted per start and written owner-only to
  `<config>.admin-token`. It is removed on Ctrl-C, and there is none when any
  listener is off loopback. Each write is checked for a loopback `Host` on a
  bound port, a matching `Origin`, a JSON `Content-Type`, and a
  constant-time token comparison.
- `classifier_settings.rs`: GET, PUT and DELETE. PUT needs `If-Match` (the
  file's SHA-256) and holds a per-process write lock. The file is edited
  order-preserving (IndexMap) in `auto_route.classifier` only, and the whole
  file is validated before any write. The order is: key first (read back),
  then `.bak`, then an atomic rename that keeps the file's mode; a failed
  write rolls the key back. `restart_required` is the saved section compared
  with the one loaded at start, or a key changed. The audit log carries
  outcomes only.
- CLI: one store per run, the token lifecycle, an `admin` summary line, and
  `hermes router admin-token`.
- Panel: a Jev Settings card on the Classifier screen, backed by
  `classifierModel.ts` (settings draft, validation, request, status words)
  and `ApiError.details`.

**Verified locally.** Router and CLI tests: 554 passed, 0 failed, including
the new `tests/classifier_settings.rs` (24). Frontend: 111/0. The render
(`render-panel.sh`, scratch ports) reported 228 `[ok]` and 0 failed:
the existing 165, plus Jev Settings (a save phase, a real SIGINT restart, a
restarted phase), checks on the saved files, the token lifecycle and the
logs. Existing render selectors were scoped to their own cards, and no
assertion changed.

**Pre-merge security verification (conditional approval).**
- **Two routers.** Each has its own token, and neither accepts the other's
  (`one_routers_token_is_not_anothers`, and over HTTP
  `two_routers_never_accept_each_others_admin_token_but_share_one_users_saved_key`).
  Under one user, both naming `TYPESAFE_API_KEY` share one store entry, as
  the variable would; this is now documented.
- **Token disclosure.** One correction. The token moved from beside
  `router.json`, where a `--config` in a shared directory would have given it
  that directory's Windows ACL, to the user's own data directory
  (`router-admin/`, `0700`; file `0600`; one file per configuration path).
  `hermes router admin-token` reads only the calling user's directory. The
  render asserts mode 600, that nothing is written beside `router.json`, that
  the command fails after a stop, and that a restart rotates the token.
- **Inference key.** It is not an admin token
  (`the_inference_key_is_not_an_admin_token`; the HTTP case is in
  `writes_without_the_admin_token_are_refused`).
- **Failed updates.** `a_failed_write_leaves_the_file_and_the_previous_key_in_place`
  covers file failure (the key is rolled back) and store failure (the file is
  untouched). Invalid and refused saves leave the file byte-identical.
- **No secret leaks.** Logs (`no_key_or_admin_token_ever_reaches_a_log_line`
  and the render's grep of the router logs), responses (`assert_no_key` on
  every view and save), the browser (render: DOM after reload, storage, URLs,
  console, all bodies) and files (render: `router.json` and `.bak`). No
  metric or trace field was added.

**MERGED and FROZEN (2026-10-09).** PR #57 (head `6d1d7f6`) was approved at
that commit, marked ready, and merged with `--match-head-commit` pinned to it.
The merge commit is **`9f86521`** (parents `47d821d`, `6d1d7f6`).

- **PR CI.** On `6d1d7f6`, check
  [37884144400](https://github.com/dlroqa/Lightweight/actions/runs/37884144400)
  and render
  [37884144406](https://github.com/dlroqa/Lightweight/actions/runs/37884144406)
  passed, every job on attempt 1.
- **Master CI.** On `9f86521`, check
  [37885116644](https://github.com/dlroqa/Lightweight/actions/runs/37885116644)
  and render
  [37885116672](https://github.com/dlroqa/Lightweight/actions/runs/37885116672)
  passed, every job on attempt 1. Read from each job's log:
  - Contract `47 passed, 2 skipped` and secrets
    `ok  no credentials, home paths or machine addresses in tracked files`
    on macOS x64, macOS arm64, Linux x64 and Windows x64.
  - Dependency policy satisfied; frontend `# pass 111 / # fail 0`; desktop
    `# pass 26 / # fail 0`.
  - Rust: 1405, 1405, 1432 and 1400 passed, 0 failed.
  - `tests/classifier_settings.rs`: 25/25 on every platform. The real OS
    credential-store round trip passed: through the Keychain and Credential
    Manager on macOS and Windows; on headless Linux it reports "unavailable".
  - The Flatpak installed and ran `hermes 0.7.0`; Linux artifacts and render
    icons passed.
  - Render: 233 `[ok]`, 0 failed (165 before this feature).
- **Smoke.** This runs in the master render job; it is the scripted Jev
  configuration smoke the brief asked for.
  1. Save switched a Lightweight router to Jev with a typed key; Pending
     Restart was shown.
  2. `hermes router admin-token` read an owner-only token from the user's
     data directory.
  3. A real SIGINT restart removed the token, and the next start minted a
     new one.
  4. Test Connection reported Connected, and every failure state was shown
     in words.
  5. Two Auto requests were classified by Jev with the saved settings: code
     to Coder (200 Coder), a greeting to General (200 General). Both reached
     the scripted System One with the right key, and the trace names the
     semantic rule.
  6. The key was found in no file, log, page, storage area, URL or response.
- **Not proven by CI.** CI ran against a scripted TypeSafe-compatible
  endpoint and needed no real credential. Compatibility with the real Jev
  service still needs a live smoke, run deliberately with an
  operator-provided key that is never echoed.
- **Security model (frozen).**
  - The Jev key lives only in the OS credential store; the environment wins;
    there is no file fallback, and Flatpak gets no new permission.
  - Writes need a per-start admin token that is separate from the inference
    key and kept in the user's own data directory.
  - Writes are accepted on loopback-only routers alone, and need a loopback
    `Host`, a matching `Origin`, JSON, at most 16 KiB, and `If-Match`.
  - `hermes router admin-token` is privileged. On shared hosts, the router's
    account and the ownership of its configuration stay part of the model
    (docs/ROUTER.md).
  - Two routers of one user that both use `TYPESAFE_API_KEY` share one saved
    key; give each its own `api_key_env` to separate them.
- **Unchanged.** v0.7.0 (`e2eb732`, Latest) is untouched. No version bump,
  tag or release. Routing, Auto, the classifier fallback, the request
  budget, metrics and traces are unchanged. R9.4 is not started.

## Live Jev validation workflow (2026-10-09, branch `ci/jev-live-validation`)

The operator created the `jev-live-validation` environment. It holds the
`TYPESAFE_API_KEY` secret, allows deployments from `master` only, and requires
`dlroqa` as reviewer.

Added `.github/workflows/jev-live.yml`. It is manual (`workflow_dispatch`),
`master`-only and runs in that environment; it builds before the secret is
present, restores the cache without saving it, and persists no git
credentials. Also added `scripts/jev-live.sh` and `e2e/jev-live.mjs`, which
run a live check, four Auto requests that Jev must classify (`chosen`) into
Coder and General, the panel's Test Connection, and checks that the key leaks
nowhere. Normal CI never sees the secret.

There is no v0.7.1 plan in the repository. This run is the live gate before
any v0.7.1 release work, which will follow the usual release process once the
operator asks for it.

The live run passed on `fc0919d` (run 37913409376). Jev chose Coder for both
coding requests and General for both general ones (`jev-latest`, confidence
1.0, 137–150 ms), Test Connection reported Connected, and the key appeared
nowhere. The operator then chose a minor bump, **v0.8.0**, instead of v0.7.1,
because the release adds backward-compatible features.

## v0.8.0 release preparation (2026-10-09, branch `release/v0.8.0`)

Branched from `fc0919d`, which was still the remote head. `release: prepare
v0.8.0` touches the same seven files as v0.7.0: the three version manifests,
both npm lockfiles, `Cargo.lock` (the 18 workspace crates only) and
`CHANGELOG.md`. The changelog's `[0.8.0]` section is the release notes. It
covers Jev Settings, the credential store, the protected admin endpoints,
`hermes router admin-token` and the live result, plus the known limits: the
macOS Keychain prompt, Flatpak, remote routers, the restart requirement and
the snippet-based candidate-route configuration.

The gates are the PR's Actions runs, the master matrix after the merge, and
one more live Jev run on the merged commit, which waits for the operator's
approval. The tag, the release workflow and publishing wait for the
operator's approval of the readiness report. v0.7.0 is untouched, and R9.4
is not started.

## Release v0.8.0 (published 2026-10-10)

**Release commit.** Merge **`a1a4833`** (`a1a4833b3f13406ca0d09c59cc377d4c8afcf009`,
PR #65; tree identical to its reviewed head `c108599`). Since the
preparation entry above, four more PRs went into the release:

- **#62** (`d194297`) moves the Flatpak to Freedesktop 25.08 (issue #61).
- **#63** (`43692e3`) records that move in `[0.8.0]`.
- **#64** (`325933a`) adds the read-only release verifier,
  `verify-release.yml` with `scripts/verify-release.sh`.
- **#65** (`a1a4833`) updates Electron 43.4.1 → **43.5.1** for
  GHSA-qmv3-fv6v-rmhq (CVE-2026-102677, high), and adds two checks:
  - the shipped-dependency advisory gate (`scripts/check-advisories.py`,
    the `dependency advisories` job, `scripts/advisory-exceptions.json`
    kept empty);
  - a byte-level Electron version check on every package
    (`scripts/electron-version.sh`).

**Gates on `a1a4833`.**

- Check
  [38001191490](https://github.com/dlroqa/Lightweight/actions/runs/38001191490):
  8/8 jobs. On all four platforms:
  - Rust: every test block ok (53 model-alias tests).
  - Contract `47 passed, 2 skipped`.
  - Frontend `# pass 111`, desktop `# pass 26`.
  - Versions agree, dependency policy satisfied, secrets ok.
  - Installed Electron `43.5.1` equals the locked version.
- The AppImage carries Electron 43.5.1. The Flatpak passes 21/21 on 25.08,
  with no end-of-life notice.
- The advisory gate reports no high or critical advisory in anything
  shipped, and its self-test still rejects Electron 43.4.1.
- Render
  [38001191502](https://github.com/dlroqa/Lightweight/actions/runs/38001191502):
  233 `[ok]`, 0 failed.
- Live Jev
  [38001375921](https://github.com/dlroqa/Lightweight/actions/runs/38001375921)
  (environment approval by the operator): 36 `[ok]`, 0 failed, and 4/4
  requests classified by Jev. The key appears only masked.

**Candidates retired before publication.** Neither was ever published, and
both audit records are kept outside the repository
(`~/release-audit/v0.8.0-retired-*`).

1. **Tag object `b403aba` on `93dd696`** (release run 37925314580,
   draft 407864468). It shipped the Flatpak on runtime 24.08, which is
   end-of-life.
2. **Tag object `47830f2` on `325933a`** (release run 37996073622,
   verifier 37996803465, draft 408381067). It shipped Electron 43.4.1.

**Release.** Annotated tag `v0.8.0` (object `91950e6`, `Release v0.8.0`) on
`a1a4833`, pushed once. Release run
[38010418685](https://github.com/dlroqa/Lightweight/actions/runs/38010418685)
passed all six jobs: the Flatpak, linux-x64, macos-universal, windows-x64,
the Intel half of the DMG, and the draft.

- **Smoke checks, all reporting `hermes 0.8.0`:**
  - The NSIS installer installs, and the installed app carries Electron
    43.5.1.
  - The DMG mounts. `hermes` has arm64 and x86_64 slices and runs on
    arm64 and on Intel. The framework carries Electron 43.5.1, and
    electron-builder packaged both slices from `electron=43.5.1`.
  - The AppImage passes 13/13, including Electron 43.5.1.
  - The Flatpak passes 21/21 on Platform, SDK and BaseApp 25.08, with
    Electron 43.5.1.
- **Provenance:** 7 subjects
  ([54516319](https://github.com/dlroqa/Lightweight/attestations/54516319),
  Rekor 3175314923).
- **Verifier:** before publication,
  [38011520531](https://github.com/dlroqa/Lightweight/actions/runs/38011520531)
  (`workflow_run`, from the run's artifacts); after publication,
  [38012338128](https://github.com/dlroqa/Lightweight/actions/runs/38012338128)
  (from the published release, GitHub digests included). Both report
  `8 files, 7 checksums, 7 attestations`, bound to `release.yml @
  refs/tags/v0.8.0, a1a4833`.
- No `workflow_dispatch` release dry run was used.

**Published state.** <https://github.com/dlroqa/Lightweight/releases/tag/v0.8.0>
(release 408474172), Latest, not a prerelease, published with
`gh release edit --draft=false --latest`. The asset digests and notes are
unchanged from the inspected draft.

| Asset | SHA-256 |
|---|---|
| `Lightweight-0.8.0-mac-universal.dmg` | `c2e3bba8284772faf752e5b569c9611f6c894e47183a665528d693863899c275` |
| `Lightweight-Setup-0.8.0-x64.exe` | `61d3d585260bc6d48e57bd6ff3651757c0b153498896551b88f724d09b2ca557` |
| `Lightweight-0.8.0-linux-x86_64.AppImage` | `535c074465818737f84e89936c512cbed65de7e7fffbebf360ed6a63ccff6b43` |
| `Lightweight-0.8.0-linux-x86_64.flatpak` | `a57914d9b631b0082439d5725ae3941b61a8c7cc23fcf375ff047d224ce261e8` |
| `hermes-0.8.0-aarch64-apple-darwin.tar.gz` | `a56f78c4a3625d41e2040d44249959a0270d577bc242ee5a3009b55fc52c950a` |
| `hermes-0.8.0-x86_64-pc-windows-msvc.zip` | `ad2e41fd81773c99054509615929b9150a288291f44aa1f8b4286c81464a1268` |
| `hermes-0.8.0-x86_64-unknown-linux-gnu.tar.gz` | `4b90b090a01cdd4892a9f30ae8a6da0b2bb27f514862eae542076f97a85535d5` |
| `SHA256SUMS` | `52903d389b00b0c176e1240e5a6cfd0c7449d82dda925efe821701b1f6558c53` |

v0.7.0 is unchanged: release 407095348, tag `3ad1dab` → `e2eb732`, and the
same 8 asset digests and publish time. Issue #61 is closed as completed.
There are no draft releases.

**Known at release, not blocking.**

- **rustls:** 0.23.43 is in `hermes`, with RUSTSEC-2026-0285 /
  GHSA-2mjx-qc3c-rqvc (moderate; client-only use, and the handshake
  transcript stays authenticated). It is fixed in 0.23.45, a one-line
  `Cargo.lock` change.
- **Desktop hardening gap:** the window has no navigation guard,
  `openExternal` accepts any scheme, and there is no CSP. Each only matters
  after the panel has been compromised.
- **Build- and test-tool advisories** (electron-builder, Vite, Playwright)
  are reported by the gate and are not shipped.

**Next:** nothing started. v0.8.0 is immutable; any later fix ships as a
new version. The v0.8.1 maintenance backlog (rustls, the navigation and
`openExternal` limits, a CSP, a weekly advisory scan, a review of the
build-tool advisories) waits for approval.
