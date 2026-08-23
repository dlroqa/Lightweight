#!/usr/bin/env bash
# Everything CI runs. Kept as a script so it is identical locally and in CI.
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== fmt ==";     cargo fmt --all --check
echo "== clippy ==";  cargo clippy --workspace --all-targets -- -D warnings
echo "== test ==";    cargo test --workspace

# Real-model header tests skip when their (gitignored) fixtures are absent.
# Fetch them if we can, and then demand them, so a green run cannot mean
# "the real-model checks quietly did nothing".
if ./scripts/fetch-real-headers.sh >/dev/null 2>&1; then
  echo "== test (real models) =="
  HERMES_REQUIRE_REAL_MODELS=1 cargo test --workspace --test real_models
else
  echo "== test (real models) == skipped: headers unavailable (offline?)"
fi
echo "== deps ==";    ./scripts/check-deps.sh
echo "All checks passed."
