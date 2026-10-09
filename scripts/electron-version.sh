#!/usr/bin/env bash
# The Electron version a built application carries, read from its own bytes.
#
# The lockfile says what `npm ci` installed and what the dependency advisories
# were checked against (`scripts/check-advisories.py`); this says what
# electron-builder actually packaged. Electron compiles its user-agent string
# in - "Chrome/<version> Electron/<version>" - so the version can be read
# without starting the application, which matters for an installer on a
# headless runner and for the half of a universal DMG this machine cannot run.
#
#   electron-version.sh <file>   the version inside an executable
#   electron-version.sh <dir>    ... inside whichever file directly in <dir>
#                                carries it (the name differs per platform)
#   electron-version.sh <x.app>  ... inside its Electron Framework
#   electron-version.sh --locked the version apps/desktop/package-lock.json pins
#
# Prints the version alone, or says on stderr why it found none and exits 1.
set -euo pipefail

target="${1:?usage: electron-version.sh <file|dir|app> | --locked}"

if [ "$target" = --locked ]; then
  cd "$(dirname "$0")/.."
  exec node -p 'require("./apps/desktop/package-lock.json").packages["node_modules/electron"].version'
fi

read_version() {
  local found
  found="$(LC_ALL=C grep -m1 -aoE 'Chrome/[0-9.]+ Electron/[0-9]+\.[0-9]+\.[0-9]+' "$1" 2>/dev/null || true)"
  found="${found%%$'\n'*}"
  [ -n "$found" ] || return 1
  echo "${found##*Electron/}"
}

if [ -d "$target/Contents/Frameworks/Electron Framework.framework" ]; then
  target="$target/Contents/Frameworks/Electron Framework.framework/Electron Framework"
fi

if [ -f "$target" ]; then
  read_version "$target" && exit 0
  echo "no Electron version string in $target" >&2
  exit 1
fi

if [ -d "$target" ]; then
  # Largest first: the Electron executable is by far the biggest file beside
  # it, so the right one is usually the first one read.
  while IFS= read -r file; do
    read_version "$file" && exit 0
  done < <(find "$target" -maxdepth 1 -type f -size +1M -exec ls -S {} + 2>/dev/null)
  echo "no file directly in $target carries an Electron version string" >&2
  exit 1
fi

echo "no such file or directory: $target" >&2
exit 1
