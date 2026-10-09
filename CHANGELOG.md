# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

## [0.8.0] - 2026-10-09

This release adds **Jev Settings** to the router panel. An operator can now
switch the classifier to Jev and give it a TypeSafe key from the browser
instead of editing `router.json` and exporting a variable. It also moves
the Flatpak to the supported Freedesktop 25.08 runtime.

**Jev Settings.**
- **Panel.** The Classifier screen sets the provider, the TypeSafe endpoint,
  model, timeout, confidence threshold and API key. It shows **Pending
  Restart** until the router restarts, and **Test Connection** checks the
  running settings.
- **Credential store.** The key is kept only in the operating system's
  credential store: the macOS Keychain, Windows Credential Manager or the
  Linux Secret Service. There is no file fallback, and `TYPESAFE_API_KEY` in
  the environment still wins.
- **Protected writes.** Saving needs a per-start admin token, separate from
  the client key. A save is accepted only on a loopback-only router, with a
  loopback `Host`, a matching `Origin`, a JSON body of at most 16 KiB and the
  current revision in `If-Match`.
- **`hermes router admin-token`.** It prints that token for the account that
  started the router. The token is kept in that account's own data
  directory, never beside `router.json`.

**Validated against the real Jev service.** The protected
`jev-live-validation` workflow ran on `fc0919d` (run 37913409376) with an
operator-held key. Jev classified all four live Auto requests as `chosen`:
two coding requests to Coder and two general ones to General, with model
`jev-latest`, confidence 1.0 and 137–150 ms per classification. The panel's
Test Connection reported Connected, and the key appeared in no log, page,
storage area or response.

**Compatibility and upgrading.** Nothing to change. A router configured
through `TYPESAFE_API_KEY` behaves exactly as in v0.7.0. Routing, Auto rules,
the classifier fallback, the shared request budget, metrics and traces are
unchanged.

**Known limits:**
- **macOS Keychain.** A Keychain item is tied to the program that wrote it,
  so a new `hermes` binary (after an update) may ask once to allow access.
- **Flatpak.** The Flatpak build has no access to the Secret Service and
  cannot save a key; set `TYPESAFE_API_KEY` in its environment instead. The
  same applies to a headless account with no session bus.
- **Remote routers.** A router with any listener off loopback has no admin
  token and refuses every write. Configure a remote router with its file and
  environment on that machine.
- **Restart required.** There is no hot reload. Saved settings take effect
  at the next router restart, and the panel shows Pending Restart until then.
- **Candidate routes.** Jev Settings changes only the provider and the Jev
  block. Candidate routes, route descriptions and include-user-text are still
  changed by copying the panel's snippet into `router.json`, checking it with
  `hermes router validate-config` and restarting.

**Not in this release:**
- No R9.4: no mixture-of-agents, parallel route execution, expert voting,
  synthesis, speculative routing or answer-quality scoring.
- No multi-user accounts or per-user settings. The admin token belongs to
  the account that runs the router.
- No hot reload, remote administration, or file-based key storage.

### Added

- **Jev Settings in the router panel.** An operator can set the classifier
  provider, the TypeSafe endpoint, model, timeout, confidence threshold and
  API key from the Classifier screen, save, restart the router, and test the
  connection, without editing `router.json` or exporting a variable. The key
  goes only to the operating system's credential store (macOS Keychain,
  Windows Credential Manager, Linux Secret Service); there is no file
  fallback, and `TYPESAFE_API_KEY` in the environment still wins. Saving
  needs a per-start admin token (`hermes router admin-token`), separate from
  the client key, and is refused on any router listening off loopback, from
  another origin or host, or against a stale revision. Only
  `auto_route.classifier` changes; the rest of the file keeps its values and
  order, the previous file is kept as `<config>.bak`, and a failed write puts
  the previous key back. Changes apply at the next restart (shown as Pending
  Restart); Test Connection checks the running settings. See
  [docs/ROUTER.md](docs/ROUTER.md#jev-settings-saving-the-classifier-provider-from-the-panel).
- `hermes router admin-token`, `GET`/`PUT /api/router/v1/classifier/settings`
  and `DELETE /api/router/v1/classifier/key`. `/api/router/v1/auto` gains
  `jev.api_key_source`.

### Changed

- **Flatpak: built on the supported Freedesktop 25.08 runtime.** The Flatpak
  moves from `org.freedesktop.Platform` 24.08, which Flathub marks
  end-of-life and which no longer receives security fixes, to 25.08 with the
  matching `org.electronjs.Electron2.BaseApp` 25.08 (#61). The application
  id, sandbox permissions and launch behaviour are unchanged. CI and the
  release now take the runtime from `apps/desktop/package.json`, and fail if
  Flathub marks the runtime, an extension or the installed bundle's runtime
  end-of-life.

## [0.7.0] - 2026-10-08

This release gives the router a **shared pre-commit request budget
(R9.3.2)** and makes the router panel's request traces **say only what
actually happened**.

**Shared request budget.** An optional `request.pre_commit_budget_ms` gives
each client request **one** deadline, shared by classification, routing,
same-route failover and cross-route fallback. No stage gets a fresh budget,
and no new attempt starts once it is spent.
- **Pre-commit only.** It covers the time until the response starts. A
  non-streamed response starts only when it is complete, so there it bounds
  the whole generation. A stream that has started continues exactly as
  before. This is not a whole-request lifetime timeout.
- **Errors.** When the budget is what ended the request, the client gets
  `504` with code `request_budget_exhausted`. A route error that had
  already happened is returned unchanged, and other timeouts and server
  errors are not reported as budget exhaustion.
- **Routing safety.** A budget cut is neutral to route history and node
  health and is never a routing input. Explicit routes get the budget but
  still never fall back to another route.
- **Observability.** Each request is counted once in `router_requests_total`,
  with its own `request_budget_exhausted` outcome. There is also a
  `request_budget` trace block, `router_request_budget_*` metrics and
  `GET /api/router/v1/request-budget`.

**Accurate request traces in the panel.**
- `Served by <route>` now appears only when the request actually
  succeeded.
- A request the budget ended reads `Budget expired …`, and one that ran out
  of context reads `Context limit exceeded while attempting <route>`.
  Any other unsuccessful ending reads `Request ended …`.
- A route step is green only when it served a successful response.

**Compatibility and upgrading.**
- Without `request.pre_commit_budget_ms` the router behaves exactly as in
  v0.6.0, and no budget metrics are emitted.
- Valid values are 1 000 – 3 600 000; `0` is refused.
  `hermes router validate-config` reports the configured budget and warns
  when a classifier's `timeout_ms` is not below it.
- Traces from a v0.6.0 router, which have no budget block, display as
  before. Successful requests, completed streams included, still read
  `Served by <route>`.

**Not in this release:**
- No R9.4: no mixture-of-agents, parallel route execution, expert voting,
  synthesis, speculative routing or answer-quality scoring.
- No post-commit stream deadline, whole-request lifetime timeout or
  client-supplied deadline. The deadline is not forwarded to nodes.
- No panel screen for configuring the budget; it is set in `router.json`.
- No explicit-route cross-route fallback, transitive fallback or scoring
  of route history from budget outcomes.
- Known panel limits, unchanged: the recent cross-route fallbacks card lists
  only requests that changed route, and same-route attempt details are plain
  text.

### Added

- **Router: shared pre-commit request budget (R9.3.2 slice 1).** An optional
  `request.pre_commit_budget_ms` (1 000 – 3 600 000; absent = off, exactly as
  before; `0` refused) gives each client request **one** deadline, from the
  moment the router has its body until its response starts:
  - classification, scoring, every same-route deployment attempt and every
    cross-route fallback route share it; nothing gets a fresh budget;
  - no new attempt starts once it is spent, and every wait before the
    response starts — connect, response head (a node's queue included), the
    classifier — ends no later than the deadline. Existing timeouts stay and
    fire first when they are shorter;
  - applies to explicit routes and `Auto` alike; an explicit route still
    never falls back to another route;
  - only before the response starts. A non-streamed response starts only
    when it is complete, so for non-streamed requests it bounds the whole
    generation; a streamed one is bounded only until streaming starts;
  - when the budget is the cause, the request ends with `504` and code
    `request_budget_exhausted`, counted once in `router_requests_total` under
    its own outcome (never `server_error`). A route error that already
    happened is returned unchanged;
  - neutral to route history and node health; never a routing input;
  - a `request_budget` trace block, `router_request_budget_*` metrics (only
    while configured), and `GET /api/router/v1/request-budget`.

### Fixed

- **Router panel: request traces no longer show a request as served when
  it was not.** On the Auto Routing screen's recent cross-route fallbacks
  card, `Served by <route>` now requires the trace's `outcome` to be `ok`.
  Before, any request that had not exhausted its fallback list read as
  served, even when it had ended without a successful response:
  - a request the pre-commit budget ended reads `Budget expired before any
    route was attempted`, `… during classification`, `… while attempting
    <route>` or `… before attempting <route>`. The panel also notes that
    the client got `504 request_budget_exhausted`;
  - a request that ended because a route's context was too small reads
    `Context limit exceeded while attempting <route>`. The panel also notes
    that the client got that route's own `400 context_length_exceeded`;
  - any other unsuccessful ending (a 4xx or 500, an interrupted stream or a
    cancelled request) reads `Request ended while
    attempting <route>`, with its outcome and status;
  - `Exhausted` and its wording are unchanged.
- **Router panel: a route step's badge is green only when that step served
  a successful response.** The hop must have committed and the request's
  outcome must be `ok`. A route that answered with an error, or whose
  attempt was cut by the budget or refused for its context, now shows as a
  warning. The step's text is unchanged.

## [0.6.0] - 2026-10-07

This release completes the router's **adaptive orchestration stack**, up to
and including bounded cross-route recovery. It builds on v0.5.0's federated
routing foundation, which already included:
- multi-node routes;
- health-aware failover;
- capability filtering;
- the `priority`, `round_robin` and `least_busy` policies.

New in v0.6.0:
- **Observability and session affinity (R6).**
- **Placement and warm standby (R7).**
- **Rule-based `Auto` routing (R8).**
- **Content-aware classification (R9.1)**, through a provider-neutral
  classifier boundary with **Lightweight and TypeSafe Jev providers
  (R9.1a)**, plus the router panel's **Auto Routing and Classifier
  screens**.
- **Adaptive logical-route scoring (R9.2).** It uses the classifier signal
  and bounded operator priors. Route history is **observational only** and
  never steers routing.
- **Explicit cross-route fallback (R9.3.1)**, with its **panel cards**:
  - `Auto` requests only, after same-route failover and before the response
    commits;
  - on `route_unavailable`, `route_exhausted` and
    `route_capability_mismatch` only;
  - one flat, non-transitive list per initial route, with at most 3 fallback
    routes;
  - traces, counters and the admin view;
  - `response.model` names the final serving route, and
    `router_requests_total` counts each request once under its final route.

**Not in this release:**
- **R9.3.2 shared pre-commit request-budget enforcement is NOT part of
  v0.6.0.** Its design is frozen in `docs/R9_3_2_SHARED_REQUEST_BUDGET.md`,
  but there is no runtime support. There is no `request.pre_commit_budget_ms`
  setting, no `504 request_budget_exhausted`, and no
  `request_budget_exhausted` metric outcome.
- No client-supplied deadlines and no post-commit stream deadline.
- No R9.4 / mixture-of-agents.
- No transitive fallback graphs and no explicit-route cross-route fallback.
- No latency-based route selection and no route-history scoring.

### Added

- **Router panel: cross-route fallback.** Auto Routing gains three cards:
  - a summary: configured or not, Auto-only scope, at most 3 fallback routes,
    the three triggers and the exclusions, each list as a chain with the
    non-transitive rule stated, same-route versus cross-route, counts per
    transition and reason, exhausted lists, and the response-identity and
    `router_requests_total` meanings;
  - a validated draft of the lists, which produces the
    `cross_route_fallback` snippet to copy, then `validate-config` and a
    restart, with no write API;
  - recent fallbacks from the router's traces: initial route, then each
    attempt, then the final route, or *Exhausted*.

  This is read-only, uses the existing panel components, adds no new
  backend endpoint, and stores nothing in the browser.

- **Router explicit cross-route fallback (R9.3.1).** An optional
  `auto_route.cross_route_fallback` maps a route to an ordered list of at
  most 3 other routes. If the route an `Auto` request resolved to cannot
  execute, the next listed route is tried, before anything is committed and
  only after the route's own deployment failover is exhausted.
  - **Triggers:** `route_unavailable`, `route_exhausted` (every planned
    deployment refused 502/503/504 before answering) and
    `route_capability_mismatch`. Never a 500, `context_length_exceeded`,
    429/4xx, latency, quality, a post-commit stream failure, cancellation, a
    classifier failure or anything from R9.2.
  - **Scope:** `Auto` requests only, whether the route came from an R8 rule,
    R9.1, R9.2 or `fallback_route`. Explicit and `default` requests never
    fall back.
  - **Behaviour:** the initial route's list is read once and never
    transitive, with at most four route attempts. Each fallback route runs
    the normal pipeline with the original requirements (R5 never weakened)
    and its own affinity. There is no reclassification, re-scoring, history,
    latency or placement action.
  - **Responses:** `model` names the serving route; an exhausted list returns
    the final attempted route's own error; one request id throughout;
    `router_requests_total` counts once.
  - **Validation at load:** unknown, reserved, `Auto`, classifier, empty,
    duplicate and self-referencing entries, lists over 3, unreachable
    sources, and cycles anywhere in the lists.
  - **Observability:** a `cross_route_fallback` trace block, a `route` on
    each deployment attempt,
    `router_cross_route_fallback_total{from_route,to_route,reason}`,
    `router_cross_route_fallback_exhausted_total{route,reason}`, and
    `cross_route_fallback` in `GET /api/router/v1/auto`.
  - **Not in this slice:** no shared end-to-end deadline, no explicit-route
    opt-in, no UI.

- **Router adaptive logical-route scoring (R9.2, slice 1).** An optional
  `auto_route.adaptive_scoring` section, off by default. It ranks **logical
  routes only, never deployments**.
  - An accepted classification's verdict route is scored against the
    classifier's fallback route, whose classifier signal is the explicit
    `classifier_baseline` (the provider's `min_confidence`):
    `W_classifier·signal + W_prior·prior`, ties to the verdict. **Only
    classifier confidence and the configured route prior affect route
    selection.** Other candidates have no signal and are not scored.
  - A verdict below `min_confidence` stays rejected: R9.1's fallback is the
    decision and the rejected route can never be revived. Explicit routes,
    deterministic rules and `Auto`'s plain fallback are never scored.
  - Validation keeps the influence radius `prior / classifier` under half
    the accepted range for every configured provider, so a confident
    classification is never overturned. There is one set of weights and no
    provider-specific weights. The neutral defaults reproduce R9.1 exactly.
  - **Route history is collected but observational only.** Its
    observations measure successful traffic volume, not validated route
    quality, so `weights.history` must be 0; any other value is refused. Per
    route, in memory only, recorded at each request's final outcome (direct
    and `Auto` traffic; never the router's own classification requests):
    decayed successes (`effective_samples`; half-life default 1 h,
    provisional), plus counters for `server_error`, `interrupted`,
    `unavailable`, `mismatch` and `neutral`. History becomes eligible for
    scoring only with a genuinely route-attributable quality signal.
  - A `scoring` trace block (route names and numbers;
    `history_active: false`), `adaptive_scoring` in
    `GET /api/router/v1/auto` (`history_mode: "observational"`,
    `history_affects_scoring: false`), an admin-only
    `POST /api/router/v1/adaptive-scoring/reset` (all routes or one; touches
    history only), and the metrics
    `router_route_scoring_decisions_total{route,overrode}`,
    `router_route_scoring_fallback_total{reason}`,
    `router_route_history_observations_total{route,outcome}` and
    `router_route_history_effective_samples{route}`.
  - Not in this slice: history scoring, latency or context-fit scoring, Jev
    per-option probabilities, availability penalties, cross-route fallback
    (R9.3), persistence, and UI.

- **Router panel: Auto Routing and Classifier screens.** `hermes router
  --web-root <dir>` serves the control panel from the router's own origin
  (no CORS); the panel detects a router via `GET /version` and shows its own
  sections, leaving a gateway's panel unchanged.
  - Auto Routing: rules in order, each reading *Route directly to …* or
    *Semantic classification*, with decision counts; logical routes with
    descriptions and availability.
  - Classifier: provider status (active provider, API key *Configured* /
    *Missing* by variable name only, last check / success / failure and kind,
    outcomes) and **Test Connection** (`POST /api/router/v1/classifier/check`)
    with every status in plain words; `model_not_listed` is a warning that a
    pinned version may still be accepted, not "invalid model".
  - A settings draft with a Lightweight / Jev provider selector and only the
    chosen provider's fields, the Jev privacy notice, include-user-text with
    what each setting sends, candidates, route descriptions and the fallback
    route — checked by the router's own rules and turned into the canonical
    `auto_route.classifier` section to paste (the router has no config write
    API, so nothing is saved from the panel). The TypeSafe key never reaches
    the browser.
  - `GET /api/router/v1/routes` now includes each route's `description`.

- **Router classifier providers and TypeSafe Jev (R9.1a).** The classifier an
  `Auto` rule invokes is now chosen by `classifier.provider`: `lightweight`
  (the default — a configured route, as in R9.1) or `jev`, TypeSafe AI's
  System One API.
  - Jev is asked `POST /v1/systemone` with one typed Choice question whose
    options are exactly the candidate routes and their descriptions; its own
    `confidence` is thresholded; a choice that is not a candidate is invalid.
    It is not a node or deployment and is never reachable through the router.
  - `classifier.jev {base_url, api_key_env, model, timeout_ms, min_confidence,
    max_input_chars, include_user_text}`: https (loopback excepted), the key
    from the environment (`TYPESAFE_API_KEY` by default) and demanded only when
    Jev is active, `model` and `timeout_ms` required.
  - Auth (`401`/`403`), rate limit (`429`/`529`), other errors, connection
    failures, timeouts and invalid answers all fall back to the classifier's
    fallback route, without retries; error bodies are never kept.
  - Each provider has its own settings block; R9.1's flat keys still read as
    the `lightweight` block. Switching provider changes no rule.
  - Classifier metrics gain a `provider` label; traces gain `provider` and
    `model`; `/api/router/v1/auto` shows both providers' settings (never a key,
    only `api_key_configured`) and a status; a start-up check and
    `POST /api/router/v1/classifier/check` call `GET /v1/models`.

- **Router content-aware `Auto` classification (R9.1).** Opt-in twice over:
  an `auto_route.classifier` section, and a rule that says `"classify": true`
  instead of naming a `route`. Without both, `Auto` is exactly R8.
  - The classifier is a configured logical route, called through the router's
    own pipeline as a nested request (`<request id>-classify`). It may
    recommend only the configured candidate `routes`; an answer naming
    anything else — a node, `Auto`, a non-candidate route — is invalid.
  - The classifier's `timeout_ms` is required whenever a classifier section
    exists: a real CPU classifier took 13–44 s, so no default fits.
  - It is sent the candidates, optional `routes[].description`s, the request's
    structural traits and the last user message cut to `max_input_chars`
    (default 2000) — no history, system prompt or tool schemas.
  - Below `min_confidence` (default 0.65), past `timeout_ms` (required, 1–120 000 ms: no single default suits CPU, GPU and remote classifiers),
    unavailable, or invalid, the request goes to the classifier's
    `fallback_route` (default: Auto's). `Auto` never fails because the
    classifier did, and classification can never recurse.
  - Only the logical route is chosen: capability filtering, route-scoped
    affinity, policies, failover, placement and the response's `model` are the
    chosen route's, unchanged. No cross-route fallback.
  - Classifier time is measured on its own and excluded from `routing_ms`.
    Traces gain a `classifier` object (outcome, chosen route, confidence,
    duration — never text); `router_classifier_requests_total{outcome}`,
    `router_classifier_route_total{route}`,
    `router_classifier_duration_seconds{outcome}`; `/api/router/v1/auto` shows
    the classifier and its candidates.
  - Not scoring, learning, latency routing or orchestration (R9.2–R9.4).

- **Router rule-based `Auto` model (R8).** Off unless the configuration has an
  `auto_route` section with `"enabled": true`; without one, `Auto` is an
  unknown model exactly as before.
  - A request with `"model": "Auto"` has its logical route chosen by ordered
    rules over its structure, read by the same extractor capability filtering
    uses: `endpoint` (`chat` / `completion`), `requires_tools`, `tool_choice`,
    `requires_reasoning`, and `min_prompt_tokens` / `max_prompt_tokens` on the
    router's prompt estimate. Conditions in a rule are ANDed, `false` means
    the trait is absent, the first matching rule wins, and no match goes to
    the required `fallback_route`.
  - Only the route is chosen. Health, capability filtering, session affinity
    (keyed by the resolved route), priority / round-robin / least-busy,
    failover and context-overflow fallback run inside it unchanged. If the
    chosen route cannot serve the request, its own error is returned; no
    other route is tried, nothing waits for or triggers a model load.
  - The response's `model` — whole bodies, every stream chunk, tool answers —
    is the route that answered, never a node-local name.
  - `/v1/models` lists `Auto` (with no context of its own);
    `/v1/capabilities` describes it under `auto` by the routes it can resolve
    to. Routing traces and log lines carry `requested_route`, `auto_rule` and
    `auto_fallback`; `router_auto_route_decisions_total{rule,route}`,
    `router_auto_route_fallback_total{route}`; and `GET /api/router/v1/auto`
    shows the rules in order with their decision counts.
  - Startup refuses unknown or reserved targets, `Auto` targeting itself, a
    route named `Auto`, duplicate or label-unsafe rule names, rules with no
    conditions, zero thresholds, rules no request could match, and more than
    64 rules.
  - Not learned routing: no prompt-content classification, scores, history,
    latency or cost.

- **Router placement and warm standby (R7).** Off unless a route has a
  `placement` target.
  - A route declares `min_ready`, `warm_standby` and the `allowed_nodes` it may
    be loaded on. A controller beside the router — never in a request — keeps
    the route at `min_ready + warm_standby` ready deployments by asking empty,
    healthy, allowed nodes to load the model they already have installed,
    through each node's own control API and credential.
  - A deployment counts as ready only once the router's own probe sees it
    serving, by the same rule requests use. The node's admission control
    decides every load; a refusal is recorded (`admission_failed`, with the
    node's code) and backed off, doubling to a configured maximum.
  - Nothing is downloaded, swapped, unloaded or rebalanced; latency and
    traffic play no part. A request for a route with nothing ready is refused
    `route_unavailable` immediately, even while a load is in progress.
  - `GET /api/router/v1/placement`, `POST /api/router/v1/placement/reconcile`,
    and per-route ready/target/loading gauges, action and failure counters and
    a reconcile-duration histogram.

- **Router session affinity (R6).** Off unless `session_affinity.enabled` is
  set. A client names a session in `X-Lightweight-Session` (configurable);
  the router never infers one from an address, a key or the prompt.
  - The session's last successful deployment is preferred **only while it is
    still healthy and able to serve the request**. Otherwise the route's
    policy chooses exactly as it would without a session, and the session
    moves to wherever the request succeeds (`sticky_unhealthy`,
    `sticky_unavailable`, `sticky_capability_mismatch`,
    `sticky_context_overflow`, `sticky_failed`). A recovered deployment does
    not pull sessions back.
  - A hit takes no policy turn: round-robin draws no cursor value, and
    least-busy keeps the session even when another deployment is idler.
  - Keyed by route and a keyed hash of the id, never the id itself. Memory
    only, with an idle TTL (default 30 minutes) and an entry limit (default
    10 000); a restart forgets every affinity. The first of two simultaneous
    first requests to commit wins.
  - `GET /api/router/v1/sessions`, and hit, miss, reassignment, entry and
    eviction metrics labelled by route and reason only.
- **Router observability (R6).** Measured only; no routing decision reads it.
  - Histograms for time to first token (from the router having the request
    to the first relayed content, reasoning or tool-call delta; streams only),
    upstream time to first token, request duration, the router's own planning
    time, each attempt's time to a response head, and the committed upstream
    body.
  - The router's prompt-token estimate compared with the node's
    `usage.prompt_tokens` when one is reported: `actual / estimated` ratio and
    signed error histograms.
  - One `RoutingTrace` per request — availability, filtering, affinity, every
    attempt, the final deployment, timings, status — in a bounded in-memory
    ring at `GET /api/router/v1/traces` (`traces.capacity`, default 200). A
    closing `request finished` log line carries the same summary.
- **Request ids reach the node's log (R6).** A gateway now reads
  `X-Request-Id`, writes it on every log line about the request — including a
  new closing `request finished` line with outcome, timings and token counts
  — and echoes it on the response. Every failover attempt carries the same
  id. A gateway never invents one; without it, logging is as before. No
  prompt text is logged.

## [0.5.0] - 2026-10-05

### Added

- **Router load balancing (R4).** Each route chooses its `strategy`:
  `priority` (the default, unchanged), `round_robin`, or `least_busy`.
  - **Round-robin** takes equal turns over the deployments that are currently
    eligible, in configured order. The per-route cursor advances once per
    request, so failover attempts do not move it.
  - **Least-busy** picks the eligible deployment with the lowest
    `in-flight / advertised concurrency limit`, compared exactly, with ties
    going to configured order. A deployment whose capacity is unknown or zero is
    used only as a fallback.
  - Health eligibility is shared by every policy. The router keeps its own
    in-flight count per deployment, and it is released on every exit path,
    client disconnects included.
  - The admin deployments view shows `active_requests` and `concurrency_limit`.
    Logs carry the policy and how the choice was made. `/metrics` gains decision
    counts by policy and a per-deployment in-flight gauge.
  - An unknown `strategy` is refused at startup. No latency, weights, session
    affinity or capability filtering is involved.
- **Router capability filtering (R5).** Before a route's policy chooses, the
  router drops every available deployment that cannot serve the request in
  hand, using that deployment's own last-probed capabilities.
  - It checks the endpoint (chat or text completion), declared `tools`, a
    `tool_choice` that must be honoured (`required`, a named function, or
    `none` beside tools), a `reasoning_effort` that asks for reasoning, and
    whether the prompt fits the deployment's context.
  - The context rule is the node's own: a prompt must leave room to generate,
    and `max_tokens` is clamped rather than required. The router counts a
    lower bound on prompt tokens, so it never refuses a request a node could
    serve.
  - Priority, round-robin and least-busy are unchanged and see only capable
    deployments. Failover never reaches a deployment that was filtered out.
  - When available deployments exist but none can serve the request, the
    answer is `400 route_capability_mismatch`. It names the route and what was
    missing, never a node. That is distinct from `model_not_found` and
    `route_unavailable`. A request the gateway would refuse as malformed gets
    the gateway's own `400` from the router.
  - A node's `400 context_length_exceeded`, which the router's lower-bound
    estimate could not foresee, moves the request, before anything is sent,
    to a deployment in the plan with a strictly larger context. If there is
    none, the node's error is returned unchanged. Every other `400`, and every
    `500`, still stands.
  - Logs carry each request's requirements and candidate counts before and
    after filtering. `/metrics` gains `router_capability_filtered_total`,
    `router_capability_mismatch_total` and
    `router_context_overflow_failovers_total`. Existing router configurations work
    unchanged.
- **User-defined model aliases.** Give any installed model a short name of your
  choosing (`Coder`, `Fast`, …) when adding it or at any time later, from the
  panel's Models screen, `hermes models alias`, or
  `PATCH /api/v1/models/{id}` (`{"alias": null}` clears it). Renaming applies
  at once without a reload. Aliases persist in the catalog, are never derived
  from the file, and existing catalogs load unchanged with no alias set.
- **Aliases are the public model ids.** `/v1/models`, `/v1/capabilities` and
  every chat and text completion response, streamed chunks included, name an
  aliased model by its alias; an unaliased model is listed by its canonical id
  as before. The alias is a stable name with no `@context` suffix.
- **Canonical ids remain supported.** Requests may name a model by its alias
  (any casing), its canonical id, `default`, or not at all, and the control API
  reports both `id` and `alias`. Aliases and canonical ids share one namespace:
  an alias may not equal any model id, `default` is reserved, duplicates are
  refused rather than renamed, and a pinned or linked model whose id is already
  an alias is refused before it downloads.
- **Lightagent compatibility.** Lightagent discovers the alias from
  `/v1/models` and sends it back as `model` with no Lightagent change; verified
  against a real engine.
- `hermes models alias` changes an alias through a running gateway that serves
  the same profile, rather than editing the catalog file under it.
- **Federated model router (`hermes router`).** A new, separately run
  `lightweight-router` crate puts one OpenAI-compatible endpoint in front of
  several Lightweight gateways.
  - Clients discover and send logical route names (`Coder`, `Fast`).
  - Each route lists node deployments in priority order. The router picks the
    first healthy one, rewrites `model` to that node's own alias, and rewrites
    the response back to the route name, in streamed chunks too. Streams are
    relayed frame by frame, and a client disconnect cancels the node's
    generation.
  - Failover happens only before any response has started: on a refused
    connection, a 502/503/504, or a node that stopped serving the model. A
    stream that fails mid-way ends with an error frame and no `[DONE]`.
  - Unknown routes are `model_not_found`. A known route with nothing healthy is
    `route_unavailable`. `default` resolves only to an explicitly configured
    `default_route`.
  - Node health comes from probing each node's `/v1/capabilities`, with a
    failure threshold.
  - Client and node credentials are separate, and every key is read from an
    environment variable.
  - Read-only `/api/router/v1/{nodes,routes,deployments,health}`, Prometheus
    `/metrics`, and `hermes router validate-config`.
  - `hermes serve` is unchanged. See `docs/ROUTER.md`.

### Fixed

- **`model: "default"` is accepted again.** It was dropped from the gateway in
  the Lightagent split; it once more selects whichever model is resident, on
  both `/v1/chat/completions` and `/v1/completions`.
- **`/v1/capabilities` reports the live concurrency limit.**
  `limits.max_concurrent_requests` was the slot count the gateway started with.
  It is now the scheduler's current count, so it follows a model load that
  resizes it, and the router's least-busy policy picks it up on its next probe.

## [0.4.1] - 2026-09-30

### Added

- **Optional Open WebUI workbench.** A separately deployable companion stack
  documents authenticated multi-gateway routing, private document extraction and
  retrieval, reranking, web search, and an administrator-only terminal profile.
- **Optional Jev model advice.** The `Lightweight Auto` Open WebUI Pipe can
  advise among explicitly allowlisted, prefixed Lightweight routes, with a
  deterministic local fallback for every timeout, invalid answer, or low-confidence
  response.

### Fixed

- The locally served Chat panel now uses its authenticated internal control route
  when a gateway key is configured. Public `/v1` clients still require a Bearer
  credential, including attempts to forge the local-control marker.

## [0.4.0] - 2026-09-29

### Added

- **Public provider capability discovery.** Authenticated clients can now use
  `GET /v1/capabilities` to discover the versioned public inference contract,
  supported OpenAI-compatible endpoints and features, model readiness, served
  context length, and concurrency limit. The endpoint shares `/v1`
  authentication, never loads a model, and intentionally excludes control-plane
  routes, filesystem paths, hardware details, engine internals, jobs, and keys.

### Changed

- **Deterministic paired-provider test support.** The test-only mock gateway can
  consume a startup-provided queue of scripted completions, enabling a
  separate-process public HTTP/SSE integration test without exposing a runtime
  test control endpoint.

## [0.2.4] - 2026-09-18

### Fixed

- The control panel again shows its values when a static gateway API key is
  configured. Its status endpoints — `/api/v1/gateway`, `/api/v1/metrics`, the
  `/api/v1/events` live feed, `/api/v1/system`, and `/api/v1/requests` — were
  being refused on loopback, so the panel (which cannot carry the key, and whose
  `EventSource` cannot send an `Authorization` header at all) was shut out of its
  own status surface. The entire `/api/v1` control surface is now admitted on
  loopback like the rest of the panel; remote requests to it remain
  key-protected, and state-changing routes keep their cross-origin guard.

## [0.2.2] - 2026-09-18

## [0.2.3] - 2026-09-18

### Fixed

- The locally served control panel now continues to reach its own management
  API when a static gateway API key is configured, without exposing that key to
  the browser. Remote API access remains key-protected.

### Changed

- Lightweight is again an exclusive local inference engine. The Lightagent
  runtime, CLI, web interface, gateway proxy, packaging, extensions, and
  release artifacts have been removed. Lightweight continues to provide its
  OpenAI-compatible API, GGUF model management, native inference CLI, and
  inference control panel.

## [0.2.1] - 2026-09-01

Public reach and multi-model serving. The gateway can now sit behind a trusted
reverse proxy or Cloudflare Tunnel with `--behind-proxy` — reachable at a real
domain, key-required, and no longer fooled into treating a remote caller as
local — and `hermes fleet` runs up to four models at once as isolated
per-tenant gateways. Per-user API keys and their rate limits now take effect the
moment they change instead of at the next restart, and the desktop icons are
rounded to the macOS squircle with a new violet-feather menu-bar mark.

### Added

- **`--behind-proxy` mode** for putting the gateway behind a trusted reverse
  proxy or Cloudflare Tunnel while it stays bound to loopback. It turns on
  API-key auth (refusing to start without a credential) and trusts the proxy's
  `CF-Connecting-IP` header — only from a loopback peer — so a remote caller is
  identified by its real address rather than passing as local. Set it with the
  flag or `HERMES_BEHIND_PROXY`. A plain loopback gateway is unchanged.
- **`hermes fleet`** runs up to four models at once, one isolated gateway per
  model. Each entry in a small JSON manifest gets its own data root, port and
  keys, so one tenant's traffic can never evict or disturb another's. The
  four-model cap and the manifest checks (duplicate ports/names, missing model
  files, a profile with no key) are enforced before anything launches.
- **A public-domain recipe** in the README: reaching the gateway at
  `https://…/v1` over a Cloudflare Tunnel with `--behind-proxy`, and serving
  several models behind per-hostname routing with `hermes fleet`.

### Changed

- **Per-user API keys and limits now take effect live.** Creating a key,
  changing its rate limit, or revoking it through the control API is honoured on
  the next request instead of at the next restart — the gateway reloads its key
  set from the store on each change. A revoked key stops working immediately.
- **The menu-bar (tray) icon** is now its own transparent mark, keyed from a
  dedicated `icon/tray-source.png`, rather than the plated brand icon.
- **The desktop app icons are rounded** to the macOS "squircle" with a
  transparent margin, so the app sits on the dock like a native one instead of a
  hard-edged square. Generated for every packaged size by `scripts/build-icons.py`.

### Fixed

- **An engine launch that loses the ephemeral-port race is retried.** The
  supervisor hands the engine a loopback port it proved free a moment earlier;
  on a busy machine another process can take it in the gap before the engine
  binds. Such a launch is now retried with a fresh port instead of surfacing the
  crash, as the design always intended. Only that transient case is retried — a
  signal, a timeout, or a genuinely unstartable engine is still reported at once.

## [0.2.0] - 2026-08-31

Remote access: the gateway can now be reached from another machine over any
overlay network, authenticated with named API keys that survive a restart and
can be rate-limited per key. A new **Access & Keys** panel and the `hermes key`
/ `hermes config` commands manage it, and the bind hosts and port persist in
`config/api.json`. The default port moves to **11434** to agree with the desktop
app and the common local-LLM clients — a behaviour change for anyone who relied
on the old `8737`.

### Added

- **Named, hashed API keys.** A gateway can now issue a key per consumer, each
  nameable and revocable on its own. Keys are stored as SHA-256 hashes and a
  display prefix in `config/api-keys.json`; the plaintext is shown once, at
  creation, and never again. Create, list and revoke them with `hermes key`, or
  on the panel's new **Access & Keys** screen. The existing `--api-key` /
  `HERMES_API_KEY` static key still works alongside them.
- **Per-key rate limits.** Each key can carry a per-minute and a per-day
  ceiling, enforced live: a key over its limit gets a `429` with a `Retry-After`.
  Loopback and anonymous callers (the panel, a local script) are never metered.
- **Persisted bind configuration** in `config/api.json` — the hosts and port the
  gateway serves on, read beneath the command-line flags so a typed `--host` or
  `--port` always wins. Edit it with `hermes config`, or the panel's *Serve on*
  control, which lists the machine's reachable addresses tagged with the reserved
  range each falls in (a Tailscale/CGNAT address reads *shared range*).
- **The `lightweight` command**, a second entry point identical to `hermes` that
  prints a feather welcome mark on an interactive terminal. `NO_COLOR` and
  `LIGHTWEIGHT_NO_BANNER` are honoured; `--json` and pipes are never decorated.
- `hermes sysinfo` reports an address's reserved-range scope, in the human output
  and as an `addresses` array under `--json`.
- **`hermes serve --port auto`** (equivalently `--port 0`) binds a kernel-assigned
  free port and prints it — the explicit way past a taken 11434 without moving the
  default. With several `--host` values it binds them all to the one shared port.
  It is a per-run choice and is never written to `api.json`.

### Changed

- **The `address in use` message on the default port is now a signpost.** Because
  11434 is also Ollama's default, `hermes serve` names that likely cause and
  suggests `--port auto`, a different `--port`, or stopping the other process,
  rather than failing with a bare "in use". The desktop shell inherits the same
  guidance and points at its own levers (`HERMES_PORT` or the *Serve on* control).

- **The default port is now 11434** (was 8737), so the CLI, the desktop shell and
  the dev proxy agree and a client assuming the common local-LLM port finds the
  gateway. Anyone who relied on the old default must now pass `--port 8737`.
- The desktop shell no longer mints a fresh API key on every launch — the bug
  that broke a key shared with a remote agent. Keys are the gateway's own, and the
  tray's "Copy API key" is now "Manage API keys…", which opens the panel.
- State-changing control endpoints under `/api/v1` now refuse a request from a
  foreign origin, and creating keys or widening the bind set is refused from a
  non-loopback peer: those take access to the machine running the gateway.

## [0.1.2] - 2026-08-30

### Added

- CPU utilization is now reported on macOS and Windows, so the Performance
  page's CPU Usage tile shows a live figure on every supported platform instead
  of only on Linux. It is read through `host_statistics` on macOS and
  `GetSystemTimes` on Windows, and normalised to the same tick units the panel
  already differences.

### Changed

- Completed the rename to **Lightweight**: the native desktop application — the
  window title, tray, menus, dialogs, and the installer and artifact names —
  now reads "Lightweight" instead of "Hermes", following the panel rename in
  0.1.1.
- Renamed the internal workspace crates from `hermes-*` to `lightweight-*`. The
  `hermes` command and its `HERMES_*` environment variables, the `hermes_*`
  metric names, the `hermes::` log targets, and the existing data directory are
  deliberately unchanged, so nothing that scripts, scrapers, or existing
  installs depend on has moved.

### Fixed

- Long file-path values no longer overflow their cards on the Settings and API
  Gateway pages; they wrap within the card instead.

## [0.1.1] - 2026-08-26

### Changed

- Renamed the desktop shell to **Lightweight**: the sidebar brand name and the
  window title bar now read "Lightweight" instead of "Hermes".

## [0.1.0] - 2026-08-25

### Added

- An OpenAI-compatible, CPU-only inference gateway backed by a supervised
  llama.cpp process.
- GGUF model discovery, verified downloads, imports, and live model switching.
- Conservative RAM admission control with context and KV-cache sizing.
- Benchmarking and machine-scoped calibration with trust checks that reject
  unsafe fits.
- A desktop UI and CLI packages for macOS, Windows, and Linux.

### Known limitations

- Calibration is intentionally deferred for pinned llama.cpp `b10590`:
  `hermes bench --fit` safely refuses every honest fit, so the shipped estimates
  remain conservative by 1.37×–2.85×.

[Unreleased]: https://github.com/dlroqa/Lightweight/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/dlroqa/Lightweight/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/dlroqa/Lightweight/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/dlroqa/Lightweight/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/dlroqa/Lightweight/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/dlroqa/Lightweight/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dlroqa/Lightweight/compare/v0.2.4...v0.4.0
[0.2.4]: https://github.com/dlroqa/Lightweight/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/dlroqa/Lightweight/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/dlroqa/Lightweight/compare/v0.2.1...v0.2.2
[0.1.2]: https://github.com/dlroqa/Lightweight/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/dlroqa/Lightweight/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/dlroqa/Lightweight/releases/tag/v0.1.0
