#!/usr/bin/env bash
# Build the Lightagent archive and run what it holds.
#
# "The file exists" is not evidence that anything works. The archive is built,
# extracted, and then its binary is *executed*, because the failures that matter
# — a stale binary, a missing shared library, a package that unpacks to the wrong
# place — all produce a file of exactly the right name and size. This mirrors
# `scripts/smoke-artifacts.sh` for the one archive `scripts/package-lightagent.sh`
# produces, and it is self-contained: it builds the binary if one is not present.
set -euo pipefail
cd "$(dirname "$0")/.."

failures=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml \
  | sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -1)"
TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
case "$TRIPLE" in
  *windows*) EXE=".exe" ;;
  *) EXE="" ;;
esac

echo "== build =="
if [ ! -f "target/release/lightagent$EXE" ] && [ ! -f "target/$TRIPLE/release/lightagent$EXE" ]; then
  cargo build --release -p lightagent
fi
pass "release binary present"

echo "== package =="
archive="$(bash ./scripts/package-lightagent.sh "$TRIPLE")"
if [ ! -f "$archive" ]; then
  fail "package-lightagent.sh produced no archive"
  echo "$failures failure(s)."; exit 1
fi
pass "archive built: $archive"

work="$(mktemp -d)"
case "$archive" in
  *.tar.gz) tar -xzf "$archive" -C "$work" ;;
  *.zip)
    # The separator inside the zip, asserted rather than assumed (see
    # smoke-artifacts.sh for why a backslash separator is a silent disaster).
    if unzip -l "$archive" | grep -q '\\'; then
      fail "the zip uses backslash separators; it would unpack to one file"
    fi
    unzip -q "$archive" -d "$work" ;;
esac

root="$work/lightagent-$VERSION-$TRIPLE"
[ -d "$root" ] && pass "unpacks to a named directory" || fail "no lightagent-$VERSION-$TRIPLE/ in the archive"

for f in "lightagent$EXE" LICENSE README.md lightagent.service lightagent.env.example; do
  [ -f "$root/$f" ] && pass "contains $f" || fail "missing $f"
done

echo "== run =="
bin="$root/lightagent$EXE"
chmod +x "$bin" 2>/dev/null || true
"$bin" --version >/dev/null 2>&1 && pass "--version runs" || fail "--version failed"
"$bin" banner --preview 2>&1 | grep -q $'▀' && pass "banner renders" || fail "banner did not render"

# `doctor` in a throwaway home: it must succeed and report the engine as not
# reachable (no gateway here), never hang or crash.
home="$(mktemp -d)"
if LIGHTAGENT_HOME="$home" "$bin" doctor 2>&1 | grep -qi "not reachable"; then
  pass "doctor runs and reports no gateway"
else
  fail "doctor did not run cleanly against no gateway"
fi
rm -rf "$home" "$work"

echo
if [ "$failures" -eq 0 ]; then
  echo "All Lightagent package checks passed."
else
  echo "$failures failure(s)."
  exit 1
fi
