#!/usr/bin/env bash
# Render the panel end to end and assert the agent screens actually work.
#
# The panel is one bundle over two servers — the inference gateway (`hermes
# serve`), which answers `/api/v1` and serves the panel, and the agent API
# (`lightagent serve`) under `/api/lightagent/v1`, which the gateway proxies.
# The screens that broke (Agent, Tools, Chat) are the proxied ones: with no
# proxy they fall to the panel fallback and render `index.html`, so the client
# chokes on the `<` of `<!doctype …>`. This script brings up both servers wired
# exactly as the product wires them, drives a real headless browser over every
# screen, and fails if an agent screen shows that fallback — the check that a
# unit test cannot make because the bug lives in the seam between two processes.
#
# Kept as a script, not inlined into the workflow, so it is identical locally
# and in CI. Run it from a checkout with the frontend deps installed:
#
#   npm ci --prefix frontend && npm ci --prefix e2e
#   npx --prefix e2e playwright install --with-deps chromium
#   scripts/render-panel.sh
#
# Environment:
#   AGENT_PORT   agent API port           (default 8735)
#   GATEWAY_PORT gateway/panel port       (default 11434)
#   OUT_DIR      where screenshots land   (default e2e/screens)
set -euo pipefail
cd "$(dirname "$0")/.."

AGENT_PORT="${AGENT_PORT:-8735}"
GATEWAY_PORT="${GATEWAY_PORT:-11434}"
OUT_DIR="${OUT_DIR:-e2e/screens}"

# Same rustup-env dance as check.sh: cargo is absent from a non-login PATH.
if ! command -v cargo >/dev/null 2>&1 && [ -f "${HOME:-}/.cargo/env" ]; then
  . "${HOME:-}/.cargo/env"
fi

# A scratch home for each server so the render never reads or writes a
# developer's real agent profile or the gateway's real data directory — and so a
# clean CI runner, which has neither, is configured from nothing rather than
# failing on a missing profile. `LIGHTAGENT_HOME` roots the agent server;
# `HERMES_GATEWAY_HOME` roots the gateway.
WORK="$(mktemp -d)"
export LIGHTAGENT_HOME="$WORK/agent-home"
export HERMES_GATEWAY_HOME="$WORK/gateway-home"
AGENT_LOG="$WORK/agent.log"
GATEWAY_LOG="$WORK/gateway.log"
AGENT_PID=""
GATEWAY_PID=""

cleanup() {
  local status=$?
  [ -n "$GATEWAY_PID" ] && kill "$GATEWAY_PID" 2>/dev/null || true
  [ -n "$AGENT_PID" ] && kill "$AGENT_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  # On a failure, the server logs are usually where the answer is; print them
  # so a red CI run explains itself without a re-run.
  if [ "$status" -ne 0 ]; then
    echo "== agent server log ==";   [ -f "$AGENT_LOG" ]   && cat "$AGENT_LOG"   || echo "(none)"
    echo "== gateway server log =="; [ -f "$GATEWAY_LOG" ] && cat "$GATEWAY_LOG" || echo "(none)"
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

# Poll a URL until it answers 2xx/3xx or the budget runs out. A server that
# never comes up is a failure with a clear message, not a hang to a timeout.
wait_for() {
  local url="$1" name="$2" tries=60
  while [ "$tries" -gt 0 ]; do
    if curl -fsS -o /dev/null "$url" 2>/dev/null; then return 0; fi
    tries=$((tries - 1))
    sleep 0.5
  done
  echo "error: $name did not become ready at $url" >&2
  return 1
}

echo "== build =="
# Debug binaries: this proves the wiring, and a release build would cost minutes
# the render does not need. The frontend is built only if it has not been.
cargo build -p lightagent --bin lightagent -p lightweight-cli --bin hermes
if [ ! -f frontend/dist/index.html ]; then
  ( cd frontend && npm run build )
fi

# The agent server refuses to start without an active profile; a fresh
# `LIGHTAGENT_HOME` has none, so scaffold one. `init` is non-interactive and
# needs no model or network — it writes the config and a `default` profile.
echo "== init the agent home =="
./target/debug/lightagent init >/dev/null

echo "== start agent API (port $AGENT_PORT) =="
./target/debug/lightagent serve --host 127.0.0.1 --port "$AGENT_PORT" \
  >"$AGENT_LOG" 2>&1 &
AGENT_PID=$!
wait_for "http://127.0.0.1:$AGENT_PORT/api/lightagent/v1/tools" "agent API"

echo "== start gateway (port $GATEWAY_PORT), proxying the agent =="
./target/debug/hermes serve --host 127.0.0.1 --port "$GATEWAY_PORT" \
  --web-root frontend/dist \
  --agent-upstream "http://127.0.0.1:$AGENT_PORT" \
  >"$GATEWAY_LOG" 2>&1 &
GATEWAY_PID=$!
wait_for "http://127.0.0.1:$GATEWAY_PORT/health" "gateway"

# The seam, asserted before the browser even opens: the panel's origin must
# serve the agent's JSON, not the document. This is the one-line version of the
# whole bug, and failing here points at the proxy rather than at the panel.
echo "== proxied endpoint returns JSON, not index.html =="
if ! curl -fsS "http://127.0.0.1:$GATEWAY_PORT/api/lightagent/v1/tools" \
  | grep -q '"tools"'; then
  echo "error: /api/lightagent/v1/tools did not return the tools JSON through the gateway" >&2
  exit 1
fi

echo "== render the panel in a headless browser =="
PANEL_BASE="http://127.0.0.1:$GATEWAY_PORT" OUT_DIR="$OUT_DIR" \
  node e2e/render.mjs

echo "Panel render complete. Screenshots in $OUT_DIR/"
