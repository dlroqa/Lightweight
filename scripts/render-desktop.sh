#!/usr/bin/env bash
# The desktop app with a Router beside its Gateway, end to end, headless.
#
# Launches the real Electron app (the sandbox on: the app refuses
# `--no-sandbox`) against a real Gateway and a real Router, and checks that
# Router & Jev Settings opens the Router's own panel in its own window while
# the Gateway window stays the Gateway's. Jev and the Router's nodes are
# scripted (`e2e/mock-jev.mjs`, `e2e/mock-node.mjs`), so no real key and no
# external service are ever needed.
#
# Two runs of `e2e/desktop-router.mjs`:
#
#   start   the app starts the Gateway, and the Router from the Router menu,
#           reading router.json from the config directory `hermes router
#           config-path` names; quitting stops both.
#   attach  a Gateway and a Router started here, outside the app; the app
#           attaches to both, and quitting leaves both serving.
#
# Needs a display (run under `xvfb-run` on a headless machine) and a host that
# allows Chromium's sandbox (on Ubuntu 24.04:
# `sysctl kernel.apparmor_restrict_unprivileged_userns=0`). From a checkout:
#
#   npm ci --prefix frontend && npm ci --prefix apps/desktop && npm ci --prefix e2e
#   xvfb-run --auto-servernum scripts/render-desktop.sh
#
# Environment:
#   DESKTOP_PORT_BASE  first of five consecutive ports (default 18434): Gateway,
#                      Router, scripted Jev, scripted Coder, scripted General
#   OUT_DIR            where screenshots and the app's logs land
#                      (default e2e/desktop-screens)
set -euo pipefail
cd "$(dirname "$0")/.."

BASE="${DESKTOP_PORT_BASE:-18434}"
GATEWAY_PORT="$BASE"
ROUTER_PORT="$((BASE + 1))"
JEV_PORT="$((BASE + 2))"
CODER_PORT="$((BASE + 3))"
GENERAL_PORT="$((BASE + 4))"
OUT_DIR="${OUT_DIR:-e2e/desktop-screens}"

if ! command -v cargo >/dev/null 2>&1 && [ -f "${HOME:-}/.cargo/env" ]; then
  . "${HOME:-}/.cargo/env"
fi

WORK="$(mktemp -d)"
JEV_PID=""
NODES_PID=""
GATEWAY_PID=""
ROUTER_PID=""

cleanup() {
  local status=$?
  for pid in "$ROUTER_PID" "$GATEWAY_PID" "$NODES_PID" "$JEV_PID"; do
    [ -n "$pid" ] && kill -INT "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    # Redacted: the key is a per-run throwaway, but a log is never the place for it.
    for log in "$WORK"/*.log; do
      [ -f "$log" ] || continue
      echo "== $(basename "$log") =="
      sed "s/${JEV_KEY:-unset-key}/[REDACTED]/g" "$log"
    done
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

wait_for() {
  local url="$1" name="$2" pid="$3" tries=120
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
cargo build -p lightweight-cli --bin hermes
if [ ! -f frontend/dist/index.html ]; then
  ( cd frontend && npm run build )
fi
# Compiled, not `npm run build`: that also runs the shell's test suite, which
# check.sh already runs on every platform.
( cd apps/desktop && npm run compile )
node apps/desktop/node_modules/electron/install.js
ELECTRON_BIN="$(cd apps/desktop && node -e 'process.stdout.write(require("electron"))')"
HERMES_BIN="$PWD/target/debug/hermes"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

echo "== scripted Jev and nodes =="
JEV_KEY="desktop-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
MOCK_JEV_PORT="$JEV_PORT" MOCK_JEV_KEY="$JEV_KEY" node e2e/mock-jev.mjs >"$WORK/jev.log" 2>&1 &
JEV_PID=$!
wait_for "http://127.0.0.1:$JEV_PORT/health" "scripted Jev" "$JEV_PID"
MOCK_NODES="$CODER_PORT:Coder:200:loaded,$GENERAL_PORT:General:200:loaded" \
  node e2e/mock-node.mjs >"$WORK/nodes.log" 2>&1 &
NODES_PID=$!
wait_for "http://127.0.0.1:$CODER_PORT/health" "scripted Coder" "$NODES_PID"
wait_for "http://127.0.0.1:$GENERAL_PORT/health" "scripted General" "$NODES_PID"

# The Router's own configuration: its own nodes and routes, the classifier on
# scripted Jev. Nothing here is the Gateway's, and the app is never asked to
# write it.
router_json() {
  cat <<JSON
{
  "listen": ["127.0.0.1:$ROUTER_PORT"],
  "nodes": [
    {"id": "this-gateway", "url": "http://127.0.0.1:$GATEWAY_PORT"},
    {"id": "coder", "url": "http://127.0.0.1:$CODER_PORT"},
    {"id": "general", "url": "http://127.0.0.1:$GENERAL_PORT"}
  ],
  "routes": [
    {"name": "General", "description": "Everyday conversation and questions", "deployments": [{"node": "general", "model": "General"}]},
    {"name": "Coder", "description": "Programming, debugging and code generation", "deployments": [{"node": "coder", "model": "Coder"}]}
  ],
  "auto_route": {
    "enabled": true,
    "fallback_route": "General",
    "rules": [{"name": "semantic", "when": {}, "classify": true}],
    "classifier": {
      "provider": "jev",
      "routes": ["General", "Coder"],
      "fallback_route": "General",
      "jev": {"base_url": "http://127.0.0.1:$JEV_PORT", "model": "jev-latest", "timeout_ms": 5000, "include_user_text": false}
    }
  }
}
JSON
}

common_env() {
  # The app's environment, as a person would launch it: its own throwaway data
  # directory, its two ports, the binary to run, and the Jev key in the
  # environment where the Router reads it. No credential store on a headless
  # runner (a debug-build switch), exactly as render-panel.sh runs its Router.
  export HERMES_GATEWAY_HOME="$1"
  export HERMES_PORT="$GATEWAY_PORT"
  export HERMES_ROUTER_PORT="$ROUTER_PORT"
  export HERMES_BIN
  export HERMES_WEB_ROOT="$PWD/frontend/dist"
  export TYPESAFE_API_KEY="$JEV_KEY"
  export LIGHTWEIGHT_ROUTER_TEST_SECRET_STORE=unavailable
  unset HERMES_ROUTER_CONFIG
}

run_desktop() { # mode, router config path
  MODE="$1" ROUTER_CONFIG="$2" ELECTRON_BIN="$ELECTRON_BIN" APP_DIR="$PWD/apps/desktop" \
    GATEWAY_PORT="$GATEWAY_PORT" ROUTER_PORT="$ROUTER_PORT" JEV_KEY="$JEV_KEY" OUT_DIR="$OUT_DIR" \
    node e2e/desktop-router.mjs
}

echo "== start mode: the app starts the Gateway, then the Router on request =="
(
  common_env "$WORK/start-home"
  # Where `hermes router config-path` says, so the app's discovery is what is
  # proved — not a path handed to it.
  config="$("$HERMES_BIN" router config-path)"
  mkdir -p "$(dirname "$config")"
  router_json >"$config"
  run_desktop start "$config"
  # The app wrote nothing of its own beside the Router's file.
  if [ -e "$(dirname "$config")/router.template.json" ]; then
    echo "the app wrote a template nobody asked for" >&2
    exit 1
  fi
)

echo "== attach mode: a Gateway and a Router already serving, started outside the app =="
common_env "$WORK/attach-home"
"$HERMES_BIN" serve --host 127.0.0.1 --port "$GATEWAY_PORT" --web-root "$HERMES_WEB_ROOT" \
  >"$WORK/attach-gateway.log" 2>&1 &
GATEWAY_PID=$!
wait_for "http://127.0.0.1:$GATEWAY_PORT/health" "external Gateway" "$GATEWAY_PID"
router_json >"$WORK/attach-router.json"
"$HERMES_BIN" router --config "$WORK/attach-router.json" --web-root "$HERMES_WEB_ROOT" \
  >"$WORK/attach-router.log" 2>&1 &
ROUTER_PID=$!
wait_for "http://127.0.0.1:$ROUTER_PORT/health" "external Router" "$ROUTER_PID"
run_desktop attach "$WORK/attach-router.json"
kill -0 "$GATEWAY_PID" && kill -0 "$ROUTER_PID" || { echo "an external process did not survive the app" >&2; exit 1; }

echo "All desktop checks passed."
