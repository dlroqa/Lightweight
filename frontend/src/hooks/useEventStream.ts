import { useEffect, useState } from "react";

import type { RequestEvent } from "../api/types";

/**
 * The live feed, from `GET /api/v1/events`.
 *
 * `EventSource` rather than a `fetch` reader: it reconnects on its own, which
 * is the behaviour wanted for a stream that is meant to stay open for as long
 * as the panel is. The gateway's stream never ends by itself.
 */
export function useRequestEvents(limit = 40): RequestEvent[] {
  const [events, setEvents] = useState<RequestEvent[]>([]);

  useEffect(() => {
    const source = new EventSource("/api/v1/events");
    source.onmessage = (message) => {
      try {
        const parsed = JSON.parse(message.data) as RequestEvent | { missed: number };
        // A lag notice carries only a count; there is no request to show.
        if (!("at_unix_ms" in parsed)) return;
        setEvents((current) => [parsed, ...current].slice(0, limit));
      } catch {
        // A frame that will not parse costs only itself.
      }
    };
    return () => source.close();
  }, [limit]);

  return events;
}
