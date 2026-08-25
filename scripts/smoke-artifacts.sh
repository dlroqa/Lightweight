#!/usr/bin/env bash
# Run every artifact this platform produced.
#
# "The file exists" is not evidence that anything works. Each artifact here is
# extracted or installed and then *executed*, because the failures that matter -
# a stale binary, a missing runtime library, a universal binary with one slice,
# an installer that unpacks to the wrong place - all produce a file of exactly
# the right name and size.
#
# Platform-scoped: it checks what this runner built, and says what it skipped.
set -euo pipefail
cd "$(dirname "$0")/.."

failures=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml \
  | sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -1)"

# ---------------------------------------------------------------------------
# The CLI archive, on every platform.
# ---------------------------------------------------------------------------
echo "== cli archive =="
archive="$(find dist-cli -maxdepth 1 \( -name '*.tar.gz' -o -name '*.zip' \) -print -quit 2>/dev/null || true)"
if [ -z "$archive" ]; then
  fail "no CLI archive in dist-cli/"
else
  work="$(mktemp -d)"
  case "$archive" in
    *.tar.gz) tar -xzf "$archive" -C "$work" ;;
    *.zip) unzip -q "$archive" -d "$work" ;;
  esac
  binary="$(find "$work" -name 'hermes' -o -name 'hermes.exe' | head -1)"
  if [ -n "$binary" ] && reported="$("$binary" --version 2>/dev/null)"; then
    pass "the archived binary runs ($reported)"
    case "$reported" in
      *"$VERSION"*) pass "and it is version $VERSION" ;;
      *) fail "it reports $reported, not $VERSION" ;;
    esac
  else
    fail "the archived binary does not run"
  fi
  [ -f "$(dirname "$binary")/LICENSE" ] && pass "the archive carries its LICENSE" \
    || fail "the archive has no LICENSE"
  rm -rf "$work"
fi

case "$(uname -s)" in
# ---------------------------------------------------------------------------
Darwin)
  echo "== dmg =="
  dmg="$(find apps/desktop/release -maxdepth 1 -name '*.dmg' -print -quit 2>/dev/null || true)"
  if [ -z "$dmg" ]; then
    fail "no DMG was produced"
  else
    mount="$(mktemp -d)"
    hdiutil attach "$dmg" -nobrowse -readonly -mountpoint "$mount" >/dev/null
    app="$(find "$mount" -maxdepth 1 -name '*.app' -print -quit)"
    if [ -z "$app" ]; then
      fail "the DMG contains no application"
    else
      pass "the DMG mounts and contains $(basename "$app")"

      # The whole point of a universal build: both slices, in the binary the
      # app actually runs. A DMG with one slice installs fine and fails on
      # half the machines it claims to support.
      staged="$app/Contents/Resources/bin/hermes"
      if [ -f "$staged" ]; then
        info="$(lipo -info "$staged" 2>&1 || true)"
        echo "        $info"
        case "$info" in
          *arm64*) pass "the bundled hermes has an arm64 slice" ;;
          *) fail "no arm64 slice: $info" ;;
        esac
        case "$info" in
          *x86_64*) pass "the bundled hermes has an x86_64 slice" ;;
          *) fail "no x86_64 slice: $info" ;;
        esac
        "$staged" --version >/dev/null 2>&1 \
          && pass "the bundled hermes runs on this machine ($(uname -m))" \
          || fail "the bundled hermes does not run"
      else
        fail "the app bundle carries no hermes binary"
      fi

      # What Gatekeeper actually says, captured rather than described. Nothing
      # here is signed, so this is expected to report an ad-hoc signature or a
      # rejection - the point is that the release notes quote a real run.
      echo "        --- codesign ---"
      codesign -dv --verbose=2 "$app" 2>&1 | sed 's/^/        /' || true
      echo "        --- spctl ---"
      spctl -a -vvv "$app" 2>&1 | sed 's/^/        /' || true
    fi
    hdiutil detach "$mount" >/dev/null || true
    rm -rf "$mount"
  fi
  ;;

# ---------------------------------------------------------------------------
Linux)
  echo "== appimage =="
  if [ -n "$(find apps/desktop/release -maxdepth 1 -name '*.AppImage' -print -quit 2>/dev/null || true)" ]; then
    ./scripts/test-artifacts.sh
  else
    echo "  skip  no AppImage was produced by this run"
  fi
  echo "  note  the glibc floor of this binary is $(objdump -T target/release/hermes 2>/dev/null \
          | grep -o 'GLIBC_[0-9.]*' | sort -V | tail -1)"
  ;;

# ---------------------------------------------------------------------------
MINGW* | MSYS* | CYGWIN* | Windows_NT)
  echo "== nsis =="
  setup="$(find apps/desktop/release -maxdepth 1 -name '*Setup*.exe' -print -quit 2>/dev/null || true)"
  if [ -z "$setup" ]; then
    fail "no NSIS installer was produced"
  else
    target="$(mktemp -d)"
    # `/S` is a silent install; `/D` must come last and be unquoted, which is
    # NSIS's own rule rather than a preference.
    "$setup" /S "/D=$(cygpath -w "$target")" || true
    # The installer returns before it has finished writing.
    for _ in $(seq 1 30); do
      [ -f "$target/Hermes.exe" ] && break
      sleep 1
    done
    if [ -f "$target/Hermes.exe" ]; then
      pass "the installer wrote an application"
    else
      fail "the installer produced no Hermes.exe"
    fi
    installed="$target/resources/bin/hermes.exe"
    if [ -f "$installed" ] && "$installed" --version >/dev/null 2>&1; then
      pass "the installed hermes runs ($("$installed" --version))"
    else
      fail "the installed hermes does not run"
    fi
    # Static CRT: a machine without the Visual C++ redistributable must not be
    # told about it by a missing-DLL dialog on first launch.
    if command -v dumpbin >/dev/null 2>&1; then
      echo "        --- dumpbin /dependents ---"
      dumpbin //dependents "$installed" 2>&1 | sed 's/^/        /' | head -20
    else
      echo "  note  dumpbin is not on PATH; the DLL dependency list was not captured"
    fi
    rm -rf "$target" || true
  fi
  ;;
esac

echo
if [ "$failures" -eq 0 ]; then
  echo "Artifact smoke tests passed."
else
  echo "$failures artifact smoke test(s) failed." >&2
  exit 1
fi
