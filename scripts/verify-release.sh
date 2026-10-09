#!/usr/bin/env bash
# Verify a release's files: the inventory, SHA256SUMS, and each binary's
# build-provenance attestation, against the actual downloaded bytes.
#
#   scripts/verify-release.sh <dir> <tag> <commit> [<github-digests-file>]
#
# <dir> holds the release's files, flat. <tag> is the release tag (v0.8.0) and
# <commit> the full SHA the tag points at. The optional digests file has one
# `<sha256>  <name>` line per asset, as GitHub reports them for a release; when
# given, each computed hash must also equal GitHub's.
#
# Every binary must carry an attestation that:
#   - names this file's exact SHA-256 as its subject,
#   - was signed through GitHub Actions' OIDC issuer, on a GitHub-hosted runner,
#   - by this repository's .github/workflows/release.yml,
#   - built from <tag> (refs/tags/<tag>) at <commit>.
# `gh attestation verify` checks the Sigstore signature, the certificate chain
# and the transparency log; the flags bind the identity. Nothing here executes
# a downloaded file, and nothing here needs more than read access.
#
# Exit 0 only when every check passes.
set -euo pipefail

dir="${1:?usage: verify-release.sh <dir> <tag> <commit> [<github-digests-file>]}"
tag="${2:?tag}"
commit="${3:?commit}"
digests="${4:-}"
repo="${GITHUB_REPOSITORY:-dlroqa/Lightweight}"
version="${tag#v}"
failures=0

pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }

if ! [[ "$commit" =~ ^[0-9a-f]{40}$ ]]; then
  echo "commit must be a full 40-character SHA, got: $commit" >&2
  exit 2
fi

# The seven binaries release.yml builds and attests, plus the checksum file.
binaries=(
  "Lightweight-$version-mac-universal.dmg"
  "Lightweight-Setup-$version-x64.exe"
  "Lightweight-$version-linux-x86_64.AppImage"
  "Lightweight-$version-linux-x86_64.flatpak"
  "hermes-$version-aarch64-apple-darwin.tar.gz"
  "hermes-$version-x86_64-pc-windows-msvc.zip"
  "hermes-$version-x86_64-unknown-linux-gnu.tar.gz"
)

echo "== release $tag at $commit ($repo) =="

# ---------------------------------------------------------------------------
# 1. Inventory: exactly the eight expected files, nothing missing, nothing else.
# ---------------------------------------------------------------------------
echo "== inventory =="
expected="$(printf '%s\n' "${binaries[@]}" SHA256SUMS | sort)"
actual="$(find "$dir" -maxdepth 1 -type f -printf '%f\n' | sort)"
for name in "${binaries[@]}" SHA256SUMS; do
  if [ -f "$dir/$name" ]; then
    pass "present: $name ($(stat -c %s "$dir/$name") bytes)"
  else
    fail "missing: $name"
  fi
done
extra="$(comm -13 <(echo "$expected") <(echo "$actual"))"
if [ -n "$extra" ]; then
  fail "unexpected files: $(echo "$extra" | tr '\n' ' ')"
else
  pass "no unexpected files (8 of 8)"
fi
if find "$dir" -mindepth 1 -type d | grep -q .; then
  fail "subdirectories present; the release is a flat set of files"
fi

# ---------------------------------------------------------------------------
# 2. SHA256SUMS: well formed, exactly the seven binaries, and every hash equal
#    to one computed here from the bytes.
# ---------------------------------------------------------------------------
echo "== SHA256SUMS =="
if [ -f "$dir/SHA256SUMS" ]; then
  sums="$dir/SHA256SUMS"
  malformed="$(grep -cvE '^[0-9a-f]{64}  [^/ ]+$' "$sums" || true)"
  if [ "$malformed" -eq 0 ]; then pass "every line is '<sha256>  <name>'"; else fail "$malformed malformed line(s)"; fi
  listed="$(awk '{print $2}' "$sums" | sort)"
  if [ "$listed" = "$(printf '%s\n' "${binaries[@]}" | sort)" ]; then
    pass "lists exactly the seven binaries, once each"
  else
    fail "lists $(echo "$listed" | tr '\n' ' ')"
  fi
else
  sums=""
fi

declare -A computed
for name in "${binaries[@]}"; do
  [ -f "$dir/$name" ] || continue
  computed[$name]="$(sha256sum "$dir/$name" | cut -d' ' -f1)"
  if [ -n "$sums" ]; then
    claimed="$(awk -v n="$name" '$2==n {print $1}' "$sums")"
    if [ "$claimed" = "${computed[$name]}" ]; then
      pass "sha256 matches SHA256SUMS: $name ${computed[$name]}"
    else
      fail "sha256 mismatch: $name computed ${computed[$name]}, SHA256SUMS says ${claimed:-nothing}"
    fi
  fi
done

if [ -n "$digests" ]; then
  echo "== GitHub asset digests =="
  for name in "${binaries[@]}" SHA256SUMS; do
    [ -f "$dir/$name" ] || continue
    mine="${computed[$name]:-$(sha256sum "$dir/$name" | cut -d' ' -f1)}"
    theirs="$(awk -v n="$name" '$2==n {print $1}' "$digests")"
    if [ "$theirs" = "$mine" ]; then
      pass "GitHub digest matches: $name"
    else
      fail "GitHub digest for $name is ${theirs:-absent}, computed $mine"
    fi
  done
fi

# ---------------------------------------------------------------------------
# 3. Attestations: verified against the downloaded bytes, bound to this
#    repository, the release workflow, the tag and the commit.
# ---------------------------------------------------------------------------
echo "== attestations =="
for name in "${binaries[@]}"; do
  [ -f "$dir/$name" ] || continue
  if out="$(gh attestation verify "$dir/$name" \
      --repo "$repo" \
      --signer-workflow "$repo/.github/workflows/release.yml" \
      --source-ref "refs/tags/$tag" \
      --source-digest "$commit" \
      --cert-oidc-issuer https://token.actions.githubusercontent.com \
      --predicate-type https://slsa.dev/provenance/v1 \
      --deny-self-hosted-runners \
      --format json 2>&1)"; then
    # Belt and braces over gh's own checks: the statement's subject is this
    # file's hash, and the certificate names the commit.
    if echo "$out" | jq -e --arg d "${computed[$name]}" --arg c "$commit" '
        length > 0 and all(.[];
          ([.verificationResult.statement.subject[].digest.sha256] | index($d)) != null
          and .verificationResult.signature.certificate.sourceRepositoryDigest == $c)
      ' >/dev/null 2>&1; then
      pass "attestation verified: $name (release.yml @ refs/tags/$tag, $commit)"
    else
      fail "attestation for $name verified but does not name this digest and commit"
    fi
  else
    fail "attestation did not verify: $name: $(echo "$out" | tail -n 3 | tr '\n' ' ')"
  fi
done

echo
if [ "$failures" -eq 0 ]; then
  echo "Release $tag verified: 8 files, 7 checksums, 7 attestations."
else
  echo "$failures release check(s) failed." >&2
  exit 1
fi
