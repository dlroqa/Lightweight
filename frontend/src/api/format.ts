/**
 * Turning the gateway's numbers into the strings on screen.
 *
 * Kept in one place so that a byte count means the same thing on every screen.
 */

const KIB = 1024;

/** Bytes as the reference shows them: `0.78 GB`, `1.12 GB`, `105 MB`. */
export function bytes(value: number | null | undefined): string {
  if (value === null || value === undefined) return "—";
  if (value < KIB) return `${value} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let size = value / KIB;
  let unit = 0;
  while (size >= KIB && unit < units.length - 1) {
    size /= KIB;
    unit += 1;
  }
  return `${size < 10 ? size.toFixed(2) : size.toFixed(size < 100 ? 1 : 0)} ${units[unit]}`;
}

/** A token count as `32K`, `128K`, `8,192`. */
export function contextLength(value: number | null | undefined): string {
  if (value === null || value === undefined) return "—";
  if (value >= 1024 && value % 1024 === 0) return `${value / 1024}K`;
  return value.toLocaleString();
}

/** A parameter count as `1.2B`, `135M`. */
export function parameters(value: number | null | undefined): string {
  if (value === null || value === undefined) return "—";
  if (value >= 1e9) return `${(value / 1e9).toFixed(value < 1e10 ? 1 : 0)}B`;
  if (value >= 1e6) return `${Math.round(value / 1e6)}M`;
  return value.toLocaleString();
}

/** Seconds as `02:14:33`. */
export function duration(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  return [hours, minutes, rest]
    .map((part) => String(part).padStart(2, "0"))
    .join(":");
}

/** A fraction as `24%`. */
export function percent(fraction: number | null | undefined, digits = 0): string {
  if (fraction === null || fraction === undefined || Number.isNaN(fraction)) {
    return "—";
  }
  return `${(fraction * 100).toFixed(digits)}%`;
}

/** A unix-seconds timestamp as a local clock time. */
export function clock(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** An ISO timestamp as `10:42:19`. */
export function clockWithSeconds(iso: string): string {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return iso;
  return at.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/** A unix-seconds timestamp as the sidebar shows it: a time, or a date. */
export function whenever(unixSeconds: number): string {
  const at = new Date(unixSeconds * 1000);
  const now = new Date();
  const sameDay = at.toDateString() === now.toDateString();
  if (sameDay) {
    return at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (at.toDateString() === yesterday.toDateString()) return "Yesterday";
  return at.toLocaleDateString([], { month: "short", day: "numeric" });
}

/** A rate, to one decimal, or an em dash when there is nothing to report. */
export function rate(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  return value.toFixed(1);
}
