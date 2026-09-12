import { useEffect, useState } from "react";

import { api } from "../api/client";
import { agentApi, type LightagentSettings as LightagentSettingsValue } from "../api/agent";
import { bytes } from "../api/format";
import { wasRead } from "../api/types";
import { Card } from "../components/Card";
import { Pill, Row, Switch } from "../components/Bits";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";
import { usePreferences } from "../state/preferences";

export function SettingsScreen() {
  const { preferences, settings, update, saveGateway, offline } = usePreferences();
  const system = usePoll(api.system, 5000);
  const gateway = usePoll(api.gateway, 10_000);

  const [contextDraft, setContextDraft] = useState<string>("");
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    setContextDraft(
      settings?.gateway.default_n_ctx ? String(settings.gateway.default_n_ctx) : "",
    );
  }, [settings?.gateway.default_n_ctx]);

  async function persist(patch: Parameters<typeof saveGateway>[0]) {
    setSaving(true);
    setSaveError(null);
    setSaved(false);
    try {
      await saveGateway(patch);
      setSaved(true);
      window.setTimeout(() => setSaved(false), 2000);
    } catch (cause) {
      setSaveError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  }

  const disk = wasRead(system.data?.disk) ? system.data.disk : null;
  const models = wasRead(disk?.models) ? disk.models : null;
  const downloads = wasRead(disk?.downloads) ? disk.downloads : null;

  return (
    <>
      <TopBar title="Settings" subtitle="Customise the panel and the gateway" />

      <div className="page">
        <AgentServerSettings />
        {offline && (
          <div className="notice notice--warn">
            Settings could not be read from the gateway, so changes are being
            kept in this browser only.
          </div>
        )}
        {saveError && <div className="notice notice--danger">{saveError}</div>}
        {saved && <div className="notice notice--info">Saved.</div>}

        <div
          className="grid"
          style={{ gridTemplateColumns: "repeat(auto-fit, minmax(300px, 1fr))" }}
        >
          <LightagentSettings />
          <Card title="Appearance">
            <div className="field" style={{ marginBottom: 16 }}>
              <label className="field__label" htmlFor="theme">
                Theme
              </label>
              <select
                id="theme"
                className="select"
                value={preferences.theme}
                onChange={(event) =>
                  update({ theme: event.target.value as typeof preferences.theme })
                }
              >
                <option value="system">Match the system</option>
                <option value="light">Light</option>
                <option value="dark">Dark</option>
              </select>
            </div>

            <ToggleRow
              label="Translucent surfaces"
              hint="Frosted panels look better over the background. Turning this off makes every surface solid, which is the safer choice if text ever feels hard to read."
              checked={preferences.translucent}
              onChange={(translucent) => update({ translucent })}
            />
            <ToggleRow
              label="Compact density"
              hint="Tightens spacing so more fits on screen."
              checked={preferences.compact}
              onChange={(compact) => update({ compact })}
            />
            <ToggleRow
              label="Collapse the sidebar"
              hint="Shows icons only."
              checked={preferences.railCollapsed}
              onChange={(railCollapsed) => update({ railCollapsed })}
            />
          </Card>

          <Card title="Privacy and history">
            <ToggleRow
              label="Keep conversation history"
              hint="Conversations are written to the gateway's data directory, readable only by your user. Turning this off refuses new writes; what is already saved stays readable so you can still look at it or delete it."
              checked={settings?.gateway.keep_history ?? true}
              disabled={saving || !settings}
              onChange={(keep_history) => void persist({ keep_history })}
            />
            <div className="card__note" style={{ marginTop: 12 }}>
              Prompts and completions are never written to the log, in any
              setting. That is separate from history and is not configurable
              here.
            </div>
          </Card>

          <Card title="Model loading">
            <div className="field">
              <label className="field__label" htmlFor="default-ctx">
                Default context length
              </label>
              <div style={{ display: "flex", gap: 8 }}>
                <input
                  id="default-ctx"
                  className="input tnum"
                  type="number"
                  min={256}
                  step={256}
                  placeholder="Fit to this machine"
                  value={contextDraft}
                  onChange={(event) => setContextDraft(event.target.value)}
                />
                <button
                  type="button"
                  className="btn"
                  disabled={saving}
                  onClick={() =>
                    void persist({
                      default_n_ctx: contextDraft.trim()
                        ? Number(contextDraft)
                        : null,
                    })
                  }
                >
                  Save
                </button>
              </div>
              <span style={{ fontSize: 11.5, color: "var(--text-muted)" }}>
                Left empty, each load picks the largest context this machine can
                safely hold. A value here is used instead — and is still checked
                against the memory estimate, so it can make a load smaller than it
                might have been but never larger than it should be.
              </span>
            </div>
          </Card>

          <Card title="Storage">
            {gateway.data?.paths ? (
              <>
                <Row label="Models">{gateway.data.paths.models}</Row>
                <Row label="Data">{gateway.data.paths.data}</Row>
                <Row label="Logs">{gateway.data.paths.logs}</Row>
              </>
            ) : (
              <div className="card__note">
                This gateway was started without a data directory.
              </div>
            )}
            {models && (
              <Row label="Free where models live">{bytes(models.available)}</Row>
            )}
            {downloads && disk?.same_filesystem === false && (
              <Row label="Free where downloads land">
                {bytes(downloads.available)}
              </Row>
            )}
            <div className="card__note" style={{ marginTop: 10 }}>
              These paths are chosen by the platform and are fixed for the life of
              the process.
            </div>
          </Card>

          <Card title="About">
            <Row label="Gateway version">{gateway.data?.version ?? "—"}</Row>
            <Row label="Backend">{gateway.data?.backend ?? "—"}</Row>
            <Row label="Processor">{system.data?.cpu.model ?? "—"}</Row>
            <Row label="Platform">
              {system.data
                ? `${system.data.os.name} · ${system.data.os.architecture}`
                : "—"}
            </Row>
          </Card>
        </div>
      </div>
    </>
  );
}

function LightagentSettings() {
  const settings = usePoll(agentApi.settings, 0);
  const [current, setCurrent] = useState<LightagentSettingsValue | null>(null);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    if (settings.data) setCurrent(settings.data);
  }, [settings.data]);

  async function persist(patch: Partial<LightagentSettingsValue>) {
    if (!current || saving) return;
    setSaving(true);
    setMessage(null);
    try {
      const saved = await agentApi.saveSettings({ ...current, ...patch });
      setCurrent(saved);
      settings.refresh();
      setMessage("Saved. New runs use these settings.");
    } catch (cause) {
      setMessage(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  }

  const value = current ?? settings.data;
  return (
    <Card title="Lightagent">
      <p className="card__note">
        Controls the Lightagent CLI runtime used by Agent chat. Changes apply to
        the next run and are shared with the terminal UI.
      </p>
      {settings.error && !value ? (
        <div className="notice notice--warn">
          <div>Could not load Lightagent settings: {settings.error.message}</div>
          <button type="button" className="btn" style={{ marginTop: 8 }}
            onClick={settings.refresh}>Retry</button>
        </div>
      ) : (
        <>
          <div className="field" style={{ marginBottom: 12 }}>
            <label className="field__label" htmlFor="agent-approval">Approval policy</label>
            <select id="agent-approval" className="select"
              value={value?.approval_policy ?? "balanced"} disabled={!value || saving}
              onChange={(event) => void persist({
                approval_policy: event.target.value as LightagentSettingsValue["approval_policy"],
              })}>
              <option value="permissive">Permissive</option>
              <option value="balanced">Balanced</option>
              <option value="strict">Strict</option>
            </select>
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10 }}>
            <NumberSetting label="Maximum turns" value={value?.max_turns}
              disabled={!value || saving} onSave={(max_turns) => persist({ max_turns })} />
            <NumberSetting label="Maximum tool calls" value={value?.max_tool_calls}
              disabled={!value || saving}
              onSave={(max_tool_calls) => persist({ max_tool_calls })} />
          </div>
          <OptionalNumberSetting label="Run time limit (seconds)"
            value={value?.wall_clock_secs} disabled={!value || saving}
            onSave={(wall_clock_secs) => persist({ wall_clock_secs })} />
          <ToggleRow label="Web tools" hint="Allow configured web search and fetch tools."
            checked={value?.web_enabled ?? false} disabled={!value || saving}
            onChange={(web_enabled) => void persist({ web_enabled })} />
          <ToggleRow label="Filesystem tools" hint="Allow confined file reads and writes."
            checked={value?.filesystem_tools_enabled ?? false} disabled={!value || saving}
            onChange={(filesystem_tools_enabled) => void persist({
              filesystem_tools_enabled,
              ...(!filesystem_tools_enabled ? { terminal_enabled: false } : {}),
            })} />
          <ToggleRow label="Terminal tool" hint="Allow approval-gated commands in the configured workspace."
            checked={value?.terminal_enabled ?? false}
            disabled={!value || saving || !value?.filesystem_tools_enabled}
            onChange={(terminal_enabled) => void persist({ terminal_enabled })} />
          <ToggleRow label="Durable memory" hint="Capture clear durable facts for future sessions."
            checked={value?.memory_enabled ?? false} disabled={!value || saving}
            onChange={(memory_enabled) => void persist({ memory_enabled })} />
          <ToggleRow label="Show reasoning in terminal" hint="Shared presentation setting for the Lightagent TUI."
            checked={value?.show_reasoning_in_tui ?? true} disabled={!value || saving}
            onChange={(show_reasoning_in_tui) => void persist({ show_reasoning_in_tui })} />
        </>
      )}
      {message && <div className="card__note" style={{ marginTop: 10 }}>{message}</div>}
    </Card>
  );
}

function NumberSetting({ label, value, disabled, onSave }: {
  label: string;
  value: number | undefined;
  disabled: boolean;
  onSave: (value: number) => Promise<void>;
}) {
  const [draft, setDraft] = useState("");
  useEffect(() => setDraft(value === undefined ? "" : String(value)), [value]);
  return (
    <div className="field">
      <label className="field__label">{label}</label>
      <input className="input tnum" type="number" min={1}
        value={draft} disabled={disabled}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={() => {
          const next = Number(draft);
          if (Number.isInteger(next) && next > 0 && next !== value) void onSave(next);
          else setDraft(value === undefined ? "" : String(value));
        }} />
    </div>
  );
}

function OptionalNumberSetting({ label, value, disabled, onSave }: {
  label: string;
  value: number | null | undefined;
  disabled: boolean;
  onSave: (value: number | null) => Promise<void>;
}) {
  const [draft, setDraft] = useState("");
  useEffect(() => setDraft(value == null ? "" : String(value)), [value]);
  return (
    <div className="field" style={{ marginTop: 10 }}>
      <label className="field__label">{label}</label>
      <input className="input tnum" type="number" min={1}
        placeholder="No limit" value={draft} disabled={disabled}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={() => {
          const next = draft.trim() ? Number(draft) : null;
          if ((next === null || Number.isInteger(next) && next > 0) && next !== value) {
            void onSave(next);
          } else {
            setDraft(value == null ? "" : String(value));
          }
        }} />
    </div>
  );
}

function ToggleRow({
  label,
  hint,
  checked,
  onChange,
  disabled,
}: {
  label: string;
  hint: string;
  checked: boolean;
  onChange: (next: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <div
      style={{
        display: "flex",
        alignItems: "flex-start",
        justifyContent: "space-between",
        gap: 16,
        padding: "12px 0",
        borderBottom: "1px solid var(--rule)",
      }}
    >
      <div style={{ minWidth: 0 }}>
        <div style={{ fontSize: 13.5, fontWeight: 500 }}>{label}</div>
        <div style={{ fontSize: 11.5, color: "var(--text-muted)", marginTop: 2 }}>
          {hint}
        </div>
      </div>
      <Switch checked={checked} onChange={onChange} label={label} disabled={disabled} />
    </div>
  );
}

function AgentServerSettings() {
  const server = usePoll(api.agentServer, 2000);
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);
  const status = server.data;
  const pending = starting || status?.status === "starting";
  const running = !server.error && status?.status === "running";
  const message = startError ?? server.error?.message ?? status?.message;

  async function startServer() {
    setStarting(true);
    setStartError(null);
    try {
      await api.startAgentServer();
      await server.refresh();
    } catch (cause) {
      setStartError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setStarting(false);
    }
  }

  return (
    <Card title="Lightagent server">
      <p className="card__note">
        Start the agent server to enable Agent chat and Agent Tools.
        A server started here runs until the gateway closes.
      </p>
      <Row label="Status">
        <span role="status" aria-live="polite">
          <Pill tone={running ? "ok" : pending ? "info" : "warn"}>
            {server.error ? "Status unavailable" : pending ? "Starting…" : running ? "Running" :
              status?.status === "failed" ? "Failed to start" :
              status?.status === "unavailable" ? "Not responding" :
              status ? "Stopped" : "Checking…"}
          </Pill>
        </span>
      </Row>
      <Row label="Address">{status?.upstream ?? "Not configured"}</Row>
      {message && (
        <div className="notice notice--warn" role="alert" style={{ whiteSpace: "pre-wrap", marginTop: 12 }}>
          {message}
        </div>
      )}
      <div style={{ display: "flex", gap: 8, marginTop: 16 }}>
        <button
          type="button"
          className="btn btn--primary"
          disabled={pending || running || !status?.can_start || !!server.error}
          onClick={() => void startServer()}
        >
          {pending ? "Starting…" : running ? "Server running" : "Start server"}
        </button>
        <button type="button" className="btn" onClick={() => void server.refresh()}>
          Refresh status
        </button>
      </div>
    </Card>
  );
}
