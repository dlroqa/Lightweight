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

This document covers milestones R0 to R8, and R9.1:

- **R0–R3:** the domain model, a transparent proxy, a multi-node registry with
  health checks, and priority routing with failover before the response starts.
- **R4:** two load-balancing policies, round-robin and least-busy.
- **R5:** request-aware capability filtering, and failover to a larger context
  after `context_length_exceeded`.
- **R6:** optional session affinity, and observability: time to first token,
  latency histograms, one request id from client to node log, per-request
  routing traces, and the router's prompt estimate compared with the node's
  own count. **Everything R6 measures is measured only: no routing decision
  reads latency, TTFT, estimator error or history.**
- **R7:** a placement controller that keeps each opted-in route at a target
  number of ready deployments by loading installed models onto empty nodes
  ahead of demand — outside the request path. See [Placement](#placement).
- **R8:** an optional, rule-based `Auto` model. A client that sends
  `model: "Auto"` asks the router to choose the logical route by the
  request's structure; the route then chooses the deployment exactly as
  before. See [Auto routing](#auto-routing). **R8 is not learned routing:**
  no classifier, embedding, score, history, latency or cost is involved.
- **R9.1 / R9.1a:** optional content-aware classification. An `Auto` rule may
  say `"classify": true`, and a classifier — a configured route on the
  operator's own gateways, or TypeSafe AI's Jev System One API — then
  recommends one of the operator's candidate routes by what the request asks
  for. It chooses a route only, never a deployment, and every failure falls
  back deterministically. See
  [Content-aware classification](#content-aware-classification-r91).
- **R9.2:** optional adaptive scoring of a classification. Off by default.
  When on, an accepted classification is weighed against the classifier's
  fallback route by the classifier's confidence and an operator prior per
  route. Route history is collected and shown, never scored, in this slice.
  It ranks **logical routes only, never deployments**, never overturns a
  confident classification, and never revives one the classifier was unsure
  of. See
  [Adaptive route scoring](#adaptive-route-scoring-r92).
- **R9.3.1:** optional explicit cross-route fallback for `Auto` requests.
  If the route `Auto` chose cannot execute (`route_unavailable`,
  `route_exhausted`, `route_capability_mismatch`), the next route the
  operator listed for it is tried. This happens only before commit and only
  after same-route failover; the list is never transitive and never longer
  than three entries. See [Cross-route fallback](#cross-route-fallback-r931).

The [roadmap](#roadmap) lists what comes after.

## Who owns what

| Layer | Decides | Owns |
|---|---|---|
| Client (Lightagent) | which capability it wants | a route name such as `Coder`, or `Auto` |
| Auto rules (in the router, R8) | which logical route, when the client sent `Auto` | ordered rules over the request's structure — never a deployment or a node |
| Classifier provider (R9.1 / R9.1a) | which candidate route, when a rule asks | a recommendation of one configured route name — never a deployment or a node. A configured route (`lightweight`) or Jev (`jev`). |
| **Router** | where the request goes | routes, the deployment registry, node health, routing policy, forwarding, stream relaying, failover, router logs and metrics |
| Node (`hermes serve`) | how the model runs | its aliases, canonical ids, GGUF files, RAM admission, the scheduler and the engine |
| Placement controller (in the router process, R7) | where a route is prepared | load requests to empty nodes, readiness confirmation, backoff — never a request's path |

The router speaks only to a node's public `/v1` surface, the same surface any
client uses. It never parses GGUF, estimates memory, loads or unloads a model,
or reads a node's alias-to-canonical mapping. If a node is not serving the model
a deployment names, that deployment is unavailable. **A request** never causes
a load. Since R7, a separate [placement controller](#placement) may ask an
empty node to load a route's model ahead of demand, through the node's own
control API, when the operator configured a placement target for that route.

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
  "session_affinity": { "enabled": true, "header": "X-Lightweight-Session", "idle_ttl_secs": 1800, "max_entries": 10000 },
  "traces": { "capacity": 200 },
  "placement": { "interval_secs": 15, "load_timeout_secs": 600, "backoff_secs": 30, "backoff_max_secs": 600 },
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
| `session_affinity.enabled` | `false` | Keep a client-named session on the deployment its last request succeeded on. Off unless set: without it, routing is exactly R5's. See [Session affinity](#session-affinity). |
| `session_affinity.header` | `X-Lightweight-Session` | The request header a session id is read from. Compared ignoring case. `Authorization`, `Cookie`, `X-Request-Id` and other headers that already mean something are refused. |
| `session_affinity.idle_ttl_secs` | 1800 | How long a session may sit unused before its affinity is forgotten. 1 to 86 400. |
| `session_affinity.max_entries` | 10 000 | The most sessions held at once (at most 1 000 000). When full, expired entries go first, then the least recently used. |
| `routes[].placement` | none | `{"min_ready": 1, "warm_standby": 1, "allowed_nodes": ["dell-7820", "t420"]}`: keep this many of the route's deployments loaded. Without it, the controller never acts for the route. See [Placement](#placement). |
| `placement.interval_secs` | 15 | How often the controller compares routes with their targets. It runs only if some route has a target. |
| `placement.load_timeout_secs` | 600 | How long one load may take, from the request to observed readiness. |
| `placement.backoff_secs` / `backoff_max_secs` | 30 / 600 | The wait after a failed load, doubled per consecutive failure, never above the maximum. |
| `traces.capacity` | 200 | How many recent routing traces `GET /api/router/v1/traces` keeps in memory. `0` keeps none; at most 10 000. |
| `auto_route` | none | `{"enabled": true, "fallback_route": "General", "rules": [...]}`: serve `Auto`. Without the section, `Auto` is an unknown model, exactly as before R8. See [Auto routing](#auto-routing). |
| `auto_route.enabled` | `false` | Off unless set. A section that is present but off is still checked. |
| `auto_route.fallback_route` | required | The route an `Auto` request goes to when no rule matches. |
| `auto_route.rules[]` | `[]` | `{"name": "tools", "when": {...}, "route": "Coder"}`, tried in order. At most 64. A rule may say `"classify": true` instead of naming a `route` (R9.1). |
| `auto_route.classifier` | none | `{"route": "RouterClassifier", "routes": ["General", "Coder", "Research"]}`: the classifier a classifying rule calls, and the candidates it may recommend. See [Content-aware classification](#content-aware-classification-r91). |
| `routes[].description` | none | What the route is for, at most 200 characters. Told only to the classifier. |

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
- out-of-range health or request timings;
- a placement with `min_ready` 0, no `allowed_nodes`, an allowed node with no
  deployment in that route, a node listed twice, or a target larger than the
  route's deployment count (it could never be met); placement timings below 1
  second, or a backoff maximum below the initial backoff;
- a session header that is not a valid header name or already means something
  else, an affinity TTL or entry limit out of range, or a trace capacity over
  the limit. These are checked even while affinity is off, so turning it on is
  never the moment a typo surfaces;
- with an `auto_route` section, on or off: a route named `Auto`; a
  `fallback_route` or rule `route` that is not a configured route, is
  `default`, or is `Auto` itself; a rule name that is empty, longer than 64
  characters, used twice (ignoring case), does not start with a letter or
  digit, or has characters outside `[A-Za-z0-9._-]`; a rule with no
  conditions; a prompt threshold of 0; and a rule no request could ever match
  (see [Rule validation](#rule-validation)). A top-level `default_route` of
  `Auto` is refused too: `Auto` is not a route.

A configuration being valid and a node being up are separate questions. The
router starts even when every node is offline, and reports those nodes as
unhealthy.

## The client surface

| Endpoint | Behaviour |
|---|---|
| `GET /v1/models` | Lists every configured route, including routes with nothing available right now, and `Auto` when it is on — with no context fields, since its context is the resolved route's. Each row has `owned_by: "lightweight-router"`. When a route's context is known it is given under the gateway's names (`context_length`, `n_ctx`, `max_tokens`, `max_output_tokens`), using the **smallest** context among the deployments the route could send a request to right now (see below). No node, address, node-local name or file appears. |
| `GET /v1/capabilities` | The gateway's contract, with the same protocol name and version (`lightweight-public-inference`, v1) and the same top-level fields, plus a `routes` array, and an `auto` object when `Auto` is on (see [Auto routing](#auto-routing)). |
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
| `Auto`, in any casing, with `auto_route` on | The route its rules choose; then exactly as if that route had been named. The response's `model` is that route. |
| `Auto`, with no `auto_route` or with it off | `404 model_not_found`, like any unknown name. |

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
The client's `Authorization`, its cookies, its session header and any other
header are not forwarded. Downstream, only `Content-Type`, `Cache-Control` and `Retry-After`
are copied from the node's response.

**Request ids:** a well-formed `X-Request-Id` from the client (up to 128 visible
ASCII characters, no spaces) is kept exactly. If the client sends none, the
router generates one (`rtr-…`). The id is sent to the node, echoed in the
response, and included in every router log line. **Every attempt of one
request carries the same id**: a failover is one logical request, so the node
that refused and the node that answered log the same id.

Since R6 the node logs it too. A gateway reads `X-Request-Id`, writes it as
`request_id` on every line about the request — `generating`, `queued behind
another request`, `admitted after waiting`, `request refused` (status and
code), `generation failed after the response had started`, and one closing
`request finished` line (`outcome` `completed`, `failed` or `cancelled`, with
`ttft_ms`, `total_ms`, `queue_wait_ms`, `prompt_tokens`, `completion_tokens`)
— and echoes it on its response. A node never invents an id: a request without
one logs as before, under the node's own completion id (`chatcmpl-…`), which is
also on every line. Both sides share one rule for a usable id
(`lightweight_gateway::request_id`), so neither rewrites what the other
accepts. `grep <id>` then finds the request in the client's, the router's
(stderr) and the node's (`gateway.log`) logs.

## Auto routing

`Auto` is a model a client can select like any route, but it has no
deployments. A request that sends `model: "Auto"` asks the router to choose
**which logical route** handles it. The chosen route then chooses **which of
its deployments** answers, exactly as if the client had named the route. The
two decisions never mix:

```text
Request ─▶ model == "Auto"? ── no ──▶ resolve by name ─┐
               │ yes                                    │
               ▼                                        ▼
       request requirements (R5)               logical route
               ▼                                        │
       first matching rule, or fallback ───────────────▶│
                                                        ▼
            health ─▶ capability filter ─▶ session affinity
                 ─▶ priority | round_robin | least_busy ─▶ deployment
```

`crates/lightweight-router/src/auto_route.rs` owns the first decision and
nothing else; `select.rs`, `capability.rs`, `affinity.rs` and `placement.rs`
are unchanged.

### Configuration

```json
"auto_route": {
  "enabled": true,
  "fallback_route": "General",
  "rules": [
    { "name": "agentic",   "when": { "requires_tools": true, "requires_reasoning": true }, "route": "AgenticReasoner" },
    { "name": "tools",     "when": { "requires_tools": true },          "route": "Coder" },
    { "name": "reasoning", "when": { "requires_reasoning": true },      "route": "Reasoning" },
    { "name": "long",      "when": { "min_prompt_tokens": 12000 },      "route": "LongContext" },
    { "name": "text",      "when": { "endpoint": "completion" },        "route": "Completion" }
  ]
}
```

The fallback is `fallback_route` rather than `default_route` because the
top-level `default_route` already means what `"model": "default"` resolves to.
The two are independent.

### Rules and their order

Rules are tried **in configured order, and the first that matches wins**. If
none matches, the request goes to `fallback_route`. Nothing is scored or
weighed: the same request against the same file always reaches the same
route. A request with tools and reasoning in the example above reaches
`AgenticReasoner` because that rule comes first; move `tools` above it and the
same request reaches `Coder`. That is intentional. Express OR by giving two
rules the same `route`.

Every condition in one `when` must hold (AND). A condition that is not written
is not looked at. A boolean condition set to `false` is a condition, not an
absence: `"requires_tools": false` matches only a request without tools.

| Condition | Matches when | Read from |
|---|---|---|
| `endpoint` | `"chat"` (`/v1/chat/completions`) or `"completion"` (`/v1/completions`) | the endpoint the request was sent to |
| `requires_tools` | the request declares at least one tool (`true`), or none (`false`). `"tools": []` declares none. | `tools` |
| `tool_choice` | the request's `tool_choice` is exactly `unspecified` (not sent), `auto`, `none`, `required` or `function` (a named function) | `tool_choice` |
| `requires_reasoning` | the request sends a `reasoning_effort` other than `none` (`true`), or does not (`false`). A template's own `chat_template_kwargs` switch is not read. | `reasoning_effort` |
| `min_prompt_tokens`, `max_prompt_tokens` | the router's prompt estimate is at least / at most this many tokens (both inclusive) | the message text |

These are exactly the facts [capability filtering](#what-a-request-requires)
reads, from the same extractor (`requirements::extract`), run **once** per
request: its result decides the route and is then reused to filter that
route's deployments. The prompt estimate is the router's **lower bound**
(message bytes ÷ 6, see [Context](#context)), not the model's own
tokenization; a threshold compares against that bound. A body the router could
not read well enough to count matches no threshold.

**No rule condition reads a prompt for meaning.** There is no "looks like
code", "asks for maths" or "is research" condition, and no rule inspects
message text for words such as "tool" or "function". Every condition is a
field the client set or the endpoint it used. Meaning is read only by the
[classifier](#content-aware-classification-r91), and only when a rule the
operator wrote asks for it.

A request the gateway would refuse — `tool_choice: "required"` with no tools,
say — is refused with the gateway's own `400` before any route is chosen,
exactly as for a named route.

### Rule validation

At startup, with the section on or off, the router refuses:

- a `fallback_route` or rule `route` that is not a configured route, is
  `default`, or is `Auto` itself. `Auto` can therefore never resolve to `Auto`,
  directly or through another rule; routes have no aliases or indirection, so
  no longer cycle can exist;
- a configured route called `Auto` (ignoring case) — it could never be reached
  once `Auto` is on. **Without** an `auto_route` section, a route called `Auto`
  is an ordinary route, as before R8;
- duplicate rule names (ignoring case), and names that are empty, over 64
  characters, not starting with a letter or digit, or outside
  `[A-Za-z0-9._-]`. A rule name is a metric label, so it is held to the node
  id's alphabet; `_fallback` is reserved for the fallback;
- a rule with no conditions (it would match every request: that is what
  `fallback_route` is for) — unless it classifies, which is how classification
  becomes the catch-all after the deterministic rules — a prompt threshold of `0`, and a rule no request can
  meet: `min_prompt_tokens` above `max_prompt_tokens`; `endpoint: "completion"`
  with `requires_tools: true`, `requires_reasoning: true` or any `tool_choice`
  but `unspecified` (a text completion carries none of them); and
  `requires_tools: false` with `tool_choice` `required` or `function`, which the
  gateway refuses;
- more than 64 rules.

A rule that an earlier rule always pre-empts is not detected; order is the
operator's to choose.

### What `Auto` does not do

- **It never chooses a deployment.** Capability filtering still runs inside
  the chosen route: a `tools → Coder` rule does not prove every `Coder`
  deployment takes tools, and one that does not is passed over as for any
  request. If none can, the answer is `Coder`'s `route_capability_mismatch`.
- **It never tries another route by itself.** If the chosen route has
  nothing available, the client gets that route's `503 route_unavailable`,
  not the fallback and not the next rule, unless the operator listed
  fallback routes for it ([cross-route fallback](#cross-route-fallback-r931),
  R9.3.1). Context-overflow failover always stays inside the chosen route.
- **It never waits or loads.** A route that placement is still loading answers
  `route_unavailable` at once. `Auto` has no placement of its own and never
  asks for a load; the chosen route's placement target is the one that applies.
- **It keeps no state.** No affinity, cursor, load count or health belongs to
  `Auto`. Every one belongs to the resolved route.

### Interaction with the rest

- **Session affinity** is keyed by the **resolved** route and the session. A
  session whose ordinary chat resolves to `General` and whose tool request
  resolves to `Coder` has two independent affinities, one per route — the same
  ones a direct request for `General` or `Coder` with that session uses. There
  is never an `Auto` bucket, and `Auto` decides the route per request, so a
  session is never held to the route of its previous turn.
- **Priority, round-robin and least-busy** order only the chosen route's
  deployments. Round-robin advances only on that route's requests (there is no
  ring across routes) and least-busy compares only that route's deployments.
- **Placement** works per route, as before. `Auto → Coder` is served by
  `Coder`'s ready deployments and `Coder`'s target.

### Response identity

The client asked for `Auto`; **the response names the route that answered**:
`"model": "Coder"` in a whole body, in every streamed chunk including the usage
chunk, and in a tool-call answer. The choice is useful to the client and true;
hiding it behind `Auto` would make an answer from `Coder` and one from
`General` look alike. Node-local aliases and canonical ids are never shown,
exactly as for a named route. A refusal from the chosen route names that route
(`No healthy deployment is available for route "Coder".`).

### Discovery

- `GET /v1/models` lists `Auto` while it is on, as `object: "model"`,
  `owned_by: "lightweight-router"`, with **no** context fields: its context is
  whichever route a request resolves to.
- `GET /v1/capabilities` does **not** list `Auto` under `routes`, and it does
  not change the top-level `features` (which already cover every available
  route). It adds:

  ```json
  "auto": { "id": "Auto", "router_resolved": true, "routes": ["AgenticReasoner", "Coder", "Reasoning", "LongContext", "Completion", "General"] }
  ```

  — the routes it can resolve to, rules first in order, then the fallback. No
  rule name and no deployment is shown to a client.

### Observability

Every `Auto` request logs one line before it is planned:

```text
auto route resolved request_id="rtr-…" requested_route="Auto" auto_rule="tools" resolved_route=Coder
  endpoint="chat" requires_tools=true tool_choice="unspecified" requires_reasoning=false estimated_prompt_tokens=840
```

`auto_rule` is `_fallback` when no rule matched. No prompt content is logged.
The `routed` and `request finished` lines carry `requested_route` and
`auto_rule` beside the route and deployment they already had, and the
[routing trace](#routing-traces) gains `requested_route`, `auto_rule` and
`auto_fallback`. The metrics are
`router_auto_route_decisions_total{rule,route}` (rule a configured name or
`_fallback`) and `router_auto_route_fallback_total{route}`, counted when the
route is chosen, whatever the route then answers. A request refused before a
route was chosen is counted under `route="Auto"` in `router_requests_total`.
`GET /api/router/v1/auto` shows the rules (see [The control API](#the-control-api)).

Choosing is a walk over at most 64 in-memory rules, comparing fields already
read: no network call, no model, and nothing asynchronous. Ten thousand
decisions that try every rule take well under the half second the unit test
allows on a debug build.

## Content-aware classification (R9.1)

R8's rules see a request's structure. Some clients make structure useless for
choosing a route: Lightagent `7d95232` declares its whole tool set on every
turn, so `requires_tools: true` matches a greeting as surely as a coding task.
R9.1 lets an `Auto` rule ask a **classifier** which route a request is for.
It still chooses **only the logical route**; the route's own pipeline then
chooses the deployment, as for every request.

```text
Auto ─▶ R8 rules, in order ─┬─ a rule names a route ──────────────────────────┐
                            ├─ a rule says "classify" ─▶ classifier route ──┐ │
                            └─ no rule matches ─▶ fallback_route ───────────┼─┤
                                    chosen candidate │ or classifier fallback ┘ ▼
                                                     └──────────────▶ logical route ─▶ health ─▶ capability
                                                                         filter ─▶ affinity ─▶ policy ─▶ deployment
```

### Configuration

```json
"routes": [
  { "name": "General",  "description": "Everyday conversation and small talk", "deployments": [ ... ] },
  { "name": "Coder",    "description": "Software engineering, debugging, code generation", "deployments": [ ... ] },
  { "name": "Research", "description": "Current events, web research, comparisons", "deployments": [ ... ] },
  { "name": "ToolAgent", "deployments": [ ... ] },
  { "name": "RouterClassifier", "deployments": [ { "node": "t420", "model": "QwenClassifier" } ] }
],
"auto_route": {
  "enabled": true,
  "fallback_route": "General",
  "classifier": {
    "provider": "lightweight",
    "routes": ["General", "Coder", "Research"],
    "fallback_route": "General",
    "lightweight": {
      "route": "RouterClassifier",
      "timeout_ms": 30000,
      "min_confidence": 0.65,
      "max_input_chars": 2000
    }
  },
  "rules": [
    { "name": "forced-tools",  "when": { "tool_choice": "required" },   "route": "ToolAgent" },
    { "name": "large-context", "when": { "min_prompt_tokens": 12000 },  "route": "Research" },
    { "name": "semantic",      "when": {}, "classify": true }
  ]
}
```

| Key | Default | Meaning |
|---|---|---|
| `classifier.provider` | `lightweight` | Who classifies: `lightweight` (a configured route) or `jev` (TypeSafe's System One API, see [Jev](#the-jev-provider)). Switching changes this one word and no rule. |
| `classifier.routes` | required | The candidates it may recommend, 1 to 16 configured routes, never `Auto`. Nothing else can be its answer. Shared by both providers. |
| `classifier.fallback_route` | `auto_route.fallback_route` | Where a classification that fails or is unsure goes. Shared. |
| `classifier.lightweight.route` | required for `lightweight` | The configured route that classifies. Any route — typically a small instruct model on one node. Never `Auto`. |
| `classifier.lightweight.timeout_ms` | **required** | How long a classification may take, 1 to 120 000. No default: see [Classification latency](#classification-latency). Never unlimited. |
| `classifier.lightweight.min_confidence` | 0.65 | A recommendation below this is not taken. 0 to 1. |
| `classifier.lightweight.max_input_chars` | 2000 | How much of the request's text is sent, 1 to 32 000. |
| `classifier.jev` | none | The Jev provider's own settings; see [Jev](#the-jev-provider). |
| `rules[].classify` | `false` | The rule asks the classifier instead of naming a `route` (exactly one of the two). A classifying rule may have an empty `when`. |
| `routes[].description` | none | What the classifier is told the route is for. No deterministic decision reads it. |

Each provider keeps its own timeout, threshold and input limit, because their
operational facts differ (a CPU model in tens of seconds, a remote API in
hundreds of milliseconds). Both blocks may be present; only the one `provider`
names is used, and the other is shown in the admin view as standby. The R9.1
shape — `route`, `timeout_ms`, `min_confidence` and `max_input_chars` written
directly in the classifier section — is still read, as the `lightweight` block;
writing both forms is refused.

**Classification is opt-in twice over.** It needs the classifier section *and*
a rule that says `"classify": true`. A classifier section with no such rule is
inert, and a configuration without either is exactly R8. A request that names a
route, an `Auto` request a deterministic rule resolves, and an `Auto` request no
rule matches are never classified. Rules are still tried in order, so
deterministic rules placed before the classifying rule always win.

### What the Lightweight classifier is, and what it sees

The classifier is **a configured logical route**, called through the router's
own pipeline as a nested request: its health, capability filtering, policy and
failover decide which of its deployments answers. It never names a node, and a
client can call it by name like any route (it is listed in `/v1/models`). The
classification request carries `<request id>-classify`, its node's own key, and
no session.

It is sent one chat request — `temperature: 0`, `reasoning_effort: "none"`,
48 tokens, not streamed — holding:

- a system message listing the candidates, each with its description, and
  asking for exactly `{"route": "<name>", "confidence": <0..1>}`. It is told
  that declared tools alone do not mean a request needs them;
- a user message with the request's structural traits (endpoint, whether
  tools are declared, whether reasoning was asked for, the prompt estimate)
  and the **last user message** — a completion's first prompt — cut to
  `max_input_chars` characters (on a character boundary, marked
  `(truncated)`). No earlier turn, system prompt, tool schema, alias or node
  name is sent.

### The Jev provider

[Jev](https://docs.typesafe.ai) is TypeSafe AI's System One model: it answers
typed questions about a state with calibrated probabilities. With
`"provider": "jev"` the router asks it one **Choice** question — which
candidate route should answer — instead of asking a configured route.

**Jev is not a deployment.** It is never in a route, never health-probed, never
behind priority, round-robin or least-busy, and never reachable by a client
through the router. It recommends a logical route; the route's own pipeline
then chooses the deployment, exactly as for the Lightweight provider.

```json
"classifier": {
  "provider": "jev",
  "routes": ["General", "Coder", "Research", "Reasoning"],
  "fallback_route": "General",
  "jev": {
    "base_url": "https://api.typesafe.ai",
    "api_key_env": "TYPESAFE_API_KEY",
    "model": "jev-latest",
    "timeout_ms": 5000,
    "min_confidence": 0.65,
    "max_input_chars": 2000,
    "include_user_text": true
  }
}
```

| Key | Default | Meaning |
|---|---|---|
| `jev.base_url` | `https://api.typesafe.ai` | Trailing slashes are removed, so `…/v1/systemone` is never `…//v1/systemone`; a path prefix (a proxy) is kept. Must be `https`, except a loopback address. No credentials, query or fragment. |
| `jev.api_key_env` | `TYPESAFE_API_KEY` | The environment variable holding the API key — TypeSafe's own convention. The key is never written in the file; a literal `api_key` is refused as unknown. Read at start, like a node's key. |
| `jev.model` | **required** | An alias such as `jev-latest` (moves with TypeSafe's releases) or a pinned versioned id such as `jev-1.13.0` (TypeSafe recommends pinning once thresholds are tuned). |
| `jev.timeout_ms` | **required** | 1 to 120 000. It is a network call; the right bound depends on the network. 5000 is a reasonable start, not a guarantee. |
| `jev.min_confidence` | 0.65 | Applied to TypeSafe's own `confidence` for the answer. |
| `jev.max_input_chars` | 2000 | 1 to 32 000. |
| `jev.include_user_text` | `true` | `false` sends only the request's structural traits and the route descriptions — more private, and less able to tell a greeting from a coding question. |

**What is sent, and where.** When `provider` is `jev`, each classified request
sends to the configured TypeSafe endpoint: the candidate route names and their
descriptions, the request's structural traits (endpoint, whether tools are
declared, `tool_choice`, whether reasoning was asked for, the router's prompt
estimate) and — unless `include_user_text` is `false` — **the last user message,
cut to `max_input_chars`**. Never earlier turns, a system prompt, tool schemas,
an alias, a node address, a request id or a node credential. That text leaves
the operator's machines; TypeSafe documents its data handling at
[docs.typesafe.ai/legal](https://docs.typesafe.ai/legal). If it must not, use
the Lightweight provider.

**The request**, per TypeSafe's API reference:

```json
POST {base_url}/v1/systemone
Authorization: Bearer <key>

{"model": "jev-latest",
 "state": {"request": "write a Rust async TCP server", "request_truncated": false,
           "endpoint": "chat", "tools_declared": true, "tool_choice": "unspecified",
           "reasoning_requested": false, "estimated_prompt_tokens": 8},
 "questions": {"route": {"type": "choice",
   "instructions": "Which route should answer this request? …",
   "criteria": {"General": "General conversation …", "Coder": "Programming, …",
                "Research": "…", "Reasoning": null}}}}
```

The `criteria` are exactly the candidates, each with its `description` (or
`null`). The answer is read from `answers.route`: it must be a `choice` naming
**one of the candidates exactly**, with a `confidence` in `[0, 1]` — TypeSafe's
own certainty derived from the probability distribution, the one figure the
router thresholds. A choice that is not a candidate (another route, `Auto`, a
node name, a different casing) is `invalid`. The response's `model` — the
versioned id behind an alias — is recorded in the trace.

**Failures**, all answered by the classifier's fallback route, never by an
error to the client and never by a retry (the timeout bounds the wait):

| Jev | Outcome |
|---|---|
| `401`, `403` | `auth_error` (the live API answers a keyless request `403`) |
| `429`, `529` | `rate_limited` |
| any other error status (`422`, `5xx`) | `provider_error` |
| refused or failed connection | `connection_error` |
| no answer within `timeout_ms` | `timeout` (the HTTPS request is dropped) |
| not the documented shape, not a candidate, confidence out of range | `invalid` |
| below `min_confidence` | `low_confidence` |

A provider's error body is never read into a log, a trace, a metric or the
admin view.

**Checking the setup.** At start the router checks Jev once, in the background
(a provider that is down never stops the router): `GET /v1/models` with the
key — is it accepted, is the model listed. `POST /api/router/v1/classifier/check`
runs the same check on demand and returns, sanitized:

```json
{"provider": "jev", "status": "ok", "model": "jev-latest", "model_listed": true,
 "http_status": 200, "checked_at": 1791300000, "duration_ms": 180.2}
```

`status` is `ok`, `model_not_listed`, `api_key_missing`, `auth_error`,
`rate_limited`, `provider_error`, `invalid_response`, `connection_error` or
`timeout`. `GET /v1/models` lists TypeSafe's **aliases**; a pinned versioned
id is accepted by `/v1/systemone` without being listed, so `model_not_listed`
is expected for one and a misconfiguration for an alias. The check never
classifies and never sends request content.

**Setting it up:**

1. Obtain a TypeSafe API key.
2. Set it in the router's environment: `export TYPESAFE_API_KEY=…` (or the
   variable `jev.api_key_env` names). Never in `router.json`.
3. In the classifier section, set `"provider": "jev"` and a `jev` block with
   at least `model` and `timeout_ms`; adjust `base_url`, `min_confidence`,
   `max_input_chars` and `include_user_text` if needed. Give the candidate
   routes `description`s — they are what Jev chooses between.
4. `hermes router validate-config` — a missing key is refused here, by name.
5. Restart the router (configuration is read at start).
6. `GET /api/router/v1/auto`: `classifier.provider` is `jev`,
   `classifier.jev.api_key_configured` is `true`, and `status.last_check`
   shows the start-up check.
7. `POST /api/router/v1/classifier/check` to re-check at any time.
8. Send an `Auto` request that a classifying rule matches, and read its
   trace: `classifier.provider`, `model`, `outcome`, `chosen_route`,
   `confidence`, `duration_ms`.

There is no settings screen for the router; it is configured by file, and
this is its read-only operator view.

### Answers, confidence and fallback

The answer must be one JSON object (text around it, such as a code fence, is
tolerated) naming one of the candidates, matched ignoring case, with a
`confidence` between 0 and 1. The request then resolves:

| Outcome | When | Route |
|---|---|---|
| `chosen` | a candidate, at or above `min_confidence` | that candidate (with [adaptive scoring](#adaptive-route-scoring-r92) on, a borderline one may lose to the fallback) |
| `low_confidence` | a candidate, below `min_confidence` | the classifier's fallback |
| `invalid` | not JSON, no `route`, a route that is not a candidate (a node name, `Auto`, a configured route outside `routes`), or a missing or out-of-range confidence | the classifier's fallback |
| `unavailable` | the classifier route refused (`route_unavailable`, a node's `503`, …) | the classifier's fallback |
| `timeout` | no answer within `timeout_ms`. The nested request is dropped, which closes its upstream connection. | the classifier's fallback |
| `auth_error`, `rate_limited`, `connection_error`, `provider_error` | Jev only; see [The Jev provider](#the-jev-provider) | the classifier's fallback |
| `nested` | a classification request reached a classifying rule — impossible by configuration, and refused at run time too | the rule's fallback |

**`Auto` never fails because the classifier did.** A route can never be
invented: the answer is checked against the candidates, and only a configured
route can be a candidate.

**Recursion is impossible.** The classifier route and every candidate are
refused at startup if they are `Auto`, and the classification is sent to the
classifier route by name, so it never reaches `Auto`. A nested request that did
would take its rule's fallback without classifying again.

### After the route is chosen

Nothing changes. The chosen route answers exactly as if the client had named
it: [capability filtering](#capability-filtering) inside it, its
[session affinity](#session-affinity) (keyed by the resolved route — a
session's greeting and its coding question have separate affinities, never one
under `Auto` or the classifier), its policy, its failover, its placement, and
its name on the response and every stream chunk. Classification decides the
logical route and nothing else. There is no cross-route fallback: if the chosen
route cannot serve the request, that route's error is returned.

### Classification latency

Classification sits in front of the answer, and costs what one short
generation on the classifier route costs. It is measured on its own —
`classifier.duration_ms` in the trace and the
`router_classifier_duration_seconds` histogram — and is **not** part of
`routing_ms` or `router_routing_duration_seconds`, which keep their R6
meaning: the router's own planning time.

How long a classification takes depends on the classifier model, the hardware
and backend it runs on, and the prompt length — so `timeout_ms` **has no
default** and must be written in the classifier section. One observation, not a
benchmark: Qwen3-1.7B (Q4_K_M) on the 4-core development CPU took roughly
**13–44 s** per classification. A GPU, a smaller model, or a faster remote node
can classify in well under a second and support a much shorter timeout. A
timeout shorter than the classifier's real latency makes **every**
classification time out and fall back — correct, but no longer classifying —
so set it from the `router_classifier_duration_seconds` you observe. It is
bounded at 120 000 ms: classification must never hold a request indefinitely.
On timeout the classification request is cancelled (its upstream connection is
closed) and the request continues at the classifier's fallback route.

### Observability

```json
"requested_route": "Auto", "auto_rule": "semantic", "route": "Coder",
"classifier": {"route": "RouterClassifier", "outcome": "chosen", "chosen_route": "Coder",
               "confidence": 0.92, "duration_ms": 412.8, "request_id": "rtr-…-classify"}
```

`chosen_route` is what the classifier named, taken or not (a `low_confidence`
trace shows what it would have chosen). The classification request has its own
trace under the classifier route. One `auto route classified` log line records
the same fields. No prompt, answer text or tool schema is ever in a trace, a
log line, a metric or the admin view. Metrics:
`router_classifier_requests_total{provider,outcome}`,
`router_classifier_route_total{provider,route}` (chosen and taken), and
`router_classifier_duration_seconds{provider,outcome}`; `router_auto_route_decisions_total`
counts the classifying rule under the route it finally resolved to.
`GET /api/router/v1/auto` adds each rule's `classify` and a `classifier`
block: the provider, candidates with descriptions, fallback, the active
provider's threshold, timeout and input limit, the rules that invoke it,
outcome counts, a `status` (`last_success_at`, `last_failure_at`,
`last_failure_kind`, `last_check`), and each configured provider's settings
under `lightweight` / `jev` with `active` — for Jev, `api_key_configured`,
never the key. The trace's classifier object carries `provider` and, for Jev,
`model` (the versioned id that answered).

## Adaptive route scoring (R9.2)

R9.1 takes a classification at face value: at or above `min_confidence` the
named route wins, below it the fallback does. R9.2 adds a small, bounded,
explainable second opinion for the **borderline** cases just above the
threshold. It weighs the classifier's confidence against a preference the
operator writes down.

> **Scoring ranks logical routes only. It never ranks or chooses
> deployments.** Its whole output is one route name. Health, capability
> filtering, session affinity and the route's policy then choose the
> deployment exactly as for a client that named the route.

> **R9.2 slice 1 collects route-history observations but does not use them
> to choose a route.** Today's history observations measure successful
> traffic volume, not validated route quality. A route that is picked more
> often succeeds more often, and scoring that would let popularity win more
> decisions. So `weights.history` must be 0, and **only classifier confidence
> and the configured route prior affect route selection.** History stays
> available for observability and for future route-quality work.
> Popularity must never masquerade as quality.

```text
explicit route ─────────────────────────────────────────────┐ never scored
Auto → deterministic rule (tool_choice=required → ToolAgent) ┤ never scored
Auto → no rule matched → fallback_route ─────────────────────┤ never scored
Auto → classifying rule → classifier (R9.1 / R9.1a)          │
         ├─ no verdict (timeout, auth_error, …) → fallback ──┤ not scored: no_verdict
         ├─ verdict below min_confidence → fallback ─────────┤ not scored: below_threshold
         └─ verdict accepted → classifier signal + prior ────┤ scored
                                                             ▼
                     logical route → health → R5 capability filter → affinity → policy

every finished request ──▶ route-history observations (success, server_error,
                            interrupted, unavailable, mismatch, neutral)
                            — recorded, decayed, shown, resettable; never scored
```

It is off by default. Absent, or `"enabled": false`, a request resolves exactly
as in R9.1, with the same traces and metrics.

### Configuration

A sibling of `classifier` under `auto_route`:

```json
"adaptive_scoring": {
  "enabled": true,
  "weights": { "classifier": 1.0, "prior": 0.1, "history": 0.0 },
  "priors": { "General": 1.0 },
  "history": { "half_life_secs": 3600, "min_samples": 20, "shrinkage_samples": 20 }
}
```

| Field | Default | Bounds | Meaning |
|---|---|---|---|
| `enabled` | `false` | | An off section is still validated. |
| `weights.classifier` | `1.0` | `(0, 1]` | Weight of the classifier signal. |
| `weights.prior` | `0.0` | `[0, 1]`, and the bound below | Weight of the operator's prior. |
| `weights.history` | `0.0` | **must be `0`** | Kept in the schema for a later phase. Any other value is refused: history is observational only in slice 1. |
| `priors` | none (all `0`) | each `[0, 1]` | A preference per route. Keys must be a classifier candidate or the classifier's fallback, and are matched as route names are. |
| `history.half_life_secs` | `3600` | `60 ..= 604800` | How fast observed history fades. **One hour is a provisional operational starting point, not an empirically tuned value.** |
| `history.min_samples` | `20` | `1 ..= 10000` | Reported as `min_samples_reached`; the threshold a future quality signal would gate on. |
| `history.shrinkage_samples` | `min_samples` | `1 ..= 10000` | Reserved for a future quality estimator; not read by routing. |

A non-zero history weight is refused when the file is loaded. It is not
clamped and not ignored:

```text
auto_route.adaptive_scoring: weights.history must be 0: adaptive route history is
observational only in this release (R9.2 slice 1). Its observations measure successful
traffic volume, not route-attributable quality, so they must not choose a route
```

**The defaults are neutral.** With the prior weight at 0, every decision is
R9.1's. That makes `"enabled": true` with no weights a safe way to watch the
traces, metrics and history observations before giving priors any influence.

There is **one** set of weights, shared by every classifier provider. There
are no Jev- or Lightweight-specific weights: the provider is recorded in the
trace as metadata, never weighed. Unknown keys are refused (`deny_unknown_fields`)
at every level. That includes latency, exploration and persistence settings,
which this version does not have.

### What contends, and the classifier baseline

A classifier returns one route and one confidence. It says nothing about the
other candidates, and scoring invents nothing for them. So, once a verdict is
**accepted**, exactly two routes contend:

| Contender | Classifier signal |
|---|---|
| the verdict route | the verdict's own `confidence` |
| the classifier's fallback route | the **classifier baseline**: the active provider's `min_confidence` |

The **classifier baseline** is R9.1's decision rule restated as a score. A
verdict is accepted only at or above `min_confidence`, so in a contest the
fallback stands exactly where an accepted verdict had to be to beat it. Other
candidates have no signal and never win. A verdict that names the fallback
itself is `uncontested`.

### The score

```text
score(route) = W_classifier · classifier_signal(route)   classifier signal ∈ [0, 1]
             + W_prior      · prior(route)               prior ∈ [0, 1], default 0
```

These are the **only** active inputs. The history contribution is 0 by
construction, whatever has been observed.

The verdict route wins ties. That is R9.1's `confidence ≥ min_confidence`
rule at equality, and with `W_prior = 0` it makes every decision R9.1's.

### The threshold is a hard boundary

| Classifier confidence | What scoring does |
|---|---|
| **below `min_confidence`** | Nothing. R9.1 rejected the route and its fallback **is** the decision (`below_threshold`). The rejected route is recorded in the trace and is **not eligible**: no prior, and no amount of observed traffic, can revive it. |
| **exactly at `min_confidence`** | The verdict and the fallback tie on classifier signal; the priors decide. With equal priors the verdict wins. |
| **just above** (within the influence radius) | A prior may move the decision to the fallback. |
| **well above** (beyond the influence radius) | The classifier decides alone. No prior can overturn it. |

### How far scoring can move a decision

The verdict wins whenever

```text
confidence − baseline  ≥  W_prior · (prior_fallback − prior_verdict) / W_classifier
```

and the right side can be at most the **influence radius**,
`W_prior / W_classifier`, since a prior spans `[0, 1]`. History has no term:
it is not an active input. Validation refuses any configuration whose radius
is not **less than half of the accepted range**, `(1 − min_confidence) / 2`.
It checks this against every configured provider block (the active one and a
standby), so switching provider cannot silently break the guarantee:

> A classification in the upper half of the accepted range is never
> overturned by any prior.

With `min_confidence` 0.65 the radius must be under 0.175. For example,
`prior 0.1` with `priors: {"General": 1.0}` gives a radius of 0.10. A Coder
verdict below 0.75 then loses to General, and one at 0.75 or above wins.
`hermes router validate-config` prints the radius, and the admin view and
every scoring trace carry it.

### Route-history observations

> **Observation is not scoring.** Adaptive route history records many outcome
> categories, but only outcomes that can safely be attributed to route-level
> quality could ever participate in a scored history signal. Deployment and
> infrastructure failures remain observable but do not train logical-route
> preference. And in slice 1 **no** history participates in scoring at all.

Each configured route keeps one small aggregate in memory: decayed successful
completions, and plain counters for every other outcome. It never stores a
request, prompt, session or deployment. It is recorded once per finished
request, for the route that handled it, at the point the router knows the
request's **final** outcome. A stream counts when it ends, not when its
response head arrived. **All** traffic to a route counts: direct requests as
well as `Auto`. The router's own classification requests (`<id>-classify`)
never count.

| Final outcome | Observed as | Would a future quality signal count it? |
|---|---|---|
| `ok`: a completed response or stream | `success` (decayed; `effective_samples`) | a sample |
| `server_error`: a 5xx answer | `server_error` | no: a 500 is never retried, so it is one deployment's answer |
| `interrupted`: a committed stream that broke off | `interrupted` | no: one node's stream, or its connection, failed |
| `unavailable` (`route_unavailable`), or every deployment refused with 502/503/504 before answering | `unavailable` | no: capacity and readiness |
| `route_capability_mismatch` | `mismatch` | no: fit, which R5 decides exactly per request |
| `client_error`: any other 4xx | `neutral` | no |
| `cancelled`: the client left | `neutral` | no |

`effective_samples` is the decayed count of successful completions only. No
other outcome brings a route closer to `min_samples`: one success and nineteen
500s is one sample.

**Why successes alone are not a quality signal.** Every failure the router can
see today belongs to one deployment, one node or one connection, so none is
evidence against the route. Successes without failures say how much a route
was used, not how well it serves. A route picked more often accrues more,
so scoring them would feed popularity back into selection.

**Decay.** The success count is multiplied by `2^(−Δt / half_life)` whenever
it is read or updated. With no traffic it fades. Decay changes what is
observed, never a winner.

**In memory only.** History is process-local and starts empty when the router
starts. There is no file, database or Redis. Every configuration change
already needs a restart, so a topology change also resets history.

**When history could become a score.** Only once Lightweight has a genuinely
route-attributable quality signal. Examples: validated task success or
failure, an operator's or user's evaluation, a verifier's result,
tool-completion quality, or a route-level evaluator. Until then
`weights.history` stays 0. None of these is implemented.

### Resetting history

```sh
curl -X POST -H "Authorization: Bearer $KEY" http://127.0.0.1:11500/api/router/v1/adaptive-scoring/reset
curl -X POST -H "Authorization: Bearer $KEY" -d '{"route": "Coder"}' \
     http://127.0.0.1:11500/api/router/v1/adaptive-scoring/reset
```

This forgets every route's observed history, or one route's, and nothing
else. It does not touch the route configuration, `Auto` rules, classifier
settings, session affinity, placement, node state or loaded models. It uses
the router's key, like every other control endpoint. The answer is
`{"reset": "all"|"route", "routes": [...], "reset_at": <unix>}`. Errors are
`404 route_not_found`, `400 invalid_request_body`, and
`409 adaptive_scoring_not_enabled` when scoring is absent or off. Prometheus
counters stay monotonic; only the observed history starts over.

### What scoring does not do

- **Use route history**, in any form, in slice 1.
- **Choose a deployment, or read anything deployment-, node- or
  session-level.** No per-deployment latency, load, health, placement
  readiness or affinity enters a score.
- **Latency or context-fit scoring.** TTFT and durations are measured as before
  and never consulted. R5 remains the exact context and capability gate.
- **Penalise availability.**
- **Try another route.** If the winning route cannot serve the request, the
  client gets that route's `route_unavailable` or
  `route_capability_mismatch`, and no second route is chosen (that is R9.3).
- **Wait for, or ask for, a load.**
- **Learn weights, explore, or use Jev's per-option probabilities.** Only the
  provider-neutral route and confidence are read.
- **Add route capability declarations.** R5's observed deployment capabilities
  stay the one capability authority.

### Observability

A `scoring` block in the trace, whenever scoring is on and a classifier ran.
It holds route names and numbers only:

```json
"scoring": {
  "enabled": true, "reason": "scored",
  "classifier_baseline": 0.65, "influence_radius": 0.1,
  "weights": {"classifier": 1.0, "prior": 0.1, "history": 0.0},
  "history_active": false,
  "classified_route": "Coder", "winner": "General", "overrode": true,
  "candidates": [
    {"route": "Coder", "basis": "verdict", "confidence": 0.70, "prior": 0.0,
     "classifier_signal": 0.70, "prior_signal": 0.0, "history_signal": 0.0, "total_score": 0.70,
     "history_observations": {"effective_samples": 5.0, "successes": 5.0, "min_samples_reached": false}},
    {"route": "General", "basis": "baseline", "confidence": 0.65, "prior": 1.0,
     "classifier_signal": 0.65, "prior_signal": 0.1, "history_signal": 0.0, "total_score": 0.75,
     "history_observations": {"effective_samples": 100.0, "successes": 100.0, "min_samples_reached": true}}
  ]
}
```

`history_active` is always `false` and `history_signal` always `0`. A
candidate's `total_score` is `classifier_signal + prior_signal`. The
`history_observations` are reported, not used. `reason` is `scored`,
`uncontested`, `below_threshold` (with `rejected_route` and
`rejected_confidence`), `no_verdict`, or `internal_error` (a non-finite score,
unreachable with a valid configuration; R9.1's route stands). The R9.1
`classifier` block is unchanged. The `auto route classified` log line adds
`classified_route`, `scoring_reason` and `scoring_overrode`.

`GET /api/router/v1/auto` gains `adaptive_scoring`: `configured`, `enabled`,
`weights`, `influence_radius`, **`history_mode: "observational"`** and
**`history_affects_scoring: false`**, `priors`, `history` (`half_life_secs`,
`half_life_provisional`, `min_samples`, `shrinkage_samples`),
`classifier_baseline`, `fallbacks` (counts by reason), and `routes`. Each
route row has `effective_samples`, `successes`, `min_samples_reached`,
`server_error`, `interrupted`, `unavailable`, `mismatch`, `neutral` and
`last_observed_at`. Without the section it is
`{"configured": false, "enabled": false}`.

Metrics, with bounded labels and scores never used as labels:
`router_route_scoring_decisions_total{route,overrode}`,
`router_route_scoring_fallback_total{reason}`,
`router_route_history_observations_total{route,outcome}` (`success`,
`server_error`, `interrupted`, `unavailable`, `mismatch`, `neutral`), and the
gauge `router_route_history_effective_samples{route}`. Both history metrics
are observational and say so in their `HELP`. There is deliberately no
history "signal" gauge: no quality score exists to report.

Scoring is arithmetic over at most two contenders. It makes no network call,
no model call and no I/O, and it is counted inside `routing_ms`. A unit test
bounds 10 000 decisions in a debug build.

## Cross-route fallback (R9.3.1)

When the logical route an `Auto` request resolved to **cannot execute** it,
the router may try the next logical route the operator listed for that
route, **before anything was committed** to the client. It is explicit
recovery from a route's inability to execute. It is not reclassification,
re-scoring, deployment failover, a quality retry or mixture-of-agents. Design:
[R9_3_CROSS_ROUTE_FALLBACK.md](R9_3_CROSS_ROUTE_FALLBACK.md).

```text
Auto → R8 rule / R9.1 classifier / R9.2 score / fallback_route → initial route
   → that route's own pipeline: health → R5 → affinity → policy → same-route failover
        committed (any status)                                   → the answer
        route_unavailable | route_exhausted | route_capability_mismatch (pre-commit)
          → next entry of the initial route's list → the same pipeline again
        anything else, or the list used up                       → that route's own error
```

### Configuration

Under `auto_route`:

```json
"cross_route_fallback": {
  "Coder":     ["General", "Reasoning"],
  "Research":  ["General"],
  "ToolAgent": ["General"]
}
```

Each key is a route `Auto` can resolve to. Its value is the **complete,
ordered** list of routes to try after it, with at most **3** entries
(`MAX_FALLBACK_ROUTES`, fixed). So a request makes at most four
logical-route attempts. There is no separate `enabled` flag: an absent
section, or a route with no entry, means no cross-route fallback, and nothing
changes.

The file is refused at load for:
- an unknown source or target, `Auto`, `default`, any reserved name, the
  classifier's route, or an empty name;
- an empty list, or one longer than 3 entries;
- a self-reference, or a duplicate (compared ignoring case);
- a source `Auto` can never resolve to;
- a **cycle anywhere** in the union of all lists. Requests never walk that
  graph, but a cycle is refused anyway.

### Rules

- **`Auto` only.** Only a request whose original `model` was `Auto` is
  eligible, however `Auto` chose its route: a deterministic R8 rule (for
  example `tool_choice=required → ToolAgent`), the R9.1 classifier, R9.2
  scoring, or `fallback_route`. A client that named a route (`Coder`, in any
  case or spelling), asked for `default`, or sent no `model` gets that route
  or its error. The router's own classification requests never fall back.
- **Same-route failover first.** A route fails only after its own pipeline
  has finished: every deployment its plan listed was tried with the usual
  pre-response failover. If one deployment refuses and another answers, the
  route answered.
- **Three triggers, all before commit:**

  | `reason` | When |
  |---|---|
  | `route_unavailable` | no available deployment at planning, or every attempt failed without an answer (unreachable, a transport error before the head, `404 model_not_found`) |
  | `route_exhausted` | every planned deployment was tried, and the route ended on a 502/503/504 refusal sent before any answer |
  | `route_capability_mismatch` | no available deployment can serve this request's requirements |

- **Never:**
  - a 500, or any other committed answer;
  - one deployment's refusal while candidates remain;
  - `context_length_exceeded` (R5's in-route overflow failover is
    unchanged);
  - 429 or other 4xx;
  - latency, TTFT or answer quality;
  - a stream that breaks after commit;
  - client cancellation;
  - a classifier failure;
  - anything from R9.2.
- **The list is read once and never transitive.** The initial route's list
  is copied into the request and followed in order. A fallback route's own
  list is never consulted: with `Coder: [General, Reasoning]` and
  `General: [Research]`, a request that started on Coder tries Coder,
  General, Reasoning, and never Research.
- **Nothing is weakened or re-decided.** Each fallback route runs R5 with the
  request's original requirements: a `tool_choice=required` request is never
  sent to a route that cannot do tools. The classifier is not asked again,
  R9.2 does not score again, and no prior, history or latency picks the next
  route. The file's order does.
- **The commit boundary.** The decision is taken at the response head. Once
  a deployment's answer is committed (stream or body, content, reasoning or
  tool-call deltas), the route can no longer change, and no answer is ever
  spliced from two routes. Today nodes execute no tools: tool calls are run
  by the client after it receives a committed response. So nothing with an
  external side effect can precede the head. **Future constraint:** if
  Lightweight ever executes server-side side effects before commit, fallback
  must also stop at that side-effect boundary.

### What the client and the operator see

- **`model` names the route that served the response**, for bodies, tool
  calls and every stream frame. `Auto → Coder → General` answers `"model":
  "General"`.
- **An exhausted list returns the final attempted route's own error**, as
  that route returns it (`503 route_unavailable` naming it, a node's
  502/503/504 refusal, or `400 route_capability_mismatch`). There is no new
  error code.
- **One request id** across every route and deployment attempt.
- **Affinity is per route.** Each route uses its own `(route, session)`
  affinity. A fallback that succeeds settles the session for *that* route
  only. The initial route's entry is untouched, and future `Auto` decisions
  are not affected.
- **No placement action.** A route still loading is simply
  `route_unavailable`.
- **No shared deadline.** R9.3.1 does not introduce an end-to-end request
  budget. A chain whose nodes time out at connect adds up to
  `request.connect_timeout_secs` per deployment per route attempt. Keep
  chains short. Health probing keeps known-down nodes out of plans at no
  cost.
- **Counting.** `router_requests_total` counts each client request **once**,
  under the final route. Route history (R9.2, observational) records each
  route attempt.

Trace (route names and reasons only; `route` is the final route, and each
deployment attempt carries its `route`):

```json
"requested_route": "Auto", "route": "General",
"cross_route_fallback": {
  "initial_route": "Coder", "final_route": "General", "exhausted": false,
  "attempts": [
    {"route": "Coder",   "outcome": "failed", "reason": "route_exhausted"},
    {"route": "General", "outcome": "committed"}
  ]
}
```

Metrics: `router_cross_route_fallback_total{from_route,to_route,reason}` and
`router_cross_route_fallback_exhausted_total{route,reason}`, labelled with
configured routes and the three reasons only. Logs: `cross-route fallback`
(`request_id`, `initial_route`, `from_route`, `to_route`, `reason`,
`attempt`) and `cross-route fallback ended without an answer`
(`final_route`, `exhausted`). `GET /api/router/v1/auto` gains
`cross_route_fallback`: `configured`, `chains`, `max_routes`, `triggers`,
`applies_to: "auto"`, `counts` (from → to → reason) and `exhausted`
(initial route → reason). There is no UI yet.

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

## Session affinity

Related requests — the turns of one conversation — can prefer the deployment
that answered the last one. That keeps one conversation on one model's
behaviour, keeps a node's prompt cache relevant, and makes a conversation easy
to follow in the logs. **Off unless `session_affinity.enabled` is set.**

### Where the session comes from

Only from an explicit header, `X-Lightweight-Session` by default. A request
without it is routed exactly as without affinity. The router never infers a
session from an IP address, an API key, a user agent or the prompt: none of
those is a conversation, and treating one as such would make an operational
hint into user tracking.

Lightagent `7d95232` sends no session or conversation identifier — its
requests carry `model`, `messages`, `stream`, `stream_options`, `tools`,
`temperature` and `max_tokens`, and a bearer key — so it is routed without
affinity, unchanged. A client that wants affinity sends the header with any
stable id of its own (up to 256 visible ASCII characters).

### The decision order

```text
Request
  → Route resolution
  → Health / availability          (every request)
  → Capability filter              (this request's needs)
  → Session affinity preference    (only if the sticky deployment survived both)
  → Priority | RoundRobin | LeastBusy
  → Proxy / failover
```

**A sticky deployment is preferred only while it is still eligible.**

- If the session's sticky deployment is among the candidates left after health
  and the capability filter, it goes first. The policy takes no turn for that
  first choice: round-robin draws no cursor value and least-busy compares no
  loads for it (`routing reason` `session_affinity`). What follows it in the
  plan — where failover goes — is the policy's own order of the rest: the
  configured order (priority), the ring after the sticky deployment
  (round-robin), or least-busy over the rest, chosen and reserved under the
  route's lock.
- If it is not among them, it is ignored and the policy chooses exactly as it
  would for a request with no session. Affinity never resurrects a deployment
  that is unhealthy, disabled, swapped to another model, or unable to serve
  this request.
- Stickiness is not soft. Under least-busy a sticky deployment keeps the
  session even when another is idler; under priority it keeps it even when the
  primary is healthy again. A node at its scheduler limit is still eligible
  (the router has no admission control and the node queues), so it keeps its
  sessions too.

The policies themselves know nothing about sessions; affinity lives in
`affinity.rs` and `Selector::plan_with_affinity`.

### When a session moves

A session settles where a request **succeeds**: a committed `2xx` answer (for a
stream, a `2xx` `text/event-stream` head — the point at which the router
commits). A refusal the router returns as the node's answer (a `400`, a `500`)
does not move or create an affinity.

| What happened | Reassignment reason |
|---|---|
| The sticky deployment's node is unhealthy or not yet known | `sticky_unhealthy` |
| It is disabled, or its node now serves another model | `sticky_unavailable` |
| It cannot serve this request (tools, `tool_choice`, reasoning, endpoint, context) | `sticky_capability_mismatch` |
| It was tried first and refused the prompt as too long; a larger deployment answered | `sticky_context_overflow` |
| It was tried first and failed before answering (connection, timeout, 502/503/504, stale model) | `sticky_failed` |

In each case the session moves to the deployment that answered. It does **not**
move back when the old one recovers: a recovered primary gets new sessions and
sessionless traffic, while existing sessions stay where they are until they
expire or their deployment stops being valid. A capability mismatch moves the
session too, because the deployment that answered can serve both kinds of
request; if it later cannot, the session moves again.

### State, TTL and bounds

- **Key:** the route and a keyed hash of the session id. `Coder` and `Research`
  under the same id are two independent affinities. The raw id is never
  stored: it is hashed on arrival with a key drawn at startup, so the map, the
  admin view and the traces cannot be turned back into ids, nor matched
  across restarts. Two ids colliding (one in 2⁶⁴ per pair) would share a
  preference among valid deployments — harmless.
- **Value:** the deployment id, when the affinity was created, and when it was
  last used. Nothing else: no prompt, message, token or address.
- **TTL:** idle time. Every lookup and every successful request refreshes it,
  so a long stream does not expire its own session. An expired entry is
  removed when next looked up, by a sweep every `min(TTL, 60 s)` (at least
  1 s), and before any eviction for space.
- **Bound:** `max_entries`. A new session arriving at a full book first drops
  expired entries, then the least recently used.
- **Memory only.** No Redis, no database, nothing on disk. A router restart
  forgets every affinity; each session's next request is routed by the policy
  and settles again.

### Concurrency

The book is one short lock around a hash map, held for a map operation and
never across a network call; sessions do not wait on each other in any way a
request could notice. Two simultaneous first requests of one session may both
be routed by the policy, possibly to different deployments. **The first to
commit establishes the affinity; the other's success does not overwrite it**,
so the session settles on one deployment and the map is never torn. A
reassignment (the sticky deployment failed) does overwrite, because it is the
newer fact.

### KV cache

Affinity keeps a conversation on the node that holds its recent prompt.
Whether that saves work depends on the engine: llama.cpp reuses a slot's
cached prompt prefix when the next request lands on the same slot, which
affinity makes more likely but does not guarantee (a node with several slots
may place the turn elsewhere). The router claims no cache benefit and measures
none; TTFT is the place to look.

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
| any | `context_overflow_failover`, `session_affinity` (first choice was the session's sticky deployment) |

## Placement

R7 keeps routes ready before requests need them. It does not make routing any
smarter. The two questions stay apart:

| | Answers | Runs |
|---|---|---|
| **Routing** (R0–R6) | which already-ready deployment serves this request? | in the request, in microseconds |
| **Placement** (R7) | where should this route be prepared? | on its own loop, every `placement.interval_secs` |

**A request never waits for a load.** A route with nothing ready is refused
`503 route_unavailable` at once, exactly as before — even while a load for it
is in progress. Once the controller has seen the load finish, the next request
finds a ready deployment.

### Targets and warm standby

A route opts in:

```json
{ "name": "Coder", "strategy": "priority",
  "deployments": [ {"node": "node-a", "model": "QwenCoder"},
                   {"node": "node-b", "model": "CoderBackup"},
                   {"node": "node-c", "model": "CoderStandby"} ],
  "placement": { "min_ready": 1, "warm_standby": 1,
                 "allowed_nodes": ["node-a", "node-b", "node-c"] } }
```

- **`min_ready`** (default 1): ready deployments the route should never fall
  below.
- **`warm_standby`** (default 0): ready deployments to keep beyond that.
- **`allowed_nodes`** (required): the nodes the controller may load the
  route's model on. Each must already host one of the route's deployments; the
  controller loads exactly that deployment's model there, by that
  deployment's name, and never anything on a node not listed.

The target is `min_ready + warm_standby`, counted over **every** ready
deployment of the route, whoever loaded it. What the states mean:

| State | Meaning |
|---|---|
| ready primary / ready warm standby | loaded and available by the router's own probe. A standby is an ordinary deployment: it takes traffic as the route's policy orders it (under priority, after the primary). Nothing is hidden from routing. |
| loading | a load this controller asked for is in progress |
| empty — a cold standby | the node is healthy and serving nothing; the model may be installed there. **Not** warm: it cannot take a request. |
| occupied | the node serves another model. Never swapped. |
| unavailable | the node is disabled, unhealthy, or not yet probed |

`GET /api/router/v1/placement` reports each route's `status`: `satisfied`,
`below_target` (the warm standby is short) or `below_min`.

### Reconciliation

Each pass, every `interval_secs` (and at once after a load finishes, or on
`POST /api/router/v1/placement/reconcile`):

1. **Observe** — the health book the router already keeps; no extra probes.
2. **Compare** — each route's ready and loading deployments with its target.
3. **Plan** — routes in configured order; within a route its allowed
   deployments in configured order. A deployment is chosen only if its node is
   healthy and **empty**, it is not backing off, and no other load is in
   progress — or chosen in this pass — on that node. Never more loads than the
   route is short. Deterministic; nothing about speed, latency or history is
   read.
4. **Act** — each load is its own task:
   1. `GET /api/v1/models` on the node: the model must be in its catalog, by id
      or alias, with its file present. Nothing is downloaded. If the node
      already reports it `loaded`, nothing is loaded; readiness is confirmed.
   2. `POST /api/v1/models/{id}/load`, empty body: the node chooses context,
      slots and threads as it does for any load.
   3. `GET /api/v1/jobs/{job}` until the job ends.
   4. The node is probed, by the health monitor's own probe, until the
      deployment is available by the request path's own rule. **Only then is it
      counted ready.** A load call returning, or a job succeeding, is not
      enough.
5. **Wait** for the next pass.

### What the controller will not do

- **Swap a model out.** A Lightweight gateway holds one model; a node serving
  anything else — another route's model included — is left alone. Two routes
  wanting the same empty node: the first in configured order gets it.
- **Unload** anything, or move a model to rebalance. R7 is additive only.
- **Download** a model. A model not in the node's catalog is
  `model_not_installed`.
- **Estimate memory.** The node's admission control judges every load; a
  refusal (`insufficient_memory`) is `admission_failed`. The router does not
  try to outsmart it.
- **Use R6's measurements.** TTFT, latency and traces are for operators;
  nothing slow is moved automatically.

### Failures and backoff

| Reason | When |
|---|---|
| `model_not_installed` | not in the node's catalog, or its file is missing |
| `admission_failed` | the node's admission refused it (`insufficient_memory`) |
| `node_busy` | the node was already in a model operation (`model_operation_in_progress`, `drain_timed_out`) |
| `node_unhealthy` | the node could not be reached for the control request |
| `load_rejected` | the node refused the control request (credential, no catalog) |
| `load_timeout` | load plus readiness took longer than `load_timeout_secs` (`code: not_ready` when the job succeeded but the deployment never became available — for example a deployment naming something the node does not advertise) |
| `model_failed` | the engine failed to start, or the job was cancelled |

A failed deployment is not tried again until its backoff passes:
`backoff_secs`, doubled for each consecutive failure, at most
`backoff_max_secs`. A success resets it. The node's own error code is kept
beside the reason.

### Credentials

The controller uses each node's own key (`api_key_env`) — the same one probes
and requests use — on the node's `/api/v1` control API. The router's client
key is never sent to a node. A node's key is not scoped, so a node that
should not be controlled should simply not be listed in any `allowed_nodes`.

### Interaction with the rest

- **Health, capabilities, policies:** unchanged. A deployment placement
  loaded is filtered for each request like any other (tools, reasoning,
  context, endpoint), and priority, round-robin and least-busy order the ready
  ones as before.
- **Session affinity:** unchanged. A session that moved off a dead deployment
  stays where it moved after placement brings the old one back.
- **State:** in memory. A restart forgets loads in progress (a node that
  accepted one finishes it on its own), results and backoff.
- **Shutdown:** stopping the router stops the controller and aborts its load
  tasks. The router starts no process of its own, so nothing is left behind.

## The panel (`--web-root`)

`hermes router --web-root <dir>` serves the control panel — the same bundle
`hermes serve --web-root` serves, built by `npm run build` in `frontend/` — at
the router's own address, for the same reason the gateway serves it: the page
and the API it calls share an origin, so no cross-origin policy is ever
written. Without the flag, `/` is a `404` and nothing changes.

```sh
export TYPESAFE_API_KEY="..."     # only when Jev is configured; never in a file or the panel
hermes router --config router.json --web-root frontend/dist
# open http://127.0.0.1:11500/
```

- The panel asks `GET /version` once; a `build` beginning `lightweight-router-`
  means a router, and it shows the router's sections — **Auto Routing** and
  **Classifier** — instead of the gateway's. A gateway's panel is unchanged.
- The panel's files need no credential; every API path keeps the router's own.
  An unknown path under `/api` or `/v1` is still the router's JSON
  `not_found`, never the panel's document. A router with a client key
  (`api_key_env`) refuses the panel's API reads with `401`, and the panel says
  so rather than storing the key: serve it from a loopback-only router, or read
  the API with `curl`.

**Auto Routing** lists the rules in the order they are tried. Each rule's action
reads either *Route directly to &lt;route&gt;* or *Semantic classification* (a rule
written `"classify": true`), with its decision count, and the logical routes
with their descriptions and availability.

**Classifier** has three parts:

- **Provider status** — the running provider, whether it is active, the API key
  status (*Configured* or *Missing*, with the variable's name — never the
  value), model and endpoint, which rules use it, last check, last success,
  last failure and its kind, and outcome counts. **Test Connection** calls
  `POST /api/router/v1/classifier/check` on the running router: for Jev, the
  router lists TypeSafe's models with its key (no request text is sent); for the
  Lightweight provider, it checks the classifier route is available. Every
  status is shown in words — *Connected*, *API key missing*, *Authentication
  failed*, *Provider rate limited the request*, *Provider error*, *Could not
  reach the provider*, *Connection timed out*, *Unexpected response*.
  `model_not_listed` reads *The configured model was not listed by the
  provider's model discovery endpoint. Pinned versions may still be accepted.*
  — TypeSafe lists aliases only, so it is a warning for a pinned version and
  likely a typo for an alias, never "invalid model".
- **Classifier settings**, a draft seeded from the running router: a
  **Classifier Provider** choice (Lightweight / Jev / TypeSafe) and only the
  chosen provider's fields. Lightweight: classifier route, timeout, minimum
  confidence, maximum input characters. Jev: base URL, API key environment
  variable and its status, model (free text — an alias or a pinned version;
  `jev-latest` is only a suggestion), model discovery status from the last
  check, timeout (required; 5000 ms is offered by a button as a starting point,
  never filled in silently), minimum confidence, maximum input characters, and
  **Include user message text in classifier request** with what each setting
  sends. When Jev is chosen, a notice says that it is an external provider and
  that, with user text included, bounded user-message content goes to the
  configured TypeSafe endpoint. Then the shared **candidates** (logical routes
  only — never `Auto`, a node, a deployment or a model file), each route's
  **description** (one line, at most 200 characters), and the **fallback
  route**, which every non-chosen outcome uses: `low_confidence`, `timeout`,
  `auth_error`, `rate_limited`, `connection_error`, `provider_error`,
  `unavailable`, `invalid`.
- **Configuration to apply.** The router reads `router.json` once at start and
  has no API that writes it, so the panel does not pretend to save. Every field
  is checked as the router would check it (the router stays the authority); a
  valid draft becomes the canonical `auto_route.classifier` section — `provider`
  and one block per provider, never the R9.1 flat keys, never a key — plus any
  changed route descriptions, to paste in, check with
  `hermes router validate-config`, and load by restarting the router.

Switching provider changes only that section: rules keep `"classify": true`.
The TypeSafe key is set where the router runs (`export TYPESAFE_API_KEY=...`,
or the variable `api_key_env` names) and never passes through the panel: it is
not a field, not in any response the panel reads, and not in browser storage.

## The control API

All of these use the client key. None of them shows a key: `auth` is reported
only as `"bearer"` or `"none"`.

| Endpoint | Shows |
|---|---|
| `GET /api/router/v1/nodes` | Each node's URL, `enabled`, health, consecutive failures, last check, last seen, last error, the model it is serving, and its version. |
| `GET /api/router/v1/routes` | Each route's `description` (or `null`), `strategy`, `available`, and its deployments with their configured position (`priority`), availability and reason. Also the `default_route`. |
| `GET /api/router/v1/deployments` | Each deployment's node, model, the routes that use it, and its availability. Also its own last-observed `capabilities`, `context_length` and `max_concurrent_requests`, and what least-busy reads: `active_requests` (the router's in-flight count) and `concurrency_limit`. |
| `GET /api/router/v1/health` | Node and route health in one read, the probe settings, and active requests. |
| `GET /api/router/v1/sessions` | Whether affinity is on, its header, TTL and limit, how many sessions are live, evictions by reason, and each live entry: `route`, `session` (an 8-hex-digit keyed fingerprint, never the id), `deployment`, `age_secs`, `idle_secs`. |
| `GET /api/router/v1/placement` | Whether placement runs, its interval and load timeout, the last pass, and per route with a target: `min_ready`, `warm_standby`, `target`, `ready`, `ready_standby`, `loading`, `pending_loads`, `status`, and each deployment's `state`, `allowed`, `last_result` (action, result, reason, the node's code, time, duration), `consecutive_failures`, `retry_in_secs`. |
| `POST /api/router/v1/placement/reconcile` | Runs a placement pass now. It plans exactly what the interval would; it cannot name a node, force a load or skip a backoff. `202`, or `409 placement_not_configured`. |
| `GET /api/router/v1/auto` | Whether `Auto` is configured and on, its `fallback_route` and `fallback_decisions`, and its rules in the order they are tried: `position`, `name`, `when` (as written), `condition` (`requires_tools=true AND requires_reasoning=true`), `route`, `classify`, `decisions`. With a classifier, a `classifier` block (route, candidates and descriptions, fallback, `min_confidence`, `timeout_ms`, `max_input_chars`, `invoked_by`, `outcomes`). With scoring, an `adaptive_scoring` block (settings, `history_mode: "observational"`, `history_affects_scoring: false`, and per-route history observations; see [Observability](#observability-2)), and a `cross_route_fallback` block (lists, `max_routes`, `triggers`, `applies_to`, counts and exhaustions). Read-only; rules change only with the file. |
| `POST /api/router/v1/adaptive-scoring/reset` | Forgets adaptive scoring's route history: all routes, or `{"route": "Coder"}` for one. Touches nothing else. `409 adaptive_scoring_not_enabled` when scoring is absent or off. See [Resetting history](#resetting-history). |
| `POST /api/router/v1/classifier/check` | Checks the active classifier provider now and records the result: for Jev, `GET /v1/models` (key accepted, model listed); for the Lightweight provider, whether its classifier route is available. Sanitized report; never classifies. `409 classifier_not_configured` without a classifier. |
| `GET /api/router/v1/traces?limit=N` | The most recent routing traces, newest first (default 50, at most `traces.capacity`). See [Routing traces](#routing-traces). |

## Observability

**Measured, never consulted.** Nothing in this section is read by a routing
decision. Priority, round-robin, least-busy, the capability filter and session
affinity behave identically whatever TTFT, latency or estimator error say. A
later milestone may choose to use the evidence; R6 only collects it.

### Logs

Logs use the target `hermes::router` (filter with `HERMES_LOG=hermes::router=debug`)
and go to stderr, never to the data directory's `gateway.log`. Each routed
request logs `routed` when it commits:

- `request_id`, `route`, `policy`, `node`, `deployment`, `reason`, `routing_ms`,
  `upstream_status`, `upstream_response_ms` and `failover_count`;
- under round-robin, also `cursor` and `selected_index`;
- under least-busy, also `active_before` and `concurrency_limit`;
- what the request required: `endpoint` (`chat` or `completion`),
  `requires_tools`, `tool_choice`, `requires_reasoning`, `required_context` and
  `max_tokens`;
- `eligible_before` and `eligible_after` the capability filter, and `filtered`,
  how many deployments each requirement ruled out
  (`tools_unsupported=1,context_too_small=1`);
- with a session: `session` (the fingerprint), `affinity` (`hit`, `miss` or
  `reassigned`) and `reassignment`.

And `request finished` when it ends — however it ends, including a client
leaving — with `stream`, `session`, `affinity`, the final `deployment`,
`attempts`, `status`, `outcome`, `routing_ms`, `ttft_ms`, `duration_ms`,
`estimated_prompt_tokens` and `actual_prompt_tokens`.

At `debug`, each ruled-out deployment is logged with all of its reasons. A
refused request logs `no available deployment can serve this request` with
the same fields and `unmet`.

Prompt text, message history, tool arguments, credentials, session ids and
file paths are never logged. The node side is described under
[Headers and credentials](#headers-and-credentials).

### Time to first token

`router_ttft_seconds{route,policy}`: **from the moment the router has the whole
request body to the moment it relays the first frame carrying generated
output.** A frame counts when one of its choices has a non-empty
`delta.content`, `delta.reasoning_content` or `delta.tool_calls` (chat) or a
non-empty `text` (completions) — the same events the gateway's own TTFT
counts. Keep-alive comments, queue notices, the role-only opening delta, empty
deltas, usage-only chunks and error frames do not. Only the first such frame
counts; later ones never move it. Streams only: a non-streamed response has no
first token the client saw, so it has no TTFT sample, only durations. A stream
that fails before any output records no TTFT.

`router_upstream_ttft_seconds{route,deployment}`: the same event, measured from
sending to the deployment that answered. The difference between the two is the
router's planning plus any failed attempts before it.

### Latency

| Metric | From → to | Recorded for |
|---|---|---|
| `router_routing_duration_seconds{route,policy}` | body parsed → plan made (route resolution, requirements, affinity lookup, eligibility, capability filter, policy) | every request whose body parsed, including one whose planning failed (`route_unavailable`, `route_capability_mismatch`, a refused request; an unknown route is recorded as `route="_unknown",policy="none"`). Microsecond buckets: the router's own work, with no upstream wait in it. |
| `router_upstream_response_seconds{route,deployment}` | one attempt sent → its response head | every attempt that got a response, including refusals that failed over |
| `router_upstream_duration_seconds{route,deployment}` | the committed attempt sent → its body ended (or the client left) | committed attempts only |
| `router_request_duration_seconds{route,policy}` | request body in hand → response finished, refused, or abandoned | every request with a route, once |

The three router-side clocks start at two different points, on purpose:

| Name | Starts | Ends |
|---|---|---|
| `routing_ms` (log, trace) = `router_routing_duration_seconds` | after the request body is parsed — the same point as before R6 | when the plan is made, or planning fails |
| TTFT (`ttft_ms`, `router_ttft_seconds`) | when the router has the complete request body | at the first generated output relayed (streams only) |
| request duration (`duration_ms`, `router_request_duration_seconds`) | when the router has the complete request body | when the response ends, is refused, or is abandoned |

So `routing_ms` is routing and planning work only; parsing the body is in TTFT
and the request duration but in no routing figure, and is not measured on its
own. `routing_ms` against the upstream numbers is how a slow router is told
apart from a slow model. Connection time is not measured separately: the HTTP client
exposes no connect hook, and an approximation would be a number nobody should
trust. Durations come from a monotonic clock and cannot be negative.

### Context estimate against the node's count

The capability filter's prompt estimate is a lower bound — message-text bytes
divided by 6 — and the node counts the real thing. When a response carries
`usage.prompt_tokens` (every non-streamed chat answer, and a stream whose
client asked for `stream_options.include_usage`, as Lightagent does), the two
are compared:

- `router_context_estimation_ratio{route}`: **`actual / estimated`**. Above 1
  is an underestimate, which a lower bound is built to be. Not recorded when
  the estimate is 0, because that ratio is not a number.
- `router_context_estimation_error_tokens{route}`: `actual − estimated`, signed;
  a negative value would mean the bound overestimated.

Nothing is recorded when the node reported no count. When a request fails over
after `context_length_exceeded`, its trace records the estimate, every
context that proved too small, and the context of the deployment that
answered. The estimator is not tightened automatically: these numbers exist so
a later release can change it on evidence. (First real-node figures, SmolLM2:
a one-line prompt estimated 4 and counted 36 — template markup dominates short
prompts; a Lightagent turn with tools estimated 124 and counted 160.)

### Routing traces

One `RoutingTrace` per routed request, kept in a memory-only ring of
`traces.capacity` (oldest dropped) and served by
`GET /api/router/v1/traces`. Distinct from `RoutingDecision`, which says where
one attempt went and why; a trace says what happened over the whole request:

```json
{
  "request_id": "rtr-…", "received_at": 1791221798, "route": "Coder",
  "requested_route": "Coder", "endpoint": "chat", "stream": true, "policy": "round_robin",
  "session": {"fingerprint": "d7630584", "affinity": "reassigned",
              "sticky": "node-a/QwenCoder", "reassignment": "sticky_failed"},
  "deployments": 2, "available": 2, "capable": 2,
  "unavailable": [], "unfit": [],
  "selected": "node-a/QwenCoder", "selection_reason": "session_affinity",
  "attempts": [
    {"deployment": "node-a/QwenCoder", "reason": "session_affinity", "outcome": "failed"},
    {"deployment": "node-b/CoderBackup", "reason": "round_robin_failover",
     "outcome": "committed", "upstream_status": 200, "response_ms": 3.1}
  ],
  "final_deployment": "node-b/CoderBackup",
  "estimated_prompt_tokens": 4, "actual_prompt_tokens": 36,
  "routing_ms": 0.27, "ttft_ms": 70.6, "duration_ms": 892.4,
  "status": 200, "outcome": "ok"
}
```

`outcome` is `ok`, `client_error`, `server_error`, `unavailable`,
`interrupted` (a committed stream that ended without `[DONE]`: the node broke
off or sent an in-band error) or `cancelled` (the client went away). A
`context_overflow` object appears after an overflow failover. No trace holds a
prompt, a message, a tool argument, a credential or a session id.

`route` is the logical route that handled the request, and `requested_route`
what the client asked for. They differ only for `Auto`, which also records
`auto_rule` (the rule that chose the route) or `auto_fallback: true`. The
route decision and the deployment decision are then both on record: `Auto`
chose `route`, and `policy`, `selected`, `selection_reason`, `attempts` and
`final_deployment` are the deployment decision inside it, unchanged.

### Metrics

Labels are only configured names — a route, its policy, a deployment — or a
reason from a fixed list. Never a session, a request id, a prompt, a tool name
or an address.

- `router_requests_total{route,outcome}`
- `router_failovers_total{route}`
- `router_routing_decisions_total{route,policy,reason}`. Failovers by policy are
  the `*_failover` reasons; affinity hits are `session_affinity`.
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
- `router_node_health{node}`: `1` healthy, `0` unhealthy, `-1` unknown.
- `router_session_affinity_hits_total{route}`,
  `router_session_affinity_misses_total{route}` (a session with no usable
  affinity: new, expired, or its deployment no longer valid),
  `router_session_affinity_reassignments_total{route,reason}`,
  `router_session_affinity_entries`, `router_session_affinity_enabled`,
  `router_session_affinity_evictions_total{reason="expired"|"capacity"}`.
- Placement (only for routes with a target):
  `router_placement_ready_deployments{route}`,
  `router_placement_target_deployments{route}`,
  `router_placement_loading_deployments{route}`,
  `router_placement_actions_total{route,action,result}`,
  `router_placement_failures_total{route,reason}`, and the histogram
  `router_placement_reconcile_duration_seconds` (one pass, without the loads
  it starts).
- Classification (R9.1 / R9.1a): `router_classifier_requests_total{provider,outcome}`,
  `router_classifier_route_total{provider,route}`, and the histogram
  `router_classifier_duration_seconds{provider,outcome}`. `provider` is
  `lightweight` or `jev`; outcomes are a fixed list.
- Adaptive scoring (R9.2, only while on):
  `router_route_scoring_decisions_total{route,overrode}`,
  `router_route_scoring_fallback_total{reason}`,
  `router_route_history_observations_total{route,outcome}` and
  `router_route_history_effective_samples{route}` (observational: route
  history does not affect routing in slice 1).
- Cross-route fallback (R9.3.1):
  `router_cross_route_fallback_total{from_route,to_route,reason}` and
  `router_cross_route_fallback_exhausted_total{route,reason}`.
- `Auto` (R8): `router_auto_route_decisions_total{rule,route}` and
  `router_auto_route_fallback_total{route}`. `rule` is a configured rule name
  — bounded (at most 64) and held to a label-safe alphabet — or `_fallback`.
- Histograms (`_bucket`, `_sum`, `_count`): `router_request_duration_seconds`,
  `router_routing_duration_seconds`, `router_ttft_seconds`
  (`{route,policy}`); `router_upstream_ttft_seconds`,
  `router_upstream_response_seconds`, `router_upstream_duration_seconds`
  (`{route,deployment}`); `router_context_estimation_ratio`,
  `router_context_estimation_error_tokens` (`{route}`).

A request for an unconfigured route is counted under `route="_unknown"`, so
clients cannot add labels by inventing model names.

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
- **Affinity is per router process.** Two routers in front of the same nodes
  keep separate books, and a restart forgets every affinity. Neither breaks a
  conversation; it only costs one policy decision per session.
- **A session moves only on success.** If every deployment fails, the session
  keeps pointing at its sticky deployment, and its next request tries it first
  again — unless health has ruled it out by then, which is the usual case.
- **TTFT is the relay's first generated frame, not the client's first byte.**
  The keep-alives and the role-only chunk before it reach the client earlier;
  they are deliberately not counted.
- **Upstream connection time is not separated** from the response-head time;
  see [Latency](#latency).
- **`Auto`'s rules see structure only.** They can tell a tool request from a
  plain chat, but not a coding question from a poem. That is what a classifying
  rule (R9.1) is for.
- **A classifier is only as good as its model.** On the development box,
  Qwen3-1.7B chose the intended route for 4 of 5 real prompts; it called a
  GPU-announcement comparison `General`, not `Research`. A wrong but confident
  answer is taken. Descriptions and the candidate list are the operator's
  levers. With [adaptive scoring](#adaptive-route-scoring-r92) on, an
  operator prior can tip a *borderline* accepted verdict toward the fallback,
  never a confident one.
- **Adaptive scoring uses no history yet.** Route history is collected but
  observational: the only scorable evidence today is successful completions,
  which measure volume, not quality. Only the classifier and operator priors
  choose; the priors are the operator's judgement, not a measurement. History
  lives in memory, restarts empty, and its one-hour half-life is provisional.
  Scoring can only tip a borderline accepted verdict toward the classifier's
  fallback: with one route and one confidence per classification, it cannot
  rank candidates the classifier did not name.
- **Classification adds a generation to every classified request.** See
  [Classification latency](#classification-latency). `timeout_ms` is required
  precisely because no one value suits a GPU, a CPU and a remote classifier.
- **Jev sends request text to an external service** (unless
  `include_user_text` is off), and its quality, latency and limits are
  TypeSafe's — rate limits are documented as dynamic. Every failure falls back,
  so Auto keeps answering, but a classification that is always falling back is
  only visible in the trace, the metrics and the admin status.
- **No provider chain.** If the active provider fails, the request takes the
  fallback route; the other provider is not tried.
- **The classifier route is a visible model.** It is a configured route, so it
  is listed in `/v1/models` and a client may call it directly.
- **`Auto`'s prompt threshold is the router's lower bound.** A prompt the model
  counts at 14 000 tokens may be estimated at 9 000, and miss a
  `min_prompt_tokens: 12000` rule. Set thresholds against the estimate; the
  trace shows both figures for real traffic.
- **No cross-route fallback.** If the route `Auto` chose is down, the request
  fails even when another route could have answered it. That is deliberate
  ([What `Auto` does not do](#what-auto-does-not-do)).
- **Lightagent `7d95232` sends its tool set with every turn.** Behind `Auto`,
  every Lightagent chat therefore matches a `requires_tools: true` rule; a
  plain-chat rule only ever sees other clients. Its status bar shows the model
  it selected (`Auto`), not the route that answered.

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

R8 (rule-based `Auto`), R9.1 (content-aware classification), the first
slice of R9.2 (adaptive route scoring) and R9.3.1 (explicit cross-route
fallback) are built. Nothing after them is. Each
later step builds on the types above without changing the public route
identity.

| Milestone | Scope |
|---|---|
| **R4** | Done: `round_robin` and `least_busy`. Deliberately left out: weighted, random, latency/EWMA/P95/TTFT, and cost-aware selection. Any of these would be a new `RoutePolicy` variant with its own ordering function. |
| **R5** | Done: request-aware capability filtering between eligibility and policy, for endpoint, tools, `tool_choice`, reasoning and context. Deliberately left out: ranking by capability, routing on the output budget, per-model tokenization, and moving a request to another route. |
| **R6** | Done: optional session affinity (explicit header, route-scoped, bounded, idle TTL, a preference only over valid candidates); TTFT, latency and planning histograms; one request id from client to node log, across failover; per-request routing traces; estimate-versus-node prompt-token telemetry. Deliberately left out: using any of it to route, soft affinity, persistence, and inferring sessions. |
| **R7** | Done: per-route `min_ready` / `warm_standby` targets on allowed nodes, a reconciliation loop that loads installed models onto empty nodes through each node's control API, readiness by the router's own probe, the node's admission as the authority, bounded backoff, `/api/router/v1/placement`. Deliberately left out: unloading, swapping, rebalancing, downloading, and any use of latency or traffic. |
| **R8** | Done: an opt-in `Auto` model that chooses the logical route by ordered, first-match rules over the R5 request requirements (endpoint, tools, `tool_choice`, reasoning, prompt estimate), with an explicit fallback; the route's own pipeline chooses the deployment; route-scoped affinity; the resolved route on every response; `requested_route`/`auto_rule` in traces, decision metrics and `/api/router/v1/auto`. Deliberately left out: prompt-content classification, scores, history, latency or cost, cross-route fallback, and live rule editing. |
| **R9.1** | Done: an opt-in classifier an `Auto` rule invokes with `"classify": true` — itself a configured route, called through the router's pipeline — choosing only among configured candidate routes (with optional route descriptions), with a confidence threshold, a timeout, bounded input, and a deterministic fallback for every failure; recursion refused; classifier time measured apart from `routing_ms`; traces, metrics and admin state. Deliberately left out: scores, history, latency, cross-route fallback, and orchestration. |
| **R9.1a** | Done: a provider-neutral classifier boundary (`classifier/`: `lightweight`, `jev`) with one `Classification` result; TypeSafe Jev System One as an optional provider (typed Choice over the candidates, its own confidence, bearer key from the environment, https, bounded failures, no retries); per-provider settings; sanitized admin state, a start-up check and `POST /api/router/v1/classifier/check`. Deliberately left out: provider chains, scoring, and anything after the route is chosen. |
| **R9.1a UI** | Done: the panel served by the router (`--web-root`) with Auto Routing and Classifier screens — provider status, Test Connection, a validated settings draft that produces the canonical configuration to paste. Deliberately left out: writing `router.json` from the panel, and a test-classification endpoint. |
| **R9.2** | Slice 1 done ([design](R9_2_ADAPTIVE_ROUTE_SCORING.md)): off-by-default scoring of an accepted classification's verdict route against the classifier fallback (whose signal is the explicit classifier baseline), by classifier signal and operator priors only; a hard below-threshold boundary; an influence radius `prior / classifier` validated under half the accepted range; route-history observations (decayed, shown, resettable) that never affect routing, with `weights.history` required to be 0; traces, metrics, admin view and an admin history reset. Deliberately left out: history scoring until a route-attributable quality signal exists, latency and context-fit scoring, Jev per-option probabilities, provider calibration, availability penalties, persistence, exploration, learned weights, and any UI. |
| **R9.3** | R9.3.1 done ([design](R9_3_CROSS_ROUTE_FALLBACK.md)): explicit, ordered per-route fallback lists (`auto_route.cross_route_fallback`, at most 3, acyclic, never transitive) for `Auto`-resolved requests only, after same-route failover and before response commit, on `route_unavailable`, `route_exhausted` and `route_capability_mismatch`. The response names the serving route; an exhausted list returns the final route's own error; requests are counted once. Deliberately left out: explicit-route fallback, context-overflow, 500 or latency triggers, a shared request budget, reason-specific lists, and UI. |
| **R9.4** | Planned: mixture-of-agents orchestration — parallel expert routes and one aggregator route, each through the normal pipeline, bounded fan-out, defined partial-failure rules, depth 1. |

Out of scope for every one of these: a request-path model load, splicing one
node's output into another's stream, and a dependency on distributed consensus.
