#!/usr/bin/env bash
# Validate the deployable workbench without starting or pulling its containers.
# This deliberately uses generated credentials: CI must never need a real
# gateway, terminal, search, or Jev key just to validate Compose interpolation.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

compose_file=deploy/workbench/compose.yaml
functions_dir=deploy/workbench/open-webui-functions

ruby -e 'require "yaml"; YAML.load_file(ARGV.fetch(0))' "$compose_file"
ruby -e 'require "yaml"; YAML.load_file(ARGV.fetch(0))' deploy/workbench/searxng/settings.yml

env_file=$(mktemp)
trap 'rm -f "$env_file"' EXIT
cat >"$env_file" <<'ENVEOF'
LIGHTWEIGHT_OPENAI_API_BASE_URLS=https://fast.example.test/v1;https://reasoning.example.test/v1
LIGHTWEIGHT_OPENAI_API_KEYS=ci-fast-key-not-a-secret;ci-reasoning-key-not-a-secret
LIGHTWEIGHT_OPENAI_API_CONFIGS={"0":{"enable":true,"prefix_id":"fast","connection_type":"external"},"1":{"enable":true,"prefix_id":"reasoning","connection_type":"external"}}
SEARXNG_SECRET=ci-searxng-secret-not-a-secret
OPEN_TERMINAL_API_KEY=ci-terminal-key-not-a-secret
TYPESAFE_API_KEY=ci-typesafe-key-not-a-secret
ENVEOF
docker compose --env-file "$env_file" -f "$compose_file" config -q

# The only host-published service is the browser UI, and it must remain local
# by default. Verify the authenticated, index-aligned remote Lightweight route
# contract without contacting a gateway or starting a container.
rendered=$(docker compose --env-file "$env_file" -f "$compose_file" config --format json)
rendered_terminal=$(docker compose --profile terminal --env-file "$env_file" -f "$compose_file" config --format json)
node -e '
  const config = JSON.parse(process.argv[1]);
  const terminalConfig = JSON.parse(process.argv[2]);
  const services = config.services || {};
  for (const name of ["qdrant", "tika", "searxng", "infinity", "open-terminal"]) {
    if ((services[name]?.ports || []).length) throw new Error(name + " must not publish host ports");
  }
  const ui = services["open-webui"] || {};
  const ports = ui.ports || [];
  if (ports.length !== 1 || ports[0].host_ip !== "127.0.0.1") throw new Error("open-webui must publish exactly one loopback port");
  const env = ui.environment || {};
  if (env.OPENAI_API_BASE_URL || env.OPENAI_API_KEY) throw new Error("use index-aligned OPENAI_API_BASE_URLS and OPENAI_API_KEYS only");
  const urls = String(env.OPENAI_API_BASE_URLS || "").split(";").filter(Boolean);
  const keys = String(env.OPENAI_API_KEYS || "").split(";").filter(Boolean);
  if (urls.length < 1 || urls.length !== keys.length) throw new Error("every remote Lightweight URL must have an index-aligned key");
  if (urls.some((url) => !/^https:\/\/[^/]+\/v1$/.test(url))) throw new Error("remote Lightweight URLs must be HTTPS endpoints ending in /v1");
  let routeConfigs;
  try { routeConfigs = JSON.parse(env.OPENAI_API_CONFIGS || "{}"); } catch (_) { throw new Error("OPENAI_API_CONFIGS must be JSON"); }
  urls.forEach((_, index) => {
    const route = routeConfigs[String(index)];
    if (!route || route.enable !== true || !route.prefix_id || route.connection_type !== "external") throw new Error("remote Lightweight route " + index + " needs enabled external prefix config");
  });
  const terminal = (terminalConfig.services || {})["open-terminal"] || {};
  if (!(terminal.profiles || []).includes("terminal")) throw new Error("open-terminal must be opt-in profile");
  if (Number(terminal.cpus) !== 2 || !["2g", "2147483648"].includes(String(terminal.mem_limit).toLowerCase()) || Number(terminal.pids_limit) !== 256) throw new Error("open-terminal resource limits are required");
  if ((terminal.volumes || []).some((volume) => String(volume.source || volume).includes("/var/run/docker.sock") || String(volume.type || "").includes("bind"))) throw new Error("open-terminal cannot receive a host bind mount or Docker socket");
  if (Object.keys(terminal.environment || {}).some((key) => key.includes("MULTI_USER"))) throw new Error("open-terminal must not enable multi-user mode");
  if (Object.keys(env).some((key) => key === "TERMINAL_SERVER_CONNECTIONS")) throw new Error("Open WebUI must not auto-connect the terminal");
' "$rendered" "$rendered_terminal"

if [ -d "$functions_dir" ]; then
  while IFS= read -r -d '' source; do
    case "$source" in
      *.py) python3 -m py_compile "$source" ;;
      *.js|*.mjs|*.cjs) node --check "$source" ;;
      *.json) node -e 'JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))' "$source" ;;
    esac
  done < <(find "$functions_dir" -type f \( -name '*.py' -o -name '*.js' -o -name '*.mjs' -o -name '*.cjs' -o -name '*.json' \) -print0)
  while IFS= read -r -d '' test; do python3 "$test"; done < <(find "$functions_dir" -maxdepth 1 -type f -name 'test_*.py' -print0)
  if rg -n --hidden --glob '!*.md' --glob '!*.example' \
      '(ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9]{20,}|AIza[0-9A-Za-z_-]{20,}|xox[baprs]-[A-Za-z0-9-]{20,})' \
      "$functions_dir"; then
    echo "Refusing a credential-looking literal in an Open WebUI Function." >&2
    exit 1
  fi
fi
