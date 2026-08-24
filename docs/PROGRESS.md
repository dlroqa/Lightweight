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
| **M5** Scheduler, metrics, per-token timings | **done** | priority bands classified from measured cost, starvation-bounded fairness, queue position reported to streamed clients, `/metrics` and `/api/v1/metrics`, per-token timings from the engine, `--concurrency` |
| **M6a** Model manager | **done** | `hermes-download` shared by engine and models, persistent catalog with atomic writes, import + pinned downloads + pasted links with per-model integrity, `hermes models`, scheduler pause/drain, hot swap over `/api/v1`, jobs with SSE progress, `serve` with no model |
| **M6b.1** Backend seams | **done** | `/api/v1/system`, `/api/v1/gateway`, `/api/v1/events`, `/api/v1/logs`, `GET /api/v1/models/{id}`; disk via `rustix` and processor time from `/proc/stat` as probes that say when they could not read; an in-flight gauge that spans the response body; the panel served from the gateway, so no CORS layer exists |
| **M6b.2** Persistence | **done** | `hermes-store`: conversations and settings under the two M0 directories that had never been written to, owner-only, atomic; `/api/v1/conversations` and `/api/v1/settings` |
| M6b.3-4 | next | the SPA, then the Electron shell |

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

## Test counts

| Suite | Count | Notes |
|---|---:|---|
| Default (`cargo test --workspace`) | 644 | no network, no model downloads — checked with outbound HTTP blocked |
| openai-SDK contract (`scripts/contract-test.sh`) | 30 | real `openai` package against the gateway over `MockBackend`; imports Hermes' own error parser |
| Real model headers | 3 | needs `scripts/fetch-real-headers.sh`; `HERMES_REQUIRE_REAL_MODELS=1` makes absence a failure |
| Real engine | 9 | needs `HERMES_TEST_MODEL=<path.gguf>`; downloads the pinned engine on first run |
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
  called them. `hermes-store` is a crate of its own rather than part of
  `hermes-catalog`: the two look alike — a directory, atomic writes, a JSON
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

## Next step

M6b.3: the SPA itself — React, TypeScript and Vite, on the seams M6b.1 opened
and the stores M6b.2 added, driven against a real gateway running a real model.
Then M6b.4, the Electron shell. The full plan is `docs/M6B-PLAN.md`.

Deliberately left, and recorded so they are chosen rather than forgotten:

- **`/v1/completions` waits at the door.** A queued request there is refused
  with a 503 rather than answered and told its position, because one such
  request can carry many prompts under a single permit and the streaming state
  machine for that is a second one. The band and the metrics apply to it
  already.
- **Nothing is preempted, and nothing can be** until the engine can pause a
  generation. A swap now waits for the running turn — measured at 116 s above —
  which is the honest behaviour rather than a fast one.
- **No pre-flight disk check before a download.** `statvfs` needs `unsafe`, and
  every crate that would host it is `forbid(unsafe_code)`; weakening that to
  save a failed download is the wrong trade. A full disk is still reported
  actionably as `low_disk`, from `ENOSPC` as it happens rather than before it.
- **A verified file can be orphaned in the downloads directory.** If the final
  move into `models/` fails — a full disk, a permissions change between the
  download and the move — the bytes are downloaded and checked but left where
  they are. The error names the file so it can be moved by hand; a retry
  re-downloads rather than adopting it, because recognising a staged file as
  resumable is a second mechanism and this failure is rare.
- **The catalog is not shared between processes.** Two `hermes` commands writing
  it at once would have one overwrite the other; each write is atomic, so the
  file is never corrupt, but there is no lock. A single gateway plus occasional
  CLI use is the shape this is built for.
