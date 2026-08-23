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
