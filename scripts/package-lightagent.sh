#!/usr/bin/env bash
# The `lightagent` binary, as an archive.
#
# Lightagent is a distinct product surface from the inference gateway: a terminal
# agent harness that drives a running gateway over HTTP. `package-cli.sh` beside
# this one ships `hermes`/`lightweight`; this ships `lightagent`, so the two can
# be released and installed independently. The service wrapper in
# `packaging/systemd/lightagent.service` installs a binary at
# `~/.local/bin/lightagent`, and that is what this archive is for.
#
# Named by Rust target triple rather than a marketing name, because the triple is
# what answers "will this run on my machine".
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
BINARY="target/$TRIPLE/release/lightagent$EXE"
[ -f "$BINARY" ] || BINARY="target/release/lightagent$EXE"
[ -f "$BINARY" ] || {
  echo "no binary at target/$TRIPLE/release/lightagent$EXE or target/release/lightagent$EXE" >&2
  echo "build it first: cargo build --release -p lightagent" >&2
  exit 1
}

NAME="lightagent-$VERSION-$TRIPLE"
OUT="dist-cli"
STAGE="$OUT/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE"

cp "$BINARY" "$STAGE/lightagent$EXE"
cp LICENSE "$STAGE/LICENSE"

# The systemd example travels with the binary on Linux, so an operator has the
# service unit in the same place as the thing it runs. Best-effort: absent on a
# checkout without it, which is not this repository but keeps the script robust.
if [ -f packaging/systemd/lightagent.service ]; then
  cp packaging/systemd/lightagent.service "$STAGE/lightagent.service"
  cp packaging/systemd/lightagent.env.example "$STAGE/lightagent.env.example"
fi

# What someone who unpacked only this archive needs to know. Deliberately short:
# that Lightagent needs a separate gateway is the one genuinely surprising fact.
cat > "$STAGE/README.md" <<EOF
# Lightagent $VERSION — $TRIPLE

A local agent harness with live tools. Run \`./lightagent --help\`, then
\`./lightagent init\` to set up its isolated home, and \`./lightagent chat\`.

- **Lightagent needs a running inference gateway.** It talks to an
  OpenAI-compatible gateway (such as \`hermes serve\`) over HTTP; it does not hold
  a model itself. Point it at one with
  \`./lightagent config set inference.base_url http://127.0.0.1:11434\`, then
  check it with \`./lightagent doctor\`.
- **Nothing here is code-signed.** Verify the download against \`SHA256SUMS\` from
  the same release.
- To run the API as a service on Linux, \`lightagent.service\` and
  \`lightagent.env.example\` are included here (and in \`packaging/systemd/\` in the
  repository). On macOS and Windows, run \`./lightagent serve\` from a terminal —
  no launchd or Task Scheduler example is shipped, because none has been tested.
EOF

mkdir -p "$OUT"
case "$TRIPLE" in
  *windows*)
    # Git Bash has no `zip`, and 7-Zip is not something to rely on being on a
    # runner. `pwsh` and `ZipFile::CreateFromDirectory` (not `Compress-Archive`,
    # which on Windows PowerShell writes backslash separators that make a
    # malformed zip) — the same choice `package-cli.sh` documents at length.
    ARCHIVE="$OUT/$NAME.zip"
    rm -f "$ARCHIVE"
    command -v pwsh >/dev/null 2>&1 || {
      echo "pwsh (PowerShell 7+) is needed to write a well-formed zip" >&2
      exit 1
    }
    pwsh -NoProfile -Command \
      "Add-Type -AssemblyName System.IO.Compression.FileSystem; \
       [System.IO.Compression.ZipFile]::CreateFromDirectory( \
         (Resolve-Path '$STAGE').Path, \
         (Join-Path (Resolve-Path '$OUT').Path '$NAME.zip'), \
         [System.IO.Compression.CompressionLevel]::Optimal, \
         \$true)"
    ;;
  *)
    ARCHIVE="$OUT/$NAME.tar.gz"
    rm -f "$ARCHIVE"
    tar -czf "$ARCHIVE" -C "$OUT" "$NAME"
    ;;
esac

rm -rf "$STAGE"
echo "$ARCHIVE"
