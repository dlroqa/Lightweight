#!/usr/bin/env bash
# The bare `hermes` binary, as an archive.
#
# The desktop installers carry a copy of this binary too, but buried inside an
# application bundle. The service wrappers in `packaging/` install a binary at
# `~/.local/bin/hermes`, and that is what this archive is for: `hermes serve`
# in a terminal or under systemd, launchd or Task Scheduler, with no Electron
# and no window.
#
# Named by Rust target triple rather than by a marketing name, because the
# triple is what answers "will this run on my machine".
set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE="${1:-}"
if [ -z "$TRIPLE" ]; then
  TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
fi

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml \
  | sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -1)"
[ -n "$VERSION" ] || { echo "could not read the workspace version" >&2; exit 1; }

case "$TRIPLE" in
  *windows*) EXE=".exe" ;;
  *) EXE="" ;;
esac

# A host build lands in `target/release`; a cross build lands under the triple.
BINARY="target/$TRIPLE/release/hermes$EXE"
[ -f "$BINARY" ] || BINARY="target/release/hermes$EXE"
[ -f "$BINARY" ] || {
  echo "no binary at target/$TRIPLE/release/hermes$EXE or target/release/hermes$EXE" >&2
  echo "build it first: cargo build --release -p hermes-cli" >&2
  exit 1
}

NAME="hermes-$VERSION-$TRIPLE"
OUT="dist-cli"
STAGE="$OUT/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE"

cp "$BINARY" "$STAGE/hermes$EXE"
cp LICENSE "$STAGE/LICENSE"

# What someone who unpacked only this archive needs to know. Deliberately short:
# the engine not being in here is the one genuinely surprising fact.
ENGINE_BUILD="$(sed -n 's/.*PINNED_BUILD: &str = "\([^"]*\)".*/\1/p' \
  crates/hermes-backend-llamacpp/src/manifest.rs | head -1)"
cat > "$STAGE/README.md" <<EOF
# Hermes $VERSION — $TRIPLE

A local CPU inference gateway. Run \`./hermes serve --help\` to start.

- **The inference engine is not in this archive.** Hermes downloads the pinned
  llama.cpp build (\`$ENGINE_BUILD\`) for this platform on first use, verifies it
  against a SHA-256 recorded in the source, and keeps it in its own cache
  directory. Nothing is compiled on your machine.
- **Nothing here is code-signed.** Verify the download against \`SHA256SUMS\`
  from the same release.
- Service wrapper examples for systemd, launchd and Task Scheduler are in
  \`packaging/\` in the repository.
EOF

mkdir -p "$OUT"
case "$TRIPLE" in
  *windows*)
    # Git Bash has no `zip`, and 7-Zip is not something to rely on being on a
    # runner. PowerShell is always there on Windows.
    ARCHIVE="$OUT/$NAME.zip"
    rm -f "$ARCHIVE"
    powershell -NoProfile -Command \
      "Compress-Archive -Path '$STAGE' -DestinationPath '$ARCHIVE'"
    ;;
  *)
    ARCHIVE="$OUT/$NAME.tar.gz"
    rm -f "$ARCHIVE"
    tar -czf "$ARCHIVE" -C "$OUT" "$NAME"
    ;;
esac

rm -rf "$STAGE"
echo "$ARCHIVE"
