#!/usr/bin/env bash
# Run the openai-SDK contract suite against the gateway.
#
# This is the test tier that points the *real* client library at a real
# gateway and asserts what the client ends up with, rather than what we
# intended to send. It needs Python and one package from PyPI, so it is
# separate from `cargo test` and skipped when the environment cannot support
# it - `scripts/check.sh` says so out loud rather than passing quietly.
set -euo pipefail
cd "$(dirname "$0")/.."

VENV="target/contract-venv"

if [ ! -x "$VENV/bin/pytest" ]; then
  echo "== creating the contract test environment =="
  python3 -m venv "$VENV"
  "$VENV/bin/pip" install --quiet --disable-pip-version-check openai pytest pyyaml
fi

echo "== building the mock gateway =="
cargo build -p hermes-gateway --features mock --bin hermes-mock-gateway

echo "== contract suite =="
exec "$VENV/bin/pytest" tests/contract -q "$@"
