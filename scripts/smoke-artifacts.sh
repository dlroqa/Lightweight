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
    *.zip)
      # The separator inside the zip, asserted rather than assumed. The format
      # specifies forward slash; Windows PowerShell's `Compress-Archive` wrote
      # backslashes, which made every entry one file with a backslash in its
      # name rather than a path. `unzip` only warns about it, and a warning is
      # exit status 1, so the first symptom was this script dying with no
      # message at all.
      if unzip -l "$archive" | grep -q '\\'; then
        fail "the zip uses backslash separators; it would unpack to one file"
      else
        pass "the zip uses the separator the format specifies"
      fi
      # Exit 1 is `unzip`'s warning status, not a failure; 2 and above are.
      unzip -q "$archive" -d "$work" || [ "$?" -le 1 ]
      ;;
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
    # The sandboxed-launch check inside opens a real window, and a release
    # runner has no display. Supplying one here rather than in the workflow
    # keeps the rule the gate is built on: the script is the contract, and a
    # workflow that wrapped it would be a second place to keep in step.
    if [ -z "${DISPLAY:-}" ] && command -v xvfb-run >/dev/null 2>&1; then
      xvfb-run --auto-servernum ./scripts/test-artifacts.sh
    else
      ./scripts/test-artifacts.sh
    fi
  else
    echo "  skip  no AppImage was produced by this run"
  fi
  # Wherever `cargo build` put it: with `--target` that is under the triple,
  # and the release builds that way. Reading only `target/release` printed an
  # empty floor into the release notes, which is worse than printing none.
  triple="$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')"
  binary="target/${triple:-none}/release/hermes"
  [ -f "$binary" ] || binary="target/release/hermes"
  if [ -f "$binary" ]; then
    echo "  note  the glibc floor of $binary is $(objdump -T "$binary" 2>/dev/null \
            | grep -o 'GLIBC_[0-9.]*' | sort -V | tail -1)"
  else
    echo "  note  no release binary on disk; the glibc floor was not read"
  fi
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
    #
    # `MSYS2_ARG_CONV_EXCL='*'` is what makes those flags survive the trip.
    # Git Bash rewrites any argument that looks like a POSIX path into a Windows
    # one before the process sees it, so `/S` arrived as something like
    # `C:/Program Files/Git/S` and NSIS never saw a silent switch at all: it
    # opened its **interactive installer** and waited for a click that a headless
    # runner is never going to produce. Both symptoms this line has shown come
    # out of that one cause - waited on, it hung for forty minutes until the job
    # was cancelled; started in the background, it wrote nothing and the poll
    # below timed out. Neither was the installer's fault.
    #
    # Started rather than waited on, because it does not come back even when it
    # works: electron-builder's NSIS launches the application once it has
    # finished (`runAfterFinish`, on by default), and this application is a
    # gateway that keeps running. What says the install worked is the files
    # appearing, which is what the loop waits for.
    MSYS2_ARG_CONV_EXCL='*' "$setup" /S "/D=$(cygpath -w "$target")" &
    installer=$!
    # The per-user location the installer falls back to if `/D` is not honoured.
    # Checked as well as `$target` so that "it installed, elsewhere" is a
    # different sentence from "it installed nothing".
    fallback="${LOCALAPPDATA:-$HOME/AppData/Local}/Programs/Hermes"
    # Wait for the *bundled* binary, `resources/bin/hermes.exe`, not the
    # top-level `Hermes.exe` launcher. NSIS extracts the two separately and in
    # no guaranteed order, so the launcher can land a beat before the resources
    # do. Gating on the launcher and then, with no second wait, running the
    # bundled binary is a race: once it broke the instant the launcher appeared
    # and the `-f` on `resources/bin/hermes.exe` failed in the same millisecond,
    # reported as "the installed hermes does not run". The file the run
    # assertion below actually needs is the one the loop must wait for.
    for _ in $(seq 1 180); do
      [ -f "$target/resources/bin/hermes.exe" ] && break
      [ -f "$fallback/resources/bin/hermes.exe" ] && break
      sleep 1
    done
    if [ -f "$target/resources/bin/hermes.exe" ]; then
      pass "the installer wrote an application"
    elif [ -f "$fallback/resources/bin/hermes.exe" ]; then
      pass "the installer wrote an application (at its default location)"
      target="$fallback"
    else
      fail "the installer produced no resources/bin/hermes.exe"
      # Said here rather than guessed at from a bare failure next time.
      if kill -0 "$installer" 2>/dev/null; then
        echo "        the installer is still running; it is not installing silently"
      else
        status=0
        wait "$installer" 2>/dev/null || status=$?
        echo "        the installer exited $status"
      fi
      echo "        asked for: $(cygpath -w "$target")"
      echo "        --- what it left there ---"
      { ls -la "$target" 2>&1 || true; } | sed 's/^/        /' | head -10
      echo "        --- and at the default location ---"
      { ls -la "$fallback" 2>&1 || true; } | sed 's/^/        /' | head -10
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

    # Everything the installer started, stopped. The application it launches on
    # finishing keeps a gateway alive and holds the installer open behind it;
    # left running, both survive the step as orphans and the directory below
    # cannot be removed because its files are still open.
    taskkill //F //IM Hermes.exe >/dev/null 2>&1 || true
    taskkill //F //IM "$(basename "$setup")" >/dev/null 2>&1 || true
    kill "$installer" 2>/dev/null || true
    wait "$installer" 2>/dev/null || true
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
