# The federated model router

`hermes router` puts one OpenAI-compatible endpoint in front of any number of
Lightweight gateways. A client discovers and sends stable logical names, such as
`Coder` or `Fast`. The router decides which node answers and what that node calls
the model. The node decides how the model runs.

```text
Lightagent ─ model="Coder" ─▶ router ─ model="QwenCoder" ─▶ node A   (primary)
           ◀ model="Coder" ──        ◀ model="QwenCoder" ──
                                     └ model="CoderBackup" ─▶ node B   (fallback)
```

This document covers milestones R0 to R4:

- **R0–R3:** the domain model, a transparent proxy, a multi-node registry with
  health checks, and priority routing with failover before the response starts.
- **R4:** two load-balancing policies, round-robin and least-busy.

The [roadmap](#roadmap) lists what comes after.

## Who owns what

| Layer | Decides | Owns |
|---|---|---|
| Client (Lightagent) | which capability it wants | a route name such as `Coder` |
| **Router** | where the request goes | routes, the deployment registry, node health, routing policy, forwarding, stream relaying, failover, router logs and metrics |
| Node (`hermes serve`) | how the model runs | its aliases, canonical ids, GGUF files, RAM admission, the scheduler and the engine |

The router speaks only to a node's public `/v1` surface, the same surface any
client uses. It never parses GGUF, estimates memory, loads or unloads a model,
or reads a node's alias-to-canonical mapping. If a node is not serving the model
a deployment names, that deployment is unavailable. The router never asks the
node to load the model.

## Concepts

Each concept maps to one type in `crates/lightweight-router/src/domain.rs`.

- **Route** (`Route`, `RouteName`). The public name a client sees in
  `/v1/models` and sends as `model`. It follows the same rules as a node alias:
  matched ignoring case, no `/`, `\` or `@`, and never `default`. A route has a
  policy and one or more deployments, listed in order.
- **Deployment** (`Deployment`, `DeploymentId`). One model on one node, written
  `node/model`, for example `dell-7820/QwenCoder`. The `model` is the identity
  that node advertises in its own `/v1/models`, which is normally its alias. Two
  routes that list the same node and model share one deployment.
- **Node** (`Node`, `NodeId`, `NodeAuth`). One Lightweight gateway, reached at
  its URL with its own credential.
- **Health** (`NodeHealth`, `DeploymentHealth`, `UnavailableReason`). Kept apart
  from the configuration, in `health.rs`, so a probe can never change what the
  operator configured.
- **Decision** (`RoutingDecision`, `RoutingReason`, `RoutingFailure`). Where one
  attempt went and why, logged with every request.
- **Policy** (`RoutePolicy`). `priority`, `round_robin` or `least_busy`, chosen
  per route. See [Route policies](#route-policies). Each strategy is an enum
  variant with its own ordering function, so a later one is an addition rather
  than a redesign.

The route name and the node alias are independent. `Coder → Coder` is allowed,
and so is `Coder → QwenCoder`.

## Configuration

The configuration is JSON, matching `fleet.json`. It is read from
`<config dir>/router.json`, or from the file passed with `--config <path>`.
Unknown keys are refused, so a misspelt `enabeld` is an error rather than being
silently ignored.

```json
{
  "listen": ["127.0.0.1:11500"],
  "api_key_env": "LIGHTWEIGHT_ROUTER_KEY",
  "default_route": "Fast",
  "health": { "interval_secs": 5, "timeout_secs": 3, "failure_threshold": 2 },
  "request": { "connect_timeout_secs": 5 },
  "nodes": [
    { "id": "dell-7820", "url": "http://192.0.2.10:11434", "api_key_env": "LIGHTWEIGHT_DELL_KEY" },
    { "id": "t420",      "url": "http://192.0.2.11:11434", "api_key_env": "LIGHTWEIGHT_T420_KEY", "enabled": true }
  ],
  "routes": [
    { "name": "Coder", "strategy": "priority", "deployments": [
        { "node": "dell-7820", "model": "QwenCoder" },
        { "node": "t420",      "model": "CoderBackup" } ] },
    { "name": "Fast", "deployments": [ { "node": "t420", "model": "Fast" } ] }
  ]
}
```

| Key | Default | Meaning |
|---|---|---|
| `listen` | `["127.0.0.1:11500"]` | `host:port` socket addresses. `--listen` replaces this list; repeat the flag for several addresses. |
| `api_key_env` | none | The environment variable holding the key clients must present. Required if any listener is not on loopback. |
| `default_route` | none | The route that `"model": "default"`, or a request with no `model`, resolves to. If it is not set, such requests are refused. |
| `health.interval_secs` | 5 | How often each enabled node is probed. |
| `health.timeout_secs` | 3 | How long one probe may take. Must not exceed the interval. |
| `health.failure_threshold` | 2 | How many consecutive failures make a healthy node unhealthy. |
| `request.connect_timeout_secs` | 5 | How long a request waits to connect to a node. A generation itself has no timeout, because a CPU prefill can take minutes. |
| `nodes[].enabled` | `true` | The operator's off switch. A disabled node is never probed or sent traffic, and its key is not required. |

**Secrets are never written in the file.** Each node names its own environment
variable, and nothing falls back to a shared key. A URL that contains a
username or password is refused, and so is a literal `api_key`.

`hermes router validate-config` runs every check and exits without listening.
Every problem is reported at once, and the router refuses to start if there are
any:

- a duplicate node id, route name or deployment;
- a route that refers to an unknown node, or has no deployments;
- a route named `default`;
- a `default_route` that does not exist;
- a URL that is malformed, uses a scheme other than `http` or `https`, or carries
  credentials, a query or a fragment;
- an environment variable that is missing or empty;
- out-of-range health or request timings.

A configuration being valid and a node being up are separate questions. The
router starts even when every node is offline, and reports those nodes as
unhealthy.

## The client surface

| Endpoint | Behaviour |
|---|---|
| `GET /v1/models` | Lists every configured route, including routes with nothing available right now. Each row has `owned_by: "lightweight-router"`. When a route's context is known it is given under the gateway's names (`context_length`, `n_ctx`, `max_tokens`, `max_output_tokens`), using the **smallest** context among the deployments the route could send a request to right now (see below). No node, address, node-local name or file appears. |
| `GET /v1/capabilities` | The gateway's contract, with the same protocol name and version (`lightweight-public-inference`, v1) and the same top-level fields, plus a `routes` array. |
| `POST /v1/chat/completions`, `POST /v1/completions` | Routed and proxied, streamed or not. |
| `GET /health` | Never refused. Returns `ok`, `degraded` or `unavailable` with route counts, and nothing more. |
| `GET /metrics` | Prometheus text, behind the client key. |

**Context and capabilities come from one eligible set.** A route's public
context, features and concurrency are all computed by `select::summarize` over
exactly the deployments `select::plan` would try for a request at that moment.
That is the same eligible set the router uses to route. The invariant holds by
construction:

> A route's public context never advertises more context, and its features never
> claim more support, than a currently eligible deployment can serve.

- A feature is claimed only if every eligible deployment supports it.
- The context is the smallest among the eligible deployments.
- The router as a whole claims only what every available route supports.
- A route with nothing eligible claims nothing and lists no context.
- If an eligible deployment has never reported its figures, nothing narrower than
  "unknown" is advertised.
- `state.model` describes the default route, and only while that route is
  available.
- `limits.max_concurrent_requests` is the smallest limit among the eligible
  deployments, or `0` when there are none.

The public figure moves when the eligible set changes. For example, with a
32K-context primary and an 8K-context backup both healthy, `Coder` reports 8K.
With the backup down it reports 32K. That is the honest figure for where
requests can actually go.

**Per-deployment state is kept separately.** Each successful probe files the
node's features, context and concurrency limit under every deployment of that
node whose model is the one being served. These records are never merged, and a
route's summary never overwrites them. They survive the node going unhealthy,
and are overwritten only by a later observation of the same deployment.
`GET /api/router/v1/deployments` shows each deployment's own `observed` record.
This is the data the capability filter reads: "the request uses tools, so
deployment A is eligible and deployment B is not." See
[Capability filtering](#capability-filtering).

**The public summary and a request's eligibility answer different questions.**
The summary says what *any* request can safely assume about a route, so it is
the conservative intersection. A particular request is checked against each
deployment's own figures. With an 8K and a 32K deployment, `Coder` advertises
8K, and a 20K-token prompt is still served, by the 32K deployment alone. A
request is never refused because of the route's summary.

### Model identity, both ways

The router parses a request only far enough to read `model`. It rewrites `model`
to the chosen deployment's node-local name and forwards every other field
unchanged. On the way back, the response's `model` is rewritten to the route's
own spelling. That applies to a whole JSON body and to every streamed chunk that
carries a `model`.

Errors from a node are forwarded as the node wrote them, with their status, code
and message, so a context overflow still parses the same way. The router rewrites
model identity only where it appears as a structured `model` field: a whole
success body, or a streamed event, including an in-band error frame that carries
one. It never edits free-text error messages.

The only Lightweight error whose message names a node-local model is the 404
`model_not_found` a node sends after swapping models. The router never forwards
it: it fails over, or, when no deployment is left, answers with its own
`route_unavailable`, which names only the route. An unknown route is refused with
a message that names only what the client itself asked for.

| Request `model` | Result |
|---|---|
| A route name, in any casing | That route. |
| `default`, empty, or no `model` | `default_route`. If there is none: `400 no_default_route`. |
| Anything else, including a node-local alias | `404 model_not_found`, the gateway's own code. No other route is substituted. |
| A route with no available deployment | `503 route_unavailable`, with `Retry-After` set to the probe interval. |

### Streaming and cancellation

Each frame is relayed as soon as its terminating blank line arrives, never
buffered. Keep-alive comments, queue notices, `[DONE]` and in-band error frames
are passed through byte for byte. Only `model` is rewritten.

When a client disconnects, hyper drops the response body. The body owns the
connection to the node, so dropping it closes that connection, and the node's
gateway then cancels its generation. This was verified against a real engine:
the node recorded the generation as `cancelled` and freed its slot.

### Headers and credentials

Two separate trust boundaries:

- **Client to router:** the router's own key (`api_key_env`), checked by the same
  `AuthPolicy` the gateway uses. On a loopback listener with no key configured,
  `Bearer no-key-required` is accepted.
- **Router to node:** each node's own key, sent on probes and on requests.

Request headers are forwarded by name only. Upstream, a request carries
`Content-Type`, `Accept`, `X-Request-Id`, and the node's own `Authorization`.
The client's `Authorization`, its cookies and any other header are not
forwarded. Downstream, only `Content-Type`, `Cache-Control` and `Retry-After`
are copied from the node's response.

**Request ids:** a well-formed `X-Request-Id` from the client (up to 128 visible
ASCII characters) is kept. If the client sends none, the router generates one
(`rtr-…`). The id is sent to the node, echoed in the response, and included in
every router log line. Gateways do not log it yet. Node-side request-id logging
is a future observability enhancement (R6), deliberately left out of this
change.

## Health

Each enabled node is probed at its own `GET /v1/capabilities`, using its
credential. That one cheap call shows:

- whether the node is reachable;
- whether it is Lightweight (by protocol name and version);
- which model it is serving (`state.model.id`, alias first);
- what it supports, its context, and its concurrency limit.

Nothing is probed during a request.

- One success makes a node `healthy` immediately, with no router restart.
- `failure_threshold` consecutive failures make it `unhealthy`. A failed
  connection or a timeout during a request counts as one of those failures.
- A node that has never been seen stays `unknown`, and an `unknown` node gets no
  traffic, even if it has come up since the last probe.
- The threshold is `health.failure_threshold`. This is a deliberately simple,
  deterministic counter, not a circuit breaker.
- A deployment is **available** only if its node is enabled and healthy and is
  serving the deployment's model (compared ignoring case, as the node compares
  aliases).
- The first probe runs before the first request is accepted. A node that is
  offline then does not stop the router from starting.

## Route policies

Every request goes through the same five steps in the same order:

1. **Eligibility.** `select::eligible` drops every deployment that cannot take
   traffic now: a disabled node, an `unknown` or `unhealthy` node, or a node that
   is not serving the deployment's model. This is the one availability rule. It
   is shared by all three policies and by the route summaries in `/v1/models`
   and `/v1/capabilities`. A policy never sees an ineligible deployment, however
   idle it looks or whosever turn it would be.
2. **Capability.** `capability::filter` drops every deployment left that cannot
   serve *this* request. See [Capability filtering](#capability-filtering).
3. **Policy.** The route's `strategy` orders what is left. The first deployment
   is the initial choice, and the rest, in order, are where failover goes.
4. **Proxy.** The proxy only walks that order. It contains no policy logic.
5. **Failover**, before commitment only, exactly as described
   [below](#failover).

| `strategy` | Initial choice | Failover order |
|---|---|---|
| `priority` (the default when `strategy` is omitted) | The first eligible deployment in configured order | The rest in configured order |
| `round_robin` | The next turn in the eligible ring | The rest of the ring after that turn |
| `least_busy` | The eligible deployment with the lowest `active / limit`; ties go to configured order | The rest in load order, as observed when the request was planned |

An unknown `strategy` (`"fastest"`) is refused when the configuration is read.
It is never treated as `priority`. Existing configurations, which use `priority`
or omit the field, work unchanged. The policy never changes a route's public
identity: `/v1/models` lists `Coder` whatever `Coder`'s strategy is, and
`default` or an omitted `model` runs the default route's own policy.

None of the policies looks at latency, time to first token, request duration,
throughput, history, weights, sessions, the request's content, or the
deployment's capabilities. Capabilities are applied before a policy runs, so
each policy orders a list that is already right for the request.

```json
{ "name": "Coder",   "strategy": "priority",    "deployments": [
    { "node": "node-a", "model": "Coder" }, { "node": "node-b", "model": "CoderBackup" } ] }
{ "name": "Fast",    "strategy": "round_robin", "deployments": [
    { "node": "node-a", "model": "Fast" }, { "node": "node-b", "model": "Fast" }, { "node": "node-c", "model": "Fast" } ] }
{ "name": "General", "strategy": "least_busy",  "deployments": [
    { "node": "node-a", "model": "General" }, { "node": "node-b", "model": "General" } ] }
```

### Priority

The first eligible deployment in configured order gets the request. When a
primary recovers, it is first again on the very next request. Priority behaves
exactly as it did in R3, and every R3 test passes as written.

### Round-robin

The **ring** is the route's eligible deployments in configured order. Each route
has one cursor, an atomic counter. Each request draws exactly one value from it
with a single `fetch_add`, and starts at `ring[cursor % ring.len()]`.

- A rotation across A, B and C goes A B C A B C.
- If B is unhealthy, the ring is A and C, so requests alternate between A and C.
  The rotation never lands on B to keep a count.
- When B recovers, it is back in the ring from the next request on.
- **One step per client request.** A failover attempt does not advance the
  cursor. It continues to the rest of the ring after the chosen deployment.
  Three requests, one of which needed failover, leave the cursor at 3.
- **Concurrency safety.** Two requests can never draw the same value, and no lock
  is needed. 2400 concurrent plans over a ring of two split exactly 1200/1200.
- The cursor lives in memory. A router restart resets the rotation, which is
  harmless.

The ring index wraps when the eligible set grows or shrinks, so the exact
position after a health change is arbitrary. Equal steps over the current ring
are what is guaranteed.

### Least-busy

**Load is the router's own count of in-flight upstream attempts** on each
deployment, divided by the **concurrency limit** that deployment's node
advertises. The router never infers capacity on its own.

**What the limit means.** It is `limits.max_concurrent_requests` from the node's
`/v1/capabilities`: the gateway scheduler's slot count. `hermes serve` sets that
from the engine's confirmed parallel slots (`n_parallel`, via `--concurrency`).
It is the scheduler's *live* count, so when a model loaded at runtime resizes the
scheduler, the node's next answer carries the new limit and the router adopts it
on its next probe. Requests beyond it queue inside the node, which owns its queue. The router reads
the limit from the per-deployment observation it already keeps for capabilities,
so there is no second capacity model.

**Comparing loads.** `active_a / limit_a` against `active_b / limit_b` is
computed exactly as `active_a × limit_b` against `active_b × limit_a` in `u128`:
no division, no floating point, no overflow. Lower wins, and an exact tie goes to
configured order (`least_busy_tiebreak`).

- **2/8 beats 1/2.** That is 25% against 50%, even though 1 is fewer than 2.
- **1/4 ties with 2/8,** so the deployment configured first wins.
- **Everything full is still a tie.** With 4/4 and 2/2, configured order picks.
  The router does no admission control of its own: the node owns its queue.

**Unknown or zero limits.** A deployment whose limit is unknown, or advertised
as 0, is never assumed to have room.

- It ranks after every deployment with a known positive limit, even a full one,
  but stays in the plan as a fallback.
- Among such deployments, the one with fewer requests in flight goes first, then
  configured order.
- Nothing ever divides by zero.

A Lightweight gateway never advertises 0: its scheduler clamps the count to at
least 1. So in practice this rule only covers a race in which a deployment has
no observation yet.

**Choosing and reserving happen together.** For each route, the router reads the
loads, orders them, and takes one slot on the first choice while holding that
route's lock. A second concurrent request therefore sees the first one's slot
already counted. Two requests can never both see an idle A and both take it.

The lock is per route, so other routes never wait on it. Two routes that share a
deployment each hold their own lock, so a request on one can momentarily miss a
reservation made on the other. That costs one slightly uneven choice, never
correctness.

**Capacity-sensitive by construction, with no weights.** With limits of 4 and
1, five requests held at once land 4/1, and ten land 8/2. That held whatever
order the threads ran in. A higher-capacity node takes proportionally more work
because its ratio rises more slowly.

### In-flight accounting

Every policy counts in-flight work, so the control API shows real numbers for
priority and round-robin routes too.

- A slot is taken when a deployment is **chosen for an upstream attempt**:
  - for the first choice, when the plan is made;
  - for a failover attempt, when that attempt starts.

  Inspecting candidates takes no slot.
- The slot is held by a `Lease`, which gives it back when dropped:
  - a pre-commit failure drops it before the next deployment's slot is taken;
  - a committed stream carries it in its body until the stream ends or the
    client goes away;
  - a whole body releases it once the body has been read.
- Success, a `4xx`/`5xx` answer, failover, a stream failure, a connection
  timeout, a client disconnect, a client timeout and a panic are all drops.
  Tests cover each of these, and a real disconnect released the slot while the
  node recorded the generation as `cancelled`.

## Capability filtering

Between eligibility and policy, the router asks one more question: **can this
deployment serve this particular request?** It is a yes or a no per deployment.
Nothing is ranked, and a deployment is never preferred for supporting more.

### What a request requires

`requirements::extract` reads the request once, with the gateway's own request
types and the same conversion the gateway runs before it generates. It reads
only fields the client set, and never the meaning of a prompt.

| Request | Requires of the deployment | Feature it reads |
|---|---|---|
| `POST /v1/chat/completions` | chat completions | `chat_completions` |
| `POST /v1/completions` | text completions | `completions` |
| `tools` with at least one entry | tool calling | `tools` |
| `tools: []` or no `tools` | nothing (the gateway also reads `[]` as no tools) | — |
| `tool_choice: "required"` or a named function | tool calling, and honouring `tool_choice` | `tools` and `tool_choice` |
| `tool_choice: "none"` beside declared tools | tool calling, and honouring `tool_choice` (it forbids a call the model could make) | `tools` and `tool_choice` |
| `tool_choice: "none"` with no tools | nothing | — |
| `tool_choice: "auto"`, or none sent | tool calling only if tools are declared (`auto` is what a node does with tools anyway) | `tools` |
| `reasoning_effort` set to an effort (`"low"`, `"high"`, …) | reasoning | `reasoning_content` |
| `reasoning_effort: "none"`, or none sent | nothing (turning thinking off is something any model can do) | — |
| any prompt | a context that can hold it | `state.model.context_length` |

`chat_template_kwargs` (such as `enable_thinking`) is a template's own switch.
The gateway forwards it without interpreting it, and so does the router: it is
not read as a reasoning request.

**Malformed requests** get the gateway's own `400`, from the router, before any
deployment is chosen. That covers `tool_choice: "required"` with no tools, a
named function that `tools` does not declare, an unknown `tool_choice`, a tool
with no name, empty `messages`, and on `/v1/completions` a token-array prompt or
an unsupported parameter. The code, `param` and message are the ones a node
would send, so a malformed request is never treated differently depending on
which node would have taken it. A body that does not even fit the request type
is forwarded as before, with only its endpoint required, and the node answers
it.

### Context

A Lightweight node refuses a request only when its prompt fills the window
(`prompt_tokens >= n_ctx`, answered `400 context_length_exceeded`). The output
budget (`max_tokens`, or `max_completion_tokens`, the smaller if both are set,
or neither) is **clamped** to what is left, never refused, because real clients
send budgets far larger than any window. Hermes sends 65536. The router keeps
exactly that rule. A deployment can serve a prompt of `p` tokens when
`p < context_length`. The budget is logged (`max_tokens`) and does not decide
eligibility. Filtering on prompt plus budget would refuse, at the router,
requests that every node would serve.

The node counts `p` with the model's own tokenizer and chat template. The
router has neither, and asking a node per request would put a network call in
the request path. So the router computes a **lower bound**: the bytes of every
message's text, divided by `BYTES_PER_TOKEN_CEILING` = 6. Tool declarations,
replayed tool calls and the template's markup are left out. Whether a template
renders them depends on the model, and a lower bound cannot count what may not
be there. It rules out
a deployment only when even that bound cannot fit. The bound is the same for
every deployment, because the router has no per-model tokenizer. Near the
boundary a request is let through, and the node, which stays the authority,
answers with its own `context_length_exceeded`. That error is the same one a
client gets talking to the node directly. The opposite mistake would be
worse: refusing a request that a deployment could have served, with an error
no node would ever have given.

Measured, not assumed. Through a real node running SmolLM2-135M, each sample
was sent as one user message with `max_tokens: 1`, and the node's own
`usage.prompt_tokens` was read back:

| Sample | Bytes | Prompt tokens | Bytes per token | Router's bound |
|---|---|---|---|---|
| English prose (README) | 6032 | 1561 | 3.86 | 1006 |
| Markdown (this file) | 6036 | 1828 | 3.30 | 1006 |
| Rust source | 6010 | 1629 | 3.69 | 1002 |
| JSON | 6000 | 3117 | 1.92 | 1000 |
| Indentation-heavy code | 6000 | 1595 | 3.76 | 1000 |
| One word repeated | 6000 | 1231 | 4.87 | 1000 |
| Base64 | 1500 | 1238 | 1.21 | 250 |
| Japanese | 1680 | 950 | 1.77 | 280 |
| Digits | 1500 | 1530 | 0.98 | 250 |
| 8 tool declarations, "hi" | 1778 | 31 | — | 0 (tools not counted) |

The bound was below the real count in every sample, and by a wide margin. The
last row is why tool declarations are not counted: SmolLM2's template drops
them, so they cost it nothing. Only one tokenizer was measured. A tokenizer
averaging more than 6 bytes a token over a whole prompt would make the bound
too high, and none measured came close.

### Unknowns are never a yes

- A deployment the router has never observed is never assumed capable. Every
  request requires at least its endpoint, so such a deployment is passed by
  (`unobserved`). In practice an available deployment always has an
  observation, because both come from the same probe.
- **Mixed versions.** Every flag the filter reads has been a required field of
  the v1 `/v1/capabilities` contract since it shipped (v0.4.0), so a node from
  any release states all of them, and nothing is inferred. A body that leaves a
  flag out is not the v1 contract. Its probe fails, and the node takes no
  traffic, rather than being read as supporting what it did not say.
- **What real nodes report today.** A Lightweight gateway advertises chat,
  completions, tools, `tool_choice` and reasoning as protocol features, all
  `true`. Between Lightweight nodes, the context window is therefore what
  actually separates deployments. The flags matter for any node that reports
  `false`, and they cost nothing when every node reports `true`.

### Policies after filtering

- **Priority** takes the first capable deployment in configured order. A
  deployment filtered out was never eligible, so the next one is a first
  choice (`primary_unavailable_fallback`), not a failover.
- **Round-robin** rotates over the capable deployments of each request. The
  ring is the request's own eligible set, and its length can change from one
  request to the next. The cursor still advances once per request, and the turn
  is `cursor % len` of that request's ring. Fairness holds over the eligible set
  of each request, not over a fixed physical ring.
- **Least-busy** compares `active / limit` among the capable deployments only.
  An idle deployment that cannot serve the request is never compared, so it
  cannot win.
- **Failover** walks only the plan, and the plan holds only capable
  deployments. A filtered deployment is never tried, even when every capable one
  has refused.

### When nothing can serve it

| Situation | Answer |
|---|---|
| No route has that name | `404 model_not_found` |
| The route exists and no deployment is available | `503 route_unavailable`, with `Retry-After` |
| Deployments are available, but none can serve this request | `400 route_capability_mismatch` |
| Only an *unavailable* deployment was last seen able to serve it | `503 route_unavailable`: waiting fixes this, changing the request does not |

```json
{"error":{"message":"Route \"Coder\" has no available deployment that supports tool calling.",
          "type":"invalid_request_error","param":"tools","code":"route_capability_mismatch"}}
```

The message names the route and the kinds of capability that were missing, all
of them, joined with "and". It never names a node, a node-local alias, a
canonical id or a file. `param` is set to `tools`, `tool_choice` or
`reasoning_effort` when one of those is at fault. The router never moves the
request to another route.

## Failover

**Failover happens only before anything has been sent to the client.** The next
deployment is tried when the current one:

- refuses the connection, fails DNS, or times out connecting. The failure is
  recorded against the node's health and the next eligible deployment is tried
  in the same request. The request never waits for the next probe. That is why
  the first request after a primary dies still succeeds, while the primary is
  still marked healthy;
- answers `502`, `503` or `504` (for example `server_busy` or no model loaded);
- answers `404 model_not_found`. That means the node swapped models since the
  last probe. The router forgets what that node was serving until the next
  probe, and the node's message is not shown to the client.

- answers `400 context_length_exceeded`, recognised by its `error.code` and
  nothing looser, **and** a later deployment in the plan advertises a strictly
  larger context. See [Context overflow](#context-overflow) below.

Every other answer commits the deployment. That includes every other `400`, and
every `500`.
**A `500` is never retried elsewhere.** It may be a deterministic failure of this
request or this model. Running the request again on another model could
duplicate work, or hide a real application error behind a different model's
answer. Lightweight's `500`s (such as `generation_failed`) say nothing to show
they are infrastructure-only, so the node's response is returned with its code
and message intact. `502`, `503` and `504` do say the node could not take the
request, which is why only they fail over. If every candidate refuses, the last node's own refusal is
returned. If every candidate fails to connect, the result is `route_unavailable`.

Once a response has started, the deployment stays committed. If the node fails
mid-stream, the client receives one
`data: {"error":{"code":"upstream_stream_interrupted",…}}` frame and no `[DONE]`.
Another node is never asked to continue an answer it did not start.

### Context overflow

The capability filter's context check is a lower bound, so a prompt can pass it
and still be longer, by the node's own count, than the deployment chosen. The
node then refuses it before generating anything:
`400 {"error":{"code":"context_length_exceeded","type":"invalid_request_error","param":"messages",…}}`.
A streamed request gets the same JSON refusal, because the node checks before
it starts the stream.

That one refusal is a capability miss the router could not see in advance, so
it is not the end of the request when the plan holds a deployment that can do
better:

- Only deployments already in the plan are tried. They passed health and the
  capability filter, so a deployment ruled out for tools, reasoning or its
  endpoint never comes back.
- Only a deployment advertising a **strictly larger** context than every one
  that has overflowed is tried. Others are passed over in plan order. With 8K,
  8K and 32K, an overflow on the first 8K goes straight to the 32K.
- The plan is not made again. Round-robin's cursor does not move a second
  time, and least-busy does not choose again. One request is one policy
  decision. The overflowed deployment's in-flight slot is returned before the
  next one is taken.
- The attempt is logged `context_overflow_failover`, with the deployment that
  overflowed and its context, the next one and its context, and the router's
  `estimated_prompt_tokens`. It is counted in
  `router_context_overflow_failovers_total{route}` and in
  `router_failovers_total`.
- When no larger deployment is left, the node's own `400
  context_length_exceeded` is returned unchanged. It is not turned into
  `route_unavailable` (the route is available) or `route_capability_mismatch`
  (the router could not have known before sending).
- Once anything has been relayed, nothing is retried. An overflow reported
  inside a stream that has started reaches the client in-band, as any other
  mid-stream error does.

`RoutingReason` in the logs is one of these:

| Policy | Reasons |
|---|---|
| `priority` | `explicit_single_deployment`, `primary_healthy`, `primary_unavailable_fallback`, `primary_failed_fallback` |
| `round_robin` | `round_robin`, `round_robin_failover` |
| `least_busy` | `least_busy`, `least_busy_tiebreak`, `least_busy_failover` |
| any | `context_overflow_failover` |

## The control API (read-only)

All of these use the client key. None of them shows a key: `auth` is reported
only as `"bearer"` or `"none"`.

| Endpoint | Shows |
|---|---|
| `GET /api/router/v1/nodes` | Each node's URL, `enabled`, health, consecutive failures, last check, last seen, last error, the model it is serving, and its version. |
| `GET /api/router/v1/routes` | Each route's `strategy`, `available`, and its deployments with their configured position (`priority`), availability and reason. Also the `default_route`. |
| `GET /api/router/v1/deployments` | Each deployment's node, model, the routes that use it, and its availability. Also its own last-observed `capabilities`, `context_length` and `max_concurrent_requests`, and what least-busy reads: `active_requests` (the router's in-flight count) and `concurrency_limit`. |
| `GET /api/router/v1/health` | Node and route health in one read, the probe settings, and active requests. |

## Observability

Logs use the target `hermes::router` (filter with `HERMES_LOG=hermes::router=debug`)
and go to stderr, never to the data directory's `gateway.log`. Each routed
request logs:

- `request_id`, `route`, `policy`, `node`, `deployment`, `reason`, `routing_ms`,
  `upstream_status` and `failover_count`;
- under round-robin, also `cursor` and `selected_index`;
- under least-busy, also `active_before` and `concurrency_limit`;
- what the request required: `endpoint` (`chat` or `completion`),
  `requires_tools`, `tool_choice`, `requires_reasoning`, `required_context` and
  `max_tokens`;
- `eligible_before` and `eligible_after` the capability filter, and `filtered`,
  how many deployments each requirement ruled out
  (`tools_unsupported=1,context_too_small=1`).

At `debug`, each ruled-out deployment is logged with all of its reasons. A
refused request logs `no available deployment can serve this request` with
the same fields and `unmet`.

Prompt text, credentials and file paths are never logged.

Metrics:

- `router_requests_total{route,outcome}`
- `router_failovers_total{route}`
- `router_routing_decisions_total{route,policy,reason}`. Failovers by policy are
  the `*_failover` reasons.
- `router_active_requests`
- `router_deployment_active_requests{deployment}`
- `router_capability_filtered_total{route,reason}`: available deployments a
  request's requirements ruled out. The reasons are `chat_unsupported`,
  `completion_unsupported`, `tools_unsupported`, `tool_choice_unsupported`,
  `reasoning_unsupported`, `context_too_small` and `unobserved`.
- `router_capability_mismatch_total{route}`: requests refused with
  `route_capability_mismatch`.
- `router_context_overflow_failovers_total{route}`: failovers to a larger
  context after `context_length_exceeded`.
- `router_node_health{node}`: `1` healthy, `0` unhealthy, `-1` unknown. A request for an unconfigured route is counted under
`route="_unknown"`, so clients cannot add labels by inventing model names.

## Limits of this version

- **The router's context check is a lower bound, not the node's count.** It
  rules out only what cannot fit. A prompt just over a deployment's window can
  still be sent there. The node answers `context_length_exceeded`, and the
  request then moves to a deployment in the plan with a larger context, if
  there is one ([Context overflow](#context-overflow)). That costs one
  round trip to the smaller node, where it counts the prompt. Exact
  per-model counting would need each deployment's tokenizer and chat
  template.
- **The output budget does not steer the choice.** A short prompt with
  `max_tokens: 20000` is eligible on an 8K deployment, where the node clamps
  the budget. It is never refused, but it can be answered with fewer tokens
  than a 32K deployment would allow. Preferring the larger deployment would be
  a ranking, and R5 only filters.
- **Capabilities are as fresh as the last probe.** After a hot swap, the router
  uses the previous figures until it probes again. If they let a request
  through, the node's own answer stands under the usual failover rules.

- **A resized node's limit reaches the router on the next probe, not at once.**
  Between a hot swap and that probe, least-busy divides by the previous limit.
  The router does not estimate it in the meantime; the node is the authority.
- **Two routes sharing a deployment can make one slightly uneven choice.** Each
  route holds its own least-busy lock (see [Least-busy](#least-busy)), so two
  simultaneous requests on different routes can both pick the same deployment.
  In-flight counts stay exact and the node queues the excess, so nothing runs
  incorrectly. A future load-balancing refinement could reserve across routes;
  a global router lock is deliberately not used.
- **A freshly started node briefly advertises its canonical id.** `hermes serve
  <file>` starts answering before it has adopted the catalog alias, because
  hashing the file takes seconds. For that moment, a deployment that names the
  alias reads as `model_not_served` and gets no traffic. The next probe after
  adoption makes it eligible. This was seen in the real smoke test.

- **Lightagent with several routes and `model = "default"`.** When `/v1/models`
  lists more than one model, Lightagent asks for an explicit model and does not
  send the literal `default`. Configure the route name, as with any gateway that
  lists several models.
- **Rewritten frames reorder JSON keys.** A frame whose `model` is rewritten is
  re-serialized, and its keys come out in sorted order. JSON gives key order no
  meaning, and the openai SDK and Lightagent both parse it unchanged.
- **The node control plane is not proxied, and not imitated.** See
  [Lightagent's runtime panel](#lightagents-runtime-panel).
- **Nodes do not log the request id.** The router sends `X-Request-Id` and logs
  it, but gateways do not log it yet (R6).
- **No latency or TTFT histograms yet.** The log lines already carry the timing
  data those metrics would be built from.

## Lightagent's runtime panel

Lightagent `7d95232` reads a gateway's `/api/v1` control plane in exactly one
live place: the provider panel (`crates/lightagent/src/serve.rs`). It treats both
endpoints as optional.

There is also a `crates/lightagent/src/runtime.rs`, which reads more and can
place models. It is not declared as a module and has no subcommand in that build,
so it never runs. It is listed below because it is the consumer a compatibility
layer would have to survive once it is wired up.

| Endpoint | Consumer | Fields read | Meaningful for a router? |
|---|---|---|---|
| `GET /api/v1/gateway` | Provider panel (live) | `engine_capabilities.reasoning_content`, which sets the "Reasoning ready" badge | Only per route, and the panel asks once per provider |
| `GET /api/v1/models` | Provider panel (live) | Per row: `id`, `name`, `state`, `supported`. They fill a disabled "Backend runtime catalog" group of models that need loading. | No. Routes are already the selectable models via `/v1/models`, and loading is placement. |
| `GET /api/v1/gateway` | `runtime.rs` (not wired) | `engine_capabilities.device` (required), `build`, `kv_cache_types`, `max_concurrent_requests`, `streaming`, `tool_calls`; `defaults.*`; `model`, `backend`, `version` | No. They describe one physical engine and its load defaults. |
| `GET /api/v1/system` | `runtime.rs` (not wired) | `os`, `cpu`, `memory` | No. They describe one machine. |
| `POST /api/v1/models/{id}/load`, `POST /api/v1/models/unload` | `runtime.rs` (not wired) | — | No. This is placement, which is out of scope until R7. |

**Decision: the router serves none of these, neither proxied nor imitated.** The
reasons:

- Proxying one node's `/api/v1` would present that node as the router.
- A router-owned `/api/v1/gateway` would have to make up the required
  `engine_capabilities.device`, which only exists for a single engine.
- Worse, a router-owned `/api/v1/gateway` would let the placement code in
  `runtime.rs`, once it is wired up, get past its gateway read and send a model
  load to the router. Without that endpoint, it stops at the gateway read, which
  is the safe failure.

What the panel loses is small. The "Reasoning ready" badge reads "Standard
reasoning", and the disabled runtime-catalog group is empty. Model selection,
chat and streaming are unaffected, because they use `/v1`.

The logical state the panel could use is already router-owned: route availability
and features in `/v1/capabilities`, and node health and deployments in
`/api/router/v1/*`. Showing it in Lightagent needs Lightagent to read those
endpoints, which is a change to Lightagent and left for later.

## Tests and cleanup

The router's integration tests run every node and router as a tokio task inside
the test process:

- real gateways over the mock engine, with `paths: None`, so nothing is written
  to disk;
- scripted nodes;
- the router itself.

They spawn no processes and create no files or directories. When a test passes,
fails an assertion, panics or times out, its runtime is dropped and every
listener closes with it. Interrupting `cargo test` (Ctrl-C or `kill -9`) ends the
one process that holds them all. This was checked by killing the test binary
mid-run with both signals: no process or listener survived, and nothing new
appeared in the temp or data directories.

The disk growth seen while building this was Cargo's own output in `target/`,
mostly `target/debug/incremental`, which Cargo rebuilds on demand.

## Roadmap

R5 (capability filtering) is built. Nothing after it is. Each later step
builds on the types above without changing the public route identity.

| Milestone | Scope |
|---|---|
| **R4** | Done: `round_robin` and `least_busy`. Deliberately left out: weighted, random, latency/EWMA/P95/TTFT, and cost-aware selection. Any of these would be a new `RoutePolicy` variant with its own ordering function. |
| **R5** | Done: request-aware capability filtering between eligibility and policy, for endpoint, tools, `tool_choice`, reasoning and context. Deliberately left out: ranking by capability, routing on the output budget, per-model tokenization, and moving a request to another route. |
| **R6** | Session affinity keyed by a client-supplied conversation id. Latency and TTFT histograms. Request ids in node logs. |
| **R7** | Placement control: a control plane that asks nodes to load models and keeps warm standbys. It never runs in the request path. |
| **R8** | A rule-based `Auto` route that maps request traits to routes. |
| **R9** | A learned or adaptive router, and mixture-of-agents integration. |

Out of scope for every one of these: a request-path model load, splicing one
node's output into another's stream, and a dependency on distributed consensus.
