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
# Environment:
#   GATEWAY_PORT  gateway/panel port        (default 11434)
#   OUT_DIR       where screenshots land    (default e2e/screens)
set -euo pipefail
cd "$(dirname "$0")/.."

GATEWAY_PORT="${GATEWAY_PORT:-11434}"
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

cleanup() {
  local status=$?
  [ -n "$GATEWAY_PID" ] && kill "$GATEWAY_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "== gateway server log =="; [ -f "$GATEWAY_LOG" ] && cat "$GATEWAY_LOG" || echo "(none)"
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

echo "Panel render complete. Screenshots in $OUT_DIR/"
