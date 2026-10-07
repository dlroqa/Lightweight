# R9.3.2 — Shared Pre-Commit Request Budget (design)

Status: **design FROZEN** (PR #45, merged as `790bbc1`). **Slice 1
(section 43) is implemented on `feature/router-shared-request-budget`, not
released.** The operator documentation is the [Pre-commit request
budget](ROUTER.md#pre-commit-request-budget-r932) section of `ROUTER.md`;
section 44 below records how the implementation read the points this design
left open. No frozen invariant was changed. No UI exists. A change to a
frozen invariant needs a new design review.

It builds on these frozen pieces: the R9.3 design
([R9_3_CROSS_ROUTE_FALLBACK.md](R9_3_CROSS_ROUTE_FALLBACK.md); its decision 6
deferred the shared budget to here), the R9.3.1 backend and the R9.3 UI.
**Nothing in this document changes an R9.3.1 rule.**

The feature is the **Pre-Commit Request Budget**: a shared, pre-commit time
budget for one client request. It is not a "request timeout". It does not
bound the response's lifetime, and after the response commits it does
nothing (section 12).

It answers exactly one question:

> **Is this client request still allowed to spend more time trying to reach
> response commit?**

It never chooses or scores a route, chooses a deployment, reorders a fallback
list, changes placement or replaces the R9.3.1 fallback policy.

## Frozen invariants

Every later section serves these. Each is a future test obligation; the test
ids (B*n*) refer to section 39.

| # | Invariant | Tests |
|---|---|---|
| I1 | **One client request = one absolute monotonic deadline**, created once and never reset, re-created or extended. | B3, B4, B5, B24 (M) |
| I2 | **The budget belongs to the client request.** It does not belong to a route, deployment, classifier provider, fallback attempt or node. | B3–B5, B27 |
| I3 | **Pre-commit only.** At response commit it stops governing the request. Post-commit behaviour is unchanged. | B20, B21, B40 |
| I4 | **Causal, not clock-based.** `request_budget_exhausted` is reported only when the deadline is the causal reason the request could not continue or reach response commit (section 7). | B23, B24, B12 |
| I5 | **Deterministic deadline/error precedence.** An operation result that is ready wins. The deadline wins only over an operation that has not completed (section 8). | B23–B25 |
| I6 | **Budget checked before starting new work.** No attempt of any kind starts with `remaining == 0`. | B9–B12 |
| I7 | **Existing per-operation limits remain**, each capped by the remaining budget: `min(own limit, remaining)`. | B13, B15 |
| I8 | **The classifier/provider timeout stays distinct from the overall deadline.** | B7, B8 |
| I9 | **Applies to explicit routes too.** Explicit routes still never cross-route fallback. | B16, B17 |
| I10 | **Neutral to R9.2.** Budget expiry is never route-quality evidence, never a scoring input, and never reorders a fallback. | B31–B33 |
| I11 | **Same-route and cross-route attempts share the one budget.** | B4, B5 |
| I12 | **Node queue / response-head wait is bounded by the remaining budget.** | B14, B15 |
| I13 | **Streaming/non-streaming asymmetry is intentional** (section 13). | B21, B22 |
| I14 | **`router_requests_total` counts one per client request**, with its own outcome `request_budget_exhausted` (never `server_error`), used only when the budget is causal. The label is the terminal attempted route when one exists. Before any attempt it is `Auto` or the client-named route; a route is never invented (section 34, frozen). | B28–B30 |
| I15 | **Upstream non-streamed cancellation must be measured** in implementation. The 504 must be prompt even if the upstream continues. | B38, B39 |
| I16 | **Config absent = disabled; `0` = invalid.** | B1, B41 |
| I17 | **R9.4 remains untouched.** | — |

---

## 1. Problem statement

```
one client request
  → classification (≤ 120 s)
  → initial route   × N deployments  (each: fresh 5 s connect, unbounded head)
  → fallback 1      × N deployments
  → fallback 2      × N deployments
  → fallback 3      × N deployments
```

A single request can gather many fresh time allowances:
- a classifier timeout;
- then up to four logical routes, each with a fresh connect timeout per
  deployment;
- an unbounded wait for every response head. For a non-streamed request, each
  head can include a node queue wait of up to 600 s that ends in a `503`.

No single number bounds how long a client waits before the router either
commits an answer or gives up.

## 2. Goals

- One optional, operator-set time budget per client request. It is one
  absolute deadline on a monotonic clock, set at one fixed start point. Every
  pre-commit stage shares it: classification, scoring, planning, every
  same-route deployment attempt and every cross-route fallback.
- When the budget is the reason the request cannot go on: stop, cancel the
  router's in-flight work, and answer with one documented error,
  `504 request_budget_exhausted`.
- When something else already ended the request: return that existing
  outcome, unchanged.
- Show budget exhaustion separately from client cancellation, route failure
  and classifier provider timeout, in traces, metrics and the admin view.
- With no budget configured: **exactly today's behaviour.**

## 3. Non-goals

- Choosing anything. The budget never ranks, scores, filters or orders a
  route, deployment or fallback.
- A post-commit stream deadline or a full response-lifetime timeout. That is
  a separate future feature (section 42), never combined with this one.
- Replacing any existing timeout. T1, T5 and T10 stay (section 4).
- A standalone inference timeout. T4 stays absent. Section 13 explains how a
  configured budget still bounds a non-streamed generation, explicitly and
  opt-in.
- Waiting for, triggering or steering placement (R7 unchanged).
- New fallback triggers. Budget expiry is **not** a fallback reason.
- Per-route, per-model or client-supplied budgets (deferred).
- Any change to R9.3.1, the R9.3 UI, or R9.4 / Mixture-of-Agents.
  R9.3.2 has nothing to do with parallel model execution, expert voting,
  aggregation, route-quality evaluation or speculative parallel fallback.

## 4. Current timeout inventory

Verified in code on master `49ce10d`. "Resets on fallback" means: does a
later deployment attempt or fallback route get a fresh full allowance?

| # | Timeout | Where | Default | Range | Owner | Per attempt? | Resets on fallback? | Before / after commit |
|---|---|---|---|---|---|---|---|---|
| T1 | **Connect** | `request.connect_timeout_secs` → `reqwest::Client::builder().connect_timeout` (`lib.rs`) | 5 s (`DEFAULT_CONNECT_TIMEOUT`) | ≥ 1 s, no maximum | router, shared client | yes, each TCP/TLS connect | **yes**, every deployment attempt on every route | before (connecting only) |
| T2 | Upstream **response head** | none | — | — | — | — | — | **unbounded today**: `attempt_one` awaits `send()` with no timeout |
| T3 | Upstream **body/stream read** | none in the router | — | — | — | — | — | unbounded; post-commit relay has no idle timeout |
| T4 | **Model inference** | none in the router or the gateway | — | — | — | — | — | absent on purpose: "a CPU prefill can take minutes, and a number chosen on a fast machine must not cut one off" (`config.rs`) |
| T5 | **Classifier** (both providers) | `auto_route.classifier.{jev,lightweight}.timeout_ms`, enforced by `tokio::time::timeout` in `classifier::classify` | **required**, no default | 1 – 120 000 ms (`MAX_TIMEOUT_MS`) | R9.1 | once per request | n/a, classification runs once | before |
| T5a | Jev | as T5 (`jev.timeout_ms`); dropping the future drops the HTTPS request | required | 1 – 120 000 ms | R9.1a | once | n/a | before |
| T5b | Lightweight classifier | as T5 (`lightweight.timeout_ms`); dropping the future drops the nested routed request (`proxy::forward_nested`) and its upstream connection | required | 1 – 120 000 ms | R9.1 | once | n/a | before |
| T6 | Jev **Test Connection** | `jev.timeout_ms` for `GET /v1/models` and its body (`jev::check`) | as T5a | as T5a | admin API | per check | n/a | admin path, not the request path |
| T7 | **Health probe** | `health.timeout_secs` / `health.interval_secs` | 3 s / 5 s | ≥ 1; timeout ≤ interval | health monitor | per probe | n/a | background, never on the request path |
| T8 | **Placement** load | `placement.load_timeout_secs`, `timeout_at` in `controller.rs`; control calls 10 s (`CONTROL_TIMEOUT`); poll 500 ms | 600 s | ≥ 1 | R7 controller | per load action | n/a | background; **the request path never waits for placement** |
| T9 | Session affinity idle TTL | `session_affinity.idle_ttl_secs` | 1800 s | — | R6 | — | — | not a request timeout |
| T10 | Gateway **queue wait** (node side) | gateway `queue_timeout` (`lightweight-gateway/src/state.rs`) | 600 s | gateway config | node | per request the node receives | **yes**, each node a request reaches queues it afresh | non-streamed: **before** commit (ends in `503 server_busy`, which the router fails over on). Streamed: **after** commit (in-band `server_busy`). |
| T11 | Gateway **SSE keep-alive** | `KEEP_ALIVE_INTERVAL` | 15 s | fixed | node | per stream | — | after commit; a ping, not a timeout |
| T12 | Gateway drain | `DRAIN_TIMEOUT` (`manager.rs`) | 600 s | fixed | node | — | — | model swap/shutdown, not a request |
| T13 | Request **body read** | axum `Bytes` extractor, default 2 MB body limit, no time limit | — | — | router HTTP layer | — | — | before the router's handler runs |

How each part of the lifecycle behaves today:

- **Router client request.** axum reads the whole body (T13). Then
  `proxy::forward_as` takes `received = Instant::now()` ("every router-side
  duration starts now") and parses the JSON. `routing_ms` starts after the
  parse (`planning_started`). Classification time is added back to
  `planning_started`, so `routing_ms` excludes it (R9.1). The request ends at
  the commit of a response head; the body or stream is then relayed.
- **Connect:** T1 only. A connect error or timeout counts against the node's
  health (`err.is_connect() || err.is_timeout()` in `proxy.rs`).
- **Upstream HTTP / time to response head:** unbounded (T2). A node that
  accepts the connection and never answers holds the request indefinitely.
- **Inference:** unbounded by design (T4). **Stream read:** unbounded (T3).
- **Classifier:** T5. On timeout the outcome is `timeout` and R9.1 uses the
  classifier's `fallback_route`.
- **Nested classifier request:** `forward_nested` → `forward_as(…, nested =
  true)`. It takes its **own** `received` today, and its request id is
  `{id}-classify`.
- **Same-route failover:** each deployment attempt gets a fresh T1 and an
  unbounded T2.
- **Cross-route fallback:** each route gets its full same-route behaviour
  again, for at most 1 + 3 = 4 routes (`MAX_FALLBACK_ROUTES`).
- **Client disconnect:** handled by ownership, not a flag. hyper drops the
  future, which closes the node connection. The `Tracker` guard records
  `outcome: "cancelled"` on drop.

**Independent timeout domains on the request path:**
- T1: per attempt, resets;
- T5: once, before routing;
- T10: node side, per node reached, resets;
- T13: HTTP layer.

Nothing bounds T2, T3 or T4. **There is no overall request deadline
anywhere.**

## 5. Architecture boundary

```
R9.1  classification ─┐
R9.2  scoring         │  all read ONE RequestBudget: "may this request
planning (R5, R6)     │  spend more time?"  None of them read it to decide
same-route attempts   │  WHAT to try, in WHICH order, or WHERE.
R9.3.1 fallback loop ─┘
───────────── response head commits ─────────────
relay body / stream: the budget no longer applies
```

The budget does two things:
- it is a **gate where new work starts** (section 19);
- it is a **cap on the waits in between** (sections 20–22).

It never changes the plan, the plan's order or its contents. No function that
decides a route, a deployment or a fallback order takes the budget as a
parameter. `scoring::decide`, `select::plan`, the selection policies and
`CrossRouteFallback::chain` keep their signatures.

## 6. Request-owned budget invariant

- **Created once**, in `proxy::forward_as`, for a non-nested (client)
  request, at the same moment as `received`. It is never created anywhere
  else.
- **Passed by value** (`Copy`) down the call chain, next to `request_id`:
  `route_request` → `resolve_auto` → `classifier::classify` →
  `attempt_route` → `attempt_one`.
- **The nested classifier request inherits it.** `forward_nested` gains a
  budget parameter. Its `forward_as` uses the parent's budget and creates
  none of its own. Its request id stays `{id}-classify`.
- **Never reconstructed** per route, per deployment, per fallback step, or
  after R9.1/R9.2. A fallback route gets only what is left.
- **No globals.** `RouterState` holds only the configured duration.

## 7. Causal exhaustion rule

> **Budget exhaustion is reported only when the deadline is the causal
> reason the request could not continue or reach response commit.**

The router answers `504 request_budget_exhausted` **if and only if**, when
the request ends without a commit, one of these holds:

- **(a) Cut:** a pre-commit wait was ended by the deadline before its
  operation completed (section 8). The operation was still able to produce a
  result, and the request could otherwise have kept waiting.
- **(b) Refused start:** a start check (section 19) refused a step that the
  frozen rules would otherwise have started now. The step is one of:
  classification, the initial route's first attempt, another planned
  deployment, or a qualifying cross-route fallback to a route still in the
  list.

In every other case the request's **existing outcome is returned
unchanged**, whatever the clock says. That includes a route error, a `500`, a
context overflow, a capability mismatch or an exhausted fallback list. There
is **no after-the-fact check**: a terminal outcome already decided is never
rewritten into a budget error because the clock passed the deadline while it
was being written.

Worked examples (budget 30.0 s):

| Situation | Result | Why |
|---|---|---|
| **A.** General is the final route; it returns `route_unavailable` at 29.9 s; the deadline passes at 30.0 s while the error is answered | `503 route_unavailable` | the route failure ended useful work; there was nothing left to start |
| **B.** General is still waiting for its response head at 30.0 s, uncommitted | `504 request_budget_exhausted` | the wait was cut (a) |
| Coder `route_exhausted` at 30.0 s; the fallback list still holds General | `504`; General attempts = 0 | refused start (b) |
| Coder `route_exhausted` at 30.0 s; the list is empty | Coder's existing error | nothing left to start |
| Coder's last deployment answers `500` at 30.0 s | the `500` | not a qualifying failure; terminal anyway |
| Head of a `503` arrived, its error body still being read at 30.0 s | `504` | the pre-decision body read is part of the operation (section 20); it had not completed |

The trace's `exhausted` is `true` only when the 504 was returned. A request
that ended with its own error after the deadline passed has `exhausted:
false`.

## 8. Deterministic error/deadline race semantics

An upstream operation and the deadline can become ready at almost the same
time. The outcome must not depend on async scheduling.

**Rule:**
- If the operation's result is available at the poll where the deadline is
  also due, **the operation result is used**.
- If the operation has not completed when the deadline is observed, **the
  request budget wins** and the operation is dropped.

**Primitive:** every bounded pre-commit wait is
`tokio::time::timeout_at(effective_deadline, operation)`.
- Verified in the locked tokio (1.53.1, `src/time/timeout.rs`):
  `Timeout::poll` polls the wrapped future **first** and the delay
  **second**, every time. "Both ready" therefore always resolves to the
  operation. This is the documented rule, not an accident of scheduling.
- tokio's cooperative-budget handling in that same `poll` still polls the
  delay when the inner future was starved. So a deadline is never missed
  because the operation used up the task budget.

**Forbidden:** an unconstrained `tokio::select!`, which picks a random branch
when several are ready. If a `select!` is ever needed on this path, it must
be `biased;` with the operation branch listed before the deadline branch.
That makes it equivalent to `timeout_at`. Code review rejects any other form.

**After the wait:**
- A failure the operation returned in time is processed normally.
- The next step then goes through the start check (section 19).
- At the start check the comparison is `now >= deadline`. Equality counts as
  expired, so `remaining == 0` never starts work.

**Classifier attribution tie** (section 15): if the provider deadline and the
request deadline are the same instant, the expiry belongs to the **request
budget**. Nothing could follow it anyway.

**Tests use controlled time:** `tokio::time::pause()` / `advance()`, and
operations completed by oneshot channels at exact instants.
- The operation completes at T − 1 ms → its result.
- The operation completes at exactly T, sent before the clock reaches T, so
  both are ready in one poll → its result.
- The operation completes at T + 1 ms → 504.
- Each case is repeated 1 000 times and must give the same answer every time
  (B25).

## 9. Absolute monotonic deadline model

```rust
/// One client request's pre-commit budget. Carried as
/// `Option<RequestBudget>`: `None` = not configured.
#[derive(Clone, Copy, Debug)]
pub struct RequestBudget {
    /// received + configured. Set once; there is no setter.
    deadline: tokio::time::Instant,
    /// The operator's number, for the trace and the 504 message only.
    configured: Duration,
    /// For `elapsed_*` in the trace only.
    started: tokio::time::Instant,
}
impl RequestBudget {
    fn start(configured: Duration) -> Self;      // called only in forward_as
    fn deadline(&self) -> tokio::time::Instant;
    fn remaining(&self) -> Duration {            // never negative
        self.deadline.saturating_duration_since(tokio::time::Instant::now())
    }
    fn expired(&self) -> bool { tokio::time::Instant::now() >= self.deadline }
}
```

- **One absolute instant.** Every stage derives its remaining time from it
  with `saturating_duration_since(now)`. **No `remaining_ms` is passed
  around** and no stage builds a deadline from a duration it was handed.
  This prevents an accidental reset, avoids rounding drift from repeated
  ms↔instant conversions, keeps the same deadline identity across every
  attempt, and makes tests simple: B3 asserts that the `deadline()` each
  stage saw is the same instant.
- **No separate overall deadline** ever exists for Coder, General,
  deployment A, deployment B or a fallback route. A per-operation
  *effective* deadline, `min(own limit instant, budget.deadline())`, is a
  local value for one wait and is never stored or passed on.
- **`tokio::time::Instant`** is monotonic (it wraps `std::time::Instant`). It
  is what `timeout_at` takes, and the placement controller already uses it.
  It also follows `tokio::time::pause`/`advance` in tests. Budget arithmetic
  never mixes it with `std::time::Instant`. `started` is taken with
  `tokio::time::Instant::now()` on the line next to `received`.

## 10. Budget start point

The budget starts **at `received` in `proxy::forward_as`**: after axum has
read the whole body, and before JSON parsing, classification and routing.

- `received` is already the origin of `router_request_duration_seconds`,
  TTFT and `duration_ms`. Router-side time keeps one start, not a second one.
- It precedes classification, so R9.1 time counts.
- JSON parsing (microseconds, bounded by the 2 MB body limit) counts. That is
  harmless and avoids a second start instant.
- **No existing metric is redefined.**
  - `routing_ms` still starts at `planning_started` and still excludes
    classifier time.
  - TTFT and `duration_ms` are unchanged.
  - The budget is a separate measurement with its own fields.

## 11. Body-read treatment

Body-read time (T13) is **outside** the budget in slice 1.
- The handler does not run until axum has the whole body, so the router can
  neither start nor enforce a budget during the upload.
- A slow uploader should not spend the router's budget.
- Bounding a slow upload is a separate HTTP-layer concern, and it stays
  deferred.

## 12. Pre-commit scope

The feature is a **pre-commit routing, recovery and generation budget**. It
is not a total response-lifetime timeout.

- It covers everything from `received` until a response head the router will
  relay is received (`Attempt::Committed`). That is the R9.3 commit point.
- It does not cover the response after that, under any circumstance.
- Naming follows from this:
  - config: `request.pre_commit_budget_ms`;
  - error code: `request_budget_exhausted`;
  - admin field: `governs: "pre_commit"`;
  - UI title: "Pre-Commit Request Budget".
- Nothing is labelled a plain "request timeout".

## 13. Streaming vs non-streaming asymmetry

Verified in the gateway, and **accepted for slice 1**:

| Request | When the node sends the head | What the pre-commit budget bounds |
|---|---|---|
| `stream: true` | **at once**; queueing happens inside the stream (`sse_stream::encode_queued`) | roughly the time until streaming starts: connect + head. Queue wait, prefill and generation are **post-commit** and **not** budgeted. |
| `stream: false` | **only after** queueing and the **whole** generation | connect + node queue wait + prefill + **the whole non-streamed generation** |

So for non-streamed requests the budget *is* a total generation bound. That
is the honest meaning of "pre-commit" with this node contract, and it is why
the budget is opt-in with no default value (section 29).
- The operator doc, the admin view and any future UI must say this in words.
- Slice 1 does not try to make the two cases symmetric.
- A post-commit stream/lifetime deadline is a **separate future feature**
  (section 42). It is never folded into this one.

## 14. Classifier interaction

- A start check comes before classification (section 19).
- The classify call is a bounded wait. Its effective deadline is the earlier
  of:
  - `received_at_classify + provider timeout_ms`;
  - `budget.deadline()`.

  It is applied with one `timeout_at`. A 120 000 ms provider timeout never
  outlives a 30 000 ms budget.
- The provider timeouts are **not replaced**. They remain the classifier's
  own safety bound.
- **Nested Lightweight classifier.** It inherits the budget (section 6). It
  runs its own start checks and caps with that same deadline, so it never
  starts a deployment attempt after expiry. If its own wait is cut, it
  answers the parent with `504 request_budget_exhausted`.
  - As a nested request it records **no** budget metric and no budget
    exhaustion of its own. The client request counts it once.
  - The parent maps a nested `504 request_budget_exhausted` to classifier
    outcome `request_budget_exhausted`.
  - When the parent's `timeout_at` drops the nested future first, the nested
    trace records `cancelled`, exactly as a provider timeout does today.
- **Jev:** one HTTPS call inside the same `timeout_at`. Dropping it drops the
  request.
- Classification time is consumed from the one budget. Routing after it sees
  only the remainder.

## 15. Provider timeout interaction

Which limit fired decides what happens next. The classifier outcome is
`request_budget_exhausted` **if and only if** one of these holds:
- the `timeout_at` fired and the request deadline was ≤ the provider
  deadline;
- the nested classifier answered `504 request_budget_exhausted`.

Every other outcome is R9.1's own.

| What fired first | Classifier outcome | Then |
|---|---|---|
| **Case A:** provider timeout, budget remains | `timeout` (unchanged) | R9.1 behaviour unchanged: the classifier's `fallback_route` is resolved. It is then subject to the start check before its first attempt, like any route. |
| **Case B:** request deadline | `request_budget_exhausted` (new value) | **Terminate.** No classifier fallback route, no new classifier attempt, no scoring, no planning, no routing work. 504, stage `classifier`. |
| Same instant | request budget wins (section 8) | as Case B |

**Once the budget is exhausted, no subsystem may create more work.** That
includes R9.1's fallback path, R9.2 scoring, R5/R6 planning, failover and
R9.3.1.

Budget exhaustion is not a provider failure:
- `router_classifier_requests_total` records it with outcome
  `request_budget_exhausted`, never `timeout` (its HELP list gains the
  value);
- `state.classifier_status` does not record it as a provider failure;
- provider-quality dashboards stay clean.

## 16. R9.2 interaction

- R9.2 runs inside the budget's lifetime, and its microseconds count. It
  never creates, resets, reads or scores the budget. `scoring::decide` keeps
  its signature. The remaining budget is never a scoring input (B32, M12).
- **History neutrality.** Budget expiry says nothing about route quality. The
  cause may be:
  - a short operator budget;
  - a slow classifier or network;
  - overloaded hardware or slower CPU inference;
  - node queueing;
  - time spent by earlier attempts.

  Therefore:
  - A route whose in-flight wait the budget **cut** gets the `Neutral`
    observation, the existing category for "the request or the client".
  - A route whose start the budget **refused** gets **no observation at
    all**, because it was never attempted. That covers the initial route in
    `route_planning` and a refused fallback route. A classifier-stage expiry
    observes no route. (Today `Tracker::finish` observes its current route
    unconditionally; the implementation must skip that for a refused route.) `Observation::of_outcome("request_budget_exhausted")` is already
    `Neutral` today: unknown outcomes fall to `_ => Neutral`. The
    implementation pins this with a test. It must never be mapped to
    `server_error`, which `Outcome::of_status(504)` would otherwise suggest.
  - A route the request had **already left** through a real R9.3.1 transition
    keeps the observation it earned when it was left. The same holds for a
    route whose own qualifying failure *completed* before a refused start,
    such as Coder `route_exhausted` with General refused. Both are recorded
    exactly as they would be with no budget. **History records what routes
    did, never what the clock did.**
  - Neutral is never scored. History is observational only (R9.2 final
    hardening). Budget state never raises or lowers route quality, never
    influences adaptive scoring and never affects a future route choice.

## 17. Same-route failover interaction

Every deployment attempt of a route consumes the **same** deadline:

```
budget 30 s
Coder/A  7 s  → remaining 23 s
Coder/B  6 s  → remaining 17 s   (route_exhausted)
General  receives 17 s, not 30 s
```

- Before each attempt in `attempt_route`'s loop, the start check runs. If it
  fails: take no lease, send nothing, and record no `router_failovers_total`
  increment for the unstarted attempt.
- During an attempt, the attempt is capped as in section 20.
- Context-overflow failover (R5) is a same-route attempt and obeys the same
  rule.
- The deployment order is the plan's; the budget never reorders it.

## 18. R9.3.1 interaction

Every frozen R9.3.1 rule is preserved:
- cross-route fallback is Auto-only;
- same-route failover comes first;
- the triggers are `route_unavailable`, `route_exhausted` and
  `route_capability_mismatch`;
- one flat list, selected once from the initial route, non-transitive;
- at most 3 fallback routes;
- explicit routes never cross-route fallback;
- a `500` and a context overflow do not fall back;
- there is no post-commit fallback;
- R5 capability filtering stays authoritative;
- affinity stays route-local;
- `response.model` is the final serving logical route.

The fallback loop in `route_request` gains exactly one check. It comes after
a qualifying failure and **before** `record_cross_route_fallback` /
`tracker.retarget(next)`. If the budget is expired, stop: 504, stage
`cross_route_fallback`, `next_unattempted_route = next`. **No transition is
counted to a route that was never attempted.** The budget decides only
whether the next step may start. It never decides which step, or whether a
failure qualifies.

Worked example:

```
budget 30 s
classifier 3 s · Coder/A 8 s · Coder/B 7 s          used 18 s, left 12 s
General/A 9 s (qualifying failure)                   used 27 s, left 3 s
Reasoning: starts, capped at 3 s                     not a fresh 30 s
if left = 0 after General: Reasoning NOT attempted, 504
```

## 19. Budget check before new work

**A hard invariant (I6).** Wrapping long operations in a timeout is not
enough. Before each significant new operation, the router checks
`budget.expired()`. If it is true (`remaining == 0`), **the operation is not
started**.

Start points:
1. before classification (Jev call or nested request);
2. inside the nested classifier request, before each of its deployment
   attempts;
3. before the initial route's first deployment attempt;
4. before each further same-route deployment attempt, including
   context-overflow failover;
5. before each cross-route fallback route (before the transition is
   recorded);
6. any future pre-commit upstream attempt, which must add a check here.

"Not started" means: no lease, no connection, no request sent, no transition
or failover metric, and no attempt entry in the trace.

Example: Coder is exhausted, the plan is `[General, Reasoning]`, and the
remaining budget is 0. The result is `504 request_budget_exhausted`, with:
- General attempts = 0 and Reasoning attempts = 0;
- trace `next_unattempted_route: "General"`;
- `router_cross_route_fallback_total{from="Coder",to="General"}` **not**
  incremented;
- `router_cross_route_fallback_exhausted_total` not incremented, because the
  list was not exhausted; time was.

There is **no minimum-remaining threshold** in slice 1. Any "don't start with
less than X ms" number would be tuned on one machine. With 50 ms left, an
attempt starts and is cut at 50 ms (deferred, section 42).

## 20. Per-attempt timeout interaction

Existing limits stay, and each is a safety ceiling capped by the budget:

```
effective limit = min(operation-specific limit, remaining overall budget)
```

- An operation **with** its own limit (T1 connect, T5 classifier) keeps it.
  Whichever is earlier fires.
- An operation **without** one (T2 response head, the pre-decision error-body
  read) gets `effective limit = remaining budget` before commit. Enabling the
  budget therefore bounds today's unbounded pre-commit head wait.
- One `timeout_at(budget.deadline(), …)` wraps each attempt's `send()`
  (connect + head) **and** the pre-decision read of a 4xx/5xx body. The
  router reads that body to tell `context_length_exceeded` or a refusal from
  other errors. It is part of the attempt's decision.
- The router gains **no** standalone per-attempt head timeout in slice 1.
  That would be a new independent timeout domain, and an inference timeout
  for non-streamed requests, which T4's rationale rejects.
- No useful existing ceiling is removed.

## 21. Connect timeout interaction

- T1 stays on the shared `reqwest::Client`. reqwest cannot change a client's
  connect timeout per request, and it does not need to: the attempt's
  `timeout_at(deadline)` gives `min(T1, remaining)` automatically.
- **T1 fires first:** reqwest reports `is_connect()`/`is_timeout()`. That is
  a deployment failure, counted against node health as today, followed by
  normal failover subject to the start check.
- **The deadline fires first:** the attempt is dropped and the result is
  budget exhaustion. It is **not** counted against node health, because the
  node did nothing wrong.
- **100 ms left, T1 = 5 s:** the request ends at about 100 ms with a 504. It
  never waits the full 5 s (B13, M15).

## 22. Node queue / response-head interaction

- A non-streamed request reaching a busy Lightweight node waits in the
  node's queue (T10, up to 600 s) **before** the node sends a head. The
  router sees only a long wait for the head.
- That wait is inside the attempt's `timeout_at(budget.deadline())`. The
  budget therefore bounds it **externally**, with no node cooperation.
  - Budget 30 s, node queue allowance 600 s: the maximum router-visible wait
    is the remaining budget, not 600 s.
- If the node's queue gives up first (`503 server_busy`) while budget
  remains, that is a normal same-route failover. The next deployment gets
  only the remainder.
- For a streamed request the queue is post-commit (section 13) and is not
  budgeted.
- The router does not forward its deadline to the node in slice 1. A node
  that refuses to queue past the router's deadline is deferred work.

## 23. Explicit-route scope

**The budget applies to all router client requests**, explicit routes
(`model: "Coder"`) and `Auto` alike. This differs from R9.3.1 on purpose:
- R9.3.1 cross-route fallback is a **routing policy**, Auto-only, because
  only `Auto` delegated the route choice;
- R9.3.2 is a **resource/time bound** on the request, and that reason does
  not depend on who chose the route.

An explicit `Coder` request still has multi-deployment failover, and each
deployment can hold it indefinitely (T2).
- An explicit request may end in `504 request_budget_exhausted`.
- It **never** gains cross-route fallback. Its fallback plan stays empty
  (`fallback_plan` is built only for `Auto`). The budget never touches that
  eligibility. Explicit semantics remain "that route or its error", and a
  budget 504 is the request's own error, not a route switch.

Nested classifier requests inherit the parent's budget and never configure
their own.

## 24. Response-commit boundary

The commit point is R9.3's: **the moment `attempt_one` receives a response
head the router will relay** (`Attempt::Committed`). From then on the budget
can never cause any of these:
- a route switch;
- a deployment retry;
- a cross-route fallback;
- answer splicing;
- a response restart.

- The non-streamed body read and `model` rewrite (`commit`) run with no
  budget, as does the stream relay (`relay`), exactly as today.
- No stream lifetime deadline is added.
- The deadline's timer is not polled after commit. The budget is not held by
  the relay at all.
- The trace records the budget state **at commit** (section 32).

## 25. Client cancellation

- The mechanism is unchanged: hyper drops the request future, ownership
  closes the node connection, and guards record `cancelled`.
- **Client cancellation is never budget exhaustion.** The deadline is a timer
  inside the request's own future. When the client leaves, that future is
  dropped and the timer never fires. The trace outcome is `cancelled` and no
  budget counter moves.
- A client that disconnects just as the deadline fires gets whichever the
  request future observed first:
  - if the future was dropped first, the outcome is `cancelled`;
  - if the deadline branch had already completed, the request is
    budget-exhausted and writing the 504 simply fails, as any response to a
    departed client does.

## 26. Upstream cancellation

What is guaranteed when the budget cuts a pre-commit wait:

| Phase | Guaranteed | Not guaranteed |
|---|---|---|
| connect pending | reqwest future dropped → connect abandoned immediately | — |
| waiting for head | connection closed; router lease released (RAII); client answered | the node noticing promptly |
| Jev call | HTTPS request dropped | Jev stopping its own work |
| Lightweight classifier | nested request dropped → its upstream connection closed | the classifier node stopping at once |

Dropping the router request closes the upstream connection.
- For a **streamed** request, upstream cancellation is proven: the node's
  writes fail.
- For a **non-streamed** generation, whether the node stops is **not
  established** by any test today.

This uncertainty does not block the design. It is an **implementation
acceptance requirement** (B38, B39):

```
start a slow non-streamed generation on a real gateway
        ↓
overall budget expires
        ↓
client receives the terminal 504 promptly
        ↓
inspect node/upstream behaviour (slot release, log, CPU)
```

| Outcome | Verdict |
|---|---|
| upstream computation cancelled promptly | **best** |
| client/router request terminates promptly; upstream generation continues briefly; the result is written in `docs/ROUTER.md` | **acceptable if documented** |
| the router waits for the full generation to finish before returning the budget error | **not acceptable**; the slice is not done |

## 27. Causal public 504 semantics

Options evaluated against the existing contracts:

| Option | Verdict |
|---|---|
| Final attempted route's error | Kept when the route's outcome already ended the request (section 7). Rejected when the budget stopped further work: it would claim, for example, `route_unavailable` (503, `Retry-After` = probe interval) when the cause was time. |
| `408 Request Timeout` | Rejected: it means the **client** was too slow sending the request. |
| `503` | Rejected: it already means `route_unavailable` (capacity) and the node's `server_busy`, and R9.3.1 trigger logic reads it as "unavailable". |
| **`504 Gateway Timeout`, `code: "request_budget_exhausted"`** | **Chosen.** A proxy that gave up waiting on upstream work is exactly a 504. OpenAI-compatible SDKs treat 5xx as retryable, which is right for a time bound. The envelope is the router's existing `server_error` shape. |

```json
HTTP/1.1 504 Gateway Timeout
{"error": {"message": "The router's pre-commit request budget of 30000 ms ran out before a response started.",
           "type": "server_error", "code": "request_budget_exhausted"}}
```

- No `Retry-After`: nothing says when a retry would be faster.
- The message names the configured budget only, never a node, model, route or
  prompt.
- The response carries the request's usual `X-Request-Id`.
- The 504 is returned **only** under section 7's rule. The same request id is
  used across all attempts and on the 504.

## 28. Config schema

The repository's conventions:
- `router.json`;
- `#[serde(deny_unknown_fields)]` sections;
- a `request` section that already holds `connect_timeout_secs`;
- the classifier's "required, choose a bound" stance on timeouts.

```json
{
  "request": {
    "connect_timeout_secs": 5,
    "pre_commit_budget_ms": 30000
  }
}
```

- **Name and location:** `request.pre_commit_budget_ms`.
  - The section is where request-path timing already lives.
  - The name states the scope, so nobody reads it as a streaming deadline.
  - Milliseconds, like the classifier's `timeout_ms`, because the two must
    compose.
- Type: `Option<u64>`. It is surfaced by `hermes router validate-config` and
  the admin endpoint.
- **Validation warning (not an error):** when a classifier `timeout_ms` is ≥
  the budget, `validate-config` warns that the classifier can consume the
  whole budget.
- **No hardcoded initial value.** 30 000 ms is only an example. The operator
  doc will say: measure your p99 time-to-head
  (`router_upstream_response_seconds`) and, for non-streamed traffic, your
  generation time; then choose.

## 29. Absence/zero semantics

| Value | Meaning |
|---|---|
| field **absent** | feature **disabled**: no budget, exactly today's behaviour |
| `0` | **invalid configuration**, refused at load: "`request.pre_commit_budget_ms` must be at least 1000" |
| 1 000 – 3 600 000 | the budget, in ms |

`0` is **not** "unlimited". Absence already means unlimited/current
behaviour. Two spellings for one meaning would make a typo silently disable
the bound. This matches `connect_timeout_secs` and classifier `timeout_ms`,
which also refuse 0. There is no `null`-as-disabled spelling; absence is the
only way to disable it.

## 30. Bounds

**1 000 ms minimum, 3 600 000 ms maximum**, both inclusive. 999 and
3 600 001 are refused.
- **Minimum:** rules out a value that could never fit a connect plus a
  classification, which in practice is a typo for seconds.
- **Maximum (1 h):** keeps the number meaningful. It still covers a long
  non-streamed CPU generation behind the gateway's 600 s queue.

Neither bound is a performance claim. Design inspection found no concrete
reason to change the suggested bounds.

## 31. Backward compatibility

- No `pre_commit_budget_ms` → the budget is `None`:
  - every check is unlimited, and no `timeout_at` wrap is installed;
  - responses, status codes, metrics (no new series values), traces (the
    `request_budget` field is omitted) and admin output are byte-identical.

  Test B1 proves this.
- Existing configs remain valid. The key is optional, and the section stays
  `deny_unknown_fields`, so a misspelt key is still refused.
- `routing_ms`, TTFT, `duration_ms`, `router_requests_total` semantics, the
  R9.3 metrics, the frozen R9.3 UI and the frozen classifier UI are
  unchanged. A new outcome value appears only when the feature is configured
  and fires. The frozen panels must render it without error (B44).

## 32. Trace schema

One `request_budget` block on `RoutingTrace`, present only when a budget is
configured. It holds timing and bounded enums only.

Committed:

```json
"request_budget": {
  "configured_ms": 30000,
  "elapsed_before_commit_ms": 27120,
  "remaining_at_commit_ms": 2880,
  "exhausted": false
}
```

Exhausted:

```json
"request_budget": {
  "configured_ms": 30000,
  "elapsed_ms": 30004,
  "remaining_ms": 0,
  "exhausted": true,
  "stage": "cross_route_fallback",
  "next_unattempted_route": "Reasoning"
}
```

Ended without commit by a non-budget outcome: `elapsed_ms` and
`remaining_ms` at the terminal decision, and `exhausted: false`. The trace
has no `stage` and no `next_unattempted_route`.

**`stage`** takes exactly four values, decided by where the request was:

| Stage | When |
|---|---|
| `classifier` | the classify wait was cut, the nested classifier answered `request_budget_exhausted`, or the start check before classification refused |
| `route_planning` | after classification (or explicit resolution), before the initial route's first deployment attempt was sent: R9.2 scoring, planning, the start check before the first attempt |
| `same_route_attempt` | the initial route had started: one of its attempts was cut, or the start check before its next deployment refused |
| `cross_route_fallback` | the start check before a fallback route refused, or a fallback route's attempt was cut or its next deployment refused |

**`next_unattempted_route`** is present **only** when a start check refused
a specific *logical route* that would otherwise have started now:
- the initial route, in `route_planning`;
- the next fallback route, in `cross_route_fallback` at the transition.

It is absent in these cases:
- an attempt was cut mid-flight (what would follow was never determined);
- only a further *deployment* was refused;
- exhaustion was at `classifier`.

It is never added to `cross_route_fallback.attempts` and never counted as a
transition.

Other trace effects:
- Trace `outcome` gains `request_budget_exhausted`. It sits beside `ok`,
  `client_error`, `server_error`, `unavailable`, `interrupted` and
  `cancelled`; `cancelled` keeps its meaning.
- With R9.3, exhaustion during fallback leaves
  `cross_route_fallback.exhausted = false`, and `final_route` = the last
  route actually attempted.
- **Classifier-stage exhaustion** has no logical route. A `RoutingTrace` is
  still recorded with `requested_route` and `route` set to the fixed `Auto`
  label (section 34), no attempts, the classifier block
  (`outcome: "request_budget_exhausted"`) and the budget block. No
  route-history observation is made for it.
- The request id is unchanged and shared by every attempt; the nested
  classifier request carries `{id}-classify` and the inherited deadline.
- Stage values never contain route names, ids or numbers. Per-attempt
  timings are not duplicated: `attempts[*]` already carries each attempt's
  head latency.

## 33. Metric schema

Low cardinality, new names only:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `router_request_budget_exhausted_total` | counter | `stage` (4 values) | client requests ended by the budget, once each |
| `router_request_budget_remaining_at_commit_seconds` | histogram | none | budget left when a budgeted request committed: the headroom operators tune by |
| `router_request_budget_configured_seconds` | gauge | none | the configured budget. **Absent when disabled** (frozen in review of PR #49): with no budget configured, none of the three budget series is exposed — never a `0` gauge, since `0` is an invalid configuration, not "off" |

- Units are seconds, the repository's convention for every router histogram
  and duration gauge (`router_request_duration_seconds`,
  `router_upstream_response_seconds`, …). The trace carries the millisecond
  values.
- No label may carry a request id, user, prompt, session, exact remaining
  milliseconds, node address or route.
- Not duplicated: `router_request_duration_seconds`,
  `router_routing_duration_seconds`, `router_upstream_response_seconds` and
  TTFT already exist (R6).

Effect on existing metrics:

| Metric | Effect of budget exhaustion |
|---|---|
| `router_requests_total` | once; section 34 |
| `router_cross_route_fallback_total` | no increment for an unattempted step |
| `router_cross_route_fallback_exhausted_total` | no increment (time ran out, not the list) |
| `router_failovers_total` | no increment for an unstarted deployment attempt |
| `router_classifier_requests_total` | outcome `request_budget_exhausted`, never `timeout` |
| `router_route_history_observations_total` | `neutral` for a cut route; nothing for a refused route (section 16) |
| node health | untouched |
| nested classifier request | records no budget metric of its own |

## 34. `router_requests_total` semantics

> **Frozen decision (approved after the design merged; recorded by PR
> "docs(router): freeze request budget outcome semantics").**
> `request_budget_exhausted` is its **own terminal outcome** of
> `router_requests_total`. It is **never** folded into `server_error`. It is
> used **only** when the pre-commit budget causally ended the request
> (section 7). R9.3.2 runtime is not implemented. No released version emits
> this value until the implementation ships.

The established rule holds: **one client request → one
`router_requests_total` observation.** It is never multiplied by deployment
attempts, same-route retries, cross-route fallbacks or classifier attempts.

**Outcome label:** a new value, `request_budget_exhausted`, alongside `ok`,
`client_error`, `server_error` and `unavailable`. The outcomes mean:

| Outcome | Meaning |
|---|---|
| `server_error` | an actual server or internal failure |
| `unavailable` | a route or deployment availability failure |
| `request_budget_exhausted` | the configured pre-commit request budget was the **causal terminal condition** |

Budget exhaustion is not necessarily a server fault. Its causes include:
- an operator-chosen budget;
- slow classifier execution or slow CPU inference;
- node queueing or network latency;
- slower hardware;
- several legitimate same-route and cross-route attempts sharing the budget.

So `server_error` would misreport it, and so would `unavailable`. This
follows the precedent of `unavailable`, which was split from `server_error`
because "capacity, not correctness, ran out".

**The outcome is causal, not clock-based** (section 7):
- **Example A:** General returns `route_unavailable` at 29.9 s, and the
  budget expires at 30.0 s afterwards. The outcome stays the route's normal
  outcome (`unavailable`), **not** `request_budget_exhausted`. A completed
  route result is never relabelled because the clock passed the deadline.
- **Example B:** General is still uncommitted, waiting for its response head,
  when the deadline expires. The outcome is `request_budget_exhausted`.

**Compatibility:**
- The value is additive. It appears only when the budget is configured and
  fires, so with no budget the series are unchanged.
- The metric's HELP text does not list outcome values and needs no change.
  The R9.3 UI's HELP lock check stays valid.
- Dashboards that sum `server_error` will **not** include budget exhaustion,
  which is intended. Operators should alert on it separately.

**Route-quality history is separate** (section 16). A
`request_budget_exhausted` request is neutral and unscored in R9.2 route
history. It never raises or lowers a route's score, never counts as a route
quality failure, never influences a future route choice and never affects
R9.3 fallback ordering. Metric observability and route-quality history are
separate concerns.

- **Route label:**

| When the budget stopped the request | `route` label | Why |
|---|---|---|
| a logical route had been attempted | **the terminal attempted route**: the last route actually attempted, including one whose in-flight attempt was cut | the R9.3.1 rule ("the one that served or last failed it") |
| a qualifying failure, then the start check refused the next fallback | the route that failed (e.g. `Coder`), **never** the refused next route (`General`) | the refused route was never attempted |
| an `Auto` request, before any route attempt began (`classifier` or `route_planning` stage) | **`Auto`** | existing convention: `resolve_auto` counts an Auto request that ends before a route takes it under the fixed `AUTO_ROUTE` label, "a fixed name, never one a client typed". No logical route is invented: neither the classifier's verdict nor its `fallback_route` nor the resolved-but-unattempted route |
| an explicit request, before its first attempt began (`route_planning` stage) | the route the client named | existing convention: an explicit request's pre-attempt refusals already count under the named route (the requirements refusal in `route_request`). The client chose it; nothing is invented. In practice reachable only under controlled time. |

Examples:

| Request | Budget expires | Counted |
|---|---|---|
| `Auto → Coder → General` | while General is uncommitted | `{route="General",outcome="request_budget_exhausted"}` once. **Not** Coder, **not** Auto, and no other entry. |
| `Auto` | before its first route attempt | `{route="Auto",outcome="request_budget_exhausted"}` once |
| explicit `model: "Coder"` | before the Coder attempt begins | `{route="Coder",outcome="request_budget_exhausted"}` once |

No route name is ever fabricated.

The existing `_unknown` label (`UNKNOWN_ROUTE`) stays reserved for names that
resolve to no route. It is never used for budget exhaustion.

## 35. Admin endpoint

`GET /api/router/v1/request-budget` is read-only and new. It is registered in
`api.rs` beside the other `/api/router/v1/*` reads, under the same
router-wide `AuthPolicy` as `/api/router/v1/auto`: loopback may run keyless,
and a non-loopback listener requires a client key.

```json
{
  "object": "router.request_budget",
  "configured": true,
  "pre_commit_budget_ms": 30000,
  "scope": "all_client_requests",
  "governs": "pre_commit",
  "exhaustions_total": 12,
  "exhaustions_by_stage": {"classifier": 1, "route_planning": 0,
                           "same_route_attempt": 7, "cross_route_fallback": 4}
}
```

- Disabled: `configured: false`, `pre_commit_budget_ms: null`, zero counts.
- No request-specific data, no secrets, no write endpoint.
- It has its own endpoint, not `/api/router/v1/auto`, because the scope is
  not Auto-only.

## 36. UI implications

**Nothing is built in R9.3.2 slice 1.** A future card, perhaps beside
Cross-Route Fallback on the router's Auto Routing screen or on an overview
screen:

```
Pre-Commit Request Budget               [Enabled]
Budget            30000 ms
Scope             All router client requests (explicit routes and Auto)
Governs           Until the response starts. Streamed answers: up to the
                  first response head. Non-streamed answers: the whole
                  generation, because the answer starts only when complete.
Expirations       12  (classifier 1 · same-route 7 · cross-route 4)
```

Like every router screen it would be snippet-only, with no config write API.
A trace row would read "budget exhausted, Reasoning not attempted". It must
never imply that the budget picks routes.

## 37. Security/privacy

The feature handles timing data only:
- durations;
- a bounded stage;
- route names already present in the trace.

No prompts, user text, credentials, session values or node URLs appear in
the trace block, metrics, admin view, log line or the 504 body. The admin
endpoint inherits the router's existing auth policy.

## 38. Performance

- Creation: one `Instant::now()` and an add.
- Start checks: one `Instant` comparison each, a handful per request.
- Caps: `timeout_at` on futures already awaited. That is a timer-wheel entry,
  with no polling loop and no extra task.
- Disabled: no timer is registered and no wrap is installed.
- The budget is never held or polled after commit.

## 39. Test plan

These are for the implementation; nothing is written now. Use
`tokio::time::pause()` with scripted upstreams wherever possible, so tests
are deterministic. Add real-gateway checks where the node's behaviour is the
question.

| # | Test | Expect |
|---|---|---|
| B1 | no config | current behaviour unchanged: statuses, bodies, traces (no `request_budget`), metrics (no new series values), admin `configured:false` |
| B2 | budget available, fast success | 200; `exhausted:false`; `remaining_at_commit_ms` > 0; histogram observed once |
| B3 | one absolute deadline | the same `deadline()` instant is observed by classify, every deployment attempt and every fallback route |
| B4 | no deadline reset across routes | Coder uses 2 s of 5 s; General is cut at ≤ 3 s, not 5 s |
| B5 | no deployment reset | Coder/A uses 3 s of 5 s; Coder/B is cut at 2 s, not 5 s |
| B6 | classifier consumes budget | a 2 s classification under a 5 s budget leaves routing ≤ 3 s; trace elapsed includes the 2 s; `routing_ms` still excludes it |
| B7 | provider timeout first, budget remains | classifier outcome `timeout`; R9.1 `fallback_route` used; the request proceeds normally |
| B8 | overall deadline first during classification | 504; stage `classifier`; **no** R9.1 fallback route, no new classifier attempt, no routing; classifier metric `request_budget_exhausted`, not `timeout`; `classifier_status` unchanged |
| B9 | check before the first deployment attempt | expired at resolution: zero attempts, zero leases, 504 stage `route_planning`, `next_unattempted_route` = initial route |
| B10 | check before a same-route retry | A fails at the deadline: B not started, no failover counted, 504 stage `same_route_attempt` |
| B11 | check before cross-route fallback | Coder fails qualifying at the deadline: General not started, 504 stage `cross_route_fallback` |
| B12 | expiry before General starts | General attempt count = 0, Reasoning = 0; `next_unattempted_route: "General"`; no `Coder→General` transition; no list-exhausted count |
| B13 | expiry during uncommitted connect | blackhole address, T1 = 5 s, 100 ms left: 504 at ~100 ms; node health not marked failed |
| B14 | expiry during uncommitted response-head wait | scripted node accepts and never answers: 504 at the deadline |
| B15 | node queue allowance > remaining budget | a node that holds a non-streamed request queued for 600 s: 504 at the remaining budget |
| B16 | explicit route enforcement | explicit `Coder` gets the same enforcement and 504 |
| B17 | explicit route never cross-routes | explicit `Coder` exhausted with a fallback list configured: Coder's own error or a budget 504, never General |
| B18 | `500` unchanged | `500` under a budget is relayed as today; no fallback; not rewritten |
| B19 | `context_length_exceeded` unchanged | same-route overflow failover and no cross-route fallback, as today, budget only gating starts |
| B20 | post-commit expiry | head commits at 4.9 s of 5 s, then 20 s of body: full answer relayed, no switch, `exhausted:false` |
| B21 | streaming | streamed head commits immediately; slow generation after it is never cut by the budget |
| B22 | non-streaming | non-streamed generation longer than the budget: 504 at the deadline (head never arrived) |
| B23 | error ready before deadline | operation fails at T − 1 ms with nothing left to try: its own error |
| B24 | deadline before completion | operation completes at T + 1 ms: 504 |
| B25 | deterministic race | paused time; operation and deadline ready in the same poll: operation result, 1 000/1 000 runs; no `select!` without `biased;` on the path |
| B26 | client cancellation ≠ exhaustion | client drops mid-attempt: trace `cancelled`, no budget counter, no 504 |
| B27 | same request id | one id on every attempt, every node log, the 504 and the trace; nested `{id}-classify` inherits the deadline |
| B28 | `router_requests_total` once | multi-deployment, multi-route expiry increments it exactly once |
| B29 | terminal-route label | `Auto → Coder → General`, expiry while General is uncommitted: `route="General", outcome="request_budget_exhausted"` |
| B30 | expiry before any route | Auto expiry during classification: `route="Auto"`; never the verdict, `fallback_route` or `_unknown`. Explicit `Coder` expiry before its attempt: `route="Coder"`. Outcome `request_budget_exhausted` in both. |
| B31 | R9.2 history neutrality | a cut route gets `neutral` only; a refused (never-attempted) route gets no observation; a completed qualifying failure keeps the observation it earns with no budget; never `server_error` |
| B32 | R9.2 scoring independence | identical scoring decisions for any remaining budget, including none |
| B33 | fallback order independence | identical fallback order and eligibility for any remaining budget |
| B34 | placement unchanged | budget configured + empty node: the request never waits for placement; no load triggered |
| B35 | trace fields | committed / exhausted / non-budget-error blocks exactly as section 32 |
| B36 | metric stage | exactly one `router_request_budget_exhausted_total{stage}` increment with the right stage, for each of the four stages |
| B37 | `next_unattempted_route` presence | present only for a refused initial or fallback route; absent for a cut attempt, a refused deployment and `classifier` |
| B38 | upstream non-streamed cancellation (real gateway) | measured and recorded in `docs/ROUTER.md`: does the node stop generating? |
| B39 | prompt 504 despite upstream | the client gets 504 within a small margin of the deadline even if the node keeps generating |
| B40 | no post-commit splicing | after commit, no second upstream request and no content from another route or deployment |
| B41 | config bounds | absent ok; `0`, 999, 3 600 001 refused; 1 000 and 3 600 000 accepted; classifier `timeout_ms` ≥ budget warns |
| B42 | nested classifier inherits | the nested request starts no deployment after expiry and records no budget metric |
| B43 | Jev classifier capped | Jev `timeout_ms` 120 000 and budget 2 000: dropped at 2 s |
| B44 | frozen UIs tolerate new values | R9.3 and classifier panels render with `request_budget_exhausted` in the counters and traces |
| B45 | node health untouched | budget-cut attempts never mark a node unhealthy |

## 40. Mutation plan

The implementation must show that each mutation is caught by at least one
test:

| # | Mutation | Caught by |
|---|---|---|
| M1 | reset full budget on each fallback route | B3, B4 |
| M2 | reset full budget on each deployment attempt | B3, B5 |
| M3 | create a new overall deadline in the classifier path | B3, B42 |
| M4 | ignore classifier time | B6, B8 |
| M5 | continue work after `remaining <= 0` | B9, B10 |
| M6 | start a fallback route though the budget is exhausted | B11, B12 |
| M7 | increment a fallback transition for a route never attempted | B12 |
| M8 | let a provider timeout mask an already-exhausted request budget | B8 |
| M9 | allow R9.1 fallback after overall budget exhaustion | B8 |
| M10 | allow post-commit route switching | B20, B40 |
| M11 | treat client cancellation as request budget exhaustion | B26 |
| M12 | use remaining budget as an R9.2 scoring input | B32 |
| M13 | use remaining budget to reorder fallback routes | B33 |
| M14 | count budget exhaustion as negative R9.2 route history | B31 |
| M15 | wait the full 5 s connect timeout when only 100 ms remain | B13 |
| M16 | wait the full node queue allowance when the budget expires earlier | B15 |
| M17 | report `request_budget_exhausted` after a normal route error already completed | B23, B18 |
| M18 | randomize the race result when operation and deadline are ready together | B25 |
| M19 | create a new request id on fallback | B27 |
| M20 | count multiple `router_requests_total` entries for one request | B28 |
| M21 | report the initial route instead of the terminal attempted route | B29 |
| M22 | invent a route label when the budget expires before the first route attempt | B30 |
| M23 | enforce the pre-commit budget after response head commit | B20, B21 |
| M24 | reset the budget after R9.1 or R9.2 | B3, B6 |
| M25 | remove the max bound, or allow `0` as unlimited | B41 |
| M26 | mark node health failed on a budget cut | B13, B45 |
| M27 | let explicit routes cross-route fallback when a budget is set | B17 |
| M28 | count budget exhaustion as `server_error` in `router_requests_total` | B29, B30 |

## 41. Implementation acceptance criteria

R9.3.2 slice 1 is done only when **all** of these hold:

1. Invariants I1–I17 hold, each covered by the tests named for it.
2. B1–B45 pass. B38/B39 ran against a real gateway, and their result is
   written in `docs/ROUTER.md` with one of section 26's acceptable outcomes.
   "Router waits for the full generation" fails the slice.
3. Mutations M1–M28 were applied one at a time and each was caught. The
   result is recorded in `docs/PROGRESS.md`.
4. With no config, behaviour is byte-identical (B1). Every existing router,
   classifier and R9.3.1 test passes unmodified. No test or lint was
   weakened.
5. No budget-reading code exists in `scoring`, `select`, the selection
   policies, `CrossRouteFallback::chain` or `placement`/`controller`.
6. No unconstrained `select!` on the request path.
7. `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D
   warnings` are clean. The full GitHub Actions matrix is green: `check.sh`
   on Linux x64, Windows x64, macOS x64 and arm64, Flatpak, Linux artifacts,
   render icons, render panel and the secrets gate. The frozen panels' render
   checks are unchanged and green.
8. A real-router smoke ran: the budget cuts a scripted slow node, and an
   explicit route and an Auto fallback chain both share one deadline.
   **Refused fallback start (two-layer evidence, approved in review of PR
   #49):** "The exact refused-fallback start condition must be proven
   deterministically in integration tests. The real binary must prove that
   an exhausted request budget cannot permit the next fallback route to
   execute or be counted." No timing hacks are used to force the exact race
   on the binary.
9. `docs/ROUTER.md` has an operator section with section 13's
   stream/non-stream table, the absence/zero/bounds rules and
   tuning guidance.

## 42. Deferred work

Explicitly **not** in R9.3.2 slice 1:
- post-commit stream lifetime deadline;
- full response-lifetime timeout;
- client-supplied deadlines;
- per-route budgets;
- per-model budgets;
- a minimum-remaining threshold before attempting a route;
- fallback ranking by remaining time;
- latency-aware route selection;
- gateway-side deadline propagation (forwarding the remaining budget so a
  node can refuse to queue past it), unless designed later;
- a request body-read deadline;
- the UI card (section 36);
- route quality scoring;
- R9.4 / Mixture-of-Agents.

## 43. Recommended first implementation slice

R9.3.2 slice 1, on `feature/router-shared-request-budget`, only after
explicit approval:

1. `RequestBudget` (section 9), created once in `forward_as`, passed by
   value, and inherited by `forward_nested`.
2. Config `request.pre_commit_budget_ms`: optional, 1 000 – 3 600 000, `0`
   refused, with the classifier-timeout warning; surfaced in
   `validate-config`.
3. Start checks at every point in section 19.
4. `timeout_at` caps: classify (min with the provider timeout, with the
   attribution of section 15) and each attempt's connect + head +
   pre-decision body read (section 20).
5. Terminal 504 `request_budget_exhausted` under section 7's causal rule and
   section 8's precedence.
6. The trace block and outcome; the `router_requests_total` outcome value
   and label rules (section 34); the three metrics; the classifier outcome
   value; `GET /api/router/v1/request-budget`.
7. Tests B1–B45, mutations M1–M28, the real-gateway cancellation measurement
   and a real-router smoke.
8. The `docs/ROUTER.md` operator section.

Not in the slice: UI, any post-commit deadline, client deadlines, any change
to R9.3.1, R9.4.

## 44. Implementation notes (slice 1)

How the implementation read points the design left open. None changes an
invariant.

| Point | Reading |
|---|---|
| `router_request_budget_configured_seconds` "0 when disabled" (section 33) vs "no new series values" with no config (section 31, B1) | Section 31 wins: with no budget, **none** of the three budget series is exposed. With one, the gauge is the configured value. **Approved and frozen** in review of PR #49; section 33 now says so. |
| The `504` envelope `type` vs the metric outcome | The envelope is `type: "server_error"`, the workspace's type for every 5xx (`ErrorKind::openai_type`) and the type of the router's own `503 route_unavailable` and `502 upstream_failed`. `router_requests_total` still counts `outcome="request_budget_exhausted"`, never `server_error`, and route history reads the trace outcome, not the envelope or the status, so the request stays `neutral`. |
| The hanging-connect tests (B13, M15) | Linux only: there a SYN to a full accept queue is dropped, so a connect hangs as one to an unreachable host does. Windows and macOS answer it with a reset (an ordinary refusal), so the hang cannot be made deterministic there. The cap is one code path for connect and head on every platform, and the head-wait, queue and non-streamed cut tests run everywhere. |
| The frozen R9.3 card on a fallback cut | The trace keeps `cross_route_fallback.exhausted: false` (time ran out, not the list), so the frozen card reads "Served by <route>" for such a trace although the request got a `504`. The backend semantics are as designed; the card's wording is a deferred follow-up for the budget UI work, not changed in slice 1. |
| `"pre_commit_budget_ms": null` | Refused like `0`. Absence is the only way to disable it (section 29). |
| The nested classification request in `router_requests_total` | Counted under its classifier route, as nested requests always have been, with outcome `request_budget_exhausted` when its own timer ended it. It records none of the three budget metrics (section 33). |
| A cut deployment attempt in `attempts[*]` | Row `outcome: "request_budget_exhausted"`, no status, no `response_ms`. |
| A cut fallback route in `cross_route_fallback.attempts` | Its row reads `outcome: "failed"`, `reason: "request_budget_exhausted"`. `exhausted` stays `false` and `final_route` is that route (section 32). |
| A refused transition (section 19) | The trace's `cross_route_fallback` block is written with the failed route as `final_route`, `exhausted: false`, and no row for the refused route. |
| Stage for a refusal of the initial route's first deployment found only after planning | `route_planning` with `next_unattempted_route`, as for the check before planning; the route is not observed and the label is `Auto` / the named route. |
| A fallback route whose first deployment is refused after its transition was counted (the clock passing during microseconds of planning) | Stage `cross_route_fallback`, no `next_unattempted_route`, labelled with that route (its transition was counted), not observed in history. |
| A committed non-success answer (a `500`, a non-model `404`, a `400`) | It commits as before: the trace records the budget at commit, and the remaining-at-commit histogram observes it. |
| Test hooks | `PhaseDelays.after_attempt_ms` (test-only, zero from a file) places "an attempt failed just as the budget ran out" exactly, beside the existing planning delays. |

## Appendix A. Questions resolved

| # | Question | Decision |
|---|---|---|
| 1 | Pre-commit or response lifetime? | **Pre-commit.** Lifetime is a separate future feature. |
| 2 | All requests or Auto only? | **All router client requests**; explicit routes still never cross-route. |
| 3 | Start point? | **`received` in `forward_as`**: after the body, before parse/classification/routing. |
| 4 | Does body read count? | **No.** |
| 5 | Does R9.1 count? | **Yes.** Classifier limit = min(provider, remaining); provider timeout ≠ budget. |
| 6 | Does R9.2 count? | **Yes** (µs); never an input; expiry is history-neutral. |
| 7 | Same-route / cross-route time? | **Shared**, one deadline, no reset. |
| 8 | At response commit? | The budget **stops governing**; the trace records remaining at commit. |
| 9 | When is it a 504? | **Only when causal** (section 7). |
| 10 | Race precedence? | **A ready operation wins**; `timeout_at` (operation polled first); no unbiased `select!`. |
| 11 | Representation? | **One absolute `tokio::time::Instant`**; remaining derived by `saturating_duration_since`. |
| 12 | Cap existing limits? | **Yes**: min(own limit, remaining); unbounded waits get the remainder. |
| 13 | Config? | **`request.pre_commit_budget_ms`**, absent = disabled, `0` invalid, 1 000 – 3 600 000. |
| 14 | `router_requests_total`? | **Frozen:** once; its own outcome `request_budget_exhausted` (not `server_error`), used only when the budget is causal; terminal attempted route; `Auto` (Auto) or the named route (explicit) before any attempt. |
| 15 | Trace stage vocabulary? | `classifier`, `route_planning`, `same_route_attempt`, `cross_route_fallback`. |
| 16 | Nested classifier request? | Inherits the deadline and start checks; records no budget metric; its 504 maps to classifier outcome `request_budget_exhausted`. |
| 17 | Upstream cancellation? | Router side immediate. Node side proven for streams; **measured for non-streamed in implementation** (B38/B39). |
| 18 | Error body read after a 4xx/5xx head? | Part of the bounded attempt; cut by the deadline → 504. |
