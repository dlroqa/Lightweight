import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Ban, Plus, Search, Send, Trash2, Wrench } from "lucide-react";

import {
  agentApi,
  type AgentSession,
  type SessionMessage,
  type SessionSummary,
  type SystemTime,
} from "../api/agent";
import { api } from "../api/client";
import { whenever } from "../api/format";
import { Empty, Pill } from "../components/Bits";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";
import { useRunEvents, type RunEvent } from "../hooks/useRunEvents";

const SESSION_KEY = "lightagent.agent.session";
const text = (value: unknown) => (typeof value === "string" ? value : "");
const unix = (value: SystemTime) => value.secs_since_epoch;

interface ToolCall {
  id: string;
  name: string;
  arguments: string;
  status: "requested" | "running" | "ok" | "error";
  result: string;
}

function foldTools(events: RunEvent[]): ToolCall[] {
  const calls = new Map<string, ToolCall>();
  for (const event of events) {
    const id = text(event.data.id);
    const call = calls.get(id);
    if (event.type === "tool.requested") {
      calls.set(id, {
        id,
        name: text(event.data.name),
        arguments: text(event.data.arguments),
        status: "requested",
        result: "",
      });
    } else if (event.type === "tool.started" && call) call.status = "running";
    else if (event.type === "tool.output" && call) {
      call.status = "ok";
      call.result = text(event.data.content);
    } else if (event.type === "tool.failed" && call) {
      call.status = "error";
      call.result = text(event.data.content);
    }
  }
  return [...calls.values()];
}

export function Agent() {
  const sessions = usePoll(() => agentApi.sessions().then((body) => body.sessions), 2000);
  const server = usePoll(api.agentServer, 2000);
  const [activeId, setActiveId] = useState<string | null>(() =>
    window.localStorage.getItem(SESSION_KEY),
  );
  const [session, setSession] = useState<AgentSession | null>(null);
  const [runId, setRunId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [steering, setSteering] = useState<string[]>([]);
  const [queuePaused, setQueuePaused] = useState(false);
  const [search, setSearch] = useState("");
  const [busy, setBusy] = useState(false);
  const [recovering, setRecovering] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const end = useRef<HTMLDivElement | null>(null);
  const dispatching = useRef(false);
  const autoStartAttempted = useRef(false);
  const selected = useRef(activeId);
  const loadGeneration = useRef(0);
  selected.current = activeId;
  const { events, done } = useRunEvents(runId);

  const answer = useMemo(
    () => events.filter((event) => event.type === "model.delta")
      .map((event) => text(event.data.content)).join(""),
    [events],
  );
  const tools = useMemo(() => foldTools(events), [events]);
  const failure = useMemo(() => {
    const event = [...events].reverse().find((row) => row.type === "error");
    return event
      ? text(event.data.message) || "The run failed."
      : events.some((row) => row.type === "run.failed")
        ? "The run failed."
        : null;
  }, [events]);
  const cancelled = events.some((event) => event.type === "run.cancelled");
  const running = busy || (runId !== null && !done);
  const hasPendingWork = running || steering.length > 0;
  const persisted = runId !== null && session?.runs.some((run) => run.run_id === runId);
  const showLiveAnswer = runId !== null && (!done || !persisted);
  const pending = useMemo(() => {
    if (done) return null;
    let open: { id: string; tool: string } | null = null;
    for (const event of events) {
      const id = text(event.data.id);
      if (event.type === "approval.required") open = { id, tool: text(event.data.name) };
      else if (
        open?.id === id &&
        ["tool.started", "tool.output", "tool.failed"].includes(event.type)
      ) open = null;
    }
    return open;
  }, [done, events]);

  const load = useCallback(async (id: string) => {
    const generation = ++loadGeneration.current;
    try {
      const loaded = await agentApi.session(id);
      if (selected.current === id && generation === loadGeneration.current) {
        setSession(loaded);
        setError(null);
      }
    } catch (cause) {
      if (selected.current === id && generation === loadGeneration.current) {
        setSession(null);
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    }
  }, []);

  useEffect(() => {
    if (activeId) void load(activeId);
    else setSession(null);
  }, [activeId, load]);

  useEffect(() => {
    if (!done || !activeId || persisted) return;
    let attempts = 0;
    const timer = window.setInterval(() => {
      void load(activeId);
      sessions.refresh();
      attempts += 1;
      if (attempts >= 20) window.clearInterval(timer);
    }, 250);
    return () => window.clearInterval(timer);
  }, [activeId, done, load, persisted, sessions.refresh]);

  useEffect(() => {
    if (!done || !activeId || steering.length === 0 || busy ||
        queuePaused || dispatching.current) return;
    const next = steering[0];
    if (!next) return;
    dispatching.current = true;
    setBusy(true);
    void (async () => {
      try {
        let created: Awaited<ReturnType<typeof agentApi.createRun>> | null = null;
        for (let attempt = 0; attempt < 20; attempt += 1) {
          try {
            created = await agentApi.createRun(next, undefined, activeId);
            break;
          } catch (cause) {
            const message = cause instanceof Error ? cause.message : String(cause);
            if (!message.startsWith("409: session already has an active run")) throw cause;
            await new Promise((resolve) => window.setTimeout(resolve, 250));
          }
        }
        if (!created) throw new Error("The previous run has not released this session. Retry the queued steer.");
        setSteering((current) => current.slice(1));
        setRunId(created.id);
        await load(activeId);
        sessions.refresh();
      } catch (cause) {
        setQueuePaused(true);
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        dispatching.current = false;
        setBusy(false);
      }
    })();
  }, [activeId, busy, done, load, queuePaused, sessions.refresh, steering]);

  useEffect(() => {
    end.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [answer, session?.messages.length, tools.length]);

  const visible = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return (sessions.data ?? []).filter(
      (row) =>
        !needle ||
        row.title.toLowerCase().includes(needle) ||
        row.profile.toLowerCase().includes(needle),
    );
  }, [search, sessions.data]);

  function select(id: string) {
    if (hasPendingWork || id === activeId) return;
    window.localStorage.setItem(SESSION_KEY, id);
    selected.current = id;
    loadGeneration.current += 1;
    setActiveId(id);
    setSession(null);
    setRunId(null);
    setError(null);
  }

  async function startNew() {
    if (hasPendingWork) return;
    setBusy(true);
    setError(null);
    try {
      const created = await agentApi.createSession();
      window.localStorage.setItem(SESSION_KEY, created.id);
      selected.current = created.id;
      loadGeneration.current += 1;
      setActiveId(created.id);
      setRunId(null);
      await load(created.id);
      sessions.refresh();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }

  async function remove(id: string) {
    if (hasPendingWork && id === activeId) return;
    try {
      await agentApi.deleteSession(id);
      if (id === activeId) {
        window.localStorage.removeItem(SESSION_KEY);
        selected.current = null;
        loadGeneration.current += 1;
        setActiveId(null);
        setSession(null);
        setRunId(null);
      }
      sessions.refresh();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  async function send() {
    const message = draft.trim();
    if (!message) return;
    if ((runId !== null && !done) || steering.length > 0 || dispatching.current) {
      setSteering((current) => [...current, message]);
      setDraft("");
      setQueuePaused(false);
      return;
    }
    if (busy) return;
    setBusy(true);
    setDraft("");
    setError(null);
    try {
      let id = activeId;
      if (!id) {
        id = (await agentApi.createSession()).id;
        window.localStorage.setItem(SESSION_KEY, id);
        selected.current = id;
        loadGeneration.current += 1;
        setActiveId(id);
      }
      const run = await agentApi.createRun(message, undefined, id);
      setRunId(run.id);
      await load(id);
      sessions.refresh();
    } catch (cause) {
      setDraft(message);
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }

  const recover = useCallback(async () => {
    setRecovering(true);
    setError(null);
    try {
      await api.startAgentServer();
      for (let attempt = 0; attempt < 60; attempt += 1) {
        const status = await api.agentServer();
        if (status.status === "running") {
          // Health alone does not prove this version exposes the API the page needs.
          await agentApi.sessions();
          server.refresh();
          sessions.refresh();
          if (activeId) await load(activeId);
          return;
        }
        if (status.status === "failed") throw new Error(status.message ?? "Agent startup failed.");
        await new Promise((resolve) => window.setTimeout(resolve, 500));
      }
      throw new Error("The agent server did not become ready in time.");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setRecovering(false);
    }
  }, [activeId, load, server.refresh, sessions.refresh]);

  useEffect(() => {
    if (server.data?.status !== "stopped" || !server.data.can_start || autoStartAttempted.current) return;
    autoStartAttempted.current = true;
    void recover();
  }, [recover, server.data]);

  async function cancel() {
    if (runId) await agentApi.cancelRun(runId).catch((cause) =>
      setError(cause instanceof Error ? cause.message : String(cause)));
  }

  async function decide(approve: boolean) {
    if (runId) await agentApi.respondApproval(runId, approve).catch((cause) =>
      setError(cause instanceof Error ? cause.message : String(cause)));
  }

  const serviceStatus = server.data?.status;
  const serviceUnavailable = serviceStatus === "stopped" || serviceStatus === "failed" || serviceStatus === "incompatible" || serviceStatus === "unavailable";
  const shownError = error ?? (!recovering && serviceUnavailable
    ? server.data?.message ?? "The Lightagent server is not running."
    : null) ?? (!recovering ? sessions.error?.message : null);
  const badge = running
    ? { tone: "accent" as const, label: "running" }
    : recovering || serviceStatus === "starting"
      ? { tone: "accent" as const, label: "starting" }
      : serviceUnavailable
        ? { tone: "danger" as const, label: "offline" }
        : failure
          ? { tone: "danger" as const, label: "failed" }
          : cancelled
            ? { tone: "warn" as const, label: "stopped" }
            : done
              ? { tone: "ok" as const, label: "done" }
              : { tone: "neutral" as const, label: "ready" };
  return (
    <>
      <TopBar
        title="Agent"
        subtitle={session?.title || "Tool-using conversations"}
        actions={
          running && (
            <button type="button" className="btn btn--danger" onClick={() => void cancel()}>
              <Ban size={15} /> Stop
            </button>
          )
        }
      />
      <div className="page" style={{ display: "grid", gridTemplateColumns: "300px minmax(0, 1fr)", gap: 16 }}>
        <aside className="card" style={{ display: "flex", flexDirection: "column", gap: 12, minHeight: 0 }}>
          <div style={{ position: "relative" }}>
            <Search size={15} style={{ position: "absolute", left: 11, top: "50%", transform: "translateY(-50%)", color: "var(--text-faint)" }} />
            <input className="input" style={{ paddingLeft: 34 }} placeholder="Search sessions…"
              value={search} onChange={(event) => setSearch(event.target.value)}
              aria-label="Search agent sessions" />
          </div>
          <button type="button" className="btn" disabled={hasPendingWork} onClick={() => void startNew()}>
            <Plus size={16} /> New session
          </button>
          <div style={{ flex: 1, overflowY: "auto", margin: "0 -6px" }}>
            {visible.length === 0 ? (
              <div className="empty" style={{ padding: 20 }}>
                <span>{sessions.loading ? "Loading sessions…" : search ? "Nothing matches." : "No agent sessions yet."}</span>
              </div>
            ) : (
              <ul style={{ margin: 0, padding: 0, listStyle: "none" }}>
                {visible.map((row) => (
                  <SessionRow key={row.id} row={row} active={row.id === activeId}
                    disabled={hasPendingWork} onOpen={() => select(row.id)}
                    onDelete={() => void remove(row.id)} />
                ))}
              </ul>
            )}
          </div>
        </aside>
        <section className="card" style={{ display: "flex", flexDirection: "column", gap: 12, minHeight: 0 }}>
          {recovering && <div className="notice notice--info" role="status">Starting the local Lightagent server…</div>}
          {shownError && (
            <div className="notice notice--danger" role="alert">
              <div>{shownError}</div>
              {server.data?.can_start && (
                <button type="button" className="btn" style={{ marginTop: 10 }}
                  disabled={recovering} onClick={() => void recover()}>
                  {recovering ? "Starting agent server…" : "Start agent server and retry"}
                </button>
              )}
            </div>
          )}
          {!session ? (
            <Empty title="Start an agent session"
              hint="Pick one from the list, or create a new session. Saved sessions can be resumed or deleted at any time." />
          ) : (
            <>
              <div style={{ flex: 1, overflowY: "auto", paddingRight: 4 }}>
                {session.messages.length === 0 && !showLiveAnswer && (
                  <Empty title="Nothing said yet"
                    hint="Messages, runs, and tool calls are saved with this session." />
                )}
                {session.messages.map((message, index) => (
                  <AgentMessage key={message.role + index} message={message} />
                ))}
                {showLiveAnswer && answer && (
                  <AgentMessage message={{ role: "assistant", content: answer }} streaming={!done} />
                )}
                {running && !answer && (
                  <div className="card" style={{ marginTop: 10 }}>
                    <strong style={{ color: "var(--accent)", fontSize: 13 }}>Agent</strong>
                    <div className="muted" style={{ marginTop: 6 }}>Working on the request…</div>
                  </div>
                )}
                {failure && <div className="notice notice--danger" style={{ marginTop: 10 }}>{failure}</div>}
                {tools.length > 0 && <ToolList title="Current tool calls" tools={tools} />}
                <SavedTools session={session} currentRunId={runId} />
                <div ref={end} />
              </div>
              {pending && (
                <div className="notice notice--warn" style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
                  <Pill tone="warn" dot>{pending.tool}</Pill>
                  <span>wants to run and needs your decision.</span>
                  <span style={{ flex: 1 }} />
                  <button type="button" className="btn btn--primary" onClick={() => void decide(true)}>Approve</button>
                  <button type="button" className="btn" onClick={() => void decide(false)}>Reject</button>
                </div>
              )}
              {steering.length > 0 && (
                <div className="notice notice--info" role="status">
                  <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                    <strong>Queued steers ({steering.length})</strong>
                    <span style={{ flex: 1 }} />
                    {queuePaused && (
                      <button type="button" className="btn" onClick={() => setQueuePaused(false)}>
                        Retry
                      </button>
                    )}
                    <button type="button" className="btn" disabled={busy}
                      onClick={() => { setSteering([]); setQueuePaused(false); }}>
                      Clear queue
                    </button>
                  </div>
                  <ol style={{ margin: "8px 0 0", paddingLeft: 20 }}>
                    {steering.map((message, index) => (
                      <li key={index} style={{ whiteSpace: "pre-wrap" }}>{message}</li>
                    ))}
                  </ol>
                  <span className="muted">
                    These run in order after the active turn finishes. Keep this tab open
                    until the queue drains.
                  </span>
                </div>
              )}
              <div style={{ display: "flex", gap: 10, alignItems: "flex-end" }}>
                <textarea className="input" rows={2} style={{ resize: "none" }}
                  value={draft} placeholder={running ? "Type a steer to queue…" : "Ask the agent…"}
                  disabled={busy || recovering || serviceUnavailable}
                  onChange={(event) => setDraft(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" && !event.shiftKey) {
                      event.preventDefault();
                      void send();
                    }
                  }} aria-label="Message" />
                <button type="button" className="btn btn--primary"
                  disabled={!draft.trim() || busy || recovering || serviceUnavailable} onClick={() => void send()}>
                  <Send size={15} /> {running || steering.length > 0 ? "Queue steer" : "Send"}
                </button>
              </div>
              <div style={{ display: "flex", gap: 10, alignItems: "center", color: "var(--text-muted)", fontSize: 12 }}>
                <span>{session.messages.length} messages</span>
                <span>{session.runs.length} runs</span>
                <span style={{ flex: 1 }} />
                <Pill tone={badge.tone} dot>{badge.label}</Pill>
              </div>
            </>
          )}
        </section>
      </div>
    </>
  );
}

function SessionRow({ row, active, disabled, onOpen, onDelete }: {
  row: SessionSummary; active: boolean; disabled: boolean;
  onOpen: () => void; onDelete: () => void;
}) {
  return (
    <li>
      <div style={{ display: "flex", gap: 8, padding: "10px 12px", borderRadius: "var(--radius)",
        background: active ? "var(--accent-soft)" : "transparent",
        opacity: disabled && !active ? 0.6 : 1 }}>
        <button type="button" disabled={disabled} onClick={onOpen}
          aria-current={active ? "true" : undefined}
          style={{ flex: 1, minWidth: 0, padding: 0, border: 0, background: "transparent",
            color: "inherit", textAlign: "left", cursor: disabled ? "default" : "pointer" }}>
          <div style={{ display: "flex", justifyContent: "space-between", gap: 8, fontSize: 13, fontWeight: active ? 600 : 500 }}>
            <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{row.title || "Untitled"}</span>
            <span className="tnum" style={{ color: "var(--text-faint)", fontSize: 11, flex: "none" }}>{whenever(unix(row.updated_at))}</span>
          </div>
          <div style={{ fontSize: 11.5, color: "var(--text-muted)", marginTop: 2 }}>
            {row.message_count} messages · {row.run_count} runs
          </div>
        </button>
        <button type="button" className="btn btn--ghost btn--icon"
          style={{ width: 26, height: 26 }} disabled={disabled && active}
          aria-label={"Delete " + (row.title || "this session")}
          onClick={(event) => { event.stopPropagation(); onDelete(); }}>
          <Trash2 size={14} />
        </button>
      </div>
    </li>
  );
}

function AgentMessage({ message, streaming }: { message: SessionMessage; streaming?: boolean }) {
  const mine = message.role === "user";
  return (
    <article style={{ marginTop: 10, padding: "12px 14px", borderRadius: "var(--radius)",
      border: "1px solid var(--border)", background: mine ? "var(--accent-soft)" : "var(--surface-raised)" }}>
      <div style={{ color: mine ? "var(--accent)" : "var(--text)", fontWeight: 600, fontSize: 13 }}>
        {mine ? "You" : "Agent"}{streaming && <span className="muted"> · responding</span>}
      </div>
      <div style={{ marginTop: 6, whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{message.content}</div>
    </article>
  );
}

function ToolList({ title, tools }: { title: string; tools: ToolCall[] }) {
  return (
    <div style={{ marginTop: 12 }}>
      <div className="muted" style={{ fontSize: 12, fontWeight: 600 }}>{title}</div>
      {tools.map((tool) => <ToolRow key={tool.id} tool={tool} />)}
    </div>
  );
}

function SavedTools({ session, currentRunId }: { session: AgentSession; currentRunId: string | null }) {
  const tools: ToolCall[] = session.runs.filter((run) => run.run_id !== currentRunId)
    .flatMap((run) => run.tools.map((tool) => ({
      id: run.run_id + tool.id, name: tool.tool, arguments: tool.arguments_preview,
      result: tool.result_excerpt, status: tool.outcome === "error" ? "error" as const : "ok" as const,
    })));
  return tools.length ? (
    <details style={{ marginTop: 12 }}>
      <summary className="muted" style={{ cursor: "pointer", fontSize: 12, fontWeight: 600 }}>
        Saved tool history ({tools.length})
      </summary>
      {tools.map((tool) => <ToolRow key={tool.id} tool={tool} />)}
    </details>
  ) : null;
}

function ToolRow({ tool }: { tool: ToolCall }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 4, padding: "10px 12px",
      marginTop: 8, border: "1px solid var(--border)", borderRadius: "var(--radius)" }}>
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <Wrench size={14} /><strong>{tool.name}</strong>
        <Pill tone={tool.status === "ok" ? "ok" : tool.status === "error" ? "danger" : "accent"}>{tool.status}</Pill>
      </div>
      {tool.arguments && tool.arguments !== "{}" && <code className="muted" style={{ fontSize: 12 }}>{tool.arguments}</code>}
      {tool.result && <span style={{ fontSize: 13, whiteSpace: "pre-wrap" }}>{tool.result}</span>}
    </div>
  );
}
