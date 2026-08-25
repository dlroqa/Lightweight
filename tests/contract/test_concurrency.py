"""Two real clients, at once, through the genuine `openai` package.

Every other file here drives one client at a time, which is what the gateway
did until M9. The claim this file exists to check is the one a Rust test
cannot make on its own: that the *client library* — its connection pool, its
streaming reader, its usage accounting — survives being one of two.
"""

import threading

from conftest import set_script
from openai import OpenAI

MODEL = "mock-model@4k"


def _client(info):
    return OpenAI(base_url=info["base_url"], api_key="no-key-required")


def test_two_clients_stream_at_once(two_slot_gateway):
    """Both clients get a complete answer, and neither raises."""
    set_script(
        two_slot_gateway,
        kind="content",
        fragments=["one ", "two ", "three ", "four "],
        prompt_tokens=11,
    )

    results = {}
    errors = {}

    def run(name):
        try:
            stream = _client(two_slot_gateway).chat.completions.create(
                model=MODEL,
                messages=[{"role": "user", "content": "count"}],
                stream=True,
            )
            text = ""
            for chunk in stream:
                if chunk.choices and chunk.choices[0].delta.content:
                    text += chunk.choices[0].delta.content
            results[name] = text
        except Exception as err:  # noqa: BLE001 - the assertion is that there is none
            errors[name] = err

    threads = [threading.Thread(target=run, args=(name,)) for name in ("a", "b")]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=30)

    assert not errors, errors
    assert results["a"] == "one two three four "
    assert results["b"] == "one two three four "


def test_a_second_client_is_served_rather_than_refused(two_slot_gateway):
    """Non-streamed, which is the path that answers 503 when it cannot wait."""
    answers = {}
    errors = {}

    def run(name):
        try:
            answers[name] = _client(two_slot_gateway).chat.completions.create(
                model=MODEL,
                messages=[{"role": "user", "content": "hello"}],
            )
        except Exception as err:  # noqa: BLE001
            errors[name] = err

    threads = [threading.Thread(target=run, args=(name,)) for name in ("a", "b")]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=30)

    assert not errors, errors
    for name in ("a", "b"):
        assert answers[name].choices[0].message.content
        assert answers[name].usage.prompt_tokens > 0
