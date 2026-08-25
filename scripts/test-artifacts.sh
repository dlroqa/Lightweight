#!/usr/bin/env bash
# What the Linux artifacts actually contain, and how they actually behave.
#
# Everything here is an assertion about a *built* artifact rather than about the
# configuration that produced it. The distinction is the whole point: the
# `--no-sandbox` this exists to keep out is added in two places, one of which is
# a shell script electron-builder regenerates on every build and which no
# configuration option reaches. Reading `package.json` would prove nothing about
# it; extracting the AppImage and running the binary proves it.
#
# Run after `npm run package`. `scripts/check.sh` does not call this: it needs a
# release build and a packaged app, which that gate deliberately does not do.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_DIR="apps/desktop/release"
EXTRACT="$APP_DIR/squashfs-root"
failures=0

pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }

appimage="$(find "$APP_DIR" -maxdepth 1 -name '*.AppImage' -print -quit 2>/dev/null || true)"
if [ -z "$appimage" ]; then
  echo "no AppImage in $APP_DIR - run (cd apps/desktop && npm run package -- --linux AppImage) first" >&2
  exit 1
fi
echo "== artifact == $appimage"

# ---------------------------------------------------------------------------
# Extract it. `--appimage-extract` is the runtime's own entry point and needs
# no FUSE, which matters on a CI runner where FUSE is often unavailable.
# ---------------------------------------------------------------------------
rm -rf "$EXTRACT"
(cd "$APP_DIR" && "./$(basename "$appimage")" --appimage-extract >/dev/null)

# ---------------------------------------------------------------------------
# 1. The generated desktop entry must not hard-code the flag.
#    This is what `linux.executableArgs: []` buys; without it electron-builder
#    writes `Exec=AppRun --no-sandbox %U`.
# ---------------------------------------------------------------------------
exec_line="$(grep -h '^Exec=' "$EXTRACT"/*.desktop || true)"
if [ -z "$exec_line" ]; then
  fail "the artifact has no desktop entry"
elif echo "$exec_line" | grep -q -- '--no-sandbox'; then
  fail "the desktop entry hard-codes --no-sandbox: $exec_line"
else
  pass "the desktop entry does not disable the sandbox ($exec_line)"
fi

# ---------------------------------------------------------------------------
# 2. The payload the shell cannot run without.
# ---------------------------------------------------------------------------
for required in \
  resources/bin/hermes \
  resources/panel/index.html \
  resources/app.asar \
  chrome-sandbox
do
  if [ -e "$EXTRACT/$required" ]; then
    pass "carries $required"
  else
    fail "missing $required"
  fi
done

if "$EXTRACT/resources/bin/hermes" --version >/dev/null 2>&1; then
  pass "the packaged hermes binary runs ($("$EXTRACT/resources/bin/hermes" --version))"
else
  fail "the packaged hermes binary does not run"
fi

# The tray and window icons travel in `dist/`, not `build/`: an icon loaded from
# `build/` is present in a checkout and absent from the artifact, which is the
# worst kind of difference because only the shipped copy is wrong.
for icon in tray.png window.png; do
  if unzip -l "$EXTRACT/resources/app.asar" >/dev/null 2>&1; then :; fi
  if grep -qa "$icon" "$EXTRACT/resources/app.asar"; then
    pass "the asar references $icon"
  else
    fail "the asar does not carry $icon"
  fi
done

# ---------------------------------------------------------------------------
# 3. The launcher's own fallback, and the guard that answers it.
#
#    electron-builder's generated `AppRun` adds `--no-sandbox` at runtime when
#    `unshare -Ur true` fails, with a comment saying it prefers starting
#    unsandboxed to crashing. That is upstream behaviour we cannot configure
#    away, so what is asserted is not its absence but that it is *answered*.
# ---------------------------------------------------------------------------
if grep -q -- '--no-sandbox' "$EXTRACT/AppRun"; then
  echo "  note  the launcher still has its own --no-sandbox fallback (upstream); the guard below is what answers it"
else
  pass "the launcher carries no --no-sandbox fallback"
fi

binary="$(find "$EXTRACT" -maxdepth 1 -type f -perm -u+x ! -name 'AppRun' ! -name '*.so*' ! -name 'chrome-sandbox' ! -name 'chrome_crashpad_handler' -print -quit)"
if [ -z "$binary" ]; then
  fail "could not find the packaged executable"
else
  # `ELECTRON_RUN_AS_NODE` turns the Electron binary into a plain Node, which
  # would make every launch below meaningless - it never reaches our code.
  refusal="$(timeout 90 env -u ELECTRON_RUN_AS_NODE "$binary" --no-sandbox 2>&1 || true)"
  status=0
  timeout 90 env -u ELECTRON_RUN_AS_NODE "$binary" --no-sandbox >/dev/null 2>&1 || status=$?

  if [ "$status" -eq 0 ]; then
    fail "launching with --no-sandbox succeeded: the app ran unsandboxed"
  else
    pass "launching with --no-sandbox is refused (exit $status)"
  fi
  if echo "$refusal" | grep -qi "flatpak"; then
    pass "the refusal names the sandboxed alternative"
  else
    fail "the refusal does not name a remedy: $refusal"
  fi
  if echo "$refusal" | grep -qi "userns\|user namespace"; then
    pass "the refusal names what was actually detected"
  else
    fail "the refusal does not say what was detected"
  fi
fi

# ---------------------------------------------------------------------------
# 4. The supported case: with user namespaces available the app must start, and
#    no process of it may carry the flag.
#
#    On a host without them there is nothing to assert here - and saying so is
#    the point, rather than reporting a pass for a case that never ran.
# ---------------------------------------------------------------------------
if unshare -Ur true 2>/dev/null; then
  home="$(mktemp -d)"
  port=18779
  set +e
  HERMES_GATEWAY_HOME="$home" HERMES_PORT="$port" \
    timeout 30 env -u ELECTRON_RUN_AS_NODE "$binary" >/dev/null 2>&1 &
  launched=$!
  sleep 20
  live=0
  flagged=""
  for pid in $(pgrep -P "$launched" 2>/dev/null) "$launched"; do
    [ -r "/proc/$pid/cmdline" ] || continue
    live=1
    if tr '\0' ' ' < "/proc/$pid/cmdline" | grep -q -- '--no-sandbox'; then
      flagged="$flagged $pid"
    fi
  done
  kill "$launched" 2>/dev/null
  wait "$launched" 2>/dev/null
  set -e
  rm -rf "$home"

  if [ "$live" -eq 0 ]; then
    fail "the app did not stay running on a host that does have user namespaces"
  elif [ -n "$flagged" ]; then
    fail "live processes carry --no-sandbox:$flagged"
  else
    pass "it runs, and no live process carries --no-sandbox"
  fi
else
  echo "  skip  the supported case: this host cannot create user namespaces, so a"
  echo "        sandboxed launch cannot be exercised here. It is checked on CI."
fi

echo
if [ "$failures" -eq 0 ]; then
  echo "Artifact checks passed."
else
  echo "$failures artifact check(s) failed." >&2
  exit 1
fi
