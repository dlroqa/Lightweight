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

# Windows puts a venv's executables in `Scripts/` and everything else uses
# `bin/`; the interpreter is `python3` on Unix and usually just `python` on a
# Windows runner. Both are resolved here rather than in `check.sh`, so the
# tier either runs on all four platforms or says which one it could not run on.
case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW* | MSYS* | CYGWIN* | Windows_NT) VENV_BIN="$VENV/Scripts"; EXE=".exe" ;;
  *) VENV_BIN="$VENV/bin"; EXE="" ;;
esac

PYTHON=""
for candidate in python3 python; do
  if command -v "$candidate" >/dev/null 2>&1; then PYTHON="$candidate"; break; fi
done
if [ -z "$PYTHON" ]; then
  echo "no python interpreter found (tried python3, python)" >&2
  exit 1
fi

if [ ! -x "$VENV_BIN/pytest$EXE" ]; then
  echo "== creating the contract test environment =="
  # `--clear` because this lives under `target/`, which CI caches: a restored
  # half of a virtualenv has a `pyvenv.cfg` and no `bin/`, and `venv` on top of
  # that leaves it exactly as broken as it found it. That is not hypothetical -
  # it is how the Linux gate failed on its second CI run, with
  # `target/contract-venv/bin/pip: No such file or directory`.
  "$PYTHON" -m venv --clear "$VENV"
  # A Python built without `ensurepip` produces a venv with no pip at all, and
  # the failure is worth naming rather than leaving as "no such file".
  if [ ! -x "$VENV_BIN/pip$EXE" ]; then
    "$VENV_BIN/python$EXE" -m ensurepip --upgrade >/dev/null 2>&1 || true
  fi
  if [ ! -x "$VENV_BIN/pip$EXE" ]; then
    echo "the virtualenv at $VENV has no pip; install python3-venv (or its" >&2
    echo "equivalent) and try again" >&2
    exit 1
  fi
  "$VENV_BIN/pip$EXE" install --quiet --disable-pip-version-check openai pytest pyyaml
fi

echo "== building the mock gateway =="
cargo build -p hermes-gateway --features mock --bin hermes-mock-gateway

echo "== contract suite =="
exec "$VENV_BIN/pytest$EXE" tests/contract -q "$@"
