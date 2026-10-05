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

This document covers the first version, milestones R0 to R3: the domain model,
a transparent proxy, a multi-node registry with health checks, and priority
routing with failover before the response starts. The [roadmap](#roadmap) lists
what comes after.

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
- **Policy** (`RoutePolicy`). Only `priority` exists. It is an enum, so each
  later strategy is a new variant rather than a redesign.

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
| `GET /v1/models` | Lists every configured route, including routes with nothing available right now. Each row has `owned_by: "lightweight-router"`. When a route's context is known it is given under the gateway's names (`context_length`, `n_ctx`, `max_tokens`, `max_output_tokens`), using the **smallest** context any of the route's deployments was last seen serving. No node, address, node-local name or file appears. |
| `GET /v1/capabilities` | The gateway's contract, with the same protocol name and version (`lightweight-public-inference`, v1) and the same top-level fields, plus a `routes` array. |
| `POST /v1/chat/completions`, `POST /v1/completions` | Routed and proxied, streamed or not. |
| `GET /health` | Never refused. Returns `ok`, `degraded` or `unavailable` with route counts, and nothing more. |
| `GET /metrics` | Prometheus text, behind the client key. |

**The rule for capabilities is conservative.** A route claims a feature only if
every deployment it could send a request to right now supports it. The router as
a whole claims only what every available route supports. A route with nothing
available claims nothing. `state.model` describes the default route, and only
while that route is available. `limits.max_concurrent_requests` is the smallest
limit among the available deployments, or `0` when none is available.

### Model identity, both ways

The router parses a request only far enough to read `model`. It rewrites `model`
to the chosen deployment's node-local name and forwards every other field
unchanged. On the way back, the response's `model` is rewritten to the route's
own spelling. That applies to a whole JSON body and to every streamed chunk that
carries a `model`.

Errors from a node are forwarded as the node wrote them, so a context overflow
still parses the same way.

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
every router log line.

## Health

Each enabled node is probed at its own `GET /v1/capabilities`, using its
credential. That one cheap call shows:

- whether the node is reachable;
- whether it is Lightweight (by protocol name and version);
- which model it is serving (`state.model.id`, alias first);
- what it supports, its context, and its concurrency limit.

Nothing is probed during a request.

- One success makes a node `healthy` immediately.
- `failure_threshold` consecutive failures make it `unhealthy`. A failed
  connection or a timeout during a request counts as one of those failures.
- A node that has never been seen stays `unknown`.
- A deployment is **available** only if its node is enabled and healthy and is
  serving the deployment's model (compared ignoring case, as the node compares
  aliases).
- The first probe runs before the first request is accepted.

## Priority routing and failover

The router takes the route's deployments in configured order and drops every one
that is not available. The first remaining deployment gets the request. Nothing
is reordered by latency. When a primary recovers, it is first again on the very
next request.

**Failover happens only before anything has been sent to the client.** The next
deployment is tried when the current one:

- refuses the connection, fails DNS, or times out connecting;
- answers `502`, `503` or `504` (for example `server_busy` or no model loaded);
- answers `404 model_not_found`. That means the node swapped models since the
  last probe. The router forgets what that node was serving until the next
  probe, and the node's message is not shown to the client.

Every other answer commits the deployment. That includes `400` and `500`: a
`500` is the node's verdict on this request, and is returned rather than run
again elsewhere. If every candidate refuses, the last node's own refusal is
returned. If every candidate fails to connect, the result is `route_unavailable`.

Once a response has started, the deployment stays committed. If the node fails
mid-stream, the client receives one
`data: {"error":{"code":"upstream_stream_interrupted",…}}` frame and no `[DONE]`.
Another node is never asked to continue an answer it did not start.

`RoutingReason` in the logs is one of `explicit_single_deployment`,
`primary_healthy`, `primary_unavailable_fallback` or `primary_failed_fallback`.

## The control API (read-only)

All of these use the client key. None of them shows a key: `auth` is reported
only as `"bearer"` or `"none"`.

| Endpoint | Shows |
|---|---|
| `GET /api/router/v1/nodes` | Each node's URL, `enabled`, health, consecutive failures, last check, last seen, last error, the model it is serving, and its version. |
| `GET /api/router/v1/routes` | Each route's strategy, `available`, and its deployments with their priority, availability and reason. Also the `default_route`. |
| `GET /api/router/v1/deployments` | Each deployment's node, model, the routes that use it, and its availability. |
| `GET /api/router/v1/health` | Node and route health in one read, the probe settings, and active requests. |

## Observability

Logs use the target `hermes::router` (filter with `HERMES_LOG=hermes::router=debug`)
and go to stderr, never to the data directory's `gateway.log`. Each routed
request logs `request_id`, `route`, `node`, `deployment`, `reason`,
`routing_ms`, `upstream_status` and `failover_count`. Prompt text, credentials
and file paths are never logged.

Metrics: `router_requests_total{route,outcome}`, `router_failovers_total{route}`,
`router_active_requests`, and `router_node_health{node}` (`1` healthy, `0`
unhealthy, `-1` unknown). A request for an unconfigured route is counted under
`route="_unknown"`, so clients cannot add labels by inventing model names.

## Limits of this version

- **Lightagent with several routes and `model = "default"`.** When `/v1/models`
  lists more than one model, Lightagent asks for an explicit model and does not
  send the literal `default`. Configure the route name, as with any gateway that
  lists several models.
- **Rewritten frames reorder JSON keys.** A frame whose `model` is rewritten is
  re-serialized, and its keys come out in sorted order. JSON gives key order no
  meaning, and the openai SDK and Lightagent both parse it unchanged.
- **The node control plane is not proxied.** The router does not serve
  `/api/v1/gateway` or `/api/v1/models`. Lightagent already treats them as
  optional, so its runtime panel shows nothing about the engine behind a router.
- **Nodes do not log the request id.** The router sends `X-Request-Id` and logs
  it, but gateways do not log it yet. Propagating it there is node-side work.
- **No latency or TTFT histograms yet.** The log lines already carry the timing
  data those metrics would be built from.

## Roadmap

None of this is built yet. Each step builds on the types above without
changing the public route identity.

| Milestone | Scope |
|---|---|
| **R4** | More `RoutePolicy` variants: round-robin, weighted, least-busy (fed by node concurrency from the probe). |
| **R5** | Capability-aware filtering: skip a deployment that cannot honour the request's features (tools, reasoning), and choose the deployment by context length. |
| **R6** | Session affinity keyed by a client-supplied conversation id. Latency and TTFT histograms. Request ids in node logs. |
| **R7** | Placement control: a control plane that asks nodes to load models and keeps warm standbys. It never runs in the request path. |
| **R8** | A rule-based `Auto` route that maps request traits to routes. |
| **R9** | A learned or adaptive router, and mixture-of-agents integration. |

Out of scope for every one of these: a request-path model load, splicing one
node's output into another's stream, and a dependency on distributed consensus.
