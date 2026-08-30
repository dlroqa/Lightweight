# M9 — the scheduler, and more than one client

Written against the source rather than against memory of it. Every claim below
about what the gateway did before this milestone was read out of `crates/` and
`frontend/src/` on the day it was written, and every number was measured on the
development machine rather than estimated.

M8 left the estimator's coefficients uncalibrated and said so. This milestone
does not close that; it closes something older and quieter. `--concurrency` has
existed since M5, has been documented since M5, and has never once worked
correctly — because the number it hands the engine does not mean what this
codebase thought it meant.

## 1. What was wrong before this milestone

| # | Where | What it did |
|---|---|---|
| 1 | `supervisor.rs:463,470` | Passed `--ctx-size C --parallel N`. The engine reads `-c` as the **total** and divides it: `n_ctx_seq = n_ctx / n_seq_max`. At four slots **every client got a quarter of the window** five surfaces advertised. |
| 2 | `estimator.rs:204-221` | Priced KV as `C × N` against an engine allocating `C` — **over-budgeting by the slot count**, in the safe direction and wrong. |
| 3 | `backend.rs:248-253` | `effective` was the *requested* params with the thread count patched in. **The engine's own numbers were never read back**, though `engine_props` and `with_engine_props` had both been written for it and had no callers. |
| 4 | `backend.rs:179`, `lightweight-backend-mock/src/lib.rs:232` | `max_concurrent_requests: 1`, hardcoded, served to the panel as a capability. **A lie the moment anybody passed `--concurrency 2`.** |
| 5 | `scheduler.rs:225-236` | `Waiter` carried an id, a band, a time and a channel. **No notion of a caller**, so one client's ten requests were served ahead of another client's one. |
| 6 | `scheduler.rs:427,548` and `stream.rs:387` | A non-streamed timeout counted **both** `timed_out` and `abandoned`; a streamed one counted **only** `abandoned`. Two counters describing overlapping sets. |
| 7 | `routes.rs:503,546` | `try_admit` then `enqueue`, two locks with a gap. A slot released **inside** the gap found an empty queue and went idle while a request was on its way into it. |
| 8 | `benchmark.rs:45` | `let _permit = state.acquire_slot(..).await;` discarded the `Option`. A queue timeout **ran the benchmark with no slot**, producing exactly the interleaved measurement the line above it says it prevents. |
| 9 | `main.rs:120-129` | `--concurrency` defaulted to a literal `1` — the one shape parameter never derived from the machine, while the context is fitted and the threads come from `CpuInfo`. |
| 10 | everywhere | **No test in the workspace had ever fired two concurrent HTTP requests at the gateway.** Every contention test manufactured its contention by stealing the single permit from the test body. |

Four of those are the product claiming something it does not do. That settles
the shape of the milestone.

## 2. Three stages

**Make one slot true, make N slots fair, then let the machine choose N.** Each
stage is shippable alone and leaves the tree green.

### M9.1 — the slot each client gets is the one it was promised

- **The context is multiplied at the `build_args` boundary.** `RuntimeParams.n_ctx`
  then means one thing everywhere above it — the window one client gets — which
  is what the overflow check, the clamp, the band ceilings, `/props`,
  `/v1/models` and the estimator all already assumed. Multiplying at the three
  sites that build params would be three places to disagree; redefining the
  field as a total would mean dividing at every site that advertises it.
- **`--no-kv-unified` and `--cont-batching` are stated**, per the module's own
  rule that every memory-shaping parameter is passed even when the default
  matches. Both defaults depend on how `--parallel` was given, and this argv
  never leaves it at auto. Neither changes behaviour; both stop a future engine
  build from changing it silently.
- **The engine's own `/props` is read once when it becomes ready**, and the
  recorded parameters are reconciled with it. A smaller window is adopted and
  warned about; a larger one is ignored, because admission control ran against
  the smaller number; a read that fails leaves the load standing, because
  trading a resident model for a metadata call is the worse answer.
- **Refusing the load was rejected.** A refusal is right when a pre-flight can
  catch the problem before the cost is paid, as `admit` does for KV types. It is
  wrong once several seconds and several hundred megabytes have been spent on
  something the gateway can simply describe correctly.

### M9.2 — the scheduler serves clients, not only requests

- **Identity is observed, never claimed.** `PeerKey` is the caller's address off
  the connection, where the kernel put it. That is the standard the bands are
  already held to — a priority that can be requested is a priority every caller
  requests — applied to identity. The IP alone, canonicalised: a port is unique
  per connection, so keying on one would give a client with four connections
  four identities and a client reusing one a single identity for a hundred
  requests.
- **It is never logged, never a metric label, never serialized, never stored.**
  A per-client label would be the first unbounded dimension `/metrics` has ever
  carried, and `metrics.rs:9-13` forbids text outright. The `Debug` impl is
  hand-written so that printing the queue cannot leak it.
- **Extraction cannot fail.** `ConnectInfo` rejects with a 500 on a server built
  without connect info, which would make adding the extractor a new way for a
  request to fail. `PeerKey`'s rejection is `Infallible`, so a router assembled
  without it degrades to one shared key and serves in the order it always did.
- **The sort key gains the caller's round**, and the round is zeroed once a
  request has aged out — otherwise the starvation guarantee would hold only
  within a caller. With one caller the rounds run 0, 1, 2 in arrival order, so
  the key is exactly the one it replaces. That is asserted rather than argued.
- **Admission became one locked decision**, and the two queue counters were made
  to mean what they say.

### M9.3 — let the machine choose, and prove it with clients

- **`--concurrency auto` is the default**, resolved from the machine's cores and
  from whether that many full-sized windows fit. The rule's constant is the
  subject of section 3.
- **The slot count follows the engine, not the command line.** `Scheduler`'s
  capacity became atomic and is re-derived on every load, beside the band
  ceilings and for the reason M6a gives for those: a number inherited across a
  swap is correct for the model it was computed for and wrong for the one being
  served.
- **A roster of what is running.** A running request was a bare `usize`, and
  `/api/v1/events` fires only when a generation *finishes* — so a gateway
  serving two clients for two minutes each looked, from outside, much like an
  idle one until they both finished at once. The roster lives in the scheduler,
  keyed by the permit, so "holds a slot" and "is listed" are the same fact.
- **The harnesses learned to hold two clients**: the mock gateway gained a slot
  count and a multi-thread runtime, and the contract suite gained two genuinely
  concurrent `openai` SDK clients driven from two threads.

## 3. What the measurements decided

`--concurrency auto` needed a divisor, and inventing one would have been the
confident wrong number this codebase refuses everywhere else. Benchmark run
`18ceefe0aa03aba1`, on the development machine — four cores, 1.5 GHz, no AVX —
swept one, two and four slots against SmolLM2-135M at 1024 tokens per client:

| slots | aggregate decode | per client | busy slots per decode | peak RSS |
|------:|-----------------:|-----------:|----------------------:|---------:|
| 1     | 3.95 t/s         | 3.95 t/s   | 1.00                  | 184 MiB  |
| 2     | ~4.8 t/s         | 2.39 t/s   | 1.30                  | 222 MiB  |
| 4     | ~4.9 t/s         | 1.23 t/s   | 1.90                  | 283 MiB  |

- **A second slot buys about a fifth more total throughput and costs each client
  close to half its speed.** A fourth buys nothing further and costs three
  quarters.
- **The reason is in the third column of the same run.** A *single* generation
  already kept 3.0 to 3.8 of the four cores busy. A second slot is not finding
  an idle core; it is taking one.
- **So `CORES_PER_SLOT` is four**, and on this machine the rule yields exactly
  one slot — which is what `--concurrency` has always defaulted to, now for a
  reason rather than by assumption. On a sixteen-core machine it yields four.
  That is the point of deriving it.
- **The engine does batch, and says so.** `busy_slots_per_decode` above one is
  the engine's own evidence that concurrent clients were served in one decode
  step rather than in turn; a live two-client run through the gateway measured
  1.267 against the sweep's 1.30.

Neither the rule nor the table is a product claim. They are facts about one
machine, which is why the rule is a divisor of that machine's core count rather
than the number it produced.

## 4. Deliberately left

- **Preemption**, for the reason `architecture.md:220-236` already gives: the
  engine cannot pause a generation, and killing one discards the prefill that
  dominates its cost.
- **Per-client rate limits or quotas.** Round-robin bounds how long a quiet
  client waits; it does not bound how much work a busy one does. A quota needs a
  policy about who the clients *are*, and this design deliberately keeps no
  identity at rest.
- **Fairness by credential.** `AuthPolicy` is one key or none; keying fairness
  on it would need a key *set*, which is an auth milestone.
- **`--kv-unified`.** Reachable is not free: a pooled cache is a shared cache,
  and two clients colliding in it looks like context truncation under load. It
  ships stated-off until a measurement asks for it.
- **An N-term in the estimator's compute model.** The sweep's peak RSS at four
  slots is within the existing model's over-estimate, so there is no residual
  asking to be fitted. Inventing a coefficient to describe it would be the thing
  M8 spent a section refusing.
- **A frontend test runner.** Still the milestone-sized decision M7 and M8 both
  declined to make; the gate remains `tsc` plus server-side tests pinning every
  field the panel reads.
- **Calibration**, which was the roadmap's M9. It moves to M10 with
  cross-platform, and `PROGRESS.md` is corrected rather than left to contradict
  the plan.

## 5. What must not be touched

Existing Prometheus metric **names** and their label sets — `timed_out` and
`abandoned` change in *value* because they were wrong, and no name and no label
changes; the `/health`, `/version`, `/props` and `/v1/models` bodies, save for
the `hermes.engine` object that `props.rs:81` has always defined and nothing
ever populated; the byte-exact golden SSE files; the `: queued position=N
waited=Ms` frame, which is pinned by one test and by three prose documents;
`clamp_max_tokens` and the `ContextOverflow` wording; `ALLOWED_KV_CACHE_TYPES`;
the rule that nothing in metrics or a benchmark record may carry text; and the
rule that no priority, and now no identity, can be claimed by a client.
