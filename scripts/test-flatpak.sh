#!/usr/bin/env bash
# What the built Flatpak actually contains, and how it actually behaves.
#
# The sibling of `scripts/test-artifacts.sh`, and it makes the same bet: every
# assertion here is about an *installed bundle* rather than about the
# `build.flatpak` block that produced it. Two of the things below cannot be seen
# in that block at all - the application id is rewritten by electron-builder
# (hyphens become underscores), and whether the app's own data directory is
# mounted `noexec` is a property of the sandbox, not of any configuration.
#
# It cannot run on the development machine: flatpak-builder builds inside
# bwrap, this host cannot create unprivileged user namespaces, and bwrap here is
# not setuid. That is why this script exists as its own file - CI is the only
# place it has ever run, and a reader should be able to see exactly what CI
# proved without reading a YAML step.
#
# Run after `npm --prefix apps/desktop run package -- --linux flatpak`.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_DIR="apps/desktop/release"
# electron-builder's `filterFlatpakAppIdentifier` replaces `-` with `_`, so the
# id here is not the `appId` from package.json. Derived rather than retyped, so
# that renaming the app cannot leave this script asserting against a stale name.
APP_ID="$(node -e 'process.stdout.write(require("./apps/desktop/package.json").build.appId.replace(/-/g,"_").replace(/[^a-zA-Z0-9._]/g,""))')"
failures=0

pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }
skip() { printf '  skip  %s\n' "$1"; }

if ! command -v flatpak >/dev/null 2>&1; then
  echo "flatpak is not installed - this script runs where the bundle can be built" >&2
  exit 1
fi

bundle="$(find "$APP_DIR" -maxdepth 1 -name '*.flatpak' -print -quit 2>/dev/null || true)"
if [ -z "$bundle" ]; then
  echo "no flatpak bundle in $APP_DIR - run" >&2
  echo "  npm --prefix apps/desktop run package -- --linux flatpak" >&2
  exit 1
fi
echo "== artifact == $bundle ($(du -h "$bundle" | cut -f1))"
echo "== app id   == $APP_ID"

# ---------------------------------------------------------------------------
# 1. It installs, and it installs under the id the desktop entry names.
# ---------------------------------------------------------------------------
flatpak install --user -y --bundle "$bundle" >/dev/null
if flatpak list --user --app --columns=application | grep -qx "$APP_ID"; then
  pass "installs as $APP_ID"
else
  fail "installed, but not as $APP_ID: $(flatpak list --user --app --columns=application | tr '\n' ' ')"
fi

# ---------------------------------------------------------------------------
# 2. The sandbox it asks for is the one that was configured.
#
#    `--filesystem=home` is electron-builder's *default* finish arg. Shipping
#    it would hand the app the whole home directory, so its absence is asserted
#    rather than assumed: the grant starts at xdg-download and widens only when
#    a test asks for it.
# ---------------------------------------------------------------------------
metadata="$(flatpak info --user --show-metadata "$APP_ID")"
if echo "$metadata" | grep -q '^shared=.*network'; then
  pass "the sandbox may reach the network (the engine and models are downloaded)"
else
  fail "no network share: the engine could never be downloaded"
fi
if echo "$metadata" | grep -q 'org.kde.StatusNotifierWatcher=talk'; then
  pass "the tray can register (org.kde.StatusNotifierWatcher)"
else
  fail "no StatusNotifierWatcher grant: the tray this shell creates cannot appear"
fi
if echo "$metadata" | grep -q '^filesystems=.*xdg-download'; then
  pass "models can be picked from the downloads directory"
else
  fail "no xdg-download grant: no model outside the sandbox could be imported"
fi
if echo "$metadata" | grep -qE '^filesystems=.*(^|;)home(;|$)'; then
  fail "the sandbox grants the whole home directory"
else
  pass "the sandbox does not grant the whole home directory"
fi

# ---------------------------------------------------------------------------
# 3. The launcher does not disable Chromium's sandbox.
#
#    The AppImage's generated `AppRun` adds `--no-sandbox` by itself whenever
#    user namespaces are unavailable; the Flatpak's wrapper delegates to
#    zypak instead, which is the reason this is the primary Linux artifact.
#    Asserted against the shipped file, not against the generator.
# ---------------------------------------------------------------------------
wrapper="$(flatpak run --command=cat "$APP_ID" /app/bin/electron-wrapper 2>/dev/null || true)"
if [ -z "$wrapper" ]; then
  fail "the bundle has no /app/bin/electron-wrapper"
elif echo "$wrapper" | grep -q -- '--no-sandbox'; then
  fail "the wrapper disables the sandbox: $(echo "$wrapper" | grep -- '--no-sandbox')"
elif echo "$wrapper" | grep -q 'zypak-wrapper'; then
  pass "the wrapper launches through zypak and adds no --no-sandbox"
else
  fail "the wrapper neither uses zypak nor is recognisable: $wrapper"
fi

# ---------------------------------------------------------------------------
# 4. The payload, and the engine's own binary, seen from inside the sandbox.
# ---------------------------------------------------------------------------
for required in \
  "/app/lib/$APP_ID/resources/bin/hermes" \
  "/app/lib/$APP_ID/resources/panel/index.html" \
  "/app/lib/$APP_ID/resources/app.asar"
do
  if flatpak run --command=test "$APP_ID" -e "$required" 2>/dev/null; then
    pass "carries $required"
  else
    fail "missing $required"
  fi
done

version="$(flatpak run --command="/app/lib/$APP_ID/resources/bin/hermes" "$APP_ID" --version 2>&1 || true)"
if echo "$version" | grep -qi 'hermes'; then
  pass "the packaged hermes binary runs against the runtime ($version)"
else
  fail "the packaged hermes binary does not run inside the runtime: $version"
fi

# ---------------------------------------------------------------------------
# 5. The highest-risk unknown in the whole Flatpak: the engine is downloaded at
#    run time into the app's data directory and then executed. If that
#    directory is mounted `noexec`, llama-server never starts - and nothing in
#    the manifest would say so. Proved by copying a real executable there and
#    running it from that path.
# ---------------------------------------------------------------------------
probe='set -e
    mkdir -p "$XDG_DATA_HOME/probe"
    cp "/app/lib/APPID/resources/bin/hermes" "$XDG_DATA_HOME/probe/engine-stand-in"
    "$XDG_DATA_HOME/probe/engine-stand-in" --version'
probe="${probe//APPID/$APP_ID}"
if out="$(flatpak run --command=sh "$APP_ID" -c "$probe" 2>&1)"; then
  pass "an executable in the app data directory runs, so a downloaded engine can start ($out)"
else
  fail "the app data directory will not execute a binary - a downloaded engine could not start: $out"
fi
flatpak run --command=sh "$APP_ID" -c 'rm -rf "$XDG_DATA_HOME/probe"' 2>/dev/null || true

# ---------------------------------------------------------------------------
# 6. The supported case: it starts, stays up, and no live process of it carries
#    --no-sandbox. Needs a display; says so rather than passing quietly.
# ---------------------------------------------------------------------------
if [ -n "${DISPLAY:-}" ]; then
  # zypak - the sandbox helper this artifact exists for - talks to the session
  # bus, and a CI runner has none: the first run of this script failed here with
  # `Failed to connect to session bus`, which says nothing about the artifact.
  # `dbus-run-session` provides one for the length of the launch.
  bus=()
  if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && command -v dbus-run-session >/dev/null 2>&1; then
    bus=(dbus-run-session --)
  fi
  home="$(mktemp -d)"
  set +e
  "${bus[@]}" flatpak run --env=HERMES_GATEWAY_HOME=/tmp/hermes-flatpak-home --env=HERMES_PORT=18781 \
    "$APP_ID" >/tmp/flatpak-run.log 2>&1 &
  launched=$!
  sleep 25
  alive=0
  kill -0 "$launched" 2>/dev/null && alive=1
  flagged="$(pgrep -af -- '--no-sandbox' 2>/dev/null | grep -i "$APP_ID\|hermes-desktop" || true)"
  flatpak kill "$APP_ID" >/dev/null 2>&1
  kill "$launched" 2>/dev/null
  wait "$launched" 2>/dev/null
  set -e
  rm -rf "$home"

  if [ "$alive" -eq 0 ]; then
    fail "the app did not stay running: $(tail -n 20 /tmp/flatpak-run.log)"
  elif [ -n "$flagged" ]; then
    fail "live processes carry --no-sandbox: $flagged"
  else
    pass "it runs sandboxed, and no live process carries --no-sandbox"
  fi
else
  skip "the launch: no DISPLAY. Run under xvfb-run to include it."
fi

echo
if [ "$failures" -eq 0 ]; then
  echo "Flatpak checks passed."
else
  echo "$failures flatpak check(s) failed." >&2
  exit 1
fi
