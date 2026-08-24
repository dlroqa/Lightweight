import { useCallback, useEffect, useRef, useState } from "react";

import { ApiError } from "../api/client";

export interface Poll<T> {
  data: T | null;
  error: ApiError | null;
  loading: boolean;
  refresh: () => void;
}

/**
 * Call `fetcher` now, then every `intervalMs`.
 *
 * Two properties matter and neither is free:
 *
 * * **A failed poll does not erase the last good answer.** A dashboard that
 *   empties itself the moment one request fails is less useful than one that
 *   keeps the last reading and says it is stale.
 * * **A late response never overwrites a newer one.** Responses can arrive out
 *   of order, and a slow poll landing after a fast one would make the numbers
 *   jump backwards.
 */
export function usePoll<T>(
  fetcher: () => Promise<T>,
  intervalMs: number,
  enabled = true,
): Poll<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const generation = useRef(0);
  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  const run = useCallback(async () => {
    const mine = ++generation.current;
    try {
      const next = await fetcherRef.current();
      if (mine !== generation.current) return;
      setData(next);
      setError(null);
    } catch (cause) {
      if (mine !== generation.current) return;
      setError(
        cause instanceof ApiError
          ? cause
          : new ApiError(0, "unknown", String(cause), []),
      );
    } finally {
      if (mine === generation.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!enabled) return;
    void run();
    if (intervalMs <= 0) return;
    const timer = window.setInterval(() => void run(), intervalMs);
    return () => window.clearInterval(timer);
  }, [run, intervalMs, enabled]);

  return { data, error, loading, refresh: run };
}
