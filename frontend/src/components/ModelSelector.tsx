import { useState } from "react";
import { ChevronDown } from "lucide-react";

import { api } from "../api/client";
import type { CatalogRow } from "../api/types";

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

  const loaded = models.find((model) => model.id === loadedId);
  const label = loaded?.name ?? (loadedId ?? "No model loaded");

  async function load(id: string) {
    setBusy(id);
    setFailure(null);
    setOpen(false);
    try {
      await api.loadModel(id);
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
        type="button"
        className="btn"
        style={{ minWidth: 210, justifyContent: "space-between", paddingLeft: 12 }}
        aria-haspopup="listbox"
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

      {open && (
        <>
          <div
            style={{ position: "fixed", inset: 0, zIndex: 40 }}
            onClick={() => setOpen(false)}
            aria-hidden="true"
          />
          <ul
            role="listbox"
            style={{
              position: "absolute",
              right: 0,
              top: "calc(100% + 6px)",
              zIndex: 50,
              margin: 0,
              padding: 6,
              listStyle: "none",
              minWidth: 280,
              maxHeight: 320,
              overflowY: "auto",
              borderRadius: "var(--radius-lg)",
              border: "1px solid var(--border)",
              background: "var(--surface-raised)",
              backdropFilter: "blur(var(--glass-blur))",
              boxShadow: "var(--shadow-lg)",
            }}
          >
            {models.length === 0 && (
              <li style={{ padding: 12, color: "var(--text-muted)", fontSize: 13 }}>
                No models installed yet.
              </li>
            )}
            {models.map((model) => (
              <li key={model.id}>
                <button
                  type="button"
                  role="option"
                  aria-selected={model.id === loadedId}
                  className="btn btn--ghost"
                  style={{ width: "100%", justifyContent: "flex-start", gap: 10 }}
                  disabled={model.state === "missing"}
                  onClick={() => void load(model.id)}
                >
                  <span
                    className="dot"
                    style={{
                      color:
                        model.id === loadedId ? "var(--ok)" : "var(--text-faint)",
                    }}
                  />
                  <span style={{ minWidth: 0, textAlign: "left" }}>
                    <span style={{ display: "block" }}>{model.name}</span>
                    <span style={{ fontSize: 11.5, color: "var(--text-muted)" }}>
                      {model.state === "missing"
                        ? "file missing"
                        : (model.quantization ?? model.architecture)}
                    </span>
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </>
      )}

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
