# M8 — benchmarking, metrics, and CPU optimization

The record of M8, written against the source rather than against memory of it.
Every claim below about what the tree did before this milestone was read out of
`crates/` and `frontend/src/` on the day it was written, and every number was
measured on the machine it names.

M7 left a gateway that is honest about memory. It was not yet honest about
**time**, and the knobs that decide how fast it runs could not be reached.

## 1. What was wrong before this milestone

| # | Where | What it did |
|---|---|---|
| 1 | `lightweight-backend-llamacpp/src/backend.rs:421` | `ResourceSnapshot.cpu_percent` was set to `None` by the only code that produced it and serialized by nothing. On a product whose premise is CPU inference, **no surface reported how much processor the engine used.** |
| 2 | `lightweight-gateway/src/metrics.rs:121-143` | `Tally` kept count, sum and max. No p95 of anything was recoverable, on a gateway where one slow request in fifty is the entire complaint. |
| 3 | `lightweight-backend-llamacpp/src/supervisor.rs:482-484` | `--metrics` was passed to the engine under a comment saying it was "for the metrics provider to scrape". **Nothing had ever scraped it.** |
| 4 | `lightweight-gateway/src/scheduler.rs:184-211` | `QueueSnapshot.wait_ms_total` and `wait_ms_max` were carried in JSON and rendered to Prometheus nowhere. |
| 5 | `lightweight-system-info/src/paths.rs:147` | `benchmarks_dir()` was chosen in M0 and created at every startup. **Nothing had ever written to it.** |
| 6 | `lightweight-memory/src/estimate.rs:96-108` | `ComputeModel`'s doc comment said "the benchmark harness fits them from observed peak RSS". There was no harness. |
| 7 | `lightweight-core/src/runtime.rs` | `n_batch` and `n_ubatch` were passed to the engine and varied by no load path. `--threads-batch`, `--poll`, `--cache-reuse`, `--load-mode` and CPU affinity appeared nowhere in `crates/`. |
| 8 | `lightweight-gateway/src/system.rs:82` | `CpuReport.thread_choices` was served and read by no screen, so the panel could not set a thread count the HTTP API already accepted. |

Three of those are the product claiming something it does not do. That settles
the shape of the milestone: **instruments, then measurements, then the knobs the
measurements justify.**

## 2. Three stages

### M8.1 — make the CPU visible

- **`cpu_percent` becomes `cpu_ticks`**, read from `/proc/<pid>/stat` and
  published unconverted, for the reason `load.rs:68-72` already gives about
  `/proc/stat`: a rate needs two readings and an interval, and converting would
  divide by a `USER_HZ` no crate here can honestly guess. The parser counts
  fields from the **last** `)`, because the second field is the executable name
  and the kernel does not escape it.
- **The panel turns two counters into cores.** `/proc/stat`'s total is summed
  over every core, so dividing it by the core count gives what one core could
  have spent in the same interval; the engine's ticks over that is a count of
  cores, with the tick rate cancelled rather than assumed.
- **`Tally` gains buckets**, stored non-cumulatively (one atomic per
  observation) and accumulated at scrape time. The ladder ends at two minutes
  because a web-shaped one ending at ten seconds would put every prefill on a
  CPU without AVX into `+Inf`.
- **The engine's `/metrics` is scraped**, once per pull, beside the `/proc`
  read. Only counters the gateway cannot compute for itself are lifted.

### M8.2 — the harness that measures

- **`lightweight-bench`**, a runner over the `InferenceBackend` trait and nothing
  else, so one implementation serves a CLI sweep and a gateway quick run and is
  testable against the mock with no engine.
- **Three scenarios**, deterministic, with prompts sized by asking the engine's
  own tokenizer.
- **Runs are saved** to `benchmarks_dir()`, owner-only, one document each, with
  a machine and engine fingerprint — because a throughput figure without them is
  not a measurement of anything.
- **`--fit` writes what the data supports and no more.** See §3.
- **Two surfaces, deliberately unequal**: `hermes bench` brings its own engine
  and reloads per bucket; `POST /api/v1/benchmarks` measures what is resident
  and changes nothing.

### M8.3 — the knobs

Six parameters reachable, every one absent by default. `n_ubatch` priced by
`?ubatch=`. `--load-mode` given a pre-flight against `RLIMIT_MEMLOCK`, a swap
credit that counts what is locked, and a refusal on `Tight`. The panel reads
`thread_choices` at last.

## 3. Why the fit fits a slope and not two coefficients

The estimator's compute term is
`vocab*ub*4 + activation*ub*embd*4 + scratch*ub*max(embd,ffn)*4` — two free
coefficients, both proportional to `n_ubatch`. From peak RSS alone they are
**collinear**: samples at one ubatch determine neither, and samples across
several determine only their sum.

So the fit is a slope and an intercept, and it says so. Reporting two
coefficients of which one is invented would be exactly the confident wrong
number this codebase refuses everywhere else. A hundred repetitions at one
ubatch is one point, and the fit returns no slope at all for it.

## 4. What the measurements decided

Two decisions in this milestone were made by a number rather than by argument.

- **The physical batch earns its control.** At 256 prompt tokens on the
  development machine, ubatch 512 prefills at 22 t/s against 11-15 t/s at 128,
  with 3.9 of 4 cores busy against 2.4.
- **The gateway's worker threads are left alone.** During a real generation the
  gateway was charged 2 ticks against the engine's 3266 — 0.06% — while the
  engine kept 3.21 of 4 cores busy. Idle tokio workers park on epoll rather
  than spinning, so capping them would save nothing.

Neither number appears in the README. They are facts about one machine.

## 5. Deliberately left

- **M9 wires the fit on.** M8 produces `calibration.json`; deciding when a fit
  is trustworthy — how many buckets, how close a fingerprint must match, when
  `Confidence::Measured` is earned — is the policy question, and it belongs with
  the milestone that consumes the data rather than the one that produces it.
- **`--numa`.** It needs a second NUMA node to mean anything, wrong settings
  degrade silently, and llama.cpp requires it to agree with how the process was
  launched. Nothing here can exercise it.
- **Defaults that need SMT or hybrid cores.** `--threads-batch > threads` and
  P-core pinning are the two real wins on hardware this machine does not have.
  The knobs ship; the defaults wait for a machine that can establish them.
- **`--cache-reuse` stays off.** It is reachable and measurable; turning it on
  by default trades output fidelity through KV shifting, which no estimate
  judges — the same argument M7 used against a stored default KV cache type.
- **No background sampler and no server-side history.** Counters and per-pull
  readings only, as `load.rs:10-21` argues.
- **No frontend test runner.** Still a milestone-sized decision.

## 6. What must not be touched

Existing Prometheus metric **names** and their values — M8 changed a `# TYPE`
from `counter` to `histogram` and added `_bucket` series, and changed no name
and no number; the `/health`, `/version`, `/props` and `/v1/models` bodies; the
byte-exact SSE golden files; `clamp_max_tokens` and the `ContextOverflow`
wording; `ALLOWED_KV_CACHE_TYPES`; the rule that nothing in metrics or in a
benchmark record may carry text.
