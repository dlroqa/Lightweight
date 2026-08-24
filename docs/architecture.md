# Architecture notes

Decisions that are not obvious from the code, and the evidence behind them.
Each was verified against a source of truth rather than assumed; where that
source is a file in another project, it is cited so the claim can be rechecked
when versions move.

## The engine is a child process, not a linked library

The alternative was FFI via the `llama-cpp-2` crate. The child-process design
wins on four counts:

1. **It builds here at all.** FFI needs cmake (llama.cpp's top-level `Makefile`
   is a hard `$(error)` stub) and libclang for `bindgen`. Neither is
   installable without sudo.
2. **Spec section 27 — never crash.** A `GGML_ASSERT` or SIGSEGV inside an FFI
   call takes down the whole application, including the UI; `catch_unwind` does
   not catch a C-level abort. A child process crash leaves the gateway alive to
   report a structured error and restart.
3. **Spec section 37 — do not reinvent.** The child already applies the
   GGUF-embedded Jinja chat template, parses per-family tool calls, and reuses
   the prompt-prefix KV cache. Reimplementing those in Rust is "reinventing
   kernels" one level up.
4. **Packaging.** A pinned, checksummed prebuilt per platform needs no end-user
   toolchain.

The cost is a loopback HTTP hop per token: tens of microseconds against
single-digit tokens per second of decode, so under 0.01%, and it sits at
priority 5 of section 31's ordering.

The gateway owns the model catalog, RAM admission control, context and thread
policy, the scheduler, metrics, auth, and the wire contract. It re-emits its own
canonical response chunks rather than forwarding the child's bytes, so an
upstream JSON change breaks an adapter test instead of breaking clients.

## Why the ggml type table is generated, not written

Every byte the RAM estimator reports traces back to two numbers per tensor type.
They were read out of the pinned engine binary by `dlopen`-ing
`libggml-base.so` and calling `ggml_blck_size`, `ggml_type_size` and
`ggml_type_name`, and `crates/hermes-core/src/ggml.rs` is generated from that
output.

Reading them from the binary rather than transcribing the C headers caught
things a careful transcription would have missed: four types that recent builds
added (`mxfp4`, `nvfp4`, `q1_0`, `q2_0`), and eight removed enum slots that
report a block size of zero — which, used as a divisor, is a panic.

It also makes the quantization arithmetic honest. `q4_K` is 144 bytes per 256
elements, which is 4.5 bits per weight, not 4. `q8_0` is 34 bytes per 32
elements, so 1.0625 bytes per element, not 1. Rounding either to the number in
its name misstates a multi-gigabyte KV cache by hundreds of megabytes.

## Why the KV cache is summed per layer

Two real models make the uniform formula wrong, and both were confirmed against
files fetched from HuggingFace:

- **LFM2-1.2B** writes `attention.head_count_kv` as a 16-element array,
  `[0,0,8,0,0,8,0,0,8,0,8,0,8,0,8,0]`. Only six of its sixteen layers have
  attention at all; the rest are short-convolution blocks. Assuming the first
  non-zero value applies throughout overstates its cache by a factor of 2.67.
- **Gemma-3-1B** declares `attention.key_length = 256` while
  `embedding_length / head_count` is `1152 / 4 = 288`. Deriving the head
  dimension instead of reading the declared one overstates by 12.5%.

Neither is handled by an architecture-specific branch, which spec section 6
forbids. Both fall out of reading the metadata as written.

## Why a declared sliding window is not discounted

Gemma 3 declares `attention.sliding_window = 512` against a 32768 context, and
applying that to every layer would cut the estimate by up to 64x. The metadata
says a window exists but not *which* layers use it, and inferring that from the
architecture name is exactly the hard-coding section 6 rules out.

Section 7 settles the direction to be wrong in: never promise that a model will
run. So the estimate is an upper bound and a windowed model simply uses less
than predicted. This is a known, deliberate over-estimate, and the right fix is
a per-layer key in the metadata, not a lookup table of architecture names.

## Why the budget is MemAvailable, and excludes swap

`MemTotal` includes everything other processes are already using, so a verdict
computed against it would approve loads that cannot fit. `MemFree` is the
opposite error: it ignores reclaimable page cache and on the development machine
reads about 1.8 GiB against `MemAvailable`'s 3.3 GiB, so budgeting from it would
refuse loads that would have worked comfortably.

Swap is reported to the user as context and never counted as headroom. Decode
touches essentially every weight once per token, so a model that "fits" only by
swapping would page continuously — the heavy swapping section 7 forbids.

## Where the numbers stop being exact

Weights and KV cache are computed exactly, from tensor shapes and per-layer
geometry. Compute buffers and runtime overhead cannot be derived from metadata
at all, so they use coefficients that the benchmark harness is meant to fit from
observed peak RSS.

Until that measurement exists, the shipped conservative defaults are used and
the estimate reports `Confidence::Coarse` — the UI says so rather than implying
a precision the numbers do not have. That is the honest reading of "no
guesswork": what can be exact is exact, and what cannot is measured, versioned
and labelled rather than invented.

## Why the engine is never reachable by anything else

The child binds loopback on an **ephemeral** port and requires a random 24-byte
API key regenerated on every launch. It is an implementation detail, not a
second public endpoint: the gateway is the surface, and spec section 23 wants
exactly one of those.

The port is reserved by binding to `127.0.0.1:0`, reading the assignment and
releasing it, which leaves a brief window where something else could take it.
The alternative is a fixed port, which fails whenever a second instance runs or
the port is already in use — far more common than losing that race, and a launch
that does lose it fails at bind and is retried.

## Why the supervisor works this hard to avoid orphans

The engine holds an entire model in memory, hundreds of megabytes to several
gigabytes. An orphaned one would be invisible and expensive, so there are three
independent mechanisms: on Linux the child asks the kernel to `SIGKILL` it when
its parent dies (`PR_SET_PDEATHSIG`), it is placed in its own session and process
group by `setsid` so signals reach the whole tree rather than ours, and tokio's
`kill_on_drop` remains as a backstop. The two syscalls run in `pre_exec` between
`fork` and `exec`, where only async-signal-safe operations are permitted; both
qualify, take no locks and allocate nothing.

## Progress reporting must never block the work it reports

Model loading sends progress over a bounded channel. Every send is `try_send`,
never `send().await`.

This is not a style preference — it was a deadlock. A cold start fills the
channel from the download loop, and if the caller is not draining it (a closed
UI, or a test holding a receiver it never reads) the next blocking send waits
forever, with the load frozen behind it. Progress is advisory: dropping an
update nobody is reading is always better than stalling the operation.

Related, and found the same way: extraction and digest re-hashing were running
directly on the async executor. Both are CPU-bound and blocking, and on a
current-thread runtime they starve every other task — including the one draining
progress. They now run under `spawn_blocking`.

## What the engine is told explicitly, and why

Every memory-shaping parameter is passed on the command line even when its
default already matches: `--ctx-size`, `--batch-size`, `--ubatch-size`,
`--parallel`, `--threads`, `--cache-type-k`, `--cache-type-v`.

The RAM estimate is computed for exactly those values. Leaving any of them to
the engine would mean the estimate describes something other than what is
running, and the engine's defaults are not always what one would guess:
`--parallel` defaults to `-1` (auto), which sizes the KV cache for a slot count
we did not choose. Verified from `llama-server --help` at the pinned build, not
from documentation.

The KV cache type is also validated before launch. Only nine ggml types are
accepted there — `f32 f16 bf16 q8_0 q4_0 q4_1 iq4_nl q5_0 q5_1` — so `q6_K`,
which is perfectly good for weights, is refused with the list of alternatives
rather than surfacing as an opaque engine exit.

## Why the gateway re-emits its own chunks

The engine already speaks an OpenAI-compatible dialect, so forwarding its SSE
bytes would have been less code. It is the wrong trade.

The engine's chunks carry its own `model` field — the *file path* of the GGUF —
and the client reads `chunk.model` back and keys its context cache on it. They
also carry `system_fingerprint`, a `timings` object shaped to llama.cpp, and
whatever the next release adds or renames.

Re-emitting means an upstream change breaks a test in `hermes-api` rather than
breaking a conversation. The translation is one file (`upstream.rs`), its
inputs are a captured transcript from the pinned build, and the output side is
pinned by byte-exact golden files.

## Why the effective context is advertised, and the ceiling is hidden

Hermes scans `/v1/models` rows *recursively* for the first key it recognizes
out of twelve — `context_length`, `n_ctx`, `max_context_length`,
`max_model_len`, and so on (`agent/model_metadata.py:1119-1132`) — and then
sizes every prompt to what it finds.

So a model that supports 128K but is loaded at 8192 must report **8192**.
Reporting the ceiling would guarantee that every long conversation overflows a
window that does not exist.

The true ceiling is still useful — the UI offers it as a context preset — so it
is reported as `hermes.model_max_context_length`. The `model_` prefix is
load-bearing: `max_context_length` is in the scanner's key set, and naming it
that would have undone the whole point.

## Why priority is measured and never claimed

The scheduler classifies a request into a band from two numbers it already has:
the prompt the engine counted, with the chat template and tool declarations
applied, and the output budget the client asked for. Nothing a client says about
its own importance is read, and there is no field to say it in.

That is not distrust of any particular client. A priority that can be requested
is a priority every caller requests — the first client to set it wins, the rest
copy it, and within a release the field means nothing. Cost is the one thing a
caller cannot exaggerate, because the gateway measures it before admitting
anything.

The output budget is read as **requested**, not as clamped. `max_tokens` is
clamped upward to the remaining window when a client omits it — Hermes defaults
it to 65536 — so classifying on the clamped value would place every client that
simply leaves the field out into the slow band, and the fast band would go
unused. An unstated budget is missing evidence, not evidence of a long request.

The ceilings are fractions of the loaded context rather than constants, for the
same reason the context itself is derived from the machine: on a 2048-token
window a 1024-token prompt is half the context and nothing like interactive,
while on a 32K window it is a rounding error.

## Why the band decides who starts, and never who stops

There is no preemption, and there cannot be. The engine has no way to pause a
generation and resume it later, and killing one to run something shorter would
discard the prefill that dominates the cost on a CPU — the work already spent is
exactly the work that would have to be repeated.

So a short request still waits for the current turn to finish. What it no longer
does is wait for every turn queued ahead of it, which is the case that produced
the failure: not one long generation, but a queue of them.

Starvation is bounded by time rather than by a count of overtakes. Once a
request has waited past the ceiling it sorts ahead of every band, and among
aged-out requests the longest wait wins. One rule, one comparison, and the
position reported to a client is computed with the same key the scheduler picks
by — so a reported position cannot contradict the order actually served.

## Why a queued streamed request is answered before it runs

The gateway used to take a slot and *then* answer. For an uncontended request
that is still what happens, and it is the better order: nothing has been written
to the client, so a failure to start generating can be an HTTP status.

For a queued request it is the wrong order. A client that has to wait minutes
for the response headers cannot tell a busy gateway from a hung one, and its
read timeout eventually decides for it. So a streamed request that finds the
slot taken is answered immediately and waits inside its own response body,
emitting a comment frame with its position.

The cost is stated rather than hidden: once the response has started, a failure
to *begin* generating can no longer be a status code and arrives as the terminal
error chunk instead. That trade is made only for a request that was genuinely
queued. Its refusal carries the same `server_busy` code the non-streamed path
returns with a 503, because which side of the headers a client happened to be on
is our implementation detail, not a difference in what went wrong.

The position is a comment (`: queued position=0 waited=30s`), not a chunk. A
queued request has produced no tokens, so any `data:` frame would be a
completion object that is not one, and every strict client would have to be
taught to ignore it. Comment frames are already discarded by the SSE decoders
this gateway is checked against — including the real `openai` package — and they
are plainly readable in `curl`, which is where "is it stuck, or is it waiting?"
actually gets asked.

## Why the scheduler hands out a grant rather than a permit

A waiting request holds a channel that the release path signals when its turn
comes. What travels through that channel is a bare grant, and the permit is
constructed by the receiver.

Sending the permit itself would be the obvious design and it deadlocks. The
release path holds the queue lock while it picks the next waiter; if that
waiter's receiver has already gone — a client that disconnected in the
microseconds between being picked and being told — the send fails and returns
the permit to a caller holding the lock, where dropping it re-enters the release
path against a lock it already owns.

The remaining race is the mirror of it: a grant delivered to a ticket that goes
away before claiming it. Nothing else knows that slot exists, so the ticket's
`Drop` looks in its own channel for an unclaimed grant and gives the slot back.
Without that, one unlucky disconnect would cost the gateway a slot for the life
of the process. It is asserted directly, because it is the kind of thing that
never shows up in a test that only exercises the happy path.

## Why metrics are counted here and timed by the engine

Prefill and decode times come from the engine, which measures them from inside
the loop. The queue wait and the time to first token are measured by the
gateway, because the engine cannot see them — it does not know a request existed
until the request is handed to it.

They are reported separately rather than summed. A slow first token and a busy
queue are different problems with different fixes, and a single latency number
that mixes them tells an operator to buy a faster CPU when the answer was to
raise the concurrency, or the reverse.

Everything is an atomic counter written on the request path. A metric that costs
a lock is a metric that changes the thing it measures, and on a gateway where
one request holds a slot for minutes, a lock on that path would be held for
minutes too.

Nothing here may carry text. Not prompts, not completions, not tool arguments,
not the model's filesystem path. Metrics are the easiest accidental route out
for exactly the content section 26 protects — they are aggregate, they look
harmless, and they are exposed at an endpoint whose whole purpose is to be
scraped by something else — so the types have nowhere to put text: every field
is a number, and the only strings are fixed label values known at compile time.
The model *id* appears, because it is the name the gateway already advertises at
`/v1/models`.

## Why cancellation is a `Drop` guard rather than a callback

The SSE response body owns a `RequestGuard` holding the job's cancellation
token and its scheduler permit. hyper drops the body when the client
disconnects; the guard's `Drop` cancels the token, which drops the upstream
`reqwest` response, which closes the TCP connection to the engine, which makes
the engine's own response reader cancel its task (`server-queue.h:218` at the
pinned build).

Nothing has to *detect* a disconnect, and no path can leak a slot, because
there is no path that skips `Drop`. Measured against the real engine: CPU time
consumed after the client walked away was **zero ticks**, and the next request
was served immediately.

## Why the prompt is counted by the engine

The pre-flight check that turns "this conversation outgrew the window" into a
parsable 400 needs an exact token count *with the model's chat template
applied*. The template lives in the GGUF and is applied by the engine, so the
count comes from the engine too — `POST /v1/chat/completions/input_tokens`,
which returns `{"input_tokens":N}` without generating anything.

Rendering the template a second time in Rust would be both a reimplementation
of what section 37 says to delegate and a source of silent drift: our count and
the engine's would disagree by exactly the tokens we got wrong.

## Why authentication is off by default, and forced when it matters

Hermes *always* sends an `Authorization` header, and when no key is configured
it sends the literal string `Bearer no-key-required`
(`agent/runtime_provider.py:1144`). A gateway that validated that would reject
every request from a correctly configured client.

The security boundary on a loopback bind is the network stack, not a token
nobody kept secret. So `AuthPolicy::Disabled` accepts any value and a missing
header alike — and `AuthPolicy::for_bind` refuses to start on a non-loopback
address without a real key, so exposure past this machine cannot happen by
accident.

## Why `max_tokens` is clamped and never rejected

Hermes defaults `max_tokens` to 65536 for a custom provider
(`agent/run_agent.py:1673`), which exceeds every context this gateway can
load. Rejecting it would break every request it makes, so the value is clamped
to `n_ctx - prompt_tokens - 1`, floored at one.

Flooring at one matters: a budget of zero produces an empty completion, and an
empty completion is exactly the `EmptyStreamError` that makes the client retry
blindly.

## Why the gateway knows nothing about the network it is on

The gateway serves whatever addresses it is told to, and draws exactly one
distinction: **loopback or not**. A LAN address, the shared-range address a
mesh VPN hands out, a unique-local IPv6 address and anything else are all the
same case, and `AuthPolicy::for_binds` is the whole of the policy — if any bind
is reachable from another machine, a key is required on all of them.

That is not indifference, it is the only design that survives contact with the
range of networks this has to run on: a plain LAN, Tailscale or Headscale,
Netmaker, ZeroTier, Nebula, a hand-rolled WireGuard, or nothing. Every one of
them is an interface holding an address. The moment a product name appears in
the code, the build has been fitted to one machine's network — the same mistake
as fitting it to one machine's CPU.

The consequence for configuration is that no address is ever written down here.
`--host` takes a name or an address, resolves it at startup, and may be
repeated, so a machine holding several addresses serves them all from one engine
and one queue. Binding by name is the better habit: an overlay can reissue an
address, and the name usually survives it.

## What the client's timeouts depend on, and why it is not our problem to fix

Hermes raises its stream read timeout from 120 s to 1800 s, and its
stale-stream window from 180 s to 900 s, for hosts it considers local — private
ranges, shared address space, unique-local IPv6, link-local, and any hostname
without a dot (`agent/model_metadata.py:906-956`). On a CPU where prefill takes
minutes, that difference decides whether long conversations work at all.

What does *not* qualify: any dotted FQDN, `.local` mDNS names included, and
routable addresses. So a deployment behind a TLS-terminating proxy or a MagicDNS
name is on the short timeouts, and has to set them explicitly.

This is a property of the client, not of the gateway, and trying to compensate
for it here would mean guessing at a client we do not control. It is documented
in the README instead, where the person choosing an address will see it.

## Why credentials avoid both `argv` and `Debug`

Two leaks of the same shape, closed the same way.

`/proc/<pid>/cmdline` is world-readable on an ordinary Linux system, so a key on
a command line is a key every local user can read. The engine's per-run key used
to be passed as `--api-key`; it travels in `LLAMA_API_KEY` now, which lands in
`/proc/<pid>/environ` and is readable only by the owner. Without that, any local
user could drive the engine directly and bypass the gateway's admission control,
context policy and auth entirely — the private port is only private in the sense
that nothing advertises it.

The gateway's own key had a subtler path out: `AuthPolicy` derived `Debug`,
`GatewayConfig` holds it, and `GatewayState`'s `Debug` prints its config, so a
single `tracing::debug!(?state)` would have written the key into a log file.
`AuthPolicy` now implements `Debug` by hand and renders `<redacted>`, which is
the same structural approach `hermes_core::Private` takes for prompts: the
guarantee holds whether or not the next person writing a log line remembers it.

## The gateway is a provider; the harness is a client

Worth stating because the code could be misread as being built *for* one
agent: it is not. This is an OpenAI-compatible model provider. It holds the
model, the memory budget, the context policy and the wire contract, and it has
no idea what is calling it — an agent harness, a chat UI, an editor plugin and
`curl` are the same thing from in here.

The evidence, rather than the assertion:

- No crate depends on a harness at build time or at run time. The only mentions
  anywhere are two doc comments citing where a fact was checked.
- The compatibility suite points the genuine `openai` package at the gateway,
  and one test additionally imports a real harness's error parser — because a
  provider is best checked by the code that will consume it. Both **skip
  cleanly** when that harness is absent, and the Rust suite never needs it.
- Everything the gateway knows about clients is generic: tolerant request
  parsing, the chunk sequence, `reasoning_effort`, the OpenAI error envelope.
  Where a decision was made because one real client behaves a particular way,
  the comment says so and names the file, so the reasoning can be rechecked
  against a different client later.

The relationship runs one way. A harness points its `base_url` at this gateway
and treats it as a model; the gateway serves it exactly as it serves anything
else. Nothing here is allowed to require that a harness exists, and nothing
here should ever be specialized to one — the moment it is, this stops being a
provider and becomes part of somebody else's agent.
