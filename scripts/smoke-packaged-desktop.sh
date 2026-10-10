#!/usr/bin/env bash
# The packaged desktop app, with a Router beside its Gateway: the release gate.
#
#   scripts/smoke-packaged-desktop.sh appimage|flatpak|nsis|dmg <package>
#
# Takes one package exactly as the release ships it, gets it running the way a
# user would - the AppImage extracted, the Flatpak installed, the Windows
# installer run silently, the DMG mounted - and runs scripts/render-desktop.sh
# against the *packaged* app: its own bundled hermes, its own panel, its own
# Electron with the sandbox on. Both modes run (the app starts the Router; the
# app attaches to one already serving), with scripted Jev and nodes, so no real
# key or external service is involved. It checks the version, the Gateway
# window, Router & Jev Settings in its own window, Test Connection, an
# authorized settings save with the admin token, ownership on quit, and that no
# key or token leaks. Nothing is built here.
#
# Linux needs a display (run under xvfb-run) and a host that allows Chromium's
# sandbox (`sysctl kernel.apparmor_restrict_unprivileged_userns=0` on Ubuntu
# 24.04), exactly as scripts/render-desktop.sh does. The e2e package's
# dependencies must be installed (`npm ci --prefix e2e`).
#
# Environment: OUT_DIR (default e2e/desktop-screens/<kind>), DESKTOP_PORT_BASE.
set -euo pipefail
cd "$(dirname "$0")/.."

kind="${1:?usage: $0 appimage|flatpak|nsis|dmg <package>}"
package="${2:?usage: $0 $kind <package>}"
[ -f "$package" ] || { echo "no package at $package" >&2; exit 1; }
package="$(cd "$(dirname "$package")" && pwd)/$(basename "$package")"
export OUT_DIR="${OUT_DIR:-e2e/desktop-screens/$kind}"

PRODUCT="$(node -e 'process.stdout.write(require("./apps/desktop/package.json").build.productName)')"
STAGE="$(mktemp -d)"
cleanup_steps=()
cleanup() {
  local status=$?
  for step in "${cleanup_steps[@]}"; do eval "$step" || true; done
  rm -rf "$STAGE" || true
  exit "$status"
}
trap cleanup EXIT

case "$kind" in
  appimage)
    # Extracted with the runtime's own entry point (a runner has no FUSE), and
    # started through its own AppRun, which is what the AppImage runs. AppRun
    # adds --no-sandbox by itself when it cannot create a user namespace; the
    # app then refuses to start, so a lifted sysctl is required, not optional.
    chmod +x "$package"
    ( cd "$STAGE" && "$package" --appimage-extract >/dev/null )
    root="$STAGE/squashfs-root"
    export DESKTOP_EXECUTABLE="$root/AppRun"
    export DESKTOP_HERMES="$root/resources/bin/hermes"
    export DESKTOP_PANEL="$root/resources/panel"
    ;;

  flatpak)
    # Installed for this user, then run by `flatpak run`, which is the only way
    # a user starts it. Its sandbox sees the host's loopback (`--share=network`)
    # but not the host's /tmp, so every file the app and the test share lives
    # in the app's own data directory, which has the same path on both sides.
    app_id="$(node -e 'process.stdout.write(require("./apps/desktop/package.json").build.appId.replace(/-/g,"_").replace(/[^a-zA-Z0-9._]/g,""))')"
    if ! flatpak info --user "$app_id" >/dev/null 2>&1; then
      flatpak install --user -y --noninteractive --bundle "$package"
    fi
    data="$HOME/.var/app/$app_id/data/desktop-smoke"
    rm -rf "$data"
    mkdir -p "$data"
    cleanup_steps+=("flatpak kill '$app_id' >/dev/null 2>&1" "rm -rf '$data'")
    export DESKTOP_HOME_BASE="$data"
    inside="/app/lib/$app_id/resources"
    forward='--env=HERMES_GATEWAY_HOME="${HERMES_GATEWAY_HOME:-}" --env=HERMES_PORT="${HERMES_PORT:-}" --env=HERMES_ROUTER_PORT="${HERMES_ROUTER_PORT:-}" --env=TYPESAFE_API_KEY="${TYPESAFE_API_KEY:-}"'
    # zypak, the sandbox helper inside, talks to the session bus; a runner has
    # none, so the launch gets one, as scripts/test-flatpak.sh does.
    cat >"$STAGE/flatpak-app" <<EOF
#!/usr/bin/env bash
bus=()
if [ -z "\${DBUS_SESSION_BUS_ADDRESS:-}" ] && command -v dbus-run-session >/dev/null 2>&1; then bus=(dbus-run-session --); fi
exec "\${bus[@]}" flatpak run $forward "$app_id" "\$@"
EOF
    cat >"$STAGE/flatpak-hermes" <<EOF
#!/usr/bin/env bash
exec flatpak run $forward --command="$inside/bin/hermes" "$app_id" "\$@"
EOF
    chmod +x "$STAGE/flatpak-app" "$STAGE/flatpak-hermes"
    export DESKTOP_EXECUTABLE="$STAGE/flatpak-app"
    export DESKTOP_HERMES="$STAGE/flatpak-hermes"
    export DESKTOP_PANEL="$inside/panel"
    ;;

  nsis)
    # Silently, into a directory of our own, as scripts/smoke-artifacts.sh
    # installs it: `/S` must reach NSIS unconverted and `/D` must come last.
    target="$STAGE/installed"
    mkdir -p "$target"
    MSYS2_ARG_CONV_EXCL='*' "$package" /S "/D=$(cygpath -w "$target")" &
    installer=$!
    for _ in $(seq 1 180); do
      [ -f "$target/resources/bin/hermes.exe" ] && [ -f "$target/$PRODUCT.exe" ] && break
      sleep 1
    done
    wait "$installer" 2>/dev/null || true
    [ -f "$target/$PRODUCT.exe" ] || { echo "the installer wrote no $PRODUCT.exe" >&2; ls -la "$target" >&2; exit 1; }
    # electron-builder's installer starts the app once it has installed; that
    # instance is not the one under test.
    sleep 5
    taskkill //F //IM "$PRODUCT.exe" >/dev/null 2>&1 || true
    taskkill //F //IM hermes.exe >/dev/null 2>&1 || true
    sleep 2
    cleanup_steps+=("taskkill //F //IM '$PRODUCT.exe' >/dev/null 2>&1")
    export DESKTOP_EXECUTABLE="$target/$PRODUCT.exe"
    export DESKTOP_HERMES="$target/resources/bin/hermes.exe"
    export DESKTOP_PANEL="$target/resources/panel"
    ;;

  dmg)
    # Mounted read-only and run from the mount, as a user who opens the DMG and
    # double-clicks would. On an Intel runner this runs the x86-64 half of the
    # universal app; on Apple Silicon, the arm64 half.
    mount="$STAGE/mount"
    mkdir -p "$mount"
    hdiutil attach "$package" -nobrowse -readonly -mountpoint "$mount" >/dev/null
    cleanup_steps+=("hdiutil detach '$mount' -force >/dev/null 2>&1")
    app="$mount/$PRODUCT.app"
    echo "this runner is $(uname -m); $(lipo -archs "$app/Contents/MacOS/$PRODUCT" 2>/dev/null || echo '?')"
    export DESKTOP_EXECUTABLE="$app/Contents/MacOS/$PRODUCT"
    export DESKTOP_HERMES="$app/Contents/Resources/bin/hermes"
    export DESKTOP_PANEL="$app/Contents/Resources/panel"
    ;;

  *)
    echo "unknown package kind: $kind" >&2
    exit 2
    ;;
esac

for path in "$DESKTOP_EXECUTABLE"; do
  [ -e "$path" ] || { echo "the package has no $path" >&2; exit 1; }
done
if [ "$kind" != flatpak ]; then
  for path in "$DESKTOP_HERMES" "$DESKTOP_PANEL/index.html"; do
    [ -e "$path" ] || { echo "the package has no $path" >&2; exit 1; }
  done
fi

echo "== $kind: $(basename "$package") =="
bash ./scripts/render-desktop.sh
