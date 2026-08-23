#!/usr/bin/env bash
# Enforce the dependency policy declared in the workspace Cargo.toml.
#
# The target machine has no sudo, and therefore no cmake, no libssl-dev and no
# libclang. Each banned crate below pulls in one of those at build time, so a
# single careless `cargo add` makes the workspace unbuildable there - and the
# failure surfaces as an inscrutable build-script error, not as "you broke the
# policy". This script makes it say the latter.
set -uo pipefail

# crate name -> why it is banned
BANNED_NAMES=(aws-lc-sys aws-lc-rs openssl-sys native-tls bindgen)
BANNED_WHY=(
  "requires cmake to build (rustls default provider; pin \`ring\` instead)"
  "pulls in aws-lc-sys, which requires cmake (pin \`ring\` instead)"
  "requires libssl-dev, which is not installable without sudo"
  "pulls in openssl-sys; use rustls end to end"
  "requires libclang, which is not installable without sudo"
)

status=0
for i in "${!BANNED_NAMES[@]}"; do
  crate="${BANNED_NAMES[$i]}"
  # `cargo tree -i` exits non-zero when the package is absent, which is the
  # outcome we want. Only a successful lookup is a policy violation.
  if tree=$(cargo tree --workspace --all-features -i "$crate" 2>/dev/null); then
    echo "POLICY VIOLATION: '$crate' is in the dependency graph."
    echo "  Reason it is banned: ${BANNED_WHY[$i]}"
    echo "  Pulled in by:"
    echo "$tree" | sed 's/^/    /' | head -20
    echo
    status=1
  else
    printf '  ok  %-14s absent\n' "$crate"
  fi
done

if [ "$status" -eq 0 ]; then
  echo "Dependency policy satisfied."
fi
exit "$status"
