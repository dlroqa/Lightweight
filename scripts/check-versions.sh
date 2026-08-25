#!/usr/bin/env bash
# One version, in four places that do not derive it from each other.
#
# The workspace, the desktop shell and the panel each carry their own `version`,
# and the installer filenames come from the desktop shell's. Nothing keeps them
# in step, so a release could ship `Hermes-0.1.0.dmg` around a `hermes 0.2.0` -
# a discrepancy nobody would notice until someone tried to reproduce a bug
# against the wrong source.
#
# When run on a tag, the tag has to agree too.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0

workspace="$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml \
  | sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -1)"
desktop="$(node -p "require('./apps/desktop/package.json').version" 2>/dev/null || echo "")"
panel="$(node -p "require('./frontend/package.json').version" 2>/dev/null || echo "")"

echo "  workspace       $workspace"
echo "  apps/desktop    $desktop"
echo "  frontend        $panel"

if [ -z "$workspace" ] || [ -z "$desktop" ] || [ -z "$panel" ]; then
  echo "could not read every version (is node installed?)" >&2
  exit 1
fi

if [ "$workspace" != "$desktop" ] || [ "$workspace" != "$panel" ]; then
  echo "  FAIL  they disagree" >&2
  fail=1
fi

# On a tag, the name of the release must be the thing being released.
if [ "${GITHUB_REF_TYPE:-}" = "tag" ]; then
  tag="${GITHUB_REF_NAME:-}"
  echo "  tag             $tag"
  if [ "$tag" != "v$workspace" ]; then
    echo "  FAIL  the tag is $tag but the workspace is $workspace (expected v$workspace)" >&2
    fail=1
  fi
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "Versions agree."
