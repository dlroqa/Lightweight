#!/usr/bin/env bash
# Render the panel end to end and assert the control-panel screens work.
#
# The panel is served by the inference gateway (`lightweight serve`), which
# answers the control API under `/api/v1` and serves the panel bundle. This
# script brings up that one server wired exactly as the product wires it,
# drives a real headless browser over every control-panel screen, and fails if
# a screen falls to the SPA fallback or throws an uncaught exception.
#
# Kept as a script, not inlined into the workflow, so it is identical locally
# and in CI. Run it from a checkout with the frontend deps installed:
#
#   npm ci --prefix frontend && npm ci --prefix e2e
#   npx --prefix e2e playwright install --with-deps chromium
#   scripts/render-panel.sh
#
# It then does the same for the panel as a router serves it
# (`lightweight router --web-root`), with the classifier on Jev pointed at a
# scripted TypeSafe endpoint (`e2e/mock-jev.mjs`), so Test Connection runs
# through the real router without ever calling the real API. Two scripted
# nodes (`e2e/mock-node.mjs`) start empty and are loaded mid-render, so the
# router serves a real cross-route fallback that succeeds as well as one that
# exhausts its list.
#
# Environment:
#   GATEWAY_PORT  gateway/panel port        (default 11434)
#   ROUTER_PORT   router/panel port         (default 11500)
#   JEV_PORT      scripted TypeSafe port    (default 11501)
#   NODE_PORT     first scripted node port  (default 11502; the second is +1;
#                 +2 is a second router with a pre-commit request budget, and
#                 +3/+4 its two scripted nodes; +5 is a router whose
#                 General overflows its context, +6 that General; +7 is a
#                 router whose General streams, +8 that General; +9 is a
#                 router whose General answers a plain 400, +10 that General;
#                 +11 is a router whose General answers 500, +12 that
#                 General. All reuse the budget router's Coder.)
#   OUT_DIR       where screenshots land    (default e2e/screens)
set -euo pipefail
cd "$(dirname "$0")/.."

GATEWAY_PORT="${GATEWAY_PORT:-11434}"
ROUTER_PORT="${ROUTER_PORT:-11500}"
JEV_PORT="${JEV_PORT:-11501}"
NODE_PORT="${NODE_PORT:-11502}"
CODER_NODE_PORT="$NODE_PORT"
GENERAL_NODE_PORT="$((NODE_PORT + 1))"
# A second router with a pre-commit request budget (R9.3.2), and its own two
# scripted nodes, so the first router and its checks stay exactly as they are.
BUDGET_ROUTER_PORT="$((NODE_PORT + 2))"
BUDGET_CODER_PORT="$((NODE_PORT + 3))"
BUDGET_GENERAL_PORT="$((NODE_PORT + 4))"
# Two more routers sharing the budget router's refusing Coder: one whose
# General refuses the prompt as too long for its context, and one whose
# General streams past a pre-commit budget. The routers above are untouched.
OVERFLOW_ROUTER_PORT="$((NODE_PORT + 5))"
OVERFLOW_GENERAL_PORT="$((NODE_PORT + 6))"
STREAM_ROUTER_PORT="$((NODE_PORT + 7))"
STREAM_GENERAL_PORT="$((NODE_PORT + 8))"
# And two whose General commits an error that is neither a fallback reason nor
# a context overflow, so the chain is not exhausted and nothing was served.
CLIENT_ERROR_ROUTER_PORT="$((NODE_PORT + 9))"
CLIENT_ERROR_GENERAL_PORT="$((NODE_PORT + 10))"
SERVER_ERROR_ROUTER_PORT="$((NODE_PORT + 11))"
SERVER_ERROR_GENERAL_PORT="$((NODE_PORT + 12))"
OUT_DIR="${OUT_DIR:-e2e/screens}"

# Same rustup-env dance as check.sh: cargo is absent from a non-login PATH.
if ! command -v cargo >/dev/null 2>&1 && [ -f "${HOME:-}/.cargo/env" ]; then
  . "${HOME:-}/.cargo/env"
fi

# A scratch home so the render never reads or writes a developer's real data
# directory — and so a clean CI runner is configured from nothing rather than
# failing on a missing directory.
WORK="$(mktemp -d)"
export HERMES_GATEWAY_HOME="$WORK/gateway-home"
GATEWAY_LOG="$WORK/gateway.log"
GATEWAY_PID=""
ROUTER_LOG="$WORK/router.log"
ROUTER_PID=""
JEV_PID=""
NODES_PID=""
BUDGET_ROUTER_LOG="$WORK/budget-router.log"
BUDGET_ROUTER_PID=""
BUDGET_NODES_PID=""
OVERFLOW_ROUTER_LOG="$WORK/overflow-router.log"
STREAM_ROUTER_LOG="$WORK/stream-router.log"
OVERFLOW_ROUTER_PID=""
STREAM_ROUTER_PID=""
CLIENT_ERROR_ROUTER_LOG="$WORK/client-error-router.log"
SERVER_ERROR_ROUTER_LOG="$WORK/server-error-router.log"
CLIENT_ERROR_ROUTER_PID=""
SERVER_ERROR_ROUTER_PID=""
TERMINAL_NODES_PID=""

cleanup() {
  local status=$?
  for pid in "$CLIENT_ERROR_ROUTER_PID" "$SERVER_ERROR_ROUTER_PID" "$OVERFLOW_ROUTER_PID" "$STREAM_ROUTER_PID" "$TERMINAL_NODES_PID" "$BUDGET_ROUTER_PID" "$BUDGET_NODES_PID" "$ROUTER_PID" "$NODES_PID" "$JEV_PID" "$GATEWAY_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "== gateway server log =="; [ -f "$GATEWAY_LOG" ] && cat "$GATEWAY_LOG" || echo "(none)"
    echo "== router log =="; [ -f "$ROUTER_LOG" ] && cat "$ROUTER_LOG" || echo "(none)"
    echo "== budget router log =="; [ -f "$BUDGET_ROUTER_LOG" ] && cat "$BUDGET_ROUTER_LOG" || echo "(none)"
    echo "== overflow router log =="; [ -f "$OVERFLOW_ROUTER_LOG" ] && cat "$OVERFLOW_ROUTER_LOG" || echo "(none)"
    echo "== stream router log =="; [ -f "$STREAM_ROUTER_LOG" ] && cat "$STREAM_ROUTER_LOG" || echo "(none)"
    echo "== client-error router log =="; [ -f "$CLIENT_ERROR_ROUTER_LOG" ] && cat "$CLIENT_ERROR_ROUTER_LOG" || echo "(none)"
    echo "== server-error router log =="; [ -f "$SERVER_ERROR_ROUTER_LOG" ] && cat "$SERVER_ERROR_ROUTER_LOG" || echo "(none)"
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

# Poll a URL until it answers 2xx/3xx or the budget runs out. A server that
# never comes up is a failure with a clear message, not a hang to a timeout.
wait_for() {
  local url="$1" name="$2" pid="$3" tries=60
  while [ "$tries" -gt 0 ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "error: $name exited before becoming ready at $url" >&2
      return 1
    fi
    if curl -fsS -o /dev/null "$url" 2>/dev/null; then return 0; fi
    tries=$((tries - 1))
    sleep 0.5
  done
  echo "error: $name did not become ready at $url" >&2
  return 1
}

echo "== build =="
# Debug binary: this proves the wiring, and a release build would cost minutes
# the render does not need. The frontend is built only if it has not been.
cargo build -p lightweight-cli --bin lightweight
if [ ! -f frontend/dist/index.html ]; then
  ( cd frontend && npm run build )
fi

echo "== start gateway (port $GATEWAY_PORT) =="
./target/debug/lightweight serve --host 127.0.0.1 --port "$GATEWAY_PORT" \
  --web-root frontend/dist \
  >"$GATEWAY_LOG" 2>&1 &
GATEWAY_PID=$!
wait_for "http://127.0.0.1:$GATEWAY_PORT/health" "gateway" "$GATEWAY_PID"

echo "== render the panel in a headless browser =="
PANEL_BASE="http://127.0.0.1:$GATEWAY_PORT" OUT_DIR="$OUT_DIR" \
  node e2e/render.mjs

echo "== start scripted TypeSafe (port $JEV_PORT) =="
# A throwaway value, generated per run: what matters is that the router has a
# key and that this exact string never reaches the browser.
JEV_KEY="render-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
MOCK_JEV_PORT="$JEV_PORT" MOCK_JEV_KEY="$JEV_KEY" node e2e/mock-jev.mjs >"$WORK/jev.log" 2>&1 &
JEV_PID=$!
wait_for "http://127.0.0.1:$JEV_PORT/health" "scripted TypeSafe" "$JEV_PID"

echo "== start scripted nodes (ports $CODER_NODE_PORT, $GENERAL_NODE_PORT) =="
# A second Coder deployment that refuses with 503 and a second General one that
# answers. Both start with no model, so until the render loads them every route
# is unavailable, as with the gateway alone. Started before the router, so its
# first probe finds them.
MOCK_NODES="$CODER_NODE_PORT:Coder:503,$GENERAL_NODE_PORT:General:200" \
  node e2e/mock-node.mjs >"$WORK/nodes.log" 2>&1 &
NODES_PID=$!
wait_for "http://127.0.0.1:$CODER_NODE_PORT/health" "scripted Coder node" "$NODES_PID"
wait_for "http://127.0.0.1:$GENERAL_NODE_PORT/health" "scripted General node" "$NODES_PID"

echo "== start router (port $ROUTER_PORT) =="
# Nodes point at the gateway above and the scripted nodes; whether the gateway
# is healthy does not matter to these screens. Jev is active, with a
# Lightweight block kept as standby.
cat >"$WORK/router.json" <<JSON
{
  "listen": ["127.0.0.1:$ROUTER_PORT"],
  "nodes": [
    {"id": "local", "url": "http://127.0.0.1:$GATEWAY_PORT"},
    {"id": "coder-b", "url": "http://127.0.0.1:$CODER_NODE_PORT"},
    {"id": "general-b", "url": "http://127.0.0.1:$GENERAL_NODE_PORT"}
  ],
  "routes": [
    {"name": "General", "description": "Everyday conversation and questions", "deployments": [{"node": "local", "model": "General"}, {"node": "general-b", "model": "General"}]},
    {"name": "Coder", "description": "Programming, debugging and code generation", "deployments": [{"node": "local", "model": "Coder"}, {"node": "coder-b", "model": "Coder"}]},
    {"name": "Research", "deployments": [{"node": "local", "model": "Research"}]}
  ],
  "auto_route": {
    "enabled": true,
    "fallback_route": "General",
    "rules": [
      {"name": "tools", "when": {"requires_tools": true}, "route": "Coder"},
      {"name": "semantic", "when": {}, "classify": true}
    ],
    "classifier": {
      "provider": "jev",
      "routes": ["General", "Coder", "Research"],
      "fallback_route": "General",
      "jev": {
        "base_url": "http://127.0.0.1:$JEV_PORT",
        "model": "jev-latest",
        "timeout_ms": 5000,
        "include_user_text": false
      },
      "lightweight": {"route": "Research", "timeout_ms": 30000}
    },
    "cross_route_fallback": {"Coder": ["General"]}
  }
}
JSON
TYPESAFE_API_KEY="$JEV_KEY" ./target/debug/lightweight router --config "$WORK/router.json" \
  --web-root frontend/dist >"$ROUTER_LOG" 2>&1 &
ROUTER_PID=$!
wait_for "http://127.0.0.1:$ROUTER_PORT/health" "router" "$ROUTER_PID"

echo "== start budget nodes (ports $BUDGET_CODER_PORT, $BUDGET_GENERAL_PORT) =="
# Coder refuses with 503 (route_exhausted, a fallback reason); General accepts
# and never answers, so the request budget cuts it. Both start loaded.
MOCK_NODES="$BUDGET_CODER_PORT:Coder:503:loaded,$BUDGET_GENERAL_PORT:General:hang:loaded" \
  node e2e/mock-node.mjs >"$WORK/budget-nodes.log" 2>&1 &
BUDGET_NODES_PID=$!
wait_for "http://127.0.0.1:$BUDGET_CODER_PORT/health" "scripted budget Coder node" "$BUDGET_NODES_PID"
wait_for "http://127.0.0.1:$BUDGET_GENERAL_PORT/health" "scripted budget General node" "$BUDGET_NODES_PID"

echo "== start budget router (port $BUDGET_ROUTER_PORT) =="
cat >"$WORK/budget-router.json" <<JSON
{
  "listen": ["127.0.0.1:$BUDGET_ROUTER_PORT"],
  "request": {"pre_commit_budget_ms": 1500},
  "nodes": [
    {"id": "coder", "url": "http://127.0.0.1:$BUDGET_CODER_PORT"},
    {"id": "general", "url": "http://127.0.0.1:$BUDGET_GENERAL_PORT"}
  ],
  "routes": [
    {"name": "General", "deployments": [{"node": "general", "model": "General"}]},
    {"name": "Coder", "deployments": [{"node": "coder", "model": "Coder"}]}
  ],
  "auto_route": {
    "enabled": true,
    "fallback_route": "General",
    "rules": [{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}],
    "cross_route_fallback": {"Coder": ["General"]}
  }
}
JSON
./target/debug/lightweight router --config "$WORK/budget-router.json" \
  --web-root frontend/dist >"$BUDGET_ROUTER_LOG" 2>&1 &
BUDGET_ROUTER_PID=$!
wait_for "http://127.0.0.1:$BUDGET_ROUTER_PORT/health" "budget router" "$BUDGET_ROUTER_PID"

echo "== start overflow, stream and error nodes (ports $OVERFLOW_GENERAL_PORT, $STREAM_GENERAL_PORT, $CLIENT_ERROR_GENERAL_PORT, $SERVER_ERROR_GENERAL_PORT) =="
MOCK_NODES="$OVERFLOW_GENERAL_PORT:General:overflow:loaded,$STREAM_GENERAL_PORT:General:stream:loaded,$CLIENT_ERROR_GENERAL_PORT:General:400:loaded,$SERVER_ERROR_GENERAL_PORT:General:500:loaded" \
  node e2e/mock-node.mjs >"$WORK/terminal-nodes.log" 2>&1 &
TERMINAL_NODES_PID=$!
wait_for "http://127.0.0.1:$OVERFLOW_GENERAL_PORT/health" "scripted overflow General node" "$TERMINAL_NODES_PID"
wait_for "http://127.0.0.1:$STREAM_GENERAL_PORT/health" "scripted streaming General node" "$TERMINAL_NODES_PID"
wait_for "http://127.0.0.1:$CLIENT_ERROR_GENERAL_PORT/health" "scripted 400 General node" "$TERMINAL_NODES_PID"
wait_for "http://127.0.0.1:$SERVER_ERROR_GENERAL_PORT/health" "scripted 500 General node" "$TERMINAL_NODES_PID"

# Auto → Coder (503) → General, the same shape as the budget router.
fallback_router_json() { # listen port, "request" member or "", General's port
  cat <<JSON
{
  "listen": ["127.0.0.1:$1"],$2
  "nodes": [
    {"id": "coder", "url": "http://127.0.0.1:$BUDGET_CODER_PORT"},
    {"id": "general", "url": "http://127.0.0.1:$3"}
  ],
  "routes": [
    {"name": "General", "deployments": [{"node": "general", "model": "General"}]},
    {"name": "Coder", "deployments": [{"node": "coder", "model": "Coder"}]}
  ],
  "auto_route": {
    "enabled": true,
    "fallback_route": "General",
    "rules": [{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}],
    "cross_route_fallback": {"Coder": ["General"]}
  }
}
JSON
}

echo "== start overflow router (port $OVERFLOW_ROUTER_PORT) =="
# No request budget: the context overflow alone ends the chain.
fallback_router_json "$OVERFLOW_ROUTER_PORT" "" "$OVERFLOW_GENERAL_PORT" >"$WORK/overflow-router.json"
./target/debug/lightweight router --config "$WORK/overflow-router.json" \
  --web-root frontend/dist >"$OVERFLOW_ROUTER_LOG" 2>&1 &
OVERFLOW_ROUTER_PID=$!
wait_for "http://127.0.0.1:$OVERFLOW_ROUTER_PORT/health" "overflow router" "$OVERFLOW_ROUTER_PID"

echo "== start stream router (port $STREAM_ROUTER_PORT) =="
# A 1500 ms pre-commit budget the stream outlives once its head is sent.
fallback_router_json "$STREAM_ROUTER_PORT" ' "request": {"pre_commit_budget_ms": 1500},' "$STREAM_GENERAL_PORT" \
  >"$WORK/stream-router.json"
./target/debug/lightweight router --config "$WORK/stream-router.json" \
  --web-root frontend/dist >"$STREAM_ROUTER_LOG" 2>&1 &
STREAM_ROUTER_PID=$!
wait_for "http://127.0.0.1:$STREAM_ROUTER_PORT/health" "stream router" "$STREAM_ROUTER_PID"

echo "== start client-error and server-error routers (ports $CLIENT_ERROR_ROUTER_PORT, $SERVER_ERROR_ROUTER_PORT) =="
# General's 400 (not context_length_exceeded) and 500 are its answer: neither
# moves the request on, so the chain ends there with `exhausted: false`.
fallback_router_json "$CLIENT_ERROR_ROUTER_PORT" "" "$CLIENT_ERROR_GENERAL_PORT" >"$WORK/client-error-router.json"
./target/debug/lightweight router --config "$WORK/client-error-router.json" \
  --web-root frontend/dist >"$CLIENT_ERROR_ROUTER_LOG" 2>&1 &
CLIENT_ERROR_ROUTER_PID=$!
wait_for "http://127.0.0.1:$CLIENT_ERROR_ROUTER_PORT/health" "client-error router" "$CLIENT_ERROR_ROUTER_PID"
fallback_router_json "$SERVER_ERROR_ROUTER_PORT" "" "$SERVER_ERROR_GENERAL_PORT" >"$WORK/server-error-router.json"
./target/debug/lightweight router --config "$WORK/server-error-router.json" \
  --web-root frontend/dist >"$SERVER_ERROR_ROUTER_LOG" 2>&1 &
SERVER_ERROR_ROUTER_PID=$!
wait_for "http://127.0.0.1:$SERVER_ERROR_ROUTER_PORT/health" "server-error router" "$SERVER_ERROR_ROUTER_PID"

echo "== render the router's panel in a headless browser =="
PANEL_BASE="http://127.0.0.1:$ROUTER_PORT" OUT_DIR="$OUT_DIR" SECRET_SENTINEL="$JEV_KEY" \
  BUDGET_PANEL_BASE="http://127.0.0.1:$BUDGET_ROUTER_PORT" \
  OVERFLOW_PANEL_BASE="http://127.0.0.1:$OVERFLOW_ROUTER_PORT" \
  STREAM_PANEL_BASE="http://127.0.0.1:$STREAM_ROUTER_PORT" \
  CLIENT_ERROR_PANEL_BASE="http://127.0.0.1:$CLIENT_ERROR_ROUTER_PORT" \
  SERVER_ERROR_PANEL_BASE="http://127.0.0.1:$SERVER_ERROR_ROUTER_PORT" \
  EXPECT_BASE_URL="http://127.0.0.1:$JEV_PORT" \
  MOCK_NODE_URLS="http://127.0.0.1:$CODER_NODE_PORT,http://127.0.0.1:$GENERAL_NODE_PORT" \
  node e2e/render-router.mjs

echo "Panel render complete. Screenshots in $OUT_DIR/"
