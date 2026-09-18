import { useRef, useState } from "react";
import { ChevronDown } from "lucide-react";

import { api, followJob } from "../api/client";
import type { CatalogRow } from "../api/types";
import { Menu, MenuItem } from "./Menu";

/**
 * The model pill in the header: what is loaded, and a way to change it.
 *
 * Swapping a model pauses the scheduler and waits for the running turn, which
 * on this hardware is measured in minutes rather than seconds. The control says
 * so while it happens rather than appearing to hang.
 */
export function ModelSelector({
  models,
  loadedId,
  onChanged,
}: {
  models: CatalogRow[];
  loadedId: string | null;
  onChanged: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const trigger = useRef<HTMLButtonElement | null>(null);

  const loaded = models.find((model) => model.id === loadedId);
  const label = loaded?.name ?? (loadedId ?? "No model loaded");

  async function load(id: string) {
    setBusy(id);
    setFailure(null);
    setOpen(false);
    try {
      // Followed to the end. The 202 says only that the job started; a load
      // refused for memory fails inside it, and stopping at the status code
      // reported that refusal as a success.
      const job = await api.loadModel(id);
      await followJob(job.job);
      onChanged();
    } catch (cause) {
      setFailure(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div style={{ position: "relative" }}>
      <button
        ref={trigger}
        type="button"
        className="btn"
        style={{ minWidth: 210, justifyContent: "space-between", paddingLeft: 12 }}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
        disabled={busy !== null}
      >
        <span style={{ display: "flex", alignItems: "center", gap: 8, minWidth: 0 }}>
          <span
            className="dot"
            style={{ color: loadedId ? "var(--ok)" : "var(--text-faint)" }}
          />
          <span
            style={{
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}
          >
            {busy ? "Loading…" : label}
          </span>
        </span>
        <ChevronDown size={16} />
      </button>

      <Menu
        open={open}
        anchorRef={trigger}
        onClose={() => setOpen(false)}
        align="end"
        minWidth={280}
        label="Choose a model to load"
      >
        {models.length === 0 && <div className="menu__empty">No models installed yet.</div>}
        {models.map((model) => (
          <MenuItem
            key={model.id}
            disabled={model.state === "missing"}
            onClick={() => void load(model.id)}
          >
            <span
              className="dot"
              style={{ color: model.id === loadedId ? "var(--ok)" : "var(--text-faint)" }}
            />
            <span style={{ minWidth: 0, textAlign: "left" }}>
              <span style={{ display: "block" }}>{model.name}</span>
              <span className="menu__meta">
                {model.state === "missing"
                  ? "file missing"
                  : (model.quantization ?? model.architecture)}
              </span>
            </span>
          </MenuItem>
        ))}
      </Menu>

      {failure && (
        <div
          className="notice notice--danger"
          style={{ position: "absolute", right: 0, top: "calc(100% + 6px)", zIndex: 60, width: 320 }}
        >
          {failure}
        </div>
      )}
    </div>
  );
}
