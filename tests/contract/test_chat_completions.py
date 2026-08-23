"""What the real `openai` client ends up with.

Every assertion here is about the *client's* result, never about our bytes.
That is the point: a gateway can emit a chunk sequence that looks correct and
still leave the SDK with an empty message, and only the SDK can tell us.
"""

import json

import pytest

from conftest import MODEL, N_CTX


def test_streamed_content_assembles_in_the_client(client, script):
    script(kind="content", fragments=["Hello", ", ", "world"])

    stream = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "hi"}],
        stream=True,
        stream_options={"include_usage": True},
    )

    content = ""
    usage = None
    finish_reason = None
    models = set()
    for chunk in stream:
        models.add(chunk.model)
        if chunk.usage is not None:
            # Read from the terminal chunk and nowhere else.
            usage = chunk.usage
        for choice in chunk.choices:
            if choice.delta.content:
                content += choice.delta.content
            if choice.finish_reason:
                finish_reason = choice.finish_reason

    assert content == "Hello, world"
    assert finish_reason == "stop"
    assert usage is not None, "the client never saw a usage chunk"
    assert usage.completion_tokens == 3
    assert usage.total_tokens == usage.prompt_tokens + usage.completion_tokens
    # The client reads `chunk.model` back and keys its own caches on it.
    assert models == {MODEL}


def test_a_non_streamed_completion_has_a_message(client, script):
    script(kind="content", fragments=["a complete answer"])

    completion = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "hi"}],
    )

    assert completion.choices, "an empty choices array is rejected outright"
    assert completion.choices[0].message.content == "a complete answer"
    assert completion.choices[0].message.role == "assistant"
    assert completion.choices[0].finish_reason == "stop"
    assert completion.model == MODEL


def test_streamed_tool_calls_accumulate_into_valid_json(client, script):
    # The agent keys tool calls by index, assigns the name and concatenates the
    # arguments. Sending the id twice, or a different id at a used index, makes
    # it open a second call and split the arguments between them.
    script(
        kind="tool_call",
        id="call_abc",
        name="read_file",
        argument_fragments=['{"path"', ': "notes.txt"', "}"],
    )

    stream = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "read notes.txt"}],
        stream=True,
    )

    calls = {}
    finish_reason = None
    for chunk in stream:
        for choice in chunk.choices:
            if choice.finish_reason:
                finish_reason = choice.finish_reason
            for delta in choice.delta.tool_calls or []:
                call = calls.setdefault(delta.index, {"id": None, "name": None, "arguments": ""})
                if delta.id:
                    assert call["id"] is None, "an id arrived twice at the same index"
                    call["id"] = delta.id
                if delta.function and delta.function.name:
                    call["name"] = delta.function.name
                if delta.function and delta.function.arguments:
                    call["arguments"] += delta.function.arguments

    assert finish_reason == "tool_calls"
    assert list(calls) == [0], "the fragments must land in exactly one call"
    call = calls[0]
    assert call["id"] == "call_abc"
    assert call["name"] == "read_file"
    assert json.loads(call["arguments"]) == {"path": "notes.txt"}


def test_a_non_streamed_tool_call_is_the_same_call(client, script):
    # An agent must not behave differently because of a transport choice.
    script(
        kind="tool_call",
        id="call_abc",
        name="read_file",
        argument_fragments=['{"path"', ': "notes.txt"', "}"],
    )

    completion = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "read notes.txt"}],
    )

    calls = completion.choices[0].message.tool_calls
    assert calls is not None and len(calls) == 1
    assert calls[0].id == "call_abc"
    assert calls[0].type == "function"
    assert calls[0].function.name == "read_file"
    assert json.loads(calls[0].function.arguments) == {"path": "notes.txt"}
    assert completion.choices[0].finish_reason == "tool_calls"


def test_reasoning_is_delivered_separately_from_content(client, script):
    script(kind="reasoning", reasoning=["let me think"], content=["the answer"])

    stream = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "hi"}],
        stream=True,
    )

    content = ""
    reasoning = ""
    for chunk in stream:
        for choice in chunk.choices:
            if choice.delta.content:
                content += choice.delta.content
            # Not part of the OpenAI schema, so the SDK leaves it in the raw
            # model dump - which is exactly where the agent reads it from.
            extra = choice.delta.model_dump()
            if extra.get("reasoning_content"):
                reasoning += extra["reasoning_content"]

    assert content == "the answer"
    assert reasoning == "let me think", "reasoning must not be merged into content"


def test_the_placeholder_api_key_is_accepted(gateway, script):
    # The agent always sends an Authorization header, and sends this literal
    # value when no key is configured (runtime_provider.py:1144).
    openai = pytest.importorskip("openai")
    script(kind="content", fragments=["fine"])
    client = openai.OpenAI(
        base_url=gateway["base_url"], api_key="no-key-required", max_retries=0
    )
    completion = client.chat.completions.create(
        model=MODEL, messages=[{"role": "user", "content": "hi"}]
    )
    assert completion.choices[0].message.content == "fine"


def test_unknown_request_fields_do_not_400(client, script):
    # `extra_body` is how the agent passes `think` and `options.num_ctx`, and
    # future versions will add more.
    script(kind="content", fragments=["ok"])
    completion = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "hi"}],
        # The agent sends this at the top level for reasoning models.
        reasoning_effort="high",
        extra_body={"think": True, "options": {"num_ctx": 8192}, "future_key": [1, 2]},
    )
    assert completion.choices[0].message.content == "ok"


def test_the_clients_default_max_tokens_is_accepted(client, script):
    # 65536 is the agent's default for a custom provider, and it exceeds every
    # context this gateway can load. Refusing it would break every request.
    script(kind="content", fragments=["ok"])
    completion = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "hi"}],
        max_tokens=65536,
    )
    assert completion.choices[0].message.content == "ok"


def test_a_model_the_gateway_does_not_have_is_a_clean_404(client, script):
    openai = pytest.importorskip("openai")
    script(kind="content", fragments=["ok"])
    with pytest.raises(openai.NotFoundError) as raised:
        client.chat.completions.create(
            model="a-model-we-never-loaded",
            messages=[{"role": "user", "content": "hi"}],
        )
    assert MODEL in str(raised.value)


def test_context_overflow_is_parsed_by_the_agents_own_parser(client, script, hermes_parser):
    # The highest-value assertion in the suite: not that our message matches a
    # regex we transcribed, but that the function the agent actually runs pulls
    # the right number out of it. Getting this wrong makes the agent re-plan
    # blindly against a window it cannot see.
    openai = pytest.importorskip("openai")
    script(kind="content", fragments=["never reached"], prompt_tokens=N_CTX + 900)

    with pytest.raises(openai.BadRequestError) as raised:
        client.chat.completions.create(
            model=MODEL,
            messages=[{"role": "user", "content": "an enormous conversation"}],
        )

    error = raised.value
    body = error.body if isinstance(error.body, dict) else json.loads(error.body)
    assert body["code"] == "context_length_exceeded"
    assert body["param"] == "messages"
    assert hermes_parser(body["message"]) == N_CTX


def test_an_empty_generation_is_reported_as_empty_not_as_a_broken_stream(client, script):
    # We never fabricate a token to avoid the client's EmptyStreamError - that
    # would be inventing output the model did not produce. What the client must
    # get is a well-formed, properly terminated stream that happens to be
    # empty, so it can decide for itself.
    script(kind="empty")

    stream = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "hi"}],
        stream=True,
        stream_options={"include_usage": True},
    )

    content = ""
    finish_reason = None
    usage = None
    for chunk in stream:
        if chunk.usage is not None:
            usage = chunk.usage
        for choice in chunk.choices:
            if choice.delta.content:
                content += choice.delta.content
            if choice.finish_reason:
                finish_reason = choice.finish_reason

    assert content == ""
    assert finish_reason == "stop"
    assert usage is not None and usage.completion_tokens == 0


def test_a_failure_after_the_first_token_surfaces_as_an_error_not_a_truncation(client, script):
    # Once headers are sent an error cannot be an HTTP status. Dropping the
    # connection would look to the client like a stream that simply stopped,
    # which is indistinguishable from a short answer and invites a blind retry.
    #
    # What the SDK actually does with our terminal error chunk - verified here
    # rather than assumed - is raise `APIError` carrying our message, after
    # delivering the content that did arrive. That is the outcome we want: the
    # partial answer is not lost, and the failure is unmistakable.
    openai = pytest.importorskip("openai")
    script(kind="fail_mid_stream", content=["partial "], error="the engine stopped")

    stream = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": "hi"}],
        stream=True,
    )

    content = ""
    with pytest.raises(openai.APIError) as raised:
        for chunk in stream:
            for choice in chunk.choices:
                if choice.delta.content:
                    content += choice.delta.content

    assert content == "partial ", "content produced before the failure must still arrive"
    assert "the engine stopped" in str(raised.value)
    assert raised.value.body["code"] == "generation_failed"


def test_models_reports_the_effective_context(client):
    models = client.models.list()
    rows = list(models)
    assert [row.id for row in rows] == [MODEL]

    row = rows[0].model_dump()
    # The agent scans rows recursively for a context length and takes the first
    # recognized key. Every spelling must agree, and the model's true ceiling
    # must not be among them.
    for key in ("context_length", "n_ctx", "max_tokens", "max_output_tokens"):
        assert row[key] == N_CTX, key
    assert row["hermes"]["model_max_context_length"] > N_CTX


def test_multi_turn_conversations_keep_working(client, script):
    # The acceptance shape: several turns, each carrying the history of the
    # last, streamed, with usage. This is what an agent session looks like.
    history = [{"role": "system", "content": "Be brief."}]
    for turn in range(3):
        script(kind="content", fragments=[f"answer {turn}"])
        history.append({"role": "user", "content": f"question {turn}"})

        stream = client.chat.completions.create(
            model=MODEL,
            messages=history,
            stream=True,
            stream_options={"include_usage": True},
        )
        content = ""
        usage = None
        for chunk in stream:
            if chunk.usage is not None:
                usage = chunk.usage
            for choice in chunk.choices:
                if choice.delta.content:
                    content += choice.delta.content

        assert content == f"answer {turn}"
        assert usage is not None
        history.append({"role": "assistant", "content": content})

    assert len(history) == 7
