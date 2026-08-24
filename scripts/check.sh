#!/usr/bin/env bash
# Everything CI runs. Kept as a script so it is identical locally and in CI.
set -euo pipefail
cd "$(dirname "$0")/.."

# cargo is absent from PATH in a non-login shell, where rustup's installer line
# is never read - the script then dies as "command not found" before a single
# check has run. Sourcing rustup's own env file is idempotent and a no-op when
# ~/.cargo/bin is already on PATH, so a working shell and a CI runner with a
# toolchain action are both left untouched. If it is still missing, say which
# tool and why, rather than leaving the caller to decode exit 127.
if ! command -v cargo >/dev/null 2>&1 && [ -f "${HOME:-}/.cargo/env" ]; then
  . "${HOME:-}/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. Install rustup, or put ~/.cargo/bin on PATH." >&2
  exit 1
fi
echo "== fmt ==";     cargo fmt --all --check
echo "== clippy ==";  cargo clippy --workspace --all-targets -- -D warnings
# The opt-in variables are deliberately unset for this run. With them set,
# `--workspace` also picks up the real-engine and model-download tests, which
# then run at default parallelism (nine engines at once on a four-core box) and
# again in their own steps below. Unsetting them here is what makes the claim
# "no network, no model downloads" true of this step even on a machine that has
# both enabled.
echo "== test ==";    env -u HERMES_TEST_MODEL -u HERMES_TEST_NETWORK cargo test --workspace

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

# The model-download tier talks to HuggingFace and fetches a real 100 MB model,
# so it is opt-in like the real-engine tier. It says when it is skipped rather
# than passing quietly.
if [ -n "${HERMES_TEST_NETWORK:-}" ]; then
  echo "== test (model downloads) =="
  cargo test -p hermes-catalog --test real_download -- --test-threads=1
else
  echo "== test (model downloads) == skipped: set HERMES_TEST_NETWORK=1 to include"
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

# The control panel. Type-checked and built, because a panel that does not
# compile is a panel that cannot be served - and the gateway serves it from
# `--web-root`, so a broken build is a broken product rather than a broken
# convenience. Allowed to be absent, like every other optional tier: a Rust-only
# checkout has no `node_modules`, and it says so rather than passing quietly.
if [ -d frontend/node_modules ]; then
  echo "== frontend (typecheck and build) =="
  ( cd frontend && npm run build )
elif command -v npm >/dev/null 2>&1; then
  echo "== frontend == skipped: run \`npm install\` in frontend/ to include it"
else
  echo "== frontend == skipped: npm unavailable"
fi

echo "== deps ==";    ./scripts/check-deps.sh
echo "== secrets =="; ./scripts/check-secrets.sh
echo "All checks passed."
