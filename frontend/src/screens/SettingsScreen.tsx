import { useEffect, useState } from "react";

import { api } from "../api/client";
import { bytes } from "../api/format";
import { wasRead } from "../api/types";
import { Card } from "../components/Card";
import { Row, Switch } from "../components/Bits";
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
