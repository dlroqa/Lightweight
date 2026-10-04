"""A model the user has named, seen from the real client.

The point of an alias is what a client sees: it discovers `Coder` in
`/v1/models`, sends `Coder` back, and is answered as `Coder` - never as the
long file-derived id it was spared from typing. These tests drive the real
`openai` package through that loop, against a gateway whose resident model the
user has named `Coder`, and assert on the SDK's own objects.

The canonical id and `default` keep working beside the alias, and are asserted
here too, because a client configured before the alias existed must not break
the day one is set.
"""

import json

import pytest

from conftest import MODEL, set_script

ALIAS = "Coder"


@pytest.fixture
def aliased_client(aliased_gateway):
    openai = pytest.importorskip("openai")
    return openai.OpenAI(
        base_url=aliased_gateway["base_url"],
        api_key="no-key-required",
        max_retries=0,
        timeout=30.0,
    )


@pytest.fixture
def aliased_script(aliased_gateway):
    def apply(**spec):
        set_script(aliased_gateway, **spec)

    return apply


def test_discovery_lists_the_alias_and_not_the_file_derived_id(aliased_client):
    ids = [model.id for model in aliased_client.models.list()]
    assert ids == [ALIAS]


def test_the_discovered_name_round_trips_through_a_chat(aliased_client, aliased_script):
    # The Lightagent loop exactly: take the id discovery offered, send it back.
    discovered = aliased_client.models.list().data[0].id
    aliased_script(kind="content", fragments=["Hello", " there"])

    completion = aliased_client.chat.completions.create(
        model=discovered,
        messages=[{"role": "user", "content": "hi"}],
    )

    assert completion.choices[0].message.content == "Hello there"
    assert completion.model == ALIAS


@pytest.mark.parametrize("named", [ALIAS, ALIAS.lower(), MODEL, "mock-model", "default"])
def test_every_name_for_the_model_reaches_it_and_is_answered_as_the_alias(
    aliased_client, aliased_script, named
):
    aliased_script(kind="content", fragments=["ok"])
    completion = aliased_client.chat.completions.create(
        model=named,
        messages=[{"role": "user", "content": "hi"}],
    )
    assert completion.choices[0].message.content == "ok"
    # One public identity, whichever name reached it.
    assert completion.model == ALIAS


def test_a_streamed_reply_names_the_alias_on_every_chunk(aliased_client, aliased_script):
    aliased_script(kind="content", fragments=["a", "b", "c"])
    stream = aliased_client.chat.completions.create(
        model=ALIAS,
        messages=[{"role": "user", "content": "hi"}],
        stream=True,
        stream_options={"include_usage": True},
    )

    chunks = list(stream)
    text = "".join(
        choice.delta.content or "" for chunk in chunks for choice in chunk.choices
    )
    assert text == "abc"
    assert {chunk.model for chunk in chunks} == {ALIAS}
    assert chunks[-1].usage is not None


def test_a_tool_call_through_the_alias_assembles_in_the_client(
    aliased_client, aliased_script
):
    aliased_script(
        kind="tool_call",
        id="call_1",
        name="get_weather",
        argument_fragments=['{"ci', 'ty": "Pa', 'ris"}'],
    )
    stream = aliased_client.chat.completions.create(
        model=ALIAS,
        messages=[{"role": "user", "content": "weather in Paris?"}],
        tools=[
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                    },
                },
            }
        ],
        stream=True,
    )

    arguments = ""
    name = None
    models = set()
    for chunk in stream:
        models.add(chunk.model)
        for choice in chunk.choices:
            for delta in choice.delta.tool_calls or []:
                name = delta.function.name or name
                arguments += delta.function.arguments or ""

    assert name == "get_weather"
    assert json.loads(arguments) == {"city": "Paris"}
    assert models == {ALIAS}


def test_a_text_completion_through_the_alias(aliased_client, aliased_script):
    aliased_script(kind="content", fragments=["(n):"])
    completion = aliased_client.completions.create(
        model=ALIAS,
        prompt="def fibonacci",
        max_tokens=8,
    )
    assert completion.choices[0].text == "(n):"
    assert completion.model == ALIAS


def test_a_streamed_text_completion_through_the_alias(aliased_client, aliased_script):
    aliased_script(kind="content", fragments=["(", "n):"])
    stream = aliased_client.completions.create(
        model=ALIAS,
        prompt="def fibonacci",
        max_tokens=8,
        stream=True,
    )
    chunks = list(stream)
    assert "".join(choice.text for chunk in chunks for choice in chunk.choices) == "(n):"
    assert {chunk.model for chunk in chunks} == {ALIAS}


def test_an_unknown_name_is_still_model_not_found(aliased_client):
    openai = pytest.importorskip("openai")
    with pytest.raises(openai.NotFoundError) as caught:
        aliased_client.chat.completions.create(
            model="Research",
            messages=[{"role": "user", "content": "hi"}],
        )
    assert caught.value.body["code"] == "model_not_found"


def test_a_gateway_with_no_alias_still_lists_and_answers_the_canonical_id(client, script):
    # The session gateway's model has no alias: nothing about it may change.
    assert [model.id for model in client.models.list()] == [MODEL]
    script(kind="content", fragments=["ok"])
    completion = client.chat.completions.create(
        model="default",
        messages=[{"role": "user", "content": "hi"}],
    )
    assert completion.model == MODEL
