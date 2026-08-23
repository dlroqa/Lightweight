"""Tool calls in a real agent loop, and the older completions endpoint.

Both halves of M4 seen from the client's side. The chat tests next door prove
content and usage survive the trip; these prove the two things an agent
actually needs — that a declared tool comes back as a *parsed* tool call the
SDK can act on, and that the loop closes when the result is replayed.

Everything asserted here is the `openai` package's own object, never our bytes.
A gateway can emit a tool-call chunk sequence that looks right and still leave
the SDK with arguments that do not parse, and only the SDK can tell us that.
"""

import json

import pytest

from conftest import MODEL


WEATHER_TOOL = {
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get the current weather for a named city",
        "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string", "description": "City name"}},
            "required": ["city"],
        },
    },
}


def test_a_streamed_tool_call_assembles_in_the_client(client, script):
    # The fragments are split the way an engine splits them: the id and name
    # arrive once, the arguments in pieces that mean nothing on their own.
    script(
        kind="tool_call",
        id="call_1",
        name="get_weather",
        argument_fragments=['{"ci', 'ty": "Pa', 'ris"}'],
    )

    stream = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "weather in Paris?"}],
        tools=[WEATHER_TOOL],
        tool_choice="auto",
        stream=True,
    )

    # Accumulate exactly the way a client does: keyed by index, name assigned,
    # arguments concatenated.
    calls = {}
    finish_reason = None
    for chunk in stream:
        for choice in chunk.choices:
            for delta in choice.delta.tool_calls or []:
                call = calls.setdefault(delta.index, {"id": None, "name": None, "args": ""})
                if delta.id:
                    call["id"] = delta.id
                if delta.function and delta.function.name:
                    call["name"] = delta.function.name
                if delta.function and delta.function.arguments:
                    call["args"] += delta.function.arguments
            if choice.finish_reason:
                finish_reason = choice.finish_reason

    assert finish_reason == "tool_calls"
    assert list(calls) == [0]
    call = calls[0]
    assert call["id"] == "call_1"
    assert call["name"] == "get_weather"
    # The whole point: the concatenated fragments must be valid JSON, or the
    # agent cannot call anything.
    assert json.loads(call["args"]) == {"city": "Paris"}


def test_a_non_streamed_tool_call_is_a_parsed_call(client, script):
    script(
        kind="tool_call",
        id="call_7",
        name="get_weather",
        argument_fragments=['{"city":', ' "Berlin"}'],
    )

    completion = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "weather in Berlin?"}],
        tools=[WEATHER_TOOL],
    )

    message = completion.choices[0].message
    assert completion.choices[0].finish_reason == "tool_calls"
    assert message.tool_calls is not None, "the SDK saw no tool calls"
    assert len(message.tool_calls) == 1
    call = message.tool_calls[0]
    assert call.id == "call_7"
    assert call.type == "function"
    assert call.function.name == "get_weather"
    assert json.loads(call.function.arguments) == {"city": "Berlin"}


def test_the_agent_loop_closes_when_the_result_is_replayed(client, script):
    """The full round trip: ask, get a call, run it, answer.

    This is the shape every agent harness drives, and the turn that used to be
    impossible: with `tools` dropped at the gateway the model was never told a
    tool existed, so the first response was prose and the loop never started.
    """
    script(
        kind="tool_call",
        id="call_1",
        name="get_weather",
        argument_fragments=['{"city": "Paris"}'],
    )

    messages = [{"role": "user", "content": "What is the weather in Paris?"}]
    first = client.chat.completions.create(
        model=MODEL, messages=messages, tools=[WEATHER_TOOL]
    )
    call = first.choices[0].message.tool_calls[0]
    arguments = json.loads(call.function.arguments)
    assert arguments == {"city": "Paris"}

    # The harness runs the tool and replays both turns. The assistant turn has
    # `content: null` and only the call it made, which is the shape that used
    # to be dropped for having no text in it.
    messages.append(
        {
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    },
                }
            ],
        }
    )
    messages.append(
        {
            "role": "tool",
            "tool_call_id": call.id,
            "content": json.dumps({"city": arguments["city"], "celsius": 17}),
        }
    )

    script(kind="content", fragments=["It is 17", " degrees in Paris."])
    second = client.chat.completions.create(
        model=MODEL, messages=messages, tools=[WEATHER_TOOL]
    )

    assert second.choices[0].finish_reason == "stop"
    assert second.choices[0].message.content == "It is 17 degrees in Paris."
    assert second.choices[0].message.tool_calls in (None, [])


def test_tool_choice_may_name_a_declared_function(client, script):
    script(
        kind="tool_call",
        id="call_2",
        name="get_weather",
        argument_fragments=['{"city": "Oslo"}'],
    )
    completion = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "Oslo"}],
        tools=[WEATHER_TOOL],
        tool_choice={"type": "function", "function": {"name": "get_weather"}},
    )
    assert completion.choices[0].message.tool_calls[0].function.name == "get_weather"


def test_naming_an_undeclared_function_is_a_400_the_client_can_read(client):
    openai = pytest.importorskip("openai")
    with pytest.raises(openai.BadRequestError) as caught:
        client.chat.completions.create(
            model=MODEL,
            messages=[{"role": "user", "content": "hi"}],
            tools=[WEATHER_TOOL],
            tool_choice={"type": "function", "function": {"name": "get_wether"}},
        )
    # The SDK parses our envelope; a body it could not read would surface as a
    # generic APIError instead, with the reason lost.
    body = caught.value.body
    assert body["code"] == "invalid_tool_choice"
    assert body["param"] == "tool_choice"
    assert "get_wether" in body["message"]


# ---------------------------------------------------------------------------
# /v1/completions
# ---------------------------------------------------------------------------


def test_a_text_completion_reaches_the_client(client, script):
    script(kind="content", fragments=[" Paris", "."])

    completion = client.completions.create(
        model=MODEL, prompt="The capital of France is", max_tokens=8
    )

    assert completion.object == "text_completion"
    assert completion.model == MODEL
    assert len(completion.choices) == 1
    assert completion.choices[0].text == " Paris."
    assert completion.choices[0].index == 0
    assert completion.choices[0].finish_reason == "stop"
    assert completion.usage is not None
    assert completion.usage.total_tokens == (
        completion.usage.prompt_tokens + completion.usage.completion_tokens
    )


def test_a_streamed_text_completion_reaches_the_client(client, script):
    script(kind="content", fragments=[" Pa", "ris"])

    stream = client.completions.create(
        model=MODEL,
        prompt="The capital of France is",
        stream=True,
        stream_options={"include_usage": True},
    )

    text = ""
    usage = None
    finish_reason = None
    for chunk in stream:
        assert chunk.object == "text_completion"
        if chunk.usage is not None:
            usage = chunk.usage
        for choice in chunk.choices:
            text += choice.text
            if choice.finish_reason:
                finish_reason = choice.finish_reason

    assert text == " Paris"
    assert finish_reason == "stop"
    # Read from a chunk with an empty `choices` array, which is OpenAI's shape
    # rather than the engine's.
    assert usage is not None, "the client never saw a usage chunk"
    assert usage.completion_tokens == 2


def test_an_array_prompt_returns_one_choice_each(client, script):
    script(kind="content", fragments=["out"])

    completion = client.completions.create(
        model=MODEL, prompt=["alpha", "beta", "gamma"], max_tokens=4
    )

    assert [choice.index for choice in completion.choices] == [0, 1, 2]
    assert all(choice.text == "out" for choice in completion.choices)
    # One request, one set of numbers, covering all three generations.
    assert completion.usage.completion_tokens == 3


def test_echo_repeats_the_prompt(client, script):
    script(kind="content", fragments=[" Paris"])
    completion = client.completions.create(
        model=MODEL, prompt="The capital of France is", echo=True, max_tokens=4
    )
    assert completion.choices[0].text == "The capital of France is Paris"


def test_a_parameter_we_cannot_honour_is_refused_by_name(client):
    openai = pytest.importorskip("openai")
    with pytest.raises(openai.BadRequestError) as caught:
        client.completions.create(model=MODEL, prompt="x", logprobs=5)
    body = caught.value.body
    assert body["code"] == "unsupported_parameter"
    assert body["param"] == "logprobs"


def test_an_overlong_completion_prompt_is_parsable_by_the_agents_own_parser(
    client, script, hermes_parser
):
    """The same guarantee the chat endpoint gives, on the older endpoint.

    Asserted with the client's real parser rather than a transcribed regex: the
    number it recovers is what a client caches and re-plans against.
    """
    openai = pytest.importorskip("openai")
    script(kind="content", fragments=["x"], prompt_tokens=9999)

    with pytest.raises(openai.BadRequestError) as caught:
        client.completions.create(model=MODEL, prompt="far too long", max_tokens=8)

    body = caught.value.body
    assert body["code"] == "context_length_exceeded"
    assert body["param"] == "prompt"
    assert hermes_parser(body["message"]) == 4096
