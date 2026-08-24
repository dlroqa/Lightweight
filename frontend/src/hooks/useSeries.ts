import { useEffect, useRef, useState } from "react";

/**
 * A bounded rolling series, for the sparklines.
 *
 * The gateway keeps cumulative counters and no history, deliberately — a ring
 * buffer in the server is state to own, invalidate and test for the benefit of
 * one screen. So the history lives here, and a sparkline means exactly "since
 * you opened this screen", which is both honest and what a control panel wants.
 */
export function useSeries(value: number | null, length = 40): number[] {
  const [series, setSeries] = useState<number[]>([]);
  const last = useRef<number | null>(null);

  useEffect(() => {
    if (value === null || !Number.isFinite(value)) return;
    // Guard against React re-running the effect for an unchanged value, which
    // would pack the series with duplicates and flatten it.
    if (last.current === value && series.length > 0) return;
    last.current = value;
    setSeries((current) => [...current, value].slice(-length));
    // `series.length` is deliberately not a dependency: including it would
    // re-run this on every append.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value, length]);

  return series;
}

/**
 * Processor utilization, differenced from two readings of `/proc/stat`.
 *
 * The endpoint publishes cumulative ticks precisely because a rate needs two
 * readings of them; this is the other half of that decision. `null` until a
 * second sample exists, and `null` again if the counters ever go backwards —
 * never `0`, which would be a claim that the machine was idle.
 */
export function useUtilization(
  times: { total: number; idle: number } | null,
): number | null {
  const previous = useRef<{ total: number; idle: number } | null>(null);
  const [utilization, setUtilization] = useState<number | null>(null);

  useEffect(() => {
    if (!times) return;
    const before = previous.current;
    previous.current = times;
    if (!before) return;

    const elapsed = times.total - before.total;
    const busy = times.total - times.idle - (before.total - before.idle);
    if (elapsed <= 0 || busy < 0) {
      setUtilization(null);
      return;
    }
    setUtilization(Math.min(1, Math.max(0, busy / elapsed)));
  }, [times]);

  return utilization;
}
