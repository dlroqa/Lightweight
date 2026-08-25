"""Fixtures for the contract suite.

The suite exists to answer a question our Rust tests structurally cannot: does
the *real* client library work against this gateway? Our own tests assert the
bytes we intended to send. These assert what the `openai` package ends up
with after parsing them — content assembled, tool-call arguments that parse as
JSON, `usage` populated from the terminal chunk. The difference is the
difference between "we implemented the spec" and "the client works".

The gateway under test is backed by the deterministic mock engine, so a case
like "the model returns nothing at all" is one HTTP call rather than an
unreproducible accident.
"""

import contextlib
import json
import os
import pathlib
import subprocess
import sys
import time
import urllib.error
import urllib.request

import pytest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
BINARY = REPO_ROOT / "target" / "debug" / "hermes-mock-gateway"

# The gateway serves a 4096-token context under this name; several tests depend
# on both numbers.
MODEL = "mock-model@4k"
N_CTX = 4096


@contextlib.contextmanager
def _gateway(concurrency=1):
    """Start a mock gateway, yield what it printed, and stop it afterwards."""
    if not BINARY.exists():
        pytest.fail(
            f"{BINARY} is missing. Build it with:\n"
            "  cargo build -p hermes-gateway --features mock --bin hermes-mock-gateway"
        )

    process = subprocess.Popen(
        [
            str(BINARY),
            "--port", "0",
            "--ctx", str(N_CTX),
            "--model", "mock-model",
            "--concurrency", str(concurrency),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        # The binary prints its bound address as one JSON line before serving.
        line = process.stdout.readline()
        if not line:
            process.kill()
            pytest.fail(f"the gateway exited before binding: {process.stderr.read()}")
        info = json.loads(line)

        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(
                    f"http://127.0.0.1:{info['port']}/health", timeout=1
                ) as response:
                    if response.status == 200:
                        break
            except (urllib.error.URLError, ConnectionError):
                time.sleep(0.05)
        else:
            pytest.fail("the gateway never became healthy")

        yield info
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()


@pytest.fixture(scope="session")
def gateway():
    """One gateway, one slot, for the whole session."""
    with _gateway() as info:
        yield info


@pytest.fixture
def two_slot_gateway():
    """A gateway with two slots, for tests that drive two clients at once.

    Its own process rather than the session one: the slot count is settled
    before the first listener binds, and a test that needs two must not change
    what every other test in the suite is running against.
    """
    with _gateway(concurrency=2) as info:
        assert info["concurrency"] == 2, info
        yield info


@pytest.fixture
def script(gateway):
    """Tell the mock engine what to produce for the next request."""

    def apply(**spec):
        set_script(gateway, **spec)

    return apply


def set_script(info, **spec):
    """Tell one gateway's mock engine what to produce next.

    A function as well as a fixture because a test with its own gateway - a
    two-slot one, say - still needs to script it, and the fixture is bound to
    the session's.
    """
    # The control route takes the script as a nested object, with the knobs
    # that are not part of the script - the prompt size it reports, the
    # prefill it simulates - beside it.
    body = {"script": {key: value for key, value in spec.items()
                       if key not in ("prompt_tokens", "prefill_ms")}}
    for key in ("prompt_tokens", "prefill_ms"):
        if key in spec:
            body[key] = spec[key]
    request = urllib.request.Request(
        f"http://127.0.0.1:{info['port']}/__test__/script",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=5) as response:
        assert response.status == 204, response.status


@pytest.fixture
def client(gateway):
    """The real `openai` client, pointed at the gateway."""
    openai = pytest.importorskip("openai")
    return openai.OpenAI(
        base_url=gateway["base_url"],
        # Exactly what the agent sends when no key is configured. A gateway
        # that rejected this would break every request it makes.
        api_key="no-key-required",
        max_retries=0,
        timeout=30.0,
    )


@pytest.fixture(scope="session")
def hermes_parser():
    """The agent's own context-limit parser, imported from its source tree.

    Testing our error wording against a regex we transcribed would only prove
    we can copy a regex. Importing the function the client actually runs is
    what makes the assertion mean something.
    """
    default = pathlib.Path.home() / ".hermes" / "hermes-agent"
    home = pathlib.Path(os.environ.get("HERMES_AGENT_HOME", default))
    if not (home / "agent" / "model_metadata.py").exists():
        pytest.skip(f"the agent source is not present at {home}")
    sys.path.insert(0, str(home))
    try:
        from agent.model_metadata import parse_context_limit_from_error
    except Exception as error:  # pragma: no cover - environment dependent
        pytest.skip(f"could not import the agent's parser: {error}")
    return parse_context_limit_from_error
