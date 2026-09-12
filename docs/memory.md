# Long-term memory

Memory belongs to a Lightagent profile, not to one chat session. Its durable
bank is stored under that profile's private `memory/memories.jsonl`. A new TUI,
API, or ACP run opens the same bank, so previously established context is
available after restarting the harness.

## Retention

The harness automatically retains a small number of explicit, durable user
statements: preferences, decisions, codebase conventions, and confirmed
resolutions. It copies the user's wording into the bank rather than asking a
second model to guess what mattered. A TUI memory cites its source session and
message number. Repeated statements deduplicate. Questions, temporary requests,
code blocks, likely credentials, and ambiguous claims stay in the session log
without becoming long-term memory. The conservative filter can miss a useful
fact; use `lightagent memory candidates <session-id>` and `memory promote` to
review and save it manually, or ask the agent to use `memory.write`.

Automatic retention is on by default. Disable it with
`lightagent config set memory.auto_capture false`. This does not delete existing
memories. `lightagent memory list`, `update`, `forget`, and `clear` let you
inspect and correct the bank.

## Recall

Before each model invocation, Lightagent selects up to three relevant memories
and adds a compact, typed catalog to the prompt. It uses offline lexical and
feature-hashed ranking by default, with optional semantic fusion when
`rag.semantic` is configured. A failed embeddings endpoint falls back to the
offline path. The `memory.search` tool can retrieve more detail, and
`session.lookup` can read a cited source message. Set
`memory.inject_recent` to `0` to disable automatic prompt injection, or adjust
`memory.top_k` for manual searches.

## Reflection

`lightagent memory reflect [topic]` and the read-only `memory.reflect` tool
build a structured working knowledge page from the bank. They group current
preferences, decisions, conventions, resolved issues, and other facts, newest
first, with source citations where available. This view is derived on demand:
updating or forgetting a fact immediately changes the page. A topic can be a
category such as `preference` or a search phrase. The agent can use this page
for repeated or complex questions and then inspect exact memories or cited
sessions when it needs evidence. Reflection does not invent conclusions beyond
the facts retained in the bank.
