# R9.3.2 — Shared pre-commit request budget (design)

Status: **design only; nothing implemented.** No runtime code, configuration,
metric, trace field, admin field or UI exists for anything in this document.
Implementation needs explicit approval of section 31 (decisions) and section 33 (first slice).

It builds on the frozen R9.3 design ([R9_3_CROSS_ROUTE_FALLBACK.md](R9_3_CROSS_ROUTE_FALLBACK.md),
decision 6 deferred the shared budget to here), the frozen R9.3.1 backend and
the frozen R9.3 UI. **Nothing in this document changes an R9.3.1 rule.**

The rules everything below obeys:

> **The budget belongs to the client request** — not to a route, a
> deployment, a fallback step or a classifier call. One client request has
> one request id and one deadline, from the first routing decision to the
> response commit.
>
> **The budget is not a routing signal.** It answers only "may this request
> keep trying?". It never chooses a logical route, a deployment or a
> fallback route, and is never an input to R9.1, R9.2, a selection policy or
> fallback order.
>
> **The response commit still wins.** Once a response head is committed the
> budget stops governing the request. It never switches, restarts or splices
> a committed response.

---

## 1. Current timeout inventory (master `49ce10d`, verified in code)

Every timeout that exists today, where it lives and how it behaves. "Resets
on fallback" means: does a later deployment attempt or fallback route get a
fresh full allowance?

| # | Timeout | Where | Default | Range | Owner | Per attempt? | Resets on fallback? | Before / after commit |
|---|---|---|---|---|---|---|---|---|
| T1 | **Connect** | `request.connect_timeout_secs` → `reqwest::Client::builder().connect_timeout` (`lib.rs`) | 5 s (`DEFAULT_CONNECT_TIMEOUT`) | ≥ 1 s, no maximum | router, shared client | yes — each TCP/TLS connect | **yes**, every deployment attempt on every route | before (connecting only) |
| T2 | Upstream **response head** | none | — | — | — | — | — | **unbounded today**: `attempt_one` awaits `send()` with no timeout |
| T3 | Upstream **body/stream read** | none in the router | — | — | — | — | — | unbounded; post-commit relay has no idle timeout |
| T4 | **Model inference** | none in the router; none in the gateway | — | — | — | — | — | deliberately absent: "a CPU prefill can take minutes, and a number chosen on a fast machine must not cut one off" (`config.rs`) |
| T5 | **Classifier** (both providers) | `auto_route.classifier.{jev,lightweight}.timeout_ms`, enforced by `tokio::time::timeout` in `classifier::classify` | **required**, no default | 1 – 120 000 ms (`MAX_TIMEOUT_MS`) | R9.1 | one per classification (one per request) | n/a — classification runs once | before |
| T5a | Jev | same as T5 (`jev.timeout_ms`); dropping the future drops the HTTPS request | required | 1 – 120 000 ms | R9.1a | once | n/a | before |
| T5b | Lightweight classifier | same as T5 (`lightweight.timeout_ms`); dropping the future drops the nested routed request (`proxy::forward_nested`) and its upstream connection | required | 1 – 120 000 ms | R9.1 | once | n/a | before |
| T6 | Jev **Test Connection** | `jev.timeout_ms` for `GET /v1/models` and its body (`jev::check`) | as T5a | as T5a | admin API | per check | n/a | admin path, not the request path |
| T7 | **Health probe** | `health.timeout_secs` / `health.interval_secs` | 3 s / 5 s | ≥ 1; timeout ≤ interval | health monitor | per probe | n/a | background, never on the request path |
| T8 | **Placement** load | `placement.load_timeout_secs`, `timeout_at` in `controller.rs`; control calls 10 s (`CONTROL_TIMEOUT`); poll 500 ms | 600 s | ≥ 1 | R7 controller | per load action | n/a | background; **the request path never waits for placement** |
| T9 | Session affinity idle TTL | `session_affinity.idle_ttl_secs` | 1800 s | — | R6 | — | — | not a request timeout |
| T10 | Gateway **queue wait** (node side) | gateway `queue_timeout` (`lightweight-gateway/src/state.rs`) | 600 s | gateway config | node | per request the node receives | **yes** — each node a request reaches queues it afresh | non-streamed: **before** commit (ends in `503 server_busy`, which the router fails over on). Streamed: **after** commit (in-band `server_busy`). |
| T11 | Gateway **SSE keep-alive** | `KEEP_ALIVE_INTERVAL` | 15 s | fixed | node | per stream | — | after commit; a ping, not a timeout |
| T12 | Gateway drain | `DRAIN_TIMEOUT` (`manager.rs`) | 600 s | fixed | node | — | — | model swap/shutdown, not a request |
| T13 | Request **body read** | axum `Bytes` extractor, default body limit (2 MB), no time limit | — | — | router HTTP layer | — | — | before the router's handler runs |

Behaviour per lifecycle element:

- **Router client request lifecycle.** axum reads the whole body (T13), then
  `proxy::forward_as` takes `received = Instant::now()` ("every router-side
  duration starts now"), parses the JSON, and `routing_ms` starts after the
  parse (`planning_started`). Classification time is added back to
  `planning_started`, so `routing_ms` excludes it (R9.1). The request ends at
  the commit of a response head, then the body or stream is relayed.
- **Connect.** T1 only. A connect timeout counts against the node's health.
- **Upstream HTTP / time to response head.** Unbounded (T2). A node that
  accepts the connection and never answers holds the request indefinitely.
- **Model inference.** Unbounded by design (T4).
- **Stream read.** Unbounded (T3). The node's own keep-alive (T11) keeps a
  quiet stream alive.
- **Classifier / Jev / local classifier.** T5. On timeout the outcome is
  `timeout` and R9.1 falls back to the classifier's `fallback_route`.
- **Same-route failover.** Each deployment attempt gets a fresh T1 and an
  unbounded T2. Bounded only by the number of planned deployments.
- **Cross-route fallback.** Each route gets its full same-route behaviour
  again, at most 1 + 3 = 4 routes (`MAX_FALLBACK_ROUTES`).
- **Placement polling/load.** T8, background only.
- **Gateway.** T10 (queue), T11, T12. No inference timeout.
- **Client disconnect / cancellation.** Ownership, not a flag (`proxy.rs`
  module doc): hyper drops the response body; the body owns the upstream
  stream; dropping it closes the node connection. The `Tracker` and
  `InFlight` guards record `outcome: "cancelled"` on drop; leases are RAII.
- **SSE streaming.** The gateway answers a streamed request with its `200`
  head **immediately** — queueing happens inside the stream
  (`sse_stream::encode_queued`). So a streamed request commits within
  connect + head latency, before any queueing or prefill.
- **Non-streamed response.** The gateway sends the head **only after
  queueing and the complete generation**. So a non-streamed request commits
  only when the whole answer exists; the router then reads the body to put
  the route name in `model` (`commit`).
- **Tool-call streaming.** Same path as SSE streaming (`relay`); tool-call
  deltas are frames like any other. Nodes execute no tools (R9.3 decision 9).

**Independent timeout domains on the request path:** T1 (per attempt,
resets), T5 (once, before routing), T10 (node side, per node reached,
resets), T13 (HTTP layer). Nothing bounds T2, T3 or T4. **There is no
overall request deadline anywhere.** The earlier statement "only a 5-second
connect timeout exists" is accurate for the router's *proxy* path, but
incomplete: the classifier timeout and the node's queue timeout also sit on
the request path.

## 2. Problem statement

```
one client request
  → classification (≤ 120 s)
  → initial route   × N deployments  (each: fresh 5 s connect, unbounded head)
  → fallback 1      × N deployments
  → fallback 2      × N deployments
  → fallback 3      × N deployments
```

A request can accumulate many fresh allowances: a classifier timeout, then
up to four routes, each with a fresh connect timeout per deployment and an
unbounded wait for every response head (for a non-streamed request, each
head can include a full node queue wait of up to 600 s before a `503`).
No single number bounds how long a client waits before the router either
commits an answer or gives up.

## 3. Goals

- One optional, operator-set time budget per client request, measured on a
  monotonic clock from one fixed start point, shared by every pre-commit
  stage: classification, scoring, planning, every same-route deployment
  attempt and every cross-route fallback.
- When the budget runs out before commit: stop trying, cancel in-flight
  router work, and answer with one deterministic, documented error.
- Make budget exhaustion visible separately from cancellation, route
  failure and classifier timeout, in traces, metrics and the admin view.
- With no budget configured: **byte-for-byte today's behaviour.**

## 4. Non-goals

- Choosing anything. The budget never ranks, scores, filters or orders a
  route, deployment or fallback.
- A response-lifetime / end-to-end streaming deadline (deferred, section 32).
- Replacing any existing timeout. T1, T5 and T10 stay as they are.
- An inference timeout. T4 stays absent; see section 8 for how a budget
  nonetheless bounds a non-streamed generation, explicitly and opt-in.
- Waiting for placement. The request path stays placement-independent.
- New fallback triggers. Budget expiry is **not** a fallback reason.
- Per-route or per-client budgets, client-supplied deadlines (deferred).
- Any change to R9.3.1 semantics, the R9.3 UI, or R9.4.

## 5. Architecture boundary

```
R9.1  classification ─┐
R9.2  scoring         │
planning (R5, R6)     │  all read ONE RequestDeadline: "is there time left?"
same-route attempts   │  none of them read it to decide WHAT to try
R9.3.1 fallback loop ─┘
───────────── response head commits ─────────────
relay body / stream: the deadline no longer applies
```

The budget is a **gate at the points where work starts** and a **cap on the
waits in between**. It never changes the plan, its order or its contents.

## 6. Request-owned budget invariant

- Created **once**, in `proxy::forward_as`, next to the request id, from the
  same `received` instant. Never created anywhere else on the request path.
- Passed **by value** (`Copy`) down the call chain, alongside `request_id`,
  into `route_request`, `resolve_auto`, `classifier::classify`,
  `attempt_route` and `attempt_one`.
- The nested classifier request (`forward_nested`) **inherits** the parent's
  deadline; it never creates its own. Its request id is already derived from
  the parent's (`{id}-classify`).
- Never reconstructed per route, per deployment or per fallback step. A
  fallback route receives what is left, not a new budget.
- No globals. `RouterState` holds only the configured duration.

Proposed representation (smallest that fits):

```rust
/// One client request's pre-commit deadline. `None` inside = no budget.
#[derive(Clone, Copy, Debug)]
pub struct RequestDeadline {
    budget: Option<(tokio::time::Instant /* deadline */, Duration /* configured */)>,
    started: tokio::time::Instant,
}
impl RequestDeadline {
    pub fn remaining(&self) -> Option<Duration>;   // None = unlimited
    pub fn expired(&self) -> bool;                 // false when unlimited
    pub fn at(&self) -> Option<tokio::time::Instant>;
}
```

`tokio::time::Instant` is monotonic (it wraps `std::time::Instant`) and is
what `tokio::time::timeout_at` takes — already used by the placement
controller (`controller.rs`) and the gateway scheduler. It also lets tests
drive the clock with `tokio::time::pause`/`advance`, as the gateway's
scheduler tests do.

## 7. Budget start point

**Recommendation: at `received` in `proxy::forward_as`** — after axum has
read the whole body, before JSON parsing, before any routing or
classification.

- It is already the origin of `router_request_duration_seconds`, TTFT and
  `duration_ms`: one start instant for "router-side time", not a new one.
- It is after the body is fully accepted, so a slow uploader does not spend
  the router's budget (section 31, Q4).
- It precedes classification, so R9.1 time counts.
- JSON parsing between `received` and `planning_started` is microseconds and
  bounded by the 2 MB body limit; counting it is harmless and avoids a
  second start instant.
- **`routing_ms` is not redefined.** It still starts at `planning_started`
  and still excludes classifier time. The budget is a separate measurement.

## 8. Pre/post-commit boundary

The commit point is R9.3's: **the moment `attempt_one` receives a response
head that the router will relay** (`Attempt::Committed`). From then on:

- The budget no longer governs the request: no route switch, no restart, no
  deployment fallback, no splicing — the existing R9.3 commit rule.
- The body read for a non-streamed answer (`commit`) and the stream relay
  (`relay`) run with no budget, exactly as today.
- The trace records the budget state **at commit** (section 23).

**First-slice scope: a pre-commit routing budget, not a response-lifetime
deadline.** Reasons: it fits the R9.3.1 architecture exactly (the commit
point already exists and is tested); a post-commit deadline would have to cut
a stream mid-answer, which needs its own in-band error semantics and is a
separate decision; and the problem in section 2 is entirely pre-commit.

**Consequence to document prominently (verified in the gateway):**

| Request | What "pre-commit" covers at a Lightweight node |
|---|---|
| `stream: true` | connect + response head. The node sends `200` immediately and queues inside the stream, so queue wait, prefill and generation are **post-commit** and **not** budgeted. |
| `stream: false` | connect + node queue wait + prefill + **the whole generation**, because the node's head is sent only when the answer is complete. |

So for non-streamed requests the budget **is** a total generation bound.
That is the honest meaning of "pre-commit" with this node contract, and it is
why the budget is opt-in and has no default value (section 20). The admin
view and future UI must say so in words.

## 9. Time included

Every item is router-controlled, pre-commit work:

| Stage | Counts? | Why |
|---|---|---|
| Body read (T13) | **no** | before `received`; the client's upload, not router work |
| JSON parse | yes (µs) | after `received`; see section 7 |
| R9.1 classification (Jev or Lightweight, network + inference) | **yes** | the request waits for it |
| R9.2 scoring | **yes** | pure arithmetic, µs; it is inside the lifecycle |
| Planning (R5 requirements, R6 affinity, eligibility, capability filter, policy) | **yes** | µs |
| Node connect (T1) | **yes** | |
| Waiting for the response head (T2), incl. node queue for non-streamed (T10) | **yes** | |
| Reading a 502/503/504 or 400 error body before deciding | **yes** | the decision is pre-commit |
| Same-route failover attempts | **yes** | same budget, no reset |
| Cross-route fallback attempts | **yes** | same budget, no reset |

## 10. Time excluded

- The request body upload (before `received`).
- Everything after commit: non-streamed body read and `model` rewrite,
  stream relay, TTFT, generation for streamed requests.
- Background work: health probes (T7), placement (T8), affinity sweeps.
- Admin API calls, including Jev Test Connection (T6).

## 11. Classifier interaction

- Effective classifier timeout = **min(configured `timeout_ms`, remaining
  budget)**, applied as a `timeout_at` on the earlier instant. A 120 000 ms
  classifier timeout never outlives a 30 000 ms budget.
- The provider timeouts are **not replaced**; they remain the classifier's
  own safety bound.
- Which one fired decides what happens next:

| What fired first | Classifier outcome | Then |
|---|---|---|
| provider timeout, budget remains | `timeout` (unchanged) | R9.1 fallback to the classifier's `fallback_route`, as today |
| request budget | new internal `request_budget_exhausted` | **terminate the request** (section 19). No classifier fallback continuation, no routing. |
| both at the same instant | budget wins (deterministic: if `deadline.expired()` when the timeout resolves, it is the budget) | terminate |

- The nested Lightweight classifier request inherits the deadline (section
  6). Dropping the classify future drops the nested request and closes its
  upstream connection, as T5b already does on timeout.
- Classifier budget expiry is **not** recorded as a classifier provider
  timeout in `router_classifier_requests_total`; it gets its own outcome
  value there (`request_budget_exhausted`) so provider-quality dashboards are
  not polluted. `state.classifier_status` likewise does not record it as a
  provider failure.

## 12. Provider and per-attempt timeout interaction

Both survive; the effective bound is always the earlier of the two:

```
effective attempt deadline = min(attempt's own limits, request deadline)
```

- **Connect (T1)** stays on the shared `reqwest::Client`. The attempt's
  `send()` (connect + head) and its pre-decision error-body read are wrapped
  in one `tokio::time::timeout_at(deadline, …)`. Whichever fires first wins:
  T1 as `err.is_connect()/is_timeout()` (a deployment failure, counted
  against node health as today), the deadline as budget exhaustion (**not**
  counted against node health — the node did nothing wrong).
- No per-request `connect_timeout` rewrite is needed: reqwest cannot change
  a client's connect timeout per request, and `timeout_at` already gives
  min(T1, remaining) because T1 fires on its own when it is the shorter.
- **Very little budget** (e.g. 50 ms left, T1 = 5 s): the attempt starts and
  is cancelled at 50 ms; the request ends as budget-exhausted, not after 5 s.
  An attempt is **not started** when `remaining == 0`.
- No new minimum-remaining threshold ("don't start with < X ms") in slice 1:
  any number would be tuned on this box. Measure first (deferred).

## 13. R9.2 interaction

None, beyond being inside the lifecycle. R9.2 never creates, resets, reads
or scores the budget. `scoring::decide` keeps its signature. A mutation that
feeds remaining time into a score must be caught (section 30, M7).

R9.2 route history: budget expiry is **not** a route-quality observation.
The route did not fail; the request ran out of time. No history observation
is recorded for it (history is observational only since R9.2's final
hardening), and it is never scored.

## 14. Same-route failover interaction

Every deployment attempt of a route consumes the **same** deadline:

```
budget 30 s
Coder/A  7 s  → remaining 23 s
Coder/B  6 s  → remaining 17 s   (route_exhausted)
General  receives 17 s, not 30 s
```

Before each attempt in `attempt_route`'s loop: if `expired()` → stop, do not
take a lease, do not send. During an attempt: `timeout_at` as section 12.
Context-overflow failover (R5) is a same-route attempt and obeys the same
rule.

## 15. R9.3 cross-route fallback interaction

- Every fallback route shares the original deadline. No reset.
- The fallback loop in `route_request` gains exactly one check: after a
  qualifying failure and **before** `record_cross_route_fallback` /
  `tracker.retarget(next)`, if the budget is expired, stop. **No transition
  is counted to a route that was never attempted.**
- All frozen R9.3.1 rules are untouched: Auto-only, same-route first, the
  three triggers, flat non-transitive list read once, max 3, explicit-route,
  500, context-overflow and post-commit exclusions, R5 filtering with the
  original requirements, route-local affinity, final route identity. The
  budget only decides whether the next step may start.

Worked example (from the brief):

```
budget 30 s
classifier 3 s · Coder/A 8 s · Coder/B 7 s          used 18 s, left 12 s
General/A 9 s (qualifying failure)                   used 27 s, left 3 s
Reasoning: starts, capped at 3 s                     not a fresh 30 s
if left = 0 after General: Reasoning is NOT attempted
```

## 16. Per-attempt timeout interaction

See section 12. Summary: the request deadline caps each attempt's waits;
each attempt's own safety limits (T1, T5) still apply when shorter. The
router gains **no** standalone per-attempt head timeout in this slice —
that would be a new independent timeout domain (and an inference timeout for
non-streamed requests), which T4's rationale argues against.

## 17. Client cancellation

- Unchanged mechanism: hyper drops the future; ownership closes the node
  connection; guards record `cancelled`.
- **Cancellation is never budget exhaustion.** The deadline is a timer
  inside the request's own future; when the client leaves, that future is
  dropped and the timer never fires. The trace outcome stays `cancelled`;
  no budget counter moves.
- A client that disconnects exactly as the deadline fires: whichever the
  request future observes first. If the future was dropped, it is
  `cancelled` (the budget code never ran). If the deadline branch already
  completed, the request is budget-exhausted and the response write simply
  fails, as any response to a departed client does.

## 18. Upstream cancellation

What is actually guaranteed when the budget fires during an attempt:

| Phase | Guaranteed | Not guaranteed |
|---|---|---|
| connect pending | reqwest future dropped → connect abandoned immediately | — |
| waiting for head | connection closed; router lease released (RAII); client answered | the node noticing promptly |
| Jev call | HTTPS request dropped | Jev stopping its own work |
| Lightweight classifier | nested request dropped → its upstream connection closed | the classifier node stopping at once |

A Lightweight node stops generating when its client goes away. For a
**streamed** request that is proven (its writes fail). For a **non-streamed**
request still queued or generating, the node's detection of a closed
connection is not established by any test today. The design therefore
states: **the client-facing request is terminal at the deadline; upstream
compute may continue briefly.** The implementation slice must add a test
that measures this against a real gateway rather than assume it (section 29,
B13), and the operator doc must carry the limitation.

## 19. Public error semantics

Evaluated against the existing contracts:

| Option | Verdict |
|---|---|
| Final attempted route's error | Rejected when the budget stopped further work: it would claim e.g. `route_unavailable` (503, `Retry-After` = probe interval) when the cause was time. Kept when nothing was left to try anyway (below). |
| `408 Request Timeout` | Rejected: means the **client** was too slow sending the request. |
| `503` | Rejected: already means `route_unavailable` (capacity) and the node's `server_busy`; SDKs and the R9.3.1 trigger logic read it as "unavailable". |
| Existing timeout response | None exists in the router: no request-level timeout error is defined. |
| **`504 Gateway Timeout`, `code: "request_budget_exhausted"`** | **Recommended.** A proxy that gave up waiting on upstream work is exactly 504. OpenAI-compatible SDKs treat 5xx as retryable, which is right for a time bound. The envelope is the router's existing `server_error` shape. |

```json
HTTP/1.1 504 Gateway Timeout
{"error": {"message": "The router's request budget of 30000 ms ran out before a response started.",
           "type": "server_error", "code": "request_budget_exhausted"}}
```

No `Retry-After` (nothing says when the next attempt would be faster). The
message names the configured budget, never a node, model or prompt.

**Deterministic precedence** (single request task, `tokio::select!` with
`biased;` on the attempt first):

1. A committed response always wins: if the attempt's head is ready in the
   same poll that the deadline fires, the head is relayed.
2. An uncommitted failure observed before the deadline is processed
   normally. Then, at the next start point:
   - nothing left to try (plan exhausted, list exhausted, or a non-qualifying
     failure) → **the existing error, unchanged** (R9.3 decision 3 holds);
   - something left to try but the deadline has passed → **504
     `request_budget_exhausted`**.
3. The deadline firing while an attempt is in flight → cancel it → **504**.

So the client sees 504 only when the budget is what stopped further work.

`router_requests_total`: still one count per client request, under the
terminal logical route: the last route actually attempted, including one
whose in-flight attempt the budget cut. A route that would have been next but
was never attempted is **never** used. When expiry happens before any route
is resolved (during classification), the label is `Auto`, as pre-routing
refusals already record via `AUTO_ROUTE`. `outcome="server_error"`,
which is what `Outcome::of_status(504)` yields today; **no new outcome label
value** is added to an existing metric. The cause is in the dedicated metric
(section 24).

## 20. Configuration proposal

The repository's conventions: `router.json`, `#[serde(deny_unknown_fields)]`
sections, a `request` section that already holds `connect_timeout_secs`, and
the classifier's "required, choose a bound" stance on timeouts.

```json
{
  "request": {
    "connect_timeout_secs": 5,
    "pre_commit_budget_ms": 30000
  }
}
```

- **Name/location:** `request.pre_commit_budget_ms`. The section is where
  request-path timing already lives; the name states the scope so nobody
  reads it as a streaming deadline. Milliseconds, like the classifier's
  `timeout_ms`, because it must compose with them.
- **Opt-in:** absent = no budget = today's behaviour. There is no default
  value when enabled; the operator writes a number.
- **`0`:** refused at load ("must be at least 1000"), consistent with
  `connect_timeout_secs` and classifier `timeout_ms`, which refuse 0. Absent
  is the only way to disable it.
- **Bounds:** 1 000 – 3 600 000 ms. The minimum rules out a value that
  could never complete a connect plus a classification; the maximum keeps
  the number meaningful while still covering a long non-streamed CPU
  generation behind the gateway's 600 s queue. Neither is a performance
  claim.
- **Validation warning (not an error):** if a configured classifier
  `timeout_ms` ≥ the budget, `validate-config` warns that the classifier can
  consume the whole budget.
- **Scope:** all router client requests — explicit routes and `Auto`
  (section 21). Nested classifier requests inherit, never configure.
- **Initial value guidance:** none hardcoded. 30 000 ms is an example only.
  The operator doc will say: measure your p99 time-to-head (from
  `router_upstream_response_seconds`) and, for non-streamed traffic, your
  generation time, then choose.

## 21. Scope: all client requests, not Auto-only

**Recommendation: all router client requests.** The budget is a resource
bound on the request, not a fallback policy. Explicit-route requests still
have classification-free but multi-deployment same-route failover, and each
deployment can still hold the request indefinitely (T2). R9.3's Auto-only
scope exists because only `Auto` delegated the route choice; that reason
does not apply to time. An explicit `Coder` request gets the same deadline
and still never falls back cross-route.

## 22. Backward compatibility

- No `pre_commit_budget_ms` → `RequestDeadline` holds `None`; every check is
  `false`/unlimited; no `timeout_at` wraps are installed (or they wrap a
  never-firing branch); responses, status codes, metrics, traces (field
  omitted) and admin output are byte-identical. Proven by test B1.
- Existing configs remain valid (the new key is optional; the section is
  already `deny_unknown_fields`, so a misspelt key is still refused).
- `routing_ms`, `router_requests_total` semantics, R9.3 metrics and the
  frozen R9.3 UI are unchanged.

## 23. Tracing

One block on `RoutingTrace`, present only when a budget is configured:

```json
"request_budget": {
  "configured_ms": 30000,
  "elapsed_ms": 27120,
  "remaining_ms": 2880,
  "exhausted": false,
  "stage": null,
  "next_route_not_attempted": null
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
  "next_route_not_attempted": "Reasoning"
}
```

- `elapsed_ms`/`remaining_ms` are taken **at commit** for a committed
  request and **at expiry** for an exhausted one. Per-attempt timings are not
  duplicated: `attempts[*]` already carries each attempt's head latency (R6).
- `stage` is a bounded vocabulary: `classifier`, `route_planning`,
  `same_route_attempt`, `cross_route_fallback`. No route names in it.
- `next_route_not_attempted` names the route the plan would have tried next,
  so the trace shows "plan remaining, budget exhausted, next route not
  attempted". It is **not** added to `cross_route_fallback.attempts` and
  never counted as a transition.
- Trace `outcome` gains the value `request_budget_exhausted` alongside
  `ok`, `client_error`, `server_error`, `unavailable`, `interrupted`,
  `cancelled`. `cancelled` keeps its meaning.
- With R9.3: an exhausted budget during a fallback leaves
  `cross_route_fallback.exhausted = false` (the list was not exhausted; time
  was) and `final_route` = last route attempted.
- Request id: unchanged and shared, as today (`{id}-classify` for the nested
  classifier request, which carries the inherited deadline).

## 24. Metrics

Low-cardinality, new names only:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `router_request_budget_exhausted_total` | counter | `stage` (4 values) | requests ended by the budget, once each |
| `router_request_budget_remaining_at_commit_seconds` | histogram | none | budget left when a budgeted request committed — the headroom operators tune by |
| `router_request_budget_configured_seconds` | gauge | none | configured budget, 0 when disabled |

Not added: labels of request id, session, user, prompt, route (routes are
already in the trace), or remaining milliseconds. Not duplicated:
`router_request_duration_seconds`, `router_routing_duration_seconds`,
`router_upstream_response_seconds` and TTFT already exist (R6).

Interaction with existing metrics:

- `router_requests_total`: once, `server_error`, terminal route (section 19).
- `router_cross_route_fallback_total`: no increment for an unattempted step.
- `router_cross_route_fallback_exhausted_total`: no increment (time ran out,
  not the list).
- `router_failovers_total`: no increment for an unstarted deployment attempt.
- `router_classifier_requests_total`: outcome `request_budget_exhausted`, not
  `timeout`.
- R9.2 history (`router_route_history_observations_total`): **no
  observation** for budget expiry (section 13).
- Node health: not touched by budget expiry.

## 25. Admin state

`GET /api/router/v1/request-budget` (read-only, new):

```json
{
  "object": "router.request_budget",
  "configured": true,
  "pre_commit_budget_ms": 30000,
  "scope": "all_client_requests",
  "governs": "pre_commit",
  "exhausted_total": 12,
  "exhausted_by_stage": {"classifier": 1, "route_planning": 0,
                         "same_route_attempt": 7, "cross_route_fallback": 4}
}
```

Its own endpoint rather than `/api/router/v1/auto`, because the scope is not
Auto-only. No write endpoint.

## 26. UI implications (document only — nothing is built)

A future card, likely on the router's Auto Routing screen beside
Cross-Route Fallback or on a routes/overview screen:

```
Shared Pre-Commit Request Budget        [Enabled]
Budget            30000 ms
Scope             All router client requests
Governs           Until the response starts. Streamed answers: up to the
                  first response head. Non-streamed answers: the whole
                  generation, because the answer starts only when complete.
Expirations       12  (classifier 1 · same-route 7 · cross-route 4)
```

Snippet-only like every router screen (no config write API), and a trace
row showing "budget exhausted — Reasoning not attempted". Must not imply the
budget picks routes.

## 27. Performance

- Creation: one `Instant::now()` (already taken) plus an add.
- Checks at start points: one `Instant` comparison each, a handful per
  request.
- Waits: `tokio::time::timeout_at` on futures already awaited — a timer
  wheel entry, no polling loop, no extra task.
- Disabled: no timer registered.

## 28. Security / privacy

Timing data only: durations, a bounded stage, route names already present
in the trace. No prompts, user text, credentials, session values or node
URLs in the trace, metrics, admin view or the 504 body.

## 29. Test plan (for the implementation; nothing written now)

All with `tokio::time::pause()` + scripted upstreams where possible
(deterministic), plus one real-gateway check.

| # | Test | Expect |
|---|---|---|
| B1 | no budget config | exact existing behaviour: statuses, bodies, traces (no `request_budget` field), metrics, admin |
| B2 | budget, fast success | 200, `exhausted:false`, `remaining_ms` > 0 at commit, histogram observed |
| B3 | slow classifier (2 s of 5 s budget) | routing sees ≤ 3 s; trace elapsed includes the 2 s |
| B4 | provider timeout first, budget remains | outcome `timeout`, R9.1 fallback route used, request proceeds |
| B5 | budget expires during classification | 504 `request_budget_exhausted`, stage `classifier`, **no** classifier fallback, no route attempted, classifier metric outcome not `timeout` |
| B6 | same-route sharing | Coder/A consumes 3 s of 5 s; Coder/B is cancelled at 2 s, not 5 s |
| B7 | cross-route sharing | Coder 2 s, General gets ≤ 3 s, Reasoning gets the smaller remainder |
| B8 | no fresh budget | a deadline that would only be met if reset per route/deployment fails |
| B9 | expiry before a fallback starts | next route not attempted; no transition counted; trace `next_route_not_attempted`; 504 |
| B10 | expiry during an uncommitted fallback attempt | attempt cancelled; 504; stage `cross_route_fallback` |
| B11 | post-commit expiry | head committed at 4.9 s of 5 s, stream runs 20 s: full stream relayed, no switch, `exhausted:false` |
| B12 | explicit route | explicit `Coder` gets the same enforcement, still never cross-route |
| B13 | upstream cancellation (real gateway) | at expiry the node connection is closed; measure whether a non-streamed generation stops; record the result in the operator doc |
| B14 | client cancellation | trace `cancelled`, budget counter unchanged |
| B15 | request id | the same id on every attempt and on the 504; nested `{id}-classify` inherits the deadline |
| B16 | metrics | exactly one `router_request_budget_exhausted_total{stage}` increment; `router_requests_total` once, `server_error`, terminal route |
| B17 | trace | configured/elapsed/remaining/exhausted/stage correct for success and expiry |
| B18 | placement | budget configured + empty node: request never waits for placement |
| B19 | R9.2 | identical scoring decisions with and without a budget, for any remaining time |
| B20 | history | budget expiry produces no route-history observation |
| B21 | connect pending at expiry | blackhole address, T1 = 5 s, 200 ms left: 504 after ~200 ms, node health not marked failed |
| B22 | race precedence | head ready in the same poll as expiry → committed; refusal before expiry with routes left → 504; refusal with nothing left → the route's own error |
| B23 | nothing left to try at expiry | last route's last deployment refuses after expiry → existing error, not 504 |
| B24 | config | absent ok; 0, 999, 3 600 001 refused; classifier timeout ≥ budget warns |

## 30. Mutation plan

Each must be caught by at least one test above:

| # | Mutation | Caught by |
|---|---|---|
| M1 | reset the full budget on each fallback route | B7, B8 |
| M2 | reset the full budget on each deployment attempt | B6, B8 |
| M3 | ignore classifier time (start the budget after classification) | B3, B5 |
| M4 | continue after the budget reaches zero | B9, B10 |
| M5 | allow fallback after post-commit expiry | B11 |
| M6 | treat client cancellation as budget exhaustion | B14 |
| M7 | use remaining budget as an R9.2 score input | B19 |
| M8 | count an unattempted fallback transition | B9, B16 |
| M9 | wait the full connect timeout after the deadline | B21 |
| M10 | create a new request id on fallback | B15 |
| M11 | let a budget-expired classifier fall back to its `fallback_route` | B5 |
| M12 | record budget expiry as route history / node health failure | B20, B21 |
| M13 | return 504 when nothing was left to try | B23 |
| M14 | nested classifier request creates its own budget | B15, B5 |

## 31. Open questions resolved

| # | Question | Recommendation |
|---|---|---|
| 1 | Pre-commit or response-lifetime? | **Pre-commit routing budget.** Response-lifetime deferred. |
| 2 | All requests or Auto only? | **All router client requests.** |
| 3 | When does it start? | **At `received` in `forward_as`**: after the body is read, before parsing and routing. |
| 4 | Does body-read time count? | **No.** |
| 5 | Does R9.1 count? | **Yes.** Classifier timeout capped by the remainder. |
| 6 | Does R9.2 count? | **Yes** (µs); never an input. |
| 7 | Same-route deployment time? | **Yes**, shared, no reset. |
| 8 | Cross-route fallback time? | **Yes**, shared, no reset. |
| 9 | At response commit? | The budget **stops governing**. Trace records remaining-at-commit. |
| 10 | Public error? | **504, `code: request_budget_exhausted`**, only when the budget stopped further work; otherwise the existing error. |
| 11 | Cap provider/per-attempt timeouts by remainder? | **Yes**: min(own limit, remaining), via `timeout_at`; own limits kept. |
| 12 | Config location/name? | **`request.pre_commit_budget_ms`** in `router.json`. |
| 13 | Opt-in? | **Yes.** Absent = disabled. |
| 14 | Safe initial default? | **None.** Operator chooses; 30 000 ms is an example only. |
| 15 | Bounds? | **1 000 – 3 600 000 ms**; 0 refused. |
| 16 | Cancellations distinguished? | Timer lives in the request future; a dropped future never reports expiry; trace `cancelled` vs `request_budget_exhausted`. |
| 17 | Trace schema? | Section 23. |
| 18 | Metrics? | Section 24: exhausted counter by stage, remaining-at-commit histogram, configured gauge. |
| 19 | Admin state? | Section 25: `GET /api/router/v1/request-budget`. |
| 20 | Upstream cancellation guarantees? | Router side: immediate (connection drop, lease release, client answered). Node side: proven for streams, **unproven for a non-streamed generation** — test B13 must measure it; documented as a limitation until then. |

## 32. Deferred work

- Response-lifetime / streaming deadline (post-commit, in-band error).
- Client-supplied deadline header, capped by the operator's budget.
- Per-route or per-client budgets.
- A minimum-remaining threshold for starting an attempt (needs measurement).
- A standalone per-attempt response-head timeout.
- Gateway-side honouring of the router's deadline (e.g. forwarding the
  remaining budget so a node can refuse to queue past it).
- The budget UI card (section 26).
- Any interaction with R9.4.

## 33. Recommended first implementation slice (R9.3.2 slice 1)

1. `RequestDeadline` (section 6) created in `forward_as`, passed by value;
   nested classifier inherits.
2. Config `request.pre_commit_budget_ms` (opt-in, 1 000 – 3 600 000, 0
   refused, classifier-timeout warning), surfaced in `validate-config`.
3. Start-point checks: before classification, before the first route
   attempt, before every deployment attempt, before every fallback route.
4. `timeout_at` caps: the classify call (min with provider timeout) and each
   attempt's connect + head + pre-decision body read.
5. Terminal 504 `request_budget_exhausted` per section 19's precedence.
6. Trace block + `request_budget_exhausted` trace outcome; the three
   metrics; `GET /api/router/v1/request-budget`.
7. Tests B1–B24, mutations M1–M14, plus a real-router smoke (scripted slow
   node + real gateway for B13).
8. `docs/ROUTER.md` operator section with the stream/non-stream table from
   section 8.

Not in the slice: UI, response-lifetime deadline, client deadlines, any
change to R9.3.1, R9.4.
