# R9.2 — Adaptive logical-route scoring (design)

Status: **slice 1 implemented** on `feature/router-route-scoring`
(`crates/lightweight-router/src/scoring/`). The operator documentation is
[ROUTER.md, Adaptive route scoring](ROUTER.md#adaptive-route-scoring-r92).
This document was written before the seven review decisions were locked;
**section 0 records those decisions and every place the implementation
departs from the original text.** Where the two disagree, section 0 wins.
R9.1, R9.1a and the classifier UI are unchanged.

## 0. Locked decisions and implementation (slice 1)

| # | Decision | Implemented as |
|---|---|---|
| 1 | Borderline accepted decisions may be influenced, within a strict bound; strong ones never. | Influence radius `(W_prior + 2·W_history) / W_classifier` must be **less than half the accepted range**, `(1 − min_confidence) / 2`, for every configured provider block (`MAX_INFLUENCE_SHARE = 0.5`). This replaces section 8's `Wp + 2·Ws < Wc·min_confidence`, which bounded only unnamed candidates and allowed a radius as large as the threshold itself. Property tests sweep random valid configurations against worst-case priors and history. |
| 2 | No provider-specific weights. | One `weights` block. The provider is trace metadata only. `jev`/`lightweight` weight keys are refused as unknown. |
| 3 | One-hour half-life, configurable, provisional. | `history.half_life_secs`, default 3600, bounds 60..=604800. Documented as an operational starting point, not tuned; the admin view says `half_life_provisional: true`. |
| 4 | Admin-only history reset. | `POST /api/router/v1/adaptive-scoring/reset`, empty body / `{}` for all, `{"route": "Coder"}` for one. Router key, like every control endpoint. Resets history aggregates only. |
| 5 | No Jev per-option probabilities. | Only `Verdict { route, confidence }` is read. Nothing is fabricated for other candidates (section 19 remains future work). |
| 6 | No route capability schema. | None added. R5 stays the capability authority. |
| 7 | `route_unavailable` is not scored. | Observed (`unavailable` counter, metric) and never scored. A request every deployment turned away with 502/503/504 before answering counts the same way. Section 17 listed that case under `server_error`; it says the same thing as `route_unavailable` (nothing ready ran it), so Decision 7 governs it. |

**The threshold is a hard boundary (supersedes open question 1 and section
8's "chosen or low_confidence").** Scoring starts from R9.1's accepted
decision:

```text
confidence < min_confidence  →  R9.1 rejects the verdict; its fallback IS the decision
                                 (reason below_threshold; the rejected route never contends)
confidence = min_confidence  →  verdict and fallback tie on classifier signal; prior/history decide
confidence > min_confidence  →  scored; overturned only within the influence radius
```

**`classifier_baseline`** is an explicit named function
(`scoring::classifier_baseline`): the active provider's `min_confidence`,
used as the fallback route's classifier signal, and only once a verdict was
accepted.

**Contenders are structural, not arithmetic.** With one verdict there are
exactly two contenders: the verdict route (signal = its confidence) and the
classifier fallback (signal = the baseline). Other candidates are not scored
at all, rather than scored at `c = 0` and kept from winning by an inequality.
A verdict naming the fallback is `uncontested`.

**Implementation deltas from the text below:**

- Config: `weights.success` is named `weights.history` (it weighs the history
  term). `history.prior_strength` is named `history.shrinkage_samples`, to
  avoid confusion with route priors. Defaults and bounds are unchanged.
- Trace: `r91_route` is `classified_route`. Components are named
  `classifier_signal`, `prior_signal`, `history_signal` and `total_score`.
  `basis` is `verdict` or `baseline`. Reasons are `scored`, `uncontested`,
  `below_threshold`, `no_verdict` and `internal_error`. Candidates are listed
  verdict first, then baseline.
- Metrics: `router_route_scoring_skipped_total` became
  `router_route_scoring_fallback_total{reason}`; the history metric's label is
  `outcome`; the per-route gauge is `router_route_history_signal` (the history
  term) rather than a success-rate gauge; the margin histogram is not built.
- History: `unavailable`, `mismatch` and `neutral` are plain counters, not
  decayed values. They are shown and never scored.
- History is recorded only while scoring is on (an absent or off section
  records nothing).
- No UI (section 24 is deferred to a follow-up).

The one rule everything below obeys:

> **Adaptive scoring ranks logical routes only. It never ranks or chooses
> deployments.**

---

## 1. Goals

- Answer one question for a request that an `Auto` rule sends to
  classification: **among the plausible logical routes, which should win?**
- Combine the classifier's verdict with an operator's stated preferences and
  with bounded, route-level evidence of how routes have actually behaved.
- Be **explainable**: every decision decomposes into named, additive
  components, recorded in the request's trace, answering "why did Coder beat
  General?" without hidden state.
- Be **bounded and safe**: off by default; when off, or when it cannot run,
  the request resolves exactly as R9.1 resolves it today.
- Be **cheap**: in memory, no network call, no model call, microseconds.

## 2. Non-goals

- Choosing, ranking or filtering **deployments**, nodes, aliases, model files
  or addresses. Health, R5 capability filtering, affinity, `priority` /
  `round_robin` / `least_busy` and R7 placement keep that job.
- Guaranteeing execution. A route can win scoring and still answer
  `route_unavailable` or `route_capability_mismatch`; that is correct.
- Cross-route retry or fallback after a route fails (R9.3).
- Mixture-of-agents (R9.4).
- Learned models of any kind: no neural ranker, online learning, bandits,
  randomized exploration, or automatic weight tuning.
- Overriding hard routing: explicit route names and non-classifying `Auto`
  rules are never scored.
- Persistence, a database, Redis, or SQL.
- Latency and context-fit signals in the first slice (sections 18 and 23 say
  why and how they could come later).

## 3. Architecture

```text
client names a route ──────────────────────────────────────────┐
                                                               │ (never scored)
client names Auto                                              │
   │                                                           │
   ▼                                                           │
R8 rules, first match ─── rule without "classify" ─────────────┤ (never scored)
   │                    └─ no rule matched → Auto fallback ────┤ (never scored)
   │ rule with "classify": true                                │
   ▼                                                           │
R9.1 / R9.1a classifier (Lightweight or Jev)                   │
   │  Classification { provider, outcome, verdict?, ... }      │
   │                                                           │
   ├─ no verdict (timeout, auth_error, …) → R9.1 route ────────┤ (scoring skipped)
   ▼                                                           │
R9.2 route scoring  (only if auto_route.adaptive_scoring.enabled)
   │  candidates = classifier candidates ∪ {classifier fallback}
   │  score(route) = classifier anchor + prior + history       │
   ▼                                                           │
ONE resolved logical route ◄───────────────────────────────────┘
   │
   ▼
existing router (unchanged): health → R5 capability filter → affinity
→ priority | round_robin | least_busy → failover within the route
```

R9.2 is one pure function called at exactly one place: in
`proxy::resolve_auto`, where today `classification.route(classifier)` turns a
classification into a route. That is the only line of R9.1 behaviour it can
change, and only when enabled. History is recorded at exactly one place:
`Tracker::finish`, which already sees every request's final outcome
(including a cancelled one, through `Drop`).

## 4. Candidate-route source

The routes R9.2 may score are, and only are:

- the active classifier's configured candidates (`auto_route.classifier.routes`,
  at most 16, in configured order), plus
- the classifier's fallback route (`classifier.fallback_route`, or
  `auto_route.fallback_route` when that is absent), if it is not already a
  candidate.

Both sets are already validated at load time as configured logical routes
(never `Auto`, never `default`). R9.2 never scores a route the operator did not
list for classification. It adds no route-level eligibility pruning in the
first slice: routes have **no static, route-level capability configuration** in
this codebase. Capabilities are observed per deployment (`/v1/capabilities`),
and R5 applies them after the route is chosen. Pruning by aggregating
deployment capabilities would give R9.2 a deployment dependency, so it is
deferred (open question 6).

## 5. Telemetry inventory (what exists on `master` today)

All runtime state is in memory and resets when the router restarts.

### Prometheus metrics (`metrics.rs`)

| Metric | Labels | Level |
|---|---|---|
| `router_requests_total` | `route`, `outcome` (`ok`, `client_error`, `server_error`, `unavailable`) | route |
| `router_failovers_total` | `route` | route |
| `router_routing_decisions_total` | `route`, `policy`, `reason` | route (reasons describe deployment choice) |
| `router_capability_filtered_total` | `route`, `reason` | route (counts deployments ruled out) |
| `router_capability_mismatch_total` | `route` | route |
| `router_context_overflow_failovers_total` | `route` | route |
| `router_active_requests` | — | global |
| `router_session_affinity_*` (enabled, entries, evictions, hits, misses, reassignments) | `route`, `reason` | session/deployment |
| `router_placement_actions_total`, `router_placement_failures_total` | `route`, `action`, `result` / `reason` | route (deployment readiness) |
| `router_placement_{ready,target,loading}_deployments` | `route` | route (deployment readiness) |
| `router_auto_route_decisions_total` | `rule`, `route` | route |
| `router_auto_route_fallback_total` | `route` | route |
| `router_classifier_requests_total` | `provider`, `outcome` | classifier provider |
| `router_classifier_route_total` | `provider`, `route` | route (as chosen by the classifier) |
| `router_deployment_active_requests` | `deployment` | **deployment** |
| `router_node_health` | `node` | **node** |
| Histogram `router_request_duration_seconds` | `route`, `policy` | route |
| Histogram `router_routing_duration_seconds` | `route`, `policy` | route |
| Histogram `router_ttft_seconds` | `route`, `policy` | route |
| Histogram `router_upstream_ttft_seconds` | `route`, `deployment` | **deployment** |
| Histogram `router_upstream_response_seconds` | `route`, `deployment` | **deployment** |
| Histogram `router_upstream_duration_seconds` | `route`, `deployment` | **deployment** |
| Histogram `router_context_estimation_ratio` | `route` | route |
| Histogram `router_context_estimation_error_tokens` | `route` | route |
| Histogram `router_classifier_duration_seconds` | `provider`, `outcome` | classifier provider |
| Histogram `router_placement_reconcile_duration_seconds` | — | global |

Two properties matter for scoring:

- Every counter and histogram is **cumulative since start**. None has a window
  or decay, so none can say "recently".
- `router_requests_total` is recorded when the **response head** arrives. A
  stream that the node breaks off afterwards stays counted as `ok`; only the
  trace records `interrupted`. A client that leaves is in the trace as
  `cancelled` and in no counter.

### Routing trace (`trace.rs`, `GET /api/router/v1/traces`)

A bounded ring (`traces.capacity`), one per request:
`request_id`, `received_at`, `route`, `requested_route`, `auto_rule`,
`auto_fallback`, `classifier { provider, route, model, outcome, chosen_route,
confidence, duration_ms, request_id, input_truncated }`, `endpoint`, `stream`,
`policy`, `session { fingerprint, affinity, sticky, reassignment }`,
`deployments`, `available`, `capable`, `unavailable[]`, `unfit[]`, `selected`,
`selection_reason`, `attempts[] { deployment, reason, outcome, upstream_status,
response_ms }`, `final_deployment`, `context_overflow { estimated_prompt_tokens,
too_small[], answered_context }`, `estimated_prompt_tokens`,
`actual_prompt_tokens`, `routing_ms`, `ttft_ms`, `duration_ms`, `status`,
`outcome` (`ok`, `client_error`, `server_error`, `unavailable`, `interrupted`,
`cancelled`).

The ring is a debugging aid. It is sized for inspection, not statistics, and
it evicts by count, so it **must not** be scoring's history store.

### Classifier data (`classifier/`)

- `Classification { provider, outcome, verdict: Option<Verdict { route,
  confidence, model }>, duration, request_id, input_truncated }`.
- **One route and one confidence**, never a distribution. A verdict exists for
  the outcomes `chosen` and `low_confidence`. Every other outcome (`invalid`,
  `unavailable`, `timeout`, `auth_error`, `rate_limited`, `connection_error`,
  `provider_error`, `nested`) has no verdict.
- `ClassifierStatus`: last success, last failure and kind, last check. This is
  provider health, not route behaviour.

### Placement and health state

`HealthBook` (per node), `PlacementBook` (per route target, per deployment
state), `LoadBook` (in-flight per deployment), and `AffinityBook` (per session
→ deployment). All of these are deployment-, node- or session-scoped.

## 6. Usable signals

| Signal | Source | Verdict |
|---|---|---|
| **Classifier verdict** (route + confidence) | `Classification.verdict` | **Use** (primary). Real data, in both providers. |
| **Classifier threshold** (`min_confidence`) | provider config | **Use**, as the fallback route's anchor (section 8). It is R9.1's own decision rule restated. |
| **Configured route prior** | new config, operator-set | **Use**. Explicit, bounded, and visible in config. |
| **Route success history** | new bounded per-route aggregate, fed from `Tracker::finish` | **Use**, weakly (sections 13–14, 17). It can't be read from existing counters, which are cumulative and record outcome at head time. |
| Route capability-mismatch / context-overflow rates | `router_capability_mismatch_total`, `router_context_overflow_failovers_total`, trace `context_overflow` | **Later** (section 18). It is request-size-dependent, so a per-route rate alone mis-states it. |
| Route TTFT / duration | `router_ttft_seconds`, `router_request_duration_seconds` | **Later**, tie-break only (section 23). Cumulative today, and dominated by which hardware happens to hold the route. |
| Jev probability distribution | TypeSafe `answers.route.probabilities` (documented; **parsed and discarded today**) | **Later** (section 19), through a provider-neutral `Verdict` extension. |

## 7. Rejected signals (must not feed scoring)

| Signal | Why |
|---|---|
| `router_upstream_{ttft,response,duration}_seconds{deployment}`, `router_deployment_active_requests`, `router_node_health`, `HealthBook`, `LoadBook` | Deployment- or node-level. Using them would make R9.2 a second deployment selector competing with the policy. If a route-level aggregate is ever needed, it is derived from route-labelled data, never from per-deployment series. |
| Placement readiness (`router_placement_*`, `PlacementBook`) | Would turn scoring into "choose whichever route is loaded". Availability is R0–R7's hard concern, and R9.3's fallback concern. |
| Session affinity (`AffinityBook`, trace `session`) | Affinity is deployment stickiness inside a route. A session that used Coder must not bias the next `Auto` decision toward Coder; that would be a separate, explicit route-affinity feature. |
| `outcome = unavailable` and `route_capability_mismatch` as success failures | Capacity and request-fit, not route quality (section 17). Counted separately for visibility, but not in the success signal. |
| `client_error` and `cancelled` | The request or the client, not the route. |
| Classifier provider failures (`timeout`, `auth_error`, …) | Provider health. A Jev timeout that sends a request to General is **not** a Coder or Research failure, and is not recorded against any route as a classifier failure. |
| Nested classification requests (`-classify`) | Internal traffic of the Lightweight provider. Excluded from history (the `nested` flag is already on the request path). |
| Trace ring contents | Bounded by count for debugging, not a statistical sample. |

## 8. Score formula proposal

For each candidate route `r` (section 4):

```text
score(r) = Wc · c(r)  +  Wp · prior(r)  +  Ws · h(r)
```

**Classifier term `c(r)`** uses only the data that exists:

```text
c(verdict.route)  = verdict.confidence          (chosen or low_confidence)
c(fallback route) = min_confidence              (the "threshold anchor")
                    — or the verdict's confidence if the verdict IS the fallback
c(any other)      = 0                           (no data; nothing invented)
```

The anchor is what makes R9.2 a strict generalisation of R9.1. R9.1's rule,
"the verdict wins if `confidence ≥ min_confidence`, otherwise the fallback",
is exactly `argmax` over `{verdict: confidence, fallback: min_confidence}`
with ties going to the verdict. So with `Wp = Ws = 0`, R9.2 picks **the same
route R9.1 picks, for every request**. That property is a required test.

**Prior term `prior(r)`** is in `[0, 1]`, operator-configured per candidate,
default `0`.

**History term `h(r)`** is in `(−1, 1)`. It is a centred, shrunk success rate
(section 12), `0` below the sample threshold. A route with no history is
**neutral**, not penalised.

**What can actually win (a design guarantee, enforced by validation):**

```text
Wp + 2·Ws  <  Wc · min_confidence
```

Under this bound, a candidate the classifier did not name (`c = 0`) can never
outscore the fallback anchor, so the first slice decides only **"the
classifier's pick, or the safe fallback?"**, informed by priors and history.
That is the honest scope of single-verdict data: scoring cannot rank routes the
classifier never spoke about without fabricating a confidence for them. With a
real distribution (section 19), every candidate gets a real `c(r)` and the
same formula ranks them all.

The bound also limits how far R9.2 can move R9.1's threshold. The verdict wins
iff

```text
confidence − min_confidence  ≥  (Wp·(prior_f − prior_v) + Ws·(h_f − h_v)) / Wc
```

and the right-hand side is at most `(Wp + 2·Ws)/Wc`. For example, with
`Wc = 1, Wp = 0.10, Ws = 0.05`, a decision can change only when the confidence
is within ±0.20 of the threshold. A strong classification is never overturned.

**Worked example** (`Wc = 1, Wp = 0.10, Ws = 0.10`, `min_confidence = 0.65`):
Jev names Coder at 0.70; General is the fallback.

```text
Coder    = 1·0.70 (classifier) + 0.10·0.0 (prior) + 0.10·(−0.60) (history) = 0.64
General  = 1·0.65 (anchor)     + 0.10·0.5 (prior) + 0.10·(+0.40) (history) = 0.74
→ General. R9.1 would have chosen Coder (0.70 ≥ 0.65): overrode = true.
  Why: Coder's recent success rate (shrunk) is well below General's, and the
  operator prefers General; together they outweigh a 0.05 confidence margin.
```

## 9. Normalisation

- `c(r)` is already in `[0, 1]`: the providers validate `confidence ∈ [0, 1]`
  today, and `min_confidence ∈ [0, 1]`.
- `prior(r) ∈ [0, 1]`, validated.
- `h(r) ∈ (−1, 1)` by construction (section 12).
- Weights are in `[0, 1]`. So `score ∈ [−Ws, Wc + Wp + Ws]`, finite by
  construction. Any NaN or infinity at run time (which should be impossible)
  is the `internal_error` fallback (section 11), never a decision.
- No cross-provider calibration is attempted. The Lightweight provider's
  confidence is a small model's self-report; Jev's is TypeSafe's certainty
  measure `(n·p_max − 1)/(n − 1)`. The anchor uses the **active** provider's
  own threshold, so each is compared only with its own units. Open question 2
  covers whether weights should be per provider.

## 10. Weight configuration

| Weight | Default | Bounds | Meaning |
|---|---|---|---|
| `classifier` | `1.0` | `(0, 1]` | Must be positive: the one required signal. |
| `prior` | `0.0` | `[0, 1]` | Off until the operator sets it. |
| `success` | `0.0` | `[0, 1]` | Off until the operator sets it. |

Defaults are deliberately **neutral**: enabling scoring with no weights set
reproduces R9.1 exactly. This follows the project's own rule: ship the knob,
and change a default only when a measured number justifies it. Suggested
starting values (`prior 0.10`, `success 0.05`) appear in the docs as examples,
not as defaults. Cross-field validation enforces `prior + 2·success <
classifier · min_confidence` against every configured provider block, so
switching providers cannot silently break the guarantee.

## 11. Cold start, and the deterministic fallback

- **Zero observations:** every `h(r) = 0`, so the score is classifier anchor
  plus priors. With default weights this is R9.1. No division happens below
  the threshold, and the shrinkage pseudo-count keeps the rate defined above
  it (`k ≥ 1`).
- **Few observations** (`effective_n < min_samples`): `h(r) = 0` (neutral).
  Missing history is never treated as failure.
- R9.2 hands the request back to **R9.1's route** when:
  - scoring is disabled (`reason = disabled`, not counted as a fallback);
  - the classification has no verdict (`no_verdict`: any classifier failure,
    or `nested`);
  - a computed score is not finite (`internal_error`).

  Insufficient history is **not** a fallback: scoring runs with neutral
  history.
- Invalid configuration is refused **at load** (like every other section), so
  it never reaches a request.

## 12. Minimum-sample handling

```text
n  = effective (decayed) observations for the route
s  = effective (decayed) successes
k  = prior_strength (pseudo-observations, default = min_samples)

ŝ  = (s + k/2) / (n + k)          # shrunk toward 0.5; defined for n = 0
h  = 0                if n < min_samples
     2·ŝ − 1          otherwise
```

Two guards act together. The gate (`min_samples`, default 20) means one
request can never make history authoritative. The shrinkage means that just
past the gate the signal is still damped toward neutral: with `k = 20`, 20/20
successes give `ŝ = 0.75`, so `h = 0.5`, not 1.0.

## 13. History representation (memory-bounded)

One small record per configured route, created at load, never per request:

```text
RouteHistory {
  success:      Decayed,   // ok
  failure:      Decayed,   // server_error, interrupted
  unavailable:  Decayed,   // recorded, not scored (section 17)
  mismatch:     Decayed,   // recorded, not scored
  neutral:      u64,       // client_error, cancelled: counted for visibility only
  last_observed_at: Option<u64>,
}
Decayed { value: f64, updated_at: Instant }
```

Memory is `O(routes)`: a few dozen bytes per route, no request-level storage.
Updates take a per-router `Mutex<Vec<RouteHistory>>`, indexed by route
position, held for nanoseconds at `Tracker::finish`. A sharded or atomic
layout is only worth it if a profile says the lock is contended.

## 14. Decay / window strategy

**Time-based exponential decay with a half-life** (default 1 h, bounds
60 s – 7 d):

```text
on observe or read: value ← value · 2^(−Δt / half_life) ;  updated_at ← now
on observe:         value ← value + 1
```

I chose this over the alternatives for these reasons:

- A **count window** (the last N outcomes) never ages when a route goes quiet.
  A route that failed 20 times and was then avoided would stay "bad" forever.
  Time decay lets avoided routes drift back to neutral, because `n` falls
  below `min_samples` and `h → 0`. That gives recovery without exploration
  traffic (section 20).
- A **ring of timestamps** costs `O(N)` memory per route for no explanatory
  gain.
- EWMA **per request** would weight a busy route's recent minutes like a quiet
  route's hours.

**No persistence in the first slice.** History resets on restart. Every
configuration change, including routes, deployments, descriptions, candidates
and provider, already requires a restart, so a topology change resets history
automatically and no epoch key is needed. Runtime changes that do not restart
the router (placement loading another deployment, a node's model being
swapped) are handled by the decay. If persistence is ever added, it must key
history by an epoch: a hash of the route's deployments (node + model) and the
classifier provider. Open question 4 covers an explicit reset endpoint.

## 15. Tie-breaking (deterministic, no randomness)

Given equal scores, in order:

1. the **classifier's verdict route** beats the fallback anchor (this preserves
   R9.1's `confidence ≥ min_confidence` semantics at equality);
2. the higher `c(r)`;
3. the earlier position in the classifier's configured `routes`, with the
   fallback (when not a candidate) after all candidates.

Scores are compared as computed `f64` values, never rounded first, so the
order doesn't depend on display precision.

## 16. Latency restrictions

There is no latency in the first slice. If added later, it must be:

- **route-level only**: a decayed aggregate of `ttft_ms` (streaming) or
  `duration_ms` from `Tracker::finish` for that route, never a deployment
  series;
- a **tie-breaker within an epsilon** (`|score_a − score_b| ≤ latency_epsilon`,
  default 0.02, max 0.05), not an additive penalty.

I recommend the epsilon form over a capped penalty because it gives a hard
guarantee. A route that loses on intent by more than epsilon cannot win on
speed, whatever the numbers. A capped penalty only bounds the damage. Latency
would also be gated by `min_samples`, and it is confounded by which hardware
currently holds the route's deployments. That confound is one more reason
for keeping its influence to near-ties.

## 17. Failure semantics (what history counts)

Recorded once per request at `Tracker::finish`, for the route that handled it,
for **every** request to that route: direct and `Auto` alike (section 20).
Nested classification requests are excluded.

| Final outcome | Category | In the success signal? |
|---|---|---|
| `ok`: completed response, completed stream, tool-call response | success | yes (success) |
| `ok` after an in-route context-overflow failover | success | yes; the route recovered by design |
| `server_error`: 5xx returned, or every deployment refused with 5xx | failure | yes (failure) |
| `interrupted`: a committed stream the node broke off | failure | yes (failure) |
| `unavailable` (`route_unavailable`: nothing healthy, or all failed pre-response) | unavailable | **no**; counted separately |
| `route_capability_mismatch` (400) | mismatch | **no**; counted separately |
| `client_error`, any other 4xx (`invalid_request`, an unrecovered `context_length_exceeded` the node returned) | neutral | no |
| `cancelled`: the client left | neutral | no |

`unavailable` stays out of the first slice's score on purpose. It measures
capacity and readiness, and scoring on it is exactly "choose whichever route
is loaded". Routing around an unavailable route is R9.3's explicit, separate
mechanism. Including it would sneak R9.3 into R9.2. Both excluded categories
are still visible in the admin view (section 24), so an operator can see them.

## 18. Context-fit treatment

R5 stays the **hard** gate: exact, per deployment, per request. History can
at most add weak preference evidence, and only later. A route-level
"overflow/mismatch rate" alone is misleading, because it depends on the size
of requests that happened to reach the route. A sound version needs
per-route, **per prompt-size band** counts (e.g. bands of
`estimated_prompt_tokens`: < 2k, 2–8k, 8–32k, > 32k) of success versus
mismatch/overflow, with the signal looked up by the incoming request's
estimated band. That is real design work, and is deferred to a later slice
with its own measurements.

## 19. Jev / Lightweight classifier integration

- R9.2 consumes the provider-neutral `Classification`. It has **no**
  `if provider == Jev` branch, now or later.
- **Lightweight provider:** one route plus a self-reported confidence. Asking a
  1–2B model for a full distribution would produce numbers no more meaningful
  than the one it gives now, so it is not proposed.
- **Jev:** TypeSafe documents `answers.route.probabilities` (option →
  probability). The router's `jev::parse_response` reads `choice` and
  `confidence` and **discards `probabilities`**. A later slice can extend the
  neutral type:

  ```text
  Verdict { route, confidence, model, route_scores: Option<Vec<(RouteName, f64)>> }
  ```

  A provider fills `route_scores` only from real data. For Jev that means
  options ⊆ candidates, each in `[0, 1]`, and a sum within a small tolerance of
  1; otherwise the field is `None`. When present, `c(r) = route_scores[r]`
  for every candidate, and the anchor is converted into the same units:
  probability `p* = (min_confidence·(n−1) + 1)/n`, inverting TypeSafe's
  `confidence = (n·p_max − 1)/(n − 1)`. When absent, the formula in section 8
  applies unchanged. No provider is required to fabricate a distribution.

## 20. Observation-bias safeguards

The feedback loop to prevent: a route selected often collects more
observations, looks more trusted, and gets selected more. The safeguards, all
deterministic:

1. **History counts all traffic to a route**, not only `Auto`'s. Direct
   requests give evidence for routes `Auto` rarely picks.
2. **Absence is neutral.** `h = 0` without enough samples, so a rarely chosen
   route is never penalised for being rarely chosen.
3. **Shrinkage** toward 0.5 (`prior_strength`) damps every rate, most strongly
   when samples are few.
4. **Bounded influence.** The validation bound (section 8) caps how far history
   can move R9.1's threshold, and history can never promote a route the
   classifier did not name.
5. **Decay gives recovery without exploration.** A route avoided after failures
   receives fewer `Auto` observations. As its decayed `n` falls below
   `min_samples`, its signal returns to neutral, and the classifier can pick it
   again. Recovery takes on the order of
   `log2(n / min_samples)` half-lives, which is documented and visible in
   the admin view.
6. **No exploration.** No randomized traffic. If the bias can't be contained
   by 1–5, the answer is a smaller `success` weight, not a bandit.

## 21. Configuration schema proposal

A sibling of `classifier` under `auto_route`. It is valid only when a
classifier is configured.

```json
"auto_route": {
  "classifier": { "...": "unchanged R9.1a section" },
  "adaptive_scoring": {
    "enabled": false,
    "weights": { "classifier": 1.0, "prior": 0.0, "success": 0.0 },
    "priors": { "General": 0.5, "Research": 0.2 },
    "history": { "half_life_secs": 3600, "min_samples": 20, "prior_strength": 20 }
  }
}
```

Validation (refuse the file, as every section does):

- `deny_unknown_fields`;
- every weight is finite and in its bounds (section 10);
- `classifier > 0`, and `prior + 2·success < classifier · min_confidence` for
  every configured provider block;
- `priors` keys are scoring candidates (section 4), matched as route names
  are; values are finite and in `[0, 1]`; no `Auto`, `default` or duplicates;
- `half_life_secs` in `60 ..= 604800`, `min_samples` in `1 ..= 10000`,
  `prior_strength` in `1 ..= 10000`;
- the whole section is optional. Absent means disabled, and the file reads
  exactly as today.

## 22. Trace schema proposal

A new optional `scoring` block in `RoutingTrace`, present only when a
classifying rule ran with scoring enabled:

```json
"scoring": {
  "reason": "scored",
  "r91_route": "Coder",
  "winner": "General",
  "overrode": true,
  "weights": { "classifier": 1.0, "prior": 0.1, "success": 0.1 },
  "candidates": [
    { "route": "General", "score": 0.74,
      "components": { "classifier": 0.65, "prior": 0.05, "history": 0.04 },
      "classifier_basis": "anchor",
      "history": { "samples": 63.2, "success_rate": 0.70, "gated": false } },
    { "route": "Coder", "score": 0.64,
      "components": { "classifier": 0.70, "prior": 0.0, "history": -0.06 },
      "classifier_basis": "verdict",
      "history": { "samples": 41.8, "success_rate": 0.20, "gated": false } }
  ]
}
```

`reason` is one of `scored`, `no_verdict` or `internal_error`. The block lists
every scoring candidate (at most 17), sorted by score. It holds route names
and numbers only: no deployment, node, session, prompt or request content.
The R9.1 `classifier` block is unchanged: its `outcome` stays the
classifier's own judgement (for example `low_confidence`), even when scoring
then picks the verdict route.

## 23. Metrics proposal (low cardinality)

| Metric | Labels | Notes |
|---|---|---|
| `router_route_scoring_decisions_total` | `route`, `overrode` (`true`/`false`) | route ≤ configured routes |
| `router_route_scoring_skipped_total` | `reason` (`no_verdict`, `internal_error`) | |
| `router_route_history_observations_total` | `route`, `kind` (`success`, `failure`, `unavailable`, `mismatch`, `neutral`) | |
| `router_route_history_effective_samples` (gauge) | `route` | decayed `n` |
| `router_route_history_success_rate` (gauge) | `route` | shrunk `ŝ` |
| `router_route_scoring_margin` (histogram) | none | winner minus runner-up |

Score values appear in traces and as gauge or histogram **values**, never as
labels. No session, request id, prompt, user or classifier text appears
anywhere.

## 24. Admin UI proposal (not built)

Following the frozen classifier UI's pattern: read the running router, and
produce a validated snippet to paste. No write API.

- `GET /api/router/v1/auto` gains `adaptive_scoring` with: `enabled`,
  weights, priors, history parameters, and per route `{effective_samples,
  success_rate, gated, failure, unavailable, mismatch, neutral,
  last_observed_at}`.
- **Auto Routing** screen: an "Adaptive Scoring" card showing Enabled/Disabled,
  the per-route history table (samples, success rate, a "not enough history"
  badge, and failure / unavailable / mismatch counts shown separately), and
  for classifying rules, "Semantic classification → scored" when enabled.
- **Classifier** screen: an "Adaptive Scoring" settings section in the same
  draft:
  - Enabled;
  - classifier, prior and success weights, with the live bound
    `prior + 2·success < classifier·min_confidence` shown and enforced;
  - a prior per candidate route;
  - minimum samples, prior strength, half-life;
  - a plain-language line: "With these weights, scoring can change a decision
    only when the classifier's confidence is within ±X of the threshold."

  The generated snippet gains `auto_route.adaptive_scoring`.
- Traces: the scoring block rendered as a small component table ("why General
  beat Coder").

## 25. Test plan (for the implementation)

Unit tests, as pure functions:

- disabled → R9.1 route for every outcome;
- **enabled with `prior = success = 0` → identical to R9.1** across a sweep of
  confidences around the threshold (the backward-compatibility property);
- cold start (no history) → classifier and priors only;
- classifier-dominant: high confidence is never overturned at the maximum
  allowed weights;
- prior-dominant near-tie: a prior flips a decision just above the threshold;
- success-history adjustment, and a failure penalty (a failing verdict route
  loses to the fallback near the threshold);
- the `min_samples` gate: 19 samples → `h = 0`; 20 → shrunk value;
- all history missing; shrinkage math at `n = 0` and large `n`;
- decay: values halve per half-life; an avoided route returns to neutral;
- the bound: a candidate the classifier did not name never wins at the
  maximum allowed weights;
- an exact score tie → verdict beats anchor → higher `c` → configured order;
- non-finite inputs → `internal_error` → R9.1 route;
- invalid config: each bound, the cross-field inequality, unknown keys, prior
  keys not candidates, `Auto` as a prior key.

Integration tests (`tests/`, scripted nodes and the scripted TypeSafe server):

- an explicit route name is never scored (no `scoring` block);
- a hard R8 rule (non-classifying) is never scored;
- an `Auto` fallback (no rule matched) is never scored;
- classifier failure (`timeout`, `auth_error`) → `no_verdict` → R9.1 fallback,
  and **no** history recorded against any candidate for the classifier failure;
- history attribution: `ok` / `server_error` / `interrupted` move the right
  counters; `unavailable` and `route_capability_mismatch` are recorded but not
  scored; `client_error` and `cancelled` are neutral; nested `-classify`
  requests are excluded;
- direct traffic feeds the same route's history;
- the trace block's components sum to the score; no deployment field in it;
- metrics labels are exactly as specified;
- after scoring picks a route with no healthy deployment →
  `route_unavailable`, with **no** second route attempted (R9.3 absent);
- the deployment chosen inside the winning route is the one the policy would
  choose anyway (scoring never touches the deployment choice).

## 26. Migration and backward compatibility

- No `adaptive_scoring` section → no change at all: same decisions, traces,
  metrics and admin view (the new admin field reads `{"configured": false}`).
- `enabled: false` → same as absent, apart from the configuration echo.
- `enabled: true` with default weights → same **decisions** as R9.1, plus
  scoring traces and history metrics. This is the safe way to observe
  history before giving it any weight.
- R9.1/R9.1a classifier configuration, both shapes, is untouched. Switching
  provider changes no rule and no scoring setting. The validation bound is
  rechecked against the new provider's `min_confidence`.
- Copy that changes when scoring is enabled: "Classifier results below this
  threshold use the deterministic fallback route" becomes conditional. The UI
  and docs must say "…unless adaptive scoring is enabled, which may prefer the
  classifier's pick or the fallback within ±X of the threshold".

## 27. Performance

- Scoring: at most 17 candidates × a few multiply-adds and one decay
  computation each. No allocation beyond the trace block, no I/O, no network,
  no model call. Expected well under 10 µs, measured into the existing
  `router_routing_duration_seconds`.
- Recording: one lock and a few float operations per finished request.
- Memory: `O(routes)`, a few dozen bytes each. The trace block is bounded by
  the candidate cap and lives in the existing bounded ring.
- An implementation PR should include a microbenchmark of `score()` and a
  check that `routing_ms` p99 is unchanged in the surface tests.

## 28. Security and privacy

- Inputs are the verdict's route and confidence, configured numbers, and
  per-route outcome counts. No prompt, message, tool argument, session id,
  client address or credential is read, stored, traced or exported.
- Configuration contains no secrets. History is in memory only.
- Metric labels are bounded to configured route names and fixed enums.
- A future Jev `route_scores` carries numbers per configured candidate name,
  never provider text.
- The admin view of history is behind the router's existing client-key policy.

## 29. Open questions (resolved for slice 1; see section 0)

Questions 1, 2, 4, 6 and 7 are decided (Decisions 1, 2, 4, 6, 7 and the hard
threshold boundary). Question 3 is answered provisionally (1 h, configurable,
untuned). Question 5 stays open until the real Jev API is verified.

1. **Low-confidence semantics when enabled:** is it acceptable that a
   `low_confidence` verdict can win when priors and history favour it, within
   the bound? (This design says yes, bounded and traced. The alternative,
   scoring only `chosen` verdicts, makes R9.2 able only to *demote* a
   classification to the fallback.)
2. **Per-provider weights:** the Lightweight provider's self-reported
   confidence and Jev's certainty are not calibrated to each other. Should
   weights live per provider block? (Proposed: no in slice 1; one set,
   validated against each provider's threshold.)
3. **Default half-life:** 1 h is an argument, not a measurement. It should be
   set from real traffic before the docs recommend it.
4. **History reset:** should there be `POST /api/router/v1/scoring/reset`
   (loopback-only) for an operator who swaps models at runtime? Proposed: only
   if decay proves too slow in practice.
5. **Jev `probabilities` stability:** are they always present, complete over
   the options, and summing to 1? This must be verified against the real API
   (no key on the development machine yet) before slice 3.
6. **Route-level pruning:** should obviously impossible routes (e.g. a
   completion request for a chat-only route) be removed before scoring?
   Routes have no static capability config, so this would need a new,
   route-level, operator-declared field rather than derived deployment data.
7. **Should `unavailable` ever enter the score**, or remain R9.3's concern
   alone? Proposed: never in R9.2.

## 30. Recommended first implementation slice

Based on the inventory, these are the pieces that are both real and
explainable today:

**Slice 1 (`feature/router-route-scoring`):**

1. a pure `scoring` module: anchor + prior + gated, shrunk, decayed success
   history; the tie-break; the validation bound;
2. `auto_route.adaptive_scoring` config with neutral defaults and full
   validation;
3. per-route `RouteHistory`, recorded at `Tracker::finish` (non-nested), with
   the failure taxonomy of section 17;
4. a single call site in `proxy::resolve_auto`;
5. the trace `scoring` block, the metrics of section 23, and `adaptive_scoring`
   in `GET /api/router/v1/auto`;
6. the tests of section 25; docs.

Not in slice 1: UI controls (slice 2, after slice 1 is reviewed), Jev
`route_scores` (slice 3, after verifying the real API), context-fit bands and
latency tie-break (each a later slice, each with its own measurements), and
persistence.
