# M7 — the KV cache, the memory budget, and the context window

The approved plan for M7, written against the source rather than against memory
of it. Every claim below about what the gateway did before this milestone was
read out of `crates/` and `frontend/src/` on the day it was written.

M6b gave the product a face. What that face showed, in three places, was not
quite true — and in a fourth it showed nothing at all where the gateway had
something to say. M7 is about the three quantities that decide whether a model
runs on a CPU-and-RAM-only machine: how big the KV cache is, how much memory
there is to spend, and how large a window to open. All three were already
computed correctly in most cases. What was missing was that they be *honest*
about the cases they got wrong, and *reachable* by the person they constrain.

## 1. What was wrong before this milestone

Read against the source, not against the roadmap.

| # | Where | What it did |
|---|---|---|
| 1 | `lightweight-memory/src/estimator.rs:166-205` | A KV cache type this build cannot size was billed at **zero bytes** with an empty `missing` list, so the estimate reported `Coarse` — a confident zero. The per-token half of the same function separately assumed 16 bits, so the two halves disagreed. |
| 2 | `lightweight-system-info/src/memory.rs:58-64` | All three probe failures offered one remedy: "Set the available-memory override in settings". **No such setting has ever existed.** |
| 3 | `lightweight-gateway/src/control.rs:578-582` | A memory probe failure returned a header with no estimate and **no reason**. `manager.rs:484-486` wrapped the same failure in a generic I/O error, discarding both the code and the remedies. `lightweight-cli/src/main.rs:363` printed `to_string()` rather than `describe`, dropping the remedies at the last layer. |
| 4 | `lightweight-observability/src/targets.rs:21` | `targets::MEMORY` — "RAM estimation, admission verdicts and memory pressure warnings" — had **no call sites anywhere**. |
| 5 | `frontend/src/api/client.ts:81` | Read `remedy.message`. The wire field is `label` (`lightweight-core/src/error.rs:143`). Every remedy the gateway has ever sent rendered as a bullet list of `undefined`. |
| 6 | `lightweight-gateway/src/manager.rs:484-501` | A swap estimated memory **while the outgoing model was still resident**, though `backend.rs:186-199` stops the old engine before starting the new one. A swap was refused by roughly the outgoing model's footprint. |
| 7 | `manager.rs:487-500` vs `control.rs:601-606` | Two rules for which context a load gets, and they disagreed: the load path reads the stored default and never `last_n_ctx`; the detail endpoint reads `last_n_ctx` and never the stored default. The estimate on screen could be for a context no button produces. |

Two more, of the same family: `BackendCapabilities.kv_cache_types` is populated
(`lightweight-inference/src/backend.rs:77-83`) and serialized by no route, while
`control.rs:290` tells the user "/health lists what this engine accepts"; and
`resource_usage()` — the engine's own RSS, the one number that makes a `Coarse`
estimate checkable — has exactly one caller in the tree, in the CLI.

A tenth, found while verifying the milestone against a real engine rather than
while planning it: **`BackendError::InsufficientMemory` has no remedy arm at
all**, and `load_model` discarded the `Estimate` one line before building the
error — so the single most important refusal in the product arrived with nothing
actionable attached. The estimator had computed "reduce the context to N",
"quantize the KV cache, saving X" and "free Y" and thrown all three away.

Six of these ten are the product telling the user something untrue. That settles
the shape of the milestone.

## 2. Three stages

**M7.1 fixes what is untrue and adds no surface. M7.2 fixes what is measured
wrongly. M7.3 adds controls, and only controls that change something.** Each
stage is shippable alone and leaves the tree green.

### M7.1 — say the true number

- **The estimator becomes one fallible pass.** `kv_cache_bytes` is written as a
  single `Result` closure so both halves of the arithmetic reach for the same
  geometry and there is no fallback left for either to disagree with. Four
  silent-zero `unwrap_or`s disappear, and `ggml.rs:365-368`'s claim that `None`
  "propagates a partial confidence rather than letting a zero slip into a sum"
  becomes true as written.
  `GgmlType::from_name` rejects unknown names, so **this is an invariant repair,
  not a live bug** — worth doing because a doc comment asserts the behaviour.
- **Per-variant memory remedies**, each naming what could not be read and
  pointing at `--force`. A new `RemedyAction::ForceLoad` carries it. An
  available-memory override was considered and rejected: it would let a user
  feed the estimator a number nobody measured, which is exactly the confident
  wrong verdict the platform stub refuses to produce.
- **A probe failure becomes sayable.** `ModelDetail.estimate` becomes
  `Probed<Estimate>`, reusing the type M6b.1 built for `/api/v1/system`;
  `ManagerError` gains `MemoryProbe`, delegating code, kind and remedies as it
  already does for `Catalog` and `Backend`; the CLI uses `describe`.
- **`targets::MEMORY` gets its call sites** — one per admission decision, in the
  manager and in `serve`. `Tight` logs at WARN, so an OOM kill twenty minutes
  later has its antecedent in the same file.
- **The panel reads `label`**, and carries the whole remedy object so a later
  stage can offer a button for the action.

### M7.2 — spend the right budget

- **An injectable memory probe on `GatewayState`.** `MemoryProbe` exists as a
  trait so tests can supply fixed numbers, and `FixedMemoryProbe` ships for it;
  the gateway was the only place constructing `SystemMemoryProbe` inline. Without
  this, nothing below is testable without depending on how much RAM the box had.
- **Credit what a swap actually releases** — `RssAnon`, not `VmRSS`. No
  `--no-mmap` appears anywhere in `crates/`, so llama.cpp mmaps the GGUF and most
  of `VmRSS` is file-backed page cache `MemAvailable` already counts. Crediting
  `VmRSS` would double-count the weights, which is an *optimistic* error — the
  one direction `memory.rs` forbids. `RssAnon` is the KV cache, the compute
  buffers and the engine baseline: anonymous, in no file LRU, genuinely returned.
  **No re-check after the old engine exits**: that means unloading before
  deciding, and a refusal would leave the user with an empty gateway in exchange
  for a model they asked to add.
- **Engine RSS published as a reading, not a rate**, through `metrics_snapshot`.
  A pull reads `/proc` once and answers — no sampler, no interval, no retained
  state, per the argument in `lightweight-system-info/src/load.rs:10-21`. `cpu_percent`
  is deliberately not serialized: nothing produces it, and a percentage from one
  sample is the invention that module exists to refuse.
- **`Verdict::Tight` is surfaced, not gated.** `largest_safe_context` only ever
  returns `Safe`, so `Tight` arises only from an explicit context or a stored
  default — always from something a person chose. That makes it a fact to report,
  not a policy to enforce.

### M7.3 — controls that change something

- **One rule for the context a load gets.** `choose_context` in the manager,
  used by both the load path and the detail endpoint, so the asymmetry becomes
  structurally impossible rather than fixed twice. `last_n_ctx` becomes history
  that is shown and does not steer: honouring it would silently disable the
  scale-with-the-machine behaviour that is the whole argument of
  `estimator.rs:796-807` — a model loaded at 32K when 8 GiB was free would keep
  asking for 32K at 2 GiB and be refused where auto-fit would have fitted.
- **The legal KV types on `GET /api/v1/gateway`** — not `/health`, which is
  probed by health checks and by the desktop shell, and which `control.rs:610-613`
  states outright is left alone.
- **Estimates for options the caller is weighing**, as query parameters on the
  existing detail endpoint. Absent, the response is byte-identical to today.
  Client-side arithmetic was rejected: changing the KV type changes bytes per
  token, and scaling that in TypeScript means a second implementation of ggml
  block geometry waiting to disagree with what the engine allocates.
- **The panel gets the two controls the estimator already recommends.**
  `Estimate::remedies()` has always said "Reduce the context to N tokens" and
  "Quantize the KV cache to q8_0, saving about X" — two remedies the panel could
  not act on. It also learns to **follow the job**: a refused load returns 202 and
  fails inside the job, which nothing in `frontend/src` consumed, so
  `InsufficientMemory` was invisible.

## 3. Deliberately left

- **No calibration.** Fitting `ComputeModel` from observed peak RSS is M9, and
  `benchmarks/` stays empty. M7 makes peak RSS *visible*; it does not consume it.
  `Confidence::Measured` remains unreachable in production until then.
- **No global default KV type.** `default_n_ctx` earns its place because context
  is bounded by memory and the estimate still judges it. KV type trades output
  quality, which no estimate judges, and a stored default would shadow the CLI's
  `--kv-type` with a precedence rule nobody asked for.
- **No batch-size control.** Still true, and for the reason
  `frontend/src/screens/Inference.tsx` gives: the engine is always given the
  `RuntimeParams` defaults and no request or load option can vary them.
- **No background memory sampler**, and no retained time series. That is the one
  thing in M7's neighbourhood that would need a new dependency, and
  `load.rs:10-21` already argues against it.
- **No frontend test runner.** Adding one is a milestone-sized decision, not a
  line item. Coverage for panel changes is `tsc --noEmit` plus server-side tests
  that pin every field the panel reads.

## 4. What must not be touched

`/v1/models` row shape and the `hermes.model_max_context_length` name;
`clamp_max_tokens` semantics and the `ContextOverflow` wording, which a real
client parses a number out of; `/health`, `/version` and `/props` bodies;
the byte-exact SSE golden files; `ALLOWED_KV_CACHE_TYPES`, which is read from
`llama-server --help` at the pinned build rather than edited; existing Prometheus
metric names; and `Estimate::remedies()` returning empty for admissible verdicts.
