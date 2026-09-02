import { useMemo, useState } from "react";
import { Ban, Send, Wrench } from "lucide-react";

import { agentApi } from "../api/agent";
import { useRunEvents, type RunEvent } from "../hooks/useRunEvents";
import { Card } from "../components/Card";
import { Empty, Pill } from "../components/Bits";
import { TopBar } from "../components/Shell";

function str(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/** A tool call reconstructed from the event stream. */
interface ToolCall {
  id: string;
  name: string;
  arguments: string;
  status: "requested" | "running" | "ok" | "error";
  result: string;
}

function foldTools(events: RunEvent[]): ToolCall[] {
  const byId = new Map<string, ToolCall>();
  for (const event of events) {
    const id = str(event.data.id);
    if (!id) continue;
    const existing = byId.get(id);
    if (event.type === "tool.requested") {
      byId.set(id, {
        id,
        name: str(event.data.name),
        arguments: str(event.data.arguments),
        status: "requested",
        result: "",
      });
    } else if (event.type === "tool.started" && existing) {
      existing.status = "running";
    } else if (event.type === "tool.output" && existing) {
      existing.status = "ok";
      existing.result = str(event.data.content);
    } else if (event.type === "tool.failed" && existing) {
      existing.status = "error";
      existing.result = str(event.data.content);
    }
  }
  return Array.from(byId.values());
}

export function Agent() {
  const [draft, setDraft] = useState("");
  const [runId, setRunId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const { events, done } = useRunEvents(runId);

  const answer = useMemo(
    () =>
      events
        .filter((event) => event.type === "model.delta")
        .map((event) => str(event.data.content))
        .join(""),
    [events],
  );
  const tools = useMemo(() => foldTools(events), [events]);
  const pending = useMemo(() => {
    if (done) return null;
    // An approval stays pending only until the same tool call reports back: on
    // approve the call emits tool.started/output, on reject a tool.failed — both
    // carry the approval's tool-call id, so a later same-id event resolves it.
    // Without this the request card would linger until the whole run ended.
    let openId: string | null = null;
    let openName = "";
    for (const event of events) {
      const id = str(event.data.id);
      if (event.type === "approval.required") {
        openId = id;
        openName = str(event.data.name);
      } else if (
        openId &&
        id === openId &&
        (event.type === "tool.started" ||
          event.type === "tool.output" ||
          event.type === "tool.failed")
      ) {
        openId = null;
      }
    }
    return openId ? { tool: openName } : null;
  }, [events, done]);
  const failure = useMemo(() => {
    // The server sends an `error` event (with the message) then a terminal
    // `run.failed`. Prefer the message; fall back if only the terminal arrived.
    const failed = events.some((event) => event.type === "run.failed");
    const errored = [...events].reverse().find((event) => event.type === "error");
    if (!failed && !errored) return null;
    return (errored && str(errored.data.message)) || "The run failed.";
  }, [events]);
  const cancelled = useMemo(
    () => events.some((event) => event.type === "run.cancelled"),
    [events],
  );
  const running = runId !== null && !done;

  async function send() {
    const message = draft.trim();
    if (!message || running) return;
    setError(null);
    setDraft("");
    try {
      const created = await agentApi.createRun(message);
      setRunId(created.id);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  async function cancel() {
    if (!runId) return;
    try {
      await agentApi.cancelRun(runId);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  async function decide(approve: boolean) {
    if (!runId) return;
    try {
      await agentApi.respondApproval(runId, approve);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  return (
    <>
      <TopBar
        title="Agent"
        subtitle="A tool-using run over the local runtime"
        actions={
          running ? (
            <button type="button" className="btn" onClick={cancel}>
              <Ban size={15} /> Stop
            </button>
          ) : undefined
        }
      />

      <div className="page">
        <Card
          title="Ask the agent"
          action={
            <Pill
              tone={
                running
                  ? "accent"
                  : failure
                    ? "danger"
                    : cancelled
                      ? "warn"
                      : done
                        ? "ok"
                        : "neutral"
              }
              dot
            >
              {running
                ? "running"
                : failure
                  ? "failed"
                  : cancelled
                    ? "stopped"
                    : done
                      ? "done"
                      : "idle"}
            </Pill>
          }
        >
          <div style={{ display: "flex", gap: 12, alignItems: "flex-end" }}>
            <div className="field" style={{ flex: 1 }}>
              <label className="field__label" htmlFor="agent-input">
                Message
              </label>
              <textarea
                id="agent-input"
                className="field__input"
                rows={2}
                value={draft}
                placeholder="e.g. what time is it in UTC?"
                disabled={running}
                onChange={(event) => setDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
                    event.preventDefault();
                    void send();
                  }
                }}
              />
            </div>
            <button
              type="button"
              className="btn btn--primary"
              disabled={running || draft.trim().length === 0}
              onClick={() => void send()}
            >
              <Send size={15} /> Send
            </button>
          </div>
          {error && (
            <p className="muted" style={{ marginTop: 10, color: "var(--danger, #c0392b)" }}>
              {error}
            </p>
          )}
        </Card>

        {pending && (
          <Card title="Approval needed">
            <div style={{ display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap" }}>
              <Pill tone="warn" dot>
                {pending.tool}
              </Pill>
              <span className="muted">wants to run and needs your decision.</span>
              <div style={{ marginLeft: "auto", display: "flex", gap: 8 }}>
                <button type="button" className="btn btn--primary" onClick={() => void decide(true)}>
                  Approve
                </button>
                <button type="button" className="btn" onClick={() => void decide(false)}>
                  Reject
                </button>
              </div>
            </div>
          </Card>
        )}

        {runId === null ? (
          <Empty title="No run yet" hint="Send a message to start a tool-using run." />
        ) : (
          <>
            <Card title="Answer">
              {answer && <p style={{ whiteSpace: "pre-wrap", margin: 0 }}>{answer}</p>}
              {failure ? (
                <p
                  style={{
                    margin: answer ? "10px 0 0" : 0,
                    whiteSpace: "pre-wrap",
                    color: "var(--danger)",
                  }}
                >
                  {failure}
                </p>
              ) : (
                !answer && <span className="muted">{running ? "Thinking…" : "No answer."}</span>
              )}
            </Card>

            {tools.length > 0 && (
              <Card title="Tool calls">
                <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
                  {tools.map((tool) => (
                    <div
                      key={tool.id}
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        gap: 4,
                        padding: "10px 12px",
                        border: "1px solid var(--border, #2a2a2a)",
                        borderRadius: 8,
                      }}
                    >
                      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                        <Wrench size={14} />
                        <strong>{tool.name}</strong>
                        <Pill
                          tone={
                            tool.status === "ok"
                              ? "ok"
                              : tool.status === "error"
                                ? "danger"
                                : "accent"
                          }
                        >
                          {tool.status}
                        </Pill>
                      </div>
                      {tool.arguments && tool.arguments !== "{}" && (
                        <code className="muted" style={{ fontSize: 12 }}>
                          {tool.arguments}
                        </code>
                      )}
                      {tool.result && (
                        <span style={{ fontSize: 13, whiteSpace: "pre-wrap" }}>{tool.result}</span>
                      )}
                    </div>
                  ))}
                </div>
              </Card>
            )}
          </>
        )}
      </div>
    </>
  );
}
