# R9.3 — Explicit cross-route fallback (design)

Status: **design only**. Nothing here is implemented. No fallback execution,
configuration field, trace field, metric or UI described here exists on
`master` (`ef4f868`). R9.1, R9.1a, the classifier UI and R9.2 slice 1 are
merged and frozen; this design builds on them without changing them.

The rules everything below obeys:

> **Cross-route fallback is explicit recovery from a logical route's
> inability to execute.** It is not semantic reclassification, not
> deployment failover, not quality retry, and not mixture-of-agents.
>
> **Never switch logical routes after response or side-effect commit.**

---

## 1. Goals

- When the logical route chosen for an `Auto` request **cannot execute** it,
  try the next logical route the operator **explicitly** listed for that
  route. Fall back only before anything was committed to the client, and only
  after the route's own deployment failover is exhausted.
- Keep the layers apart. R9.1 and R9.2 choose the first route. The
  deployment router (health → R5 → affinity → policy → same-route failover)
  serves each route. R9.3 only decides *which explicit route is next* when a
  route as a whole failed in a qualifying, pre-commit way.
- Be deterministic, bounded and explainable. The order is configuration
  order. Chains are validated when the file loads. Every hop is traced and
  counted, and no step runs without a configured line that says so.
- Keep backward compatibility exact: with no R9.3 configuration nothing
  changes.

## 2. Non-goals

- Changing deployment failover, the 500 policy, health, R5, affinity,
  placement, or any route policy.
- Falling back on a 500, on a single deployment's refusal while the route
  still has candidates, on latency or TTFT, on answer quality, on
  confidence, on tool results, after a stream started, on client errors or on
  cancellation.
- Re-running classification or R9.2 scoring, or reading route history,
  latency or priors to choose a fallback.
- Implicit or global fallback ("everything ends at General") unless
  configured.
- Fallback for explicitly named routes (slice 1). Fallback *to* `Auto`.
- Parallel or multi-route execution, voting or aggregation (R9.4).
- Loading models, waiting for placement, or changing placement targets.
- UI (a later slice).

## 3. Architecture boundary

```text
client model ──┬── "Coder" (explicit) ──────────────────────────────┐ no R9.3 (slice 1)
               └── "Auto" → R8 rules → R9.1 classify → R9.2 score ──┤
                                                                    ▼
                                         initial logical route  (e.g. Coder)
                                                                    │
     ┌──────────────── one route attempt (existing code, unchanged) ┴──────────┐
     │ health → R5 capability filter → affinity(route, session) → policy       │
     │ → deployment attempts with same-route failover (pre-response only)      │
     └─────────────────────────────┬────────────────────────────────────────────┘
                                   │
          committed (any answer)   │   qualifying route-level failure, nothing committed
          ◀────────────────────────┤──────────────────────────────▶ R9.3: next explicit route?
          relayed as today         │                                  │ yes → one route attempt for it
                                   │                                  │ no  → the route's own error
```

R9.3 is a loop **around** the existing per-route attempt, at one place: the
terminal error paths of `proxy::route_request`. Every qualifying failure is
decided on a path that returns *before* `commit()` is ever called, so the
commit boundary is structural, not a flag.

## 4. Terminology

The router already uses "fallback" for two other things, so R9.3 needs its
own words.

| Term | Meaning | Exists |
|---|---|---|
| `auto_route.fallback_route` | where `Auto` goes when no rule matches | R8 |
| `classifier.fallback_route` | where a failed or unsure classification goes | R9.1 |
| deployment failover | the next **deployment of the same route**, pre-response | R0–R5 |
| **cross-route fallback** | the next **logical route** the operator listed, after the whole route failed pre-commit | R9.3 (this) |
| initial route | the route R8/R9.1/R9.2 resolved `Auto` to | — |
| final route | the route that committed the response, or the last one tried | — |
| route attempt | one pass of the existing pipeline for one logical route | — |

## 5. Route-level failure outcomes that exist today

Read from `proxy::route_request` and `attempt_one` on `master`:

| Path | What the client gets today | Committed? | Same-route failover exhausted? |
|---|---|---|---|
| `plan_with_affinity` → `RoutingFailure::RouteUnavailable` (no healthy, served, available deployment) | `503 route_unavailable` (+ `Retry-After`) | no: nothing sent | yes: nothing to try |
| `plan_with_affinity` → `RoutingFailure::CapabilityMismatch` (available deployments, none able to serve this request) | `400 route_capability_mismatch` | no: nothing sent | yes |
| every attempted deployment failed **without** an answer worth returning (connect refused/timeout, transport error before head, `404 model_not_found`) | `503 route_unavailable` | no | yes: loop ended |
| every attempted deployment **refused with 502/503/504** (`Attempt::Next(Some(refusal))`) | the **last node's own** refusal (e.g. `503 overloaded`) | no: refusals are read, never relayed mid-flight | yes |
| `400 context_length_exceeded` and no larger-context deployment left | the node's `400 context_length_exceeded` | no | yes |
| any other answer: 2xx, 500, 429, other 4xx, a 404 that is not `model_not_found` | relayed | **yes: `Attempt::Committed` at the response head** | n/a |

Unknown route, no default route and `Auto`'s own requirement refusals happen
before a route exists and are never R9.3's concern.

## 6. Triggering failure conditions (slice 1)

| Trigger (`reason`) | From | Why it qualifies |
|---|---|---|
| `route_unavailable` | both `route_unavailable` paths above | the route as a whole had nothing that could take the request; nothing was generated or committed |
| `route_exhausted` | every attempted deployment refused with 502/503/504 | the same fact in another form: every deployment turned the request away before answering, failover is exhausted, nothing committed. R9.2 history already counts it as `unavailable` |
| `route_capability_mismatch` | the plan refused for fit | no available deployment of the route can serve this request; another route might. R5 still filters the next route normally |

`route_exhausted` is the one addition to the two candidates in the brief.
Without it, the most common unavailability on a busy CPU node (`503
overloaded` from every deployment) would never fall back, although it is as
safe as `route_unavailable`. If it is not wanted, it is one line to drop
(open question 1).

## 7. Excluded failure conditions

- **A 500 from a deployment.** It is committed today and never retried. It is
  one deployment's answer, not the route's, and R9.3 does not change that.
- **One deployment's 502/503/504 while the route still has candidates.**
  Same-route failover handles that; R9.3 sees only the exhausted route.
- **`400 context_length_exceeded` after in-route overflow failover is
  exhausted.** It is pre-commit and route-level, but it depends on request
  size and is close to a capability mismatch the router could not foresee.
  Deferred (open question 2).
- **429, any other 4xx, and every committed answer.**
- **Slow TTFT, latency, answer quality, low confidence, tool-result quality
  and user dissatisfaction**: none of these is an execution failure.
- **A stream interrupted after commit**: the client gets the in-band error as
  today.
- **Client cancellation**: nobody is left to answer.
- **A classifier failure**: R9.1 already resolves it to its fallback; that
  is not a route failure.

## 8. Response-commit boundary

The existing boundary, unchanged, is the response head:

- `attempt_one` returns `Attempt::Committed` for any status except
  `400 context_length_exceeded`, `404 model_not_found` and 502/503/504.
- From `Attempt::Committed`, `route_request` calls `commit()`. A stream is
  relayed frame by frame through `FrameRewriter` and a body is read and
  forwarded. Affinity settles, `record_request` counts the outcome, and the
  route can no longer change.
- This applies equally to SSE and non-streaming responses, to content,
  `reasoning_content` and tool-call deltas: the decision is taken at the
  **head**, before the first frame is read. Nothing a route generated is ever
  shown to the client and then followed by another route's output.

R9.3 acts only on the terminal **error** returns of `route_request` (section
5's uncommitted rows), never inside `commit()` or `relay()`. **A route switch
after commit is unreachable by construction:** the committed branch
`return`s.

## 9. Side-effect and tool boundary

This is the question that decides whether pre-response fallback is safe.

**What executes where today:**

- A Lightweight node never executes a tool. The gateway converts tool
  definitions into the model's prompt and returns `tool_calls` (or streamed
  tool-call deltas) to its caller. The only process it spawns is its engine
  (`supervisor.rs`, `Command::new(server_path)`). There is no server-side
  tool, MCP client, web fetch or action anywhere in the gateway, inference or
  backend crates.
- The router forwards and never executes anything either.
- Tools run **in the client** (Lightagent, an SDK) after it receives a
  committed response with a tool call.

**Therefore, on the router→node path, side-effect commit can come no earlier
than response commit.** Every qualifying R9.3 failure happens before the
response head, so no tool call was delivered and no tool can have run. The
worst cost of a fallback is wasted compute: a node that accepted the
connection, started prefill, then failed at the transport level before
answering.

**The model starts a tool call, and the route fails before the client
receives it.** If the failure is before the response head (transport error,
502/503/504), the tool call was internal generation that nobody saw, so
fallback is safe and the next route generates from scratch. If the head was
received, the response is committed, and any later failure is in-band, with
no fallback.

**Rule:** cross-route fallback only before the response head, and so before
any tool call or other externally visible output reaches the client.

**Forward constraint (not slice 1):** if a node ever executes server-side
tools or actions, its side effects could precede the head. Such a node would
have to declare a capability (for example `server_side_effects: true` in
`/v1/capabilities`), and R9.3 would then never fall back from a route that
sent the request to one. A stronger `side_effect_commit` boundary is only
needed if that capability ever exists.

**Other side effects:**

- The R9.1 classification (a nested Lightweight request, or one Jev HTTPS
  call) ran once, before the first route. Fallback never repeats it.
- Placement and affinity writes happen only on commit (affinity settles on a
  *successful* commit) or in the background controller.

## 10. Same-route failover precedence

R9.3 runs only after one route attempt has fully finished in a qualifying
way. Every deployment of `Coder` the existing plan includes is tried, in
policy order, with the existing pre-response failover rules, before `Coder →
General` is considered. A route with deployment A unavailable and deployment
B healthy **is not a route failure**: B answers, the route is `Coder`, and
R9.3 never runs. This is mandatory, and tested (section 27).

## 11. Configuration schema proposal

A new optional section **under `auto_route`**, because slice 1 applies only
to `Auto`-resolved requests:

```json
"auto_route": {
  "...": "unchanged R8 / R9.1 / R9.2 sections",
  "cross_route_fallback": {
    "Coder":     ["General"],
    "Research":  ["General"],
    "Reasoning": ["Research", "General"]
  }
}
```

- Each key is a logical route `Auto` can resolve to. Its value is the
  **ordered, complete** list of routes to try after it, in order.
- **The list is flat and non-transitive.** When `Reasoning` fails, R9.3 tries
  `Research`, then `General`: exactly `Reasoning`'s list, never `Research`'s
  own list. What happens is readable from one line of the file, and runtime
  never walks a graph.
- Absent section, or a route with no entry, means no cross-route fallback.
  There is no separate `enabled` flag: the section's presence is the
  switch, as with `session_affinity` and `placement`.
- There is one list for all triggers. Reason-specific lists
  (`on_unavailable`, `on_capability_mismatch`) are deferred; nothing so far
  shows they would differ (open question 4).

**Alternative considered:** a per-route field (`routes[].fallback`, like
`placement`). It reads naturally, but a route-level field that silently does
nothing for explicit requests is a trap. If explicit-route fallback is ever
opted into, a per-route field with an explicit scope would fit that slice
better.

## 12. Graph validation (refused at load, like every section)

For every key and every listed target:

- **unknown route**: not a configured route;
- **`Auto`, or `default`, or another reserved name**: a target must be a
  concrete route, never a request to re-run selection;
- **the Lightweight classifier's route** (the active or standby provider's
  `route`): it is a classifier, not an answering route;
- **empty name**, **an empty list**, and **duplicates within a list**
  (`["General", "general"]`, matched as route names are);
- **a self-reference** (`"Coder": ["Coder"]`);
- **cycles in the union graph**, where each `key → target` is an edge:
  `A → B, B → A` and `A → B, B → C, C → A` are refused. Runtime never follows
  chains transitively, so a cycle could not loop. It is still refused, as the
  brief requires, because it is almost always a mistake, and refusing now
  keeps a transitive variant possible later without a breaking change;
- **a list longer than `MAX_FALLBACK_ROUTES`** (proposed **3**, so at most 4
  routes per request);
- **a key `Auto` can never resolve to** (not a rule route, not a classifier
  candidate or fallback, not `auto_route.fallback_route`). Its entry could
  never run, so it is refused as a typo;
- **unknown keys inside the section**: it is a map from route name to list,
  so any non-list value is a parse error.

The cycle check is a DFS over at most `routes × 3` edges, run once at load.

## 13. Maximum depth

A request makes at most **1 + `MAX_FALLBACK_ROUTES` = 4 route attempts**,
however the file is written: the list length is validated and lists are not
transitive. Each route attempt is bounded by its own plan, which lists each
deployment at most once. So the worst case is
`Σ deployments(route attempted) + 1` deployment attempts, with the extra one
for context-overflow failover, which stays inside a route.

## 14. Explicit-route policy

**Slice 1: no cross-route fallback for an explicitly named route.**
`model: "Coder"` means "Coder or fail". A client that pinned a route must
never silently get another model's answer. An explicit route that fails
returns its own error, exactly as today. Opting explicit requests in would be
a separate, explicit configuration in a later slice, with its own scope
field and its own review.

The same holds for `default` (and an omitted `model`), which is a named
route too, and for the router's own **nested classification requests**:
their failure is R9.1's to handle, and it already resolves to the classifier
fallback.

## 15. Auto policy

R9.3 applies to every `Auto` request **after** `Auto` resolved it to a
concrete route, whichever way it did: a deterministic R8 rule, a
classification (R9.1), scoring (R9.2), or `Auto`'s plain `fallback_route`.
All of them are the router's own choice, made for a client that asked the
router to choose. That is what R9.3 is for.

(Open question 3: should a *deterministic* rule's route be eligible? A
`tool_choice=required → ToolAgent` rule may express a hard requirement,
where falling back to `General` would be wrong. R5 still refuses a
`General` that cannot do tools, so the risk is semantic, not a broken
request. Recommendation: eligible. The operator controls it by not listing
`ToolAgent`.)

## 16. Fallback attempt order

```text
attempt 1: initial route R0              (existing pipeline, all of R0's deployments)
attempt 2: chain(R0)[0]                  only if attempt 1 ended in a qualifying failure
attempt 3: chain(R0)[1]                  only if attempt 2 ended in a qualifying failure
attempt 4: chain(R0)[2]                  likewise
then: the last route's own error
```

- Each attempt runs the **unchanged** pipeline for its route: plan from the
  same health and deployment snapshot semantics (re-read per attempt, as
  today per request), the **same request requirements** (read once, reused),
  R5 filtering, that route's affinity, policy and failover.
- A route attempt that **commits** (any status) ends the request. A
  committed 500 from the fallback route is the answer, as it would be for any
  request.
- A **non-qualifying** pre-commit outcome cannot occur; section 5 lists them
  all.
- **No re-classification, no R9.2 re-scoring, no priors, no history, no
  latency.** The order is the configuration's order.

## 17. Timeout and deadline handling

**What exists:** there is no per-request deadline. The router sets only
`request.connect_timeout_secs` (default 5 s), because a CPU prefill can take
minutes and no generation limit chosen on one machine is right for another.

**Decision for slice 1:** no new deadline system. R9.3 adds only route
attempts that end **before** a response head. The added latency per failed
route is at most the sum of its deployments' connect timeouts, or the time
for a node to refuse; plan-time `route_unavailable` and capability mismatch
cost microseconds. With at most four route attempts and each route's
deployments tried once, the worst case is bounded by configuration:
`connect_timeout × Σ deployments over the attempted routes`. A client that
gives up cancels the whole request, as today: dropping the future drops
every in-flight attempt.

A shared request budget (for example `cross_route_fallback_budget_ms`, which
is checked before each further route attempt and never interrupts one) is
the natural extension if real traffic shows long chains of slow connect
failures. It is deferred (open question 5).

## 18. Affinity interaction

- Affinity is keyed by **route and session** (`AffinityBook::session(&route,
  headers)`), so each route attempt looks up **its own** route's affinity.
  `General`'s affinity is used for `General`; `Coder`'s is never reused for
  it.
- When the fallback route commits successfully, the session settles on that
  route's deployment under normal affinity semantics, keyed `(General,
  session)`. That only affects future requests that also resolve to
  `General`.
- `Coder`'s entry for the session is left as it is. Nothing committed on
  `Coder`, so there is nothing to settle or reassign there, and it expires
  by its idle TTL as usual.
- Fallback never changes which route a future `Auto` request resolves to.
  Affinity stays a deployment preference inside a route.

## 19. Placement interaction

R9.3 never loads, waits or changes targets. A route whose placement is still
loading answers `route_unavailable` exactly as today, and that qualifies like
any other unavailability. R7 prepares capacity; R9.3 recovers from its
absence. Neither calls the other.

## 20. R9.2 interaction

- R9.2 decides the **initial** route only, unchanged. Its trace block
  (`winner`, `overrode`, …) still describes that decision.
- Fallback uses configuration order: no re-scoring, no priors, no history,
  no classifier signal.
- Route-history observations (R9.2, observational) are recorded per route
  attempt. The initial route records its qualifying failure as `unavailable`
  or `mismatch`, and the final route records its final outcome. History
  still never affects routing.

## 21. Response identity

The response names the route that **served** it. `FrameRewriter` already
rewrites `model` to the committing route, so `Auto → Coder → General`
answers `model: "General"`. The client is never told it got `Coder`'s model
when it got `General`'s. The trace keeps the whole story:

```text
requested_route = Auto
initial_route   = Coder      (R8/R9.1/R9.2's choice)
route           = General    (final: served it, or was tried last)
```

## 22. Trace schema proposal

One trace per request, as today. `route` and every deployment field describe
the **final** route attempt. A new optional block appears only when a
fallback was considered:

```json
"cross_route_fallback": {
  "initial_route": "Coder",
  "final_route": "General",
  "exhausted": false,
  "attempts": [
    { "route": "Coder",   "outcome": "failed", "reason": "route_unavailable" },
    { "route": "General", "outcome": "committed" }
  ]
}
```

- `reason` is one of `route_unavailable`, `route_exhausted` and
  `route_capability_mismatch`, never a generic "fallback".
- `exhausted: true` means the chain ended without a commit, and the client
  got the last route's error.
- Route names and categories only; no deployment, node, session or prompt.
- Deployment detail stays where it is today. The existing `attempts[]`
  (per deployment) gains an additive `route` field, so a deployment attempt
  remains attributable when two routes were tried. `unavailable` and `unfit`
  describe the final route; earlier routes are summarised by their reason.

## 23. Metrics proposal

| Metric | Labels | Cardinality |
|---|---|---|
| `router_cross_route_fallback_total` | `from_route`, `to_route`, `reason` | configured pairs only (≤ keys × 3) × 3 reasons |
| `router_cross_route_fallback_exhausted_total` | `route` (initial), `reason` (last) | keys × 3 |

- `router_requests_total` keeps its meaning of one count per request, under
  the **final** route. The initial route's failure is visible in
  `router_cross_route_fallback_total{from_route}`, and also in R9.2's
  observation counters. (Open question 6: count each route attempt in
  `router_requests_total` instead.)
- `router_auto_route_decisions_total` still counts the initial decision.
- Never a request id, session, prompt, user or address as a label.

## 24. Admin view proposal (API only in slice 1)

`GET /api/router/v1/auto` gains:

```json
"cross_route_fallback": {
  "configured": true,
  "chains": { "Coder": ["General"], "Research": ["General"] },
  "max_routes": 3,
  "counts": { "Coder": { "General": { "route_unavailable": 4 } } },
  "exhausted": { "Research": { "route_unavailable": 1 } }
}
```

With no section it is `{"configured": false}`. Validation state needs no
field: an invalid file never loads. The UI, chains on the Auto Routing
screen, is a later slice.

## 25. Errors

- **Chain exhausted:** the client gets the **last attempted route's own
  error**, as that route would have returned it: `503 route_unavailable`
  naming that route, or the last node's 502/503/504 refusal, or `400
  route_capability_mismatch`. A specific existing error is never replaced by
  a generic 500. (Open question 7: return the initial route's error
  instead.)
- **No chain:** exactly today's error.
- **Configuration:** a new `ConfigError::BadCrossRouteFallback { problem }`,
  rendered `auto_route.cross_route_fallback: …` like its siblings.

## 26. Backward compatibility, security, performance

- **Compatibility.** No section means byte-identical behaviour: the same
  decisions, errors, traces, metrics and admin view (the admin field reads
  `{"configured": false}`). Explicit requests are never affected in slice 1.
- **Security and privacy.** Inputs are route names, failure categories and
  configuration. Nothing new is stored or logged. One `cross-route fallback`
  log line per hop carries `request_id`, `from_route`, `to_route`, `reason`
  and `attempt`; no prompt, no secret. There is no external call of its own.
- **Performance.** The chain is a `Vec` lookup by route index, computed at
  load. Deciding "next route" is constant time with no I/O. Only the route
  attempts themselves cost network and inference.

## 27. Test plan (for the implementation)

**Configuration**
- no section → existing behaviour, request for request
- one fallback; a multi-entry list
- refused: unknown target, `Auto`, `default`, the classifier route, a
  self-loop, a two-route cycle, a three-route cycle, a duplicate (also by
  case), an empty list or name, more than 3 entries, an unreachable key,
  and an off-shape value

**Route unavailable**
- primary unavailable → fallback succeeds: `model` is the fallback, trace
  and metrics are as specified
- primary and fallback unavailable → the last route's `route_unavailable`,
  `exhausted: true`
- a three-entry list → the third route succeeds
- every deployment of the primary refuses 503 (`route_exhausted`) → fallback
  succeeds

**Capability mismatch**
- primary mismatch → fallback able to serve succeeds; R5 still ran on it
  (an unable fallback is refused in turn)
- fallback also mismatched → next entry, or the terminal error

**Precedence and exclusions (mandatory)**
- deployment A unavailable, B healthy → stays on the primary, no
  cross-route fallback
- one deployment answers 500 → today's 500, no fallback, no second route
  contacted
- a stream commits, then breaks → in-band error, no second route
- an explicitly named route that is unavailable → its own error, no fallback
- a nested classification request → no fallback
- a client cancellation mid-chain → the request ends and nothing more is
  tried

**Auto, R9.2, affinity, placement**
- Auto → Coder (by rule, by classification, by scoring, by `fallback_route`)
  → General
- no re-classification (classifier hit count unchanged) and no re-scoring
  (the scoring block shows the initial decision only)
- `General`'s affinity used; `Coder`'s untouched; the session settles on
  `General`'s deployment
- no placement action and no load requested on fallback
- R9.2 observations: `unavailable` for Coder, the final outcome for General

**Bounds**
- at most 4 route attempts, and each deployment at most once per route
  attempt

## 28. Mutation-test plan

Each deliberate break must fail at least one test:

1. remove cycle validation;
2. allow fallback after commit (for example on a committed 500 or a broken
   stream);
3. fall back before same-route failover is exhausted (on the first
   deployment's 503);
4. fall back for an explicit route;
5. follow chains transitively;
6. re-run classification or scoring on fallback;
7. drop the list-length bound;
8. treat a 500 as a trigger;
9. reuse the initial route's affinity for the fallback route.

## 29. Open questions

1. Include `route_exhausted` (every deployment refused 502/503/504) as a
   trigger? Recommended: **yes**, as reasoned in section 6.
2. Treat an exhausted `400 context_length_exceeded` as a trigger? Deferred.
   It is size-dependent and needs its own measurements.
3. Are deterministic R8 rule routes eligible? Recommended: yes, controlled by
   which keys are listed.
4. Reason-specific lists? Deferred until two triggers need different targets.
5. A shared fallback budget (`…_budget_ms`)? Deferred until measured.
6. Count each route attempt in `router_requests_total`? Recommended: no;
   keep one count per request under the final route.
7. On exhaustion, return the last route's error or the initial route's?
   Recommended: the last route's.
8. Explicit-route opt-in: whether, and with what scope field. Not slice 1.
9. A server-side-effect capability, for gateways that might one day execute
   tools (section 9). Not needed today.

## 30. Recommended first implementation slice

**`feature/router-cross-route-fallback`:**

1. `auto_route.cross_route_fallback` as flat, ordered, non-transitive lists,
   with every validation in section 12 and `MAX_FALLBACK_ROUTES = 3`;
2. applies to `Auto`-resolved requests only; never to explicit, `default` or
   nested requests;
3. triggers: `route_unavailable`, `route_exhausted` (subject to open question
   1) and `route_capability_mismatch`, each only from the uncommitted
   terminal paths of `route_request`, after same-route failover;
4. one loop around the existing per-route attempt. That attempt is
   refactored so a qualifying failure is returned to the loop instead of
   finishing the trace, with its behaviour otherwise unchanged;
5. response identity from the committing route; one trace with the
   `cross_route_fallback` block and `route` on deployment attempts; the two
   metrics; the admin field; the log line;
6. no 500 fallback, no latency or quality trigger, no reclassification, no
   R9.2 re-scoring, no history influence, no placement action, no mid-stream
   switch, no MoA, no UI;
7. the tests of section 27 and the mutations of section 28; docs.
