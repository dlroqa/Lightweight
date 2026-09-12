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
 * idle. `EventSource` reconnects on its own. A transient stream error must
 * not make a still-running model look idle; the run endpoint is the fallback
 * source of truth when a terminal event is missed.
 */
export function useRunEvents(runId: string | null): { events: RunEvent[]; done: boolean } {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [done, setDone] = useState(false);
  const [observedId, setObservedId] = useState<string | null>(null);

  useEffect(() => {
    setObservedId(runId);
    if (!runId) {
      setEvents([]);
      setDone(false);
      return;
    }
    setEvents([]);
    setDone(false);

    const source = new EventSource(agentApi.eventsUrl(runId));
    const listeners: Array<[string, EventListener]> = [];
    let closed = false;
    const finish = () => {
      if (closed) return;
      closed = true;
      setDone(true);
      source.close();
      window.clearInterval(statusTimer);
    };
    const checkStatus = async () => {
      if (closed) return;
      try {
        const run = await agentApi.run(runId);
        if (closed || !TERMINAL.has(`run.${run.status}`)) return;
        setEvents((current) => current.some((event) => TERMINAL.has(event.type))
          ? current
          : [...current, { type: `run.${run.status}`, data: {} }]);
        finish();
      } catch {
        // Let EventSource reconnect; a failed status probe is not completion.
      }
    };

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
          finish();
        }
      };
      source.addEventListener(name, handler as EventListener);
      listeners.push([name, handler as EventListener]);
    }

    source.onerror = () => { void checkStatus(); };
    const statusTimer = window.setInterval(() => void checkStatus(), 2000);

    return () => {
      closed = true;
      window.clearInterval(statusTimer);
      for (const [name, handler] of listeners) {
        source.removeEventListener(name, handler);
      }
      source.close();
    };
  }, [runId]);

  // A new id can render before this effect has connected its EventSource.
  // Never expose the previous run's terminal state or events in that gap.
  return observedId === runId ? { events, done } : { events: [], done: false };
}
