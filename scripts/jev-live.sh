#!/usr/bin/env bash
# Live Jev validation (.github/workflows/jev-live.yml): the router's Jev
# classifier against the real TypeSafe API.
#
# A loopback router, Jev as its classifier (base https://api.typesafe.ai,
# model $JEV_MODEL), the key read from TYPESAFE_API_KEY by the router itself;
# two scripted nodes serve the routes, so the one live party is TypeSafe.
# e2e/jev-live.mjs then proves, through the router's own API and the panel:
# the key is accepted and the model listed; Auto requests are classified by
# Jev (outcome `chosen`, never a fallback) into the configured routes and
# served there; and the key reaches no page, storage or answer. This script
# finally fails if the key appears in any router log.
#
# The key is never printed: no `set -x`, no echo of it, comparisons only with
# `grep -qF`. Requires: TYPESAFE_API_KEY. Optional: JEV_MODEL (jev-latest),
# ROUTER_PORT (11600), NODE_PORT (11601, +1), OUT_DIR (e2e/jev-live).
set -euo pipefail
set +x
cd "$(dirname "$0")/.."

if [ -z "${TYPESAFE_API_KEY:-}" ]; then
  echo "TYPESAFE_API_KEY is not set: run this through the jev-live-validation environment" >&2
  exit 2
fi
JEV_MODEL="${JEV_MODEL:-jev-latest}"
ROUTER_PORT="${ROUTER_PORT:-11600}"
CODER_PORT="${NODE_PORT:-11601}"
GENERAL_PORT="$((CODER_PORT + 1))"
OUT_DIR="${OUT_DIR:-e2e/jev-live}"
mkdir -p "$OUT_DIR"

WORK="$(mktemp -d)"
export HERMES_GATEWAY_HOME="$WORK/home"
ROUTER_LOG="$WORK/router.log"
NODES_PID=""
ROUTER_PID=""
cleanup() {
  for pid in "$ROUTER_PID" "$NODES_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

wait_for() { # url, what, pid
  for _ in $(seq 1 150); do
    if curl -fsS "$1" >/dev/null 2>&1; then return 0; fi
    if ! kill -0 "$3" 2>/dev/null; then echo "$2 exited before answering" >&2; return 1; fi
    sleep 0.2
  done
  echo "$2 did not answer at $1" >&2
  return 1
}

echo "== start scripted nodes (ports $CODER_PORT, $GENERAL_PORT) =="
MOCK_NODES="$CODER_PORT:Coder:200:loaded,$GENERAL_PORT:General:200:loaded" \
  node e2e/mock-node.mjs >"$WORK/nodes.log" 2>&1 &
NODES_PID=$!
wait_for "http://127.0.0.1:$CODER_PORT/health" "scripted Coder node" "$NODES_PID"
wait_for "http://127.0.0.1:$GENERAL_PORT/health" "scripted General node" "$NODES_PID"

echo "== start the router with Jev on the live TypeSafe API (model $JEV_MODEL) =="
cat >"$WORK/router.json" <<JSON
{
  "listen": ["127.0.0.1:$ROUTER_PORT"],
  "nodes": [
    {"id": "coder", "url": "http://127.0.0.1:$CODER_PORT"},
    {"id": "general", "url": "http://127.0.0.1:$GENERAL_PORT"}
  ],
  "routes": [
    {"name": "General", "description": "Everyday conversation, questions, writing and general knowledge", "deployments": [{"node": "general", "model": "General"}]},
    {"name": "Coder", "description": "Programming, debugging, code review and code generation", "deployments": [{"node": "coder", "model": "Coder"}]}
  ],
  "default_route": "General",
  "auto_route": {
    "enabled": true,
    "fallback_route": "General",
    "rules": [
      {"name": "semantic", "when": {"requires_tools": false}, "classify": true}
    ],
    "classifier": {
      "provider": "jev",
      "routes": ["General", "Coder"],
      "fallback_route": "General",
      "jev": {
        "base_url": "https://api.typesafe.ai",
        "model": "$JEV_MODEL",
        "timeout_ms": 20000,
        "include_user_text": true
      }
    }
  }
}
JSON
# The key is in this one process's environment (inherited from the step); the
# scripted nodes and the browser never get it.
./target/debug/lightweight router --config "$WORK/router.json" --web-root frontend/dist \
  >"$ROUTER_LOG" 2>&1 &
ROUTER_PID=$!
wait_for "http://127.0.0.1:$ROUTER_PORT/health" "router" "$ROUTER_PID"

echo "== validate =="
status=0
ROUTER_BASE="http://127.0.0.1:$ROUTER_PORT" JEV_MODEL="$JEV_MODEL" OUT_DIR="$OUT_DIR" \
  LIVE_KEY_SENTINEL="$TYPESAFE_API_KEY" node e2e/jev-live.mjs || status=$?

echo "== the key is in no router log =="
if grep -qF -- "$TYPESAFE_API_KEY" "$ROUTER_LOG" "$WORK/nodes.log"; then
  echo "  [FAIL] the TypeSafe key appears in a log" >&2
  status=1
else
  echo "  [ok] the TypeSafe key appears in no router or node log"
fi
# The router's own account of the start-up check and every classification,
# for the run's log: outcome lines only, which carry no request text or key.
grep -E "classifier provider checked|classifier" "$ROUTER_LOG" | sed -E 's/\x1b\[[0-9;]*m//g' | tail -20 || true
exit "$status"
