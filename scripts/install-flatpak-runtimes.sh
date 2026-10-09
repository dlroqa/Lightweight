#!/usr/bin/env bash
# Install the Flatpak runtime, SDK and Electron BaseApp the bundle is built on,
# and refuse any that Flathub has marked end-of-life.
#
# The branches come from the `build.flatpak` block of apps/desktop/package.json,
# the one place electron-builder reads them from. CI and the release used to
# type `24.08` themselves, three copies that had to move together; they now
# call this script instead, so a runtime update is one edit.
#
# An end-of-life runtime still installs and builds, which is how 24.08 shipped
# with nothing but an `Info:` line in the log (issue #61). Here that notice is
# a failure: a runtime that no longer receives security fixes is not one to
# ship an application on.
#
# Needs node (for package.json) and flatpak. Writes the install output to
# $FLATPAK_INSTALL_LOG (default flatpak-install.log) for the CI artifact.
set -euo pipefail
cd "$(dirname "$0")/.."

runtime="$(node -p "require('./apps/desktop/package.json').build.flatpak.runtimeVersion")"
base="$(node -p "require('./apps/desktop/package.json').build.flatpak.baseVersion")"
log="${FLATPAK_INSTALL_LOG:-flatpak-install.log}"
echo "== flatpak runtime $runtime, Electron BaseApp $base (from apps/desktop/package.json) =="

flatpak remote-add --if-not-exists --user flathub \
  https://flathub.org/repo/flathub.flatpakrepo
flatpak install --user -y --noninteractive flathub \
  "org.freedesktop.Platform//$runtime" \
  "org.freedesktop.Sdk//$runtime" \
  "org.electronjs.Electron2.BaseApp//$base" 2>&1 | tee "$log"

failures=0
for ref in "org.freedesktop.Platform//$runtime" "org.freedesktop.Sdk//$runtime" \
           "org.electronjs.Electron2.BaseApp//$base"; do
  if flatpak info --user "$ref" >/dev/null 2>&1; then
    printf '  ok    installed %s\n' "$ref"
  else
    printf '  FAIL  not installed: %s\n' "$ref"
    failures=$((failures + 1))
  fi
done

# Flatpak prints `Info: runtime <id> branch <b> is end-of-life, with reason:`
# for the runtime and for extensions it pulls in (the GL driver, for one).
if grep -qi 'end-of-life' "$log"; then
  printf '  FAIL  Flathub marks an installed runtime end-of-life:\n'
  grep -i -A2 'end-of-life' "$log" | sed 's/^/        /'
  failures=$((failures + 1))
else
  printf '  ok    no installed runtime or extension is marked end-of-life\n'
fi

[ "$failures" -eq 0 ] || exit 1
