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
# The real-engine tests need a GGUF model and download the pinned engine, so
# they are opt-in. Point HERMES_TEST_MODEL at a .gguf to include them.
if [ -n "${HERMES_TEST_MODEL:-}" ]; then
  echo "== test (real engine) =="
  cargo test -p hermes-backend-llamacpp --test real_engine -- --test-threads=1
else
  echo "== test (real engine) == skipped: set HERMES_TEST_MODEL to a .gguf to include"
fi

# The openai-SDK contract suite: the real client library against a real
# gateway, asserting what the client ends up with rather than what we sent.
# Needs python3 and one package from PyPI, so it is allowed to be absent - but
# it says so, rather than passing quietly.
if command -v python3 >/dev/null 2>&1; then
  echo "== test (openai contract suite) =="
  ./scripts/contract-test.sh
else
  echo "== test (openai contract suite) == skipped: python3 unavailable"
fi

echo "== deps ==";    ./scripts/check-deps.sh
echo "== secrets =="; ./scripts/check-secrets.sh
echo "All checks passed."
