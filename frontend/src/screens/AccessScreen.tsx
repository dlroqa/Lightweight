import { useMemo, useState } from "react";

import {
  Check,
  Copy,
  KeyRound,
  Plus,
  ShieldCheck,
  Trash2,
  TriangleAlert,
} from "lucide-react";

import { api, ApiError } from "../api/client";
import type { ApiKeyLimit, ApiKeyView, CreatedKey } from "../api/types";
import { Card } from "../components/Card";
import { Empty, ErrorState, Loading, Pill } from "../components/Bits";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";

/** Whether this page is being viewed over a loopback address. */
function viewingLocally(): boolean {
  const host = window.location.hostname;
  return (
    host === "127.0.0.1" ||
    host === "localhost" ||
    host === "::1" ||
    host === "[::1]" ||
    host.startsWith("127.")
  );
}

function relativeTime(unixSeconds: number | null): string {
  if (!unixSeconds) return "never";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86_400)}d ago`;
}

function describeLimit(limit: ApiKeyLimit): string {
  const parts: string[] = [];
  if (limit.per_minute != null) parts.push(`${limit.per_minute}/min`);
  if (limit.per_day != null) parts.push(`${limit.per_day}/day`);
  return parts.length ? parts.join(" · ") : "unlimited";
}

export function AccessScreen() {
  const local = useMemo(viewingLocally, []);
  const keys = usePoll(api.keys, 5000);
  const gateway = usePoll(api.gateway, 10_000);

  // The reachable, non-loopback listener the connection block should point a
  // remote agent at — a block showing 127.0.0.1 to someone wiring another
  // machine is worse than none. Falls back to the first listener otherwise.
  const listeners = gateway.data?.listeners ?? [];
  const reachable = listeners.find((l) => !l.loopback) ?? listeners[0];
  const baseUrl = reachable
    ? `http://${reachable.address}/v1`
    : "http://127.0.0.1:11434/v1";

  return (
    <>
      <TopBar
        title="Access & Keys"
        subtitle="Issue keys for remote agents and cap how hard each may hit the engine"
      />
      <div className="page" style={{ display: "grid", gap: 18 }}>
        {!local && (
          <div className="notice notice--warn">
            You are viewing this panel from another machine. Keys can be listed
            here, but creating and revoking them is only allowed from the machine
            running the gateway.
          </div>
        )}

        <ConnectCard baseUrl={baseUrl} exposed={Boolean(reachable && !reachable.loopback)} />

        <KeysCard keys={keys.data} error={keys.error} loading={keys.loading} local={local} onChange={keys.refresh} />
      </div>
    </>
  );
}

function ConnectCard({ baseUrl, exposed }: { baseUrl: string; exposed: boolean }) {
  const [tab, setTab] = useState<"env" | "python" | "curl">("env");

  const snippets: Record<typeof tab, string> = {
    env: `OPENAI_BASE_URL=${baseUrl}\nOPENAI_API_KEY=sk-lw-…`,
    python: `from openai import OpenAI\n\nclient = OpenAI(\n    base_url="${baseUrl}",\n    api_key="sk-lw-…",\n)`,
    curl: `curl ${baseUrl}/models \\\n  -H "Authorization: Bearer sk-lw-…"`,
  };

  return (
    <Card title="Connect an agent">
      <p className="card__note" style={{ marginTop: 0 }}>
        Point any OpenAI-compatible client at this gateway. Create a key below,
        then paste it in place of <code>sk-lw-…</code>.
      </p>
      {!exposed && (
        <div className="notice notice--info" style={{ marginBottom: 12 }}>
          The gateway is currently bound to loopback only, so this address is
          reachable from this machine alone. Bind a reachable address on the API
          Gateway screen to serve other machines.
        </div>
      )}
      <div style={{ display: "flex", gap: 6, marginBottom: 10 }}>
        {(["env", "python", "curl"] as const).map((key) => (
          <button
            key={key}
            type="button"
            className={`btn btn--ghost${tab === key ? " is-active" : ""}`}
            style={tab === key ? { borderColor: "var(--accent)", color: "var(--text)" } : undefined}
            onClick={() => setTab(key)}
          >
            {key === "env" ? "Env" : key === "python" ? "Python" : "curl"}
          </button>
        ))}
      </div>
      <CodeBlock text={snippets[tab]} />
    </Card>
  );
}

function CodeBlock({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard access can be denied; the text is selectable regardless.
    }
  }
  return (
    <div style={{ position: "relative" }}>
      <pre
        className="tnum"
        style={{
          margin: 0,
          padding: "14px 16px",
          background: "var(--surface-sunken)",
          border: "1px solid var(--rule)",
          borderRadius: "var(--radius)",
          fontSize: 12.5,
          overflowX: "auto",
          whiteSpace: "pre",
        }}
      >
        {text}
      </pre>
      <button
        type="button"
        className="btn btn--icon"
        aria-label="Copy to clipboard"
        style={{ position: "absolute", top: 8, right: 8 }}
        onClick={copy}
      >
        {copied ? <Check size={15} color="var(--ok)" /> : <Copy size={15} />}
      </button>
    </div>
  );
}

function KeysCard({
  keys,
  error,
  loading,
  local,
  onChange,
}: {
  keys: ApiKeyView[] | null;
  error: ApiError | null;
  loading: boolean;
  local: boolean;
  onChange: () => void;
}) {
  const [name, setName] = useState("");
  const [perMinute, setPerMinute] = useState("");
  const [perDay, setPerDay] = useState("");
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<CreatedKey | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  async function create() {
    setCreating(true);
    setActionError(null);
    try {
      const limit: ApiKeyLimit = {
        per_minute: perMinute.trim() ? Number(perMinute) : null,
        per_day: perDay.trim() ? Number(perDay) : null,
      };
      const key = await api.createKey({ name: name.trim(), limit });
      setCreated(key);
      setName("");
      setPerMinute("");
      setPerDay("");
      onChange();
    } catch (cause) {
      setActionError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setCreating(false);
    }
  }

  return (
    <Card
      title="API keys"
      action={
        keys && keys.length > 0 ? (
          <Pill tone="neutral">
            {keys.length} {keys.length === 1 ? "key" : "keys"}
          </Pill>
        ) : undefined
      }
    >
      {created && <RevealOnce created={created} onDismiss={() => setCreated(null)} />}

      {local ? (
        <div
          style={{
            display: "flex",
            gap: 8,
            flexWrap: "wrap",
            alignItems: "flex-end",
            marginBottom: 16,
          }}
        >
          <label className="field" style={{ flex: "2 1 180px" }}>
            <span className="field__label">Key name</span>
            <input
              className="input"
              placeholder="e.g. research-agent"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="field" style={{ flex: "1 1 90px" }}>
            <span className="field__label">Per minute</span>
            <input
              className="input"
              inputMode="numeric"
              placeholder="∞"
              value={perMinute}
              onChange={(e) => setPerMinute(e.target.value.replace(/[^0-9]/g, ""))}
            />
          </label>
          <label className="field" style={{ flex: "1 1 90px" }}>
            <span className="field__label">Per day</span>
            <input
              className="input"
              inputMode="numeric"
              placeholder="∞"
              value={perDay}
              onChange={(e) => setPerDay(e.target.value.replace(/[^0-9]/g, ""))}
            />
          </label>
          <button
            type="button"
            className="btn btn--primary"
            disabled={creating}
            onClick={create}
          >
            <Plus size={15} /> Create key
          </button>
        </div>
      ) : (
        <div className="notice notice--info" style={{ marginBottom: 14 }}>
          Create keys from the machine running the gateway, or with{" "}
          <code>hermes key create</code>.
        </div>
      )}

      {actionError && (
        <div className="notice notice--danger" style={{ marginBottom: 12 }}>
          {actionError}
        </div>
      )}

      {error ? (
        <ErrorState error={error} onRetry={onChange} />
      ) : loading && !keys ? (
        <Loading what="keys" />
      ) : keys && keys.length > 0 ? (
        <div style={{ display: "grid", gap: 8 }}>
          {keys.map((key) => (
            <KeyRow key={key.id} value={key} local={local} onChange={onChange} onError={setActionError} />
          ))}
        </div>
      ) : (
        <Empty
          title="No API keys yet"
          hint={local ? "Create one above to let an agent authenticate." : undefined}
        />
      )}
    </Card>
  );
}

function RevealOnce({ created, onDismiss }: { created: CreatedKey; onDismiss: () => void }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(created.key);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      /* selectable regardless */
    }
  }
  return (
    <div
      className="notice notice--warn"
      role="status"
      aria-live="polite"
      style={{ marginBottom: 16, display: "grid", gap: 10 }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8, fontWeight: 600 }}>
        <ShieldCheck size={16} /> Copy this key now — it is shown only once
      </div>
      <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
        <code
          className="tnum"
          style={{
            flex: 1,
            padding: "9px 12px",
            background: "var(--surface-sunken)",
            borderRadius: "var(--radius)",
            overflowX: "auto",
            whiteSpace: "nowrap",
          }}
        >
          {created.key}
        </code>
        <button type="button" className="btn" onClick={copy}>
          {copied ? <Check size={15} color="var(--ok)" /> : <Copy size={15} />}
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <p className="card__note" style={{ margin: 0 }}>
        It is stored hashed and cannot be recovered — if it is lost, revoke it and
        create another. <button type="button" className="btn btn--ghost" onClick={onDismiss}>Done</button>
      </p>
    </div>
  );
}

function KeyRow({
  value,
  local,
  onChange,
  onError,
}: {
  value: ApiKeyView;
  local: boolean;
  onChange: () => void;
  onError: (message: string) => void;
}) {
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);

  async function revoke() {
    setBusy(true);
    try {
      await api.revokeKey(value.id);
      onChange();
    } catch (cause) {
      onError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
      setConfirming(false);
    }
  }

  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: 12,
        padding: "12px 14px",
        border: "1px solid var(--rule)",
        borderRadius: "var(--radius)",
      }}
    >
      <KeyRound size={16} style={{ color: "var(--text-faint)", flexShrink: 0 }} />
      <div style={{ minWidth: 0, flex: 1 }}>
        <div style={{ fontWeight: 500, display: "flex", gap: 8, alignItems: "center" }}>
          {value.name || <span style={{ color: "var(--text-faint)" }}>(unnamed)</span>}
          <code className="tnum" style={{ color: "var(--text-muted)", fontSize: 12.5 }}>
            {value.prefix}…
          </code>
        </div>
        <div className="card__note" style={{ marginTop: 3 }}>
          {describeLimit(value.limit)} · {value.today} today · last used {relativeTime(value.last_used)}
        </div>
      </div>
      {value.in_last_minute > 0 && (
        <Pill tone="accent">{value.in_last_minute}/min now</Pill>
      )}
      {local &&
        (confirming ? (
          <span style={{ display: "inline-flex", gap: 6 }}>
            <button type="button" className="btn btn--danger" disabled={busy} onClick={revoke}>
              <TriangleAlert size={14} /> Confirm revoke
            </button>
            <button type="button" className="btn btn--ghost" onClick={() => setConfirming(false)}>
              Cancel
            </button>
          </span>
        ) : (
          <button
            type="button"
            className="btn btn--ghost"
            aria-label={`Revoke ${value.name || "key"}`}
            onClick={() => setConfirming(true)}
          >
            <Trash2 size={15} /> Revoke
          </button>
        ))}
    </div>
  );
}
