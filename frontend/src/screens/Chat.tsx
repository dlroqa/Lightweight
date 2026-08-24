import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Plus, Search, Send, Square, Trash2 } from "lucide-react";

import { api, ApiError } from "../api/client";
import { rate, whenever } from "../api/format";
import type { Conversation, ConversationSummary, StoredMessage } from "../api/types";
import { Empty, ErrorState, Pill } from "../components/Bits";
import { ModelSelector } from "../components/ModelSelector";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";
import { usePreferences } from "../state/preferences";

/**
 * The chat screen.
 *
 * The transcript is the source of truth while a turn is in flight, and is
 * written back to the gateway once the turn ends. Saving mid-stream would mean
 * a file rewritten per token, and the gateway's own conversation store replaces
 * the whole document on every write for good reasons of its own.
 */
export function Chat() {
  const { preferences } = usePreferences();
  const conversations = usePoll(api.conversations, 0);
  const models = usePoll(api.models, 10_000);
  const metrics = usePoll(api.metrics, 2000);

  const [activeId, setActiveId] = useState<string | null>(null);
  const [conversation, setConversation] = useState<Conversation | null>(null);
  const [draft, setDraft] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [pending, setPending] = useState("");
  const [liveRate, setLiveRate] = useState<number | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const [historyOff, setHistoryOff] = useState(false);

  const abort = useRef<AbortController | null>(null);
  const transcriptEnd = useRef<HTMLDivElement | null>(null);

  const loadedId = metrics.data?.model?.id ?? null;
  const loadedModel = models.data?.find(
    (model) => loadedId !== null && loadedId.startsWith(model.id),
  );

  useEffect(() => {
    if (!activeId) {
      setConversation(null);
      return;
    }
    let cancelled = false;
    void api
      .conversation(activeId)
      .then((loaded) => !cancelled && setConversation(loaded))
      .catch(() => !cancelled && setConversation(null));
    return () => {
      cancelled = true;
    };
  }, [activeId]);

  useEffect(() => {
    transcriptEnd.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [conversation?.messages.length, pending]);

  const visible = useMemo(() => {
    const rows = conversations.data ?? [];
    const needle = search.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter(
      (row) =>
        row.title.toLowerCase().includes(needle) ||
        row.preview.toLowerCase().includes(needle),
    );
  }, [conversations.data, search]);

  const startNew = useCallback(async () => {
    setFailure(null);
    try {
      const created = await api.createConversation();
      setActiveId(created.id);
      setConversation(created);
      conversations.refresh();
      setHistoryOff(false);
    } catch (cause) {
      if (cause instanceof ApiError && cause.code === "history_disabled") {
        // History is off by choice. Chat still works; it is simply not kept.
        setHistoryOff(true);
        setActiveId(null);
        setConversation({
          id: "",
          title: "",
          created_at: Math.floor(Date.now() / 1000),
          updated_at: Math.floor(Date.now() / 1000),
          messages: [],
        });
      } else {
        setFailure(cause instanceof Error ? cause.message : String(cause));
      }
    }
  }, [conversations]);

  async function send() {
    const text = draft.trim();
    if (!text || streaming) return;
    if (!conversation) {
      await startNew();
      return;
    }

    const now = Math.floor(Date.now() / 1000);
    const user: StoredMessage = { role: "user", content: text, at: now };
    const history = [...conversation.messages, user];
    setConversation({ ...conversation, messages: history });
    setDraft("");
    setStreaming(true);
    setPending("");
    setFailure(null);
    setLiveRate(null);

    const controller = new AbortController();
    abort.current = controller;
    const startedAt = performance.now();
    let answer = "";
    let reasoning = "";
    let completionTokens = 0;
    let promptTokens = 0;

    try {
      const response = await fetch("/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        signal: controller.signal,
        body: JSON.stringify({
          model: loadedId ?? loadedModel?.id ?? "",
          stream: true,
          stream_options: { include_usage: true },
          messages: history.map(({ role, content }) => ({ role, content })),
          temperature: preferences.sampling.temperature,
          top_p: preferences.sampling.top_p,
          top_k: preferences.sampling.top_k,
          min_p: preferences.sampling.min_p,
          repeat_penalty: preferences.sampling.repeat_penalty,
          presence_penalty: preferences.sampling.presence_penalty,
          frequency_penalty: preferences.sampling.frequency_penalty,
          max_tokens: preferences.sampling.max_tokens,
          ...(preferences.sampling.seed >= 0
            ? { seed: preferences.sampling.seed }
            : {}),
        }),
      });

      if (!response.ok || !response.body) {
        const body = (await response.json().catch(() => null)) as
          | { error?: { message?: string } }
          | null;
        throw new Error(
          body?.error?.message ?? `The gateway answered ${response.status}.`,
        );
      }

      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";

      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });

        // SSE frames are separated by a blank line. Anything after the last
        // one is a partial frame and stays in the buffer.
        const frames = buffer.split("\n\n");
        buffer = frames.pop() ?? "";

        for (const frame of frames) {
          const line = frame
            .split("\n")
            .find((candidate) => candidate.startsWith("data: "));
          if (!line) continue;
          const payload = line.slice(6).trim();
          if (payload === "[DONE]") continue;

          try {
            const chunk = JSON.parse(payload) as {
              choices?: {
                delta?: { content?: string; reasoning_content?: string };
              }[];
              usage?: { completion_tokens?: number; prompt_tokens?: number };
            };
            const delta = chunk.choices?.[0]?.delta;
            if (delta?.content) {
              answer += delta.content;
              setPending(answer);
            }
            if (delta?.reasoning_content) reasoning += delta.reasoning_content;
            if (chunk.usage) {
              completionTokens = chunk.usage.completion_tokens ?? completionTokens;
              promptTokens = chunk.usage.prompt_tokens ?? promptTokens;
            }
          } catch {
            // A frame that will not parse costs only itself.
          }
        }
      }

      const elapsed = (performance.now() - startedAt) / 1000;
      const tokensPerSecond =
        completionTokens > 0 && elapsed > 0 ? completionTokens / elapsed : null;
      setLiveRate(tokensPerSecond);

      const assistant: StoredMessage = {
        role: "assistant",
        content: answer,
        at: Math.floor(Date.now() / 1000),
        completion_tokens: completionTokens || undefined,
        prompt_tokens: promptTokens || undefined,
        tokens_per_second: tokensPerSecond ?? undefined,
        ...(reasoning ? { reasoning_content: reasoning } : {}),
      };
      const finished = [...history, assistant];
      const title =
        conversation.title || text.slice(0, 60) + (text.length > 60 ? "…" : "");
      setConversation({ ...conversation, title, messages: finished });

      if (!historyOff && conversation.id) {
        await api
          .saveConversation(conversation.id, {
            title,
            model: loadedId,
            messages: finished,
            created_at: conversation.created_at,
          })
          .then(() => conversations.refresh())
          .catch((cause: ApiError) => {
            if (cause.code === "history_disabled") setHistoryOff(true);
            else setFailure(cause.message);
          });
      }
    } catch (cause) {
      if (controller.signal.aborted) {
        // Stopping is a deliberate act, not a failure. Whatever arrived is kept.
        if (answer) {
          setConversation((current) =>
            current
              ? {
                  ...current,
                  messages: [
                    ...history,
                    {
                      role: "assistant",
                      content: answer,
                      at: Math.floor(Date.now() / 1000),
                    },
                  ],
                }
              : current,
          );
        }
      } else {
        setFailure(cause instanceof Error ? cause.message : String(cause));
      }
    } finally {
      setStreaming(false);
      setPending("");
      abort.current = null;
    }
  }

  function stop() {
    abort.current?.abort();
  }

  async function remove(id: string) {
    await api.deleteConversation(id).catch(() => undefined);
    if (activeId === id) setActiveId(null);
    conversations.refresh();
  }

  return (
    <>
      <TopBar
        title="Chat"
        subtitle="Conversational interface with your model"
        actions={
          <ModelSelector
            models={models.data ?? []}
            loadedId={loadedModel?.id ?? null}
            onChanged={() => {
              models.refresh();
              metrics.refresh();
            }}
          />
        }
      />

      <div
        className="page"
        style={{ display: "grid", gridTemplateColumns: "300px minmax(0, 1fr)", gap: 16 }}
      >
        <aside className="card" style={{ display: "flex", flexDirection: "column", gap: 12, minHeight: 0 }}>
          <div style={{ position: "relative" }}>
            <Search
              size={15}
              style={{
                position: "absolute",
                left: 11,
                top: "50%",
                transform: "translateY(-50%)",
                color: "var(--text-faint)",
              }}
            />
            <input
              className="input"
              style={{ paddingLeft: 34 }}
              placeholder="Search conversations…"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              aria-label="Search conversations"
            />
          </div>

          <button type="button" className="btn" onClick={() => void startNew()}>
            <Plus size={16} />
            New conversation
          </button>

          {historyOff && (
            <div className="notice notice--warn">
              History is turned off in settings, so this conversation is not being
              saved.
            </div>
          )}

          <div style={{ flex: 1, overflowY: "auto", margin: "0 -6px" }}>
            {visible.length === 0 ? (
              <div className="empty" style={{ padding: 20 }}>
                <span>{search ? "Nothing matches." : "No conversations yet."}</span>
              </div>
            ) : (
              <ul style={{ margin: 0, padding: 0, listStyle: "none" }}>
                {visible.map((row) => (
                  <ConversationRow
                    key={row.id}
                    row={row}
                    active={row.id === activeId}
                    onOpen={() => setActiveId(row.id)}
                    onDelete={() => void remove(row.id)}
                  />
                ))}
              </ul>
            )}
          </div>
        </aside>

        <section
          className="card"
          style={{ display: "flex", flexDirection: "column", gap: 12, minHeight: 0 }}
        >
          {conversations.error && !conversation ? (
            <ErrorState error={conversations.error} onRetry={conversations.refresh} />
          ) : !conversation ? (
            <Empty
              title="Start a conversation"
              hint={
                loadedId
                  ? "Pick one from the list, or start a new conversation."
                  : "No model is loaded yet — choose one from the selector above first."
              }
            />
          ) : (
            <>
              <div style={{ flex: 1, overflowY: "auto", paddingRight: 4 }}>
                {conversation.messages.length === 0 && !pending && (
                  <Empty
                    title="Nothing said yet"
                    hint="Type below to begin. Everything the model reports about the turn is kept with it."
                  />
                )}
                {conversation.messages.map((message, index) => (
                  <Message key={`${message.at}-${index}`} message={message} />
                ))}
                {pending && (
                  <Message
                    message={{ role: "assistant", content: pending, at: 0 }}
                    streaming
                  />
                )}
                {streaming && !pending && (
                  <div className="card" style={{ marginTop: 10 }}>
                    <span style={{ color: "var(--accent)", fontWeight: 600, fontSize: 13 }}>
                      Assistant
                    </span>
                    <div style={{ color: "var(--text-muted)", fontSize: 13, marginTop: 6 }}>
                      Working on the prompt…
                    </div>
                  </div>
                )}
                <div ref={transcriptEnd} />
              </div>

              {failure && <div className="notice notice--danger">{failure}</div>}

              <div style={{ display: "flex", gap: 10, alignItems: "flex-end" }}>
                <textarea
                  className="input"
                  rows={2}
                  style={{ resize: "none" }}
                  placeholder={loadedId ? "Type your message…" : "Load a model first"}
                  value={draft}
                  disabled={!loadedId}
                  onChange={(event) => setDraft(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" && !event.shiftKey) {
                      event.preventDefault();
                      void send();
                    }
                  }}
                  aria-label="Message"
                />
                {streaming ? (
                  <button type="button" className="btn btn--danger" onClick={stop}>
                    <Square size={15} />
                    Stop
                  </button>
                ) : (
                  <button
                    type="button"
                    className="btn btn--primary"
                    disabled={!loadedId || draft.trim() === ""}
                    onClick={() => void send()}
                  >
                    <Send size={15} />
                    Send
                  </button>
                )}
              </div>

              <div
                style={{
                  display: "flex",
                  flexWrap: "wrap",
                  alignItems: "center",
                  gap: 16,
                  fontSize: 12,
                  color: "var(--text-muted)",
                }}
              >
                <span>Temperature {preferences.sampling.temperature}</span>
                <span>Top P {preferences.sampling.top_p}</span>
                <span>
                  Context {metrics.data?.model?.n_ctx.toLocaleString() ?? "—"}
                </span>
                <span style={{ flex: 1 }} />
                {liveRate !== null && (
                  <Pill tone="ok" dot>
                    {rate(liveRate)} tok/s
                  </Pill>
                )}
              </div>
            </>
          )}
        </section>
      </div>
    </>
  );
}

function ConversationRow({
  row,
  active,
  onOpen,
  onDelete,
}: {
  row: ConversationSummary;
  active: boolean;
  onOpen: () => void;
  onDelete: () => void;
}) {
  return (
    <li>
      <div
        style={{
          display: "flex",
          alignItems: "flex-start",
          gap: 8,
          padding: "10px 12px",
          borderRadius: "var(--radius)",
          background: active ? "var(--accent-soft)" : "transparent",
          cursor: "pointer",
        }}
        onClick={onOpen}
      >
        <div style={{ flex: 1, minWidth: 0 }}>
          <div
            style={{
              display: "flex",
              justifyContent: "space-between",
              gap: 8,
              fontSize: 13,
              fontWeight: active ? 600 : 500,
            }}
          >
            <span
              style={{
                overflow: "hidden",
                textOverflow: "ellipsis",
                whiteSpace: "nowrap",
              }}
            >
              {row.title || "Untitled"}
            </span>
            <span
              className="tnum"
              style={{ color: "var(--text-faint)", fontSize: 11, flex: "none" }}
            >
              {whenever(row.updated_at)}
            </span>
          </div>
          <div
            style={{
              fontSize: 11.5,
              color: "var(--text-muted)",
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
              marginTop: 2,
            }}
          >
            {row.preview || `${row.message_count} messages`}
          </div>
        </div>
        <button
          type="button"
          className="btn btn--ghost btn--icon"
          style={{ width: 26, height: 26 }}
          aria-label={`Delete ${row.title || "this conversation"}`}
          onClick={(event) => {
            event.stopPropagation();
            onDelete();
          }}
        >
          <Trash2 size={14} />
        </button>
      </div>
    </li>
  );
}

function Message({
  message,
  streaming,
}: {
  message: StoredMessage;
  streaming?: boolean;
}) {
  const mine = message.role === "user";
  return (
    <article
      style={{
        marginTop: 10,
        padding: "12px 14px",
        borderRadius: "var(--radius-lg)",
        border: "1px solid var(--border)",
        background: mine ? "var(--accent-soft)" : "var(--surface-raised)",
      }}
    >
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          gap: 12,
          marginBottom: 6,
        }}
      >
        <span
          style={{
            fontSize: 12.5,
            fontWeight: 600,
            color: mine ? "var(--accent)" : "var(--accent)",
          }}
        >
          {mine ? "You" : "Assistant"}
        </span>
        {message.at > 0 && (
          <span className="tnum" style={{ fontSize: 11, color: "var(--text-faint)" }}>
            {new Date(message.at * 1000).toLocaleTimeString([], {
              hour: "2-digit",
              minute: "2-digit",
            })}
          </span>
        )}
      </div>

      {message.reasoning_content && (
        <details style={{ marginBottom: 8 }}>
          <summary
            style={{ fontSize: 12, color: "var(--text-muted)", cursor: "pointer" }}
          >
            Reasoning
          </summary>
          <div
            style={{
              whiteSpace: "pre-wrap",
              fontSize: 12.5,
              color: "var(--text-muted)",
              marginTop: 6,
            }}
          >
            {message.reasoning_content}
          </div>
        </details>
      )}

      <div style={{ whiteSpace: "pre-wrap", fontSize: 13.5, lineHeight: 1.6 }}>
        {message.content}
        {streaming && <span aria-hidden="true">▍</span>}
      </div>

      {(message.completion_tokens || message.tokens_per_second) && (
        <div
          style={{
            display: "flex",
            gap: 14,
            marginTop: 8,
            fontSize: 11.5,
            color: "var(--text-muted)",
          }}
          className="tnum"
        >
          {message.tokens_per_second !== undefined && (
            <span>{rate(message.tokens_per_second)} tok/s</span>
          )}
          {message.completion_tokens !== undefined && (
            <span>{message.completion_tokens} tokens</span>
          )}
        </div>
      )}
    </article>
  );
}
