import type { ReactNode } from "react";

import { AlertTriangle, Loader2 } from "lucide-react";

import { ApiError } from "../api/client";

export function Pill({
  tone,
  children,
  dot,
}: {
  tone: "ok" | "warn" | "danger" | "info" | "accent" | "neutral";
  children: ReactNode;
  dot?: boolean;
}) {
  return (
    <span className={`pill pill--${tone}`}>
      {dot && <span className="dot" />}
      {children}
    </span>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      className="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    />
  );
}

/**
 * What to show when a request failed.
 *
 * The gateway's remedies are rendered, not swallowed: the error taxonomy has
 * carried "what to do about it" since M0 so that a UI can offer a next step
 * instead of an apology.
 */
export function ErrorState({ error, onRetry }: { error: ApiError; onRetry?: () => void }) {
  return (
    <div className="empty">
      <AlertTriangle size={22} color="var(--danger)" />
      <strong>{error.message}</strong>
      {error.remedies.length > 0 && (
        <ul style={{ margin: 0, padding: 0, listStyle: "none", maxWidth: "52ch" }}>
          {error.remedies.map((remedy) => (
            <li key={remedy.label} style={{ marginTop: 4 }}>
              {remedy.label}
            </li>
          ))}
        </ul>
      )}
      {onRetry && (
        <button type="button" className="btn" onClick={onRetry} style={{ marginTop: 8 }}>
          Try again
        </button>
      )}
    </div>
  );
}

export function Loading({ what }: { what: string }) {
  return (
    <div className="empty">
      <Loader2 size={20} className="spin" />
      <span>Loading {what}…</span>
    </div>
  );
}

export function Empty({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="empty">
      <strong>{title}</strong>
      {hint && <span style={{ maxWidth: "48ch" }}>{hint}</span>}
    </div>
  );
}

/**
 * A row of label and value, as the detail cards use throughout the reference.
 */
export function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "space-between",
        gap: 16,
        padding: "9px 0",
        borderBottom: "1px solid var(--rule)",
        fontSize: 13,
      }}
    >
      <span style={{ color: "var(--text-muted)" }}>{label}</span>
      <span className="tnum" style={{ fontWeight: 500, textAlign: "right" }}>
        {children}
      </span>
    </div>
  );
}
