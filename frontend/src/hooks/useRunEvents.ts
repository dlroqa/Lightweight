import { useEffect, useState } from "react";

import { agentApi } from "../api/agent";

/**
 * One decoded event from a run's SSE stream.
 *
 * `type` is the canonical name the API sent (`run.started`, `model.delta`,
 * `tool.output`, `run.completed`, …); `data` is its parsed JSON payload.
 */
export interface RunEvent {
  type: string;
  data: Record<string, unknown>;
}

/** The named events the API emits, listened for individually as SSE requires. */
const EVENT_NAMES = [
  "run.started",
  "model.delta",
  "tool.requested",
  "tool.started",
  "tool.output",
  "tool.failed",
  "approval.required",
  "turn.completed",
  "run.completed",
  "run.cancelled",
  "run.failed",
  "error",
];

const TERMINAL = new Set(["run.completed", "run.cancelled", "run.failed"]);

/**
 * Stream a run's events over SSE. Returns the events so far and whether the run
 * has reached a terminal state. Passing a new `runId` starts fresh; `null` is
 * idle. `EventSource` reconnects on its own, and a terminal event closes it.
 */
export function useRunEvents(runId: string | null): { events: RunEvent[]; done: boolean } {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [done, setDone] = useState(false);

  useEffect(() => {
    if (!runId) {
      setEvents([]);
      setDone(false);
      return;
    }
    setEvents([]);
    setDone(false);

    const source = new EventSource(agentApi.eventsUrl(runId));
    const listeners: Array<[string, EventListener]> = [];

    for (const name of EVENT_NAMES) {
      const handler = (event: MessageEvent) => {
        let data: Record<string, unknown> = {};
        try {
          data = JSON.parse(event.data) as Record<string, unknown>;
        } catch {
          // A frame that will not parse costs only itself.
        }
        setEvents((current) => [...current, { type: name, data }]);
        if (TERMINAL.has(name)) {
          setDone(true);
          source.close();
        }
      };
      source.addEventListener(name, handler as EventListener);
      listeners.push([name, handler as EventListener]);
    }

    source.onerror = () => {
      // The stream closed (the run ended and the server hung up, or a network
      // fault). Either way there is no more to read.
      setDone(true);
      source.close();
    };

    return () => {
      for (const [name, handler] of listeners) {
        source.removeEventListener(name, handler);
      }
      source.close();
    };
  }, [runId]);

  return { events, done };
}
