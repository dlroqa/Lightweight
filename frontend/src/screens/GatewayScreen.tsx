import { useState } from "react";
import { Check, Copy, Lock, Unlock } from "lucide-react";

import { api } from "../api/client";
import { duration } from "../api/format";
import { Card } from "../components/Card";
import { ErrorState, Loading, Pill, Row } from "../components/Bits";
import { TopBar } from "../components/Shell";
import { useRequestEvents } from "../hooks/useEventStream";
import { usePoll } from "../hooks/usePoll";

const ENDPOINTS = [
  { method: "POST", path: "/v1/chat/completions" },
  { method: "POST", path: "/v1/completions" },
  { method: "GET", path: "/v1/models" },
  { method: "GET", path: "/health" },
  { method: "GET", path: "/metrics" },
];

/**
 * What this gateway is, where it answers, and what it is doing.
 *
 * Everything on this screen is read-only, and says why. The address, the port
 * and the key are settled before the first listener is bound; changing one
 * means restarting the listener, which nothing inside the process can do to
 * itself. The gateway names those fields in `restart_required` so the panel can
 * be honest rather than offering a control that would quietly fail.
 */
export function GatewayScreen() {
  const gateway = usePoll(api.gateway, 3000);
  const metrics = usePoll(api.metrics, 2000);
  const events = useRequestEvents(15);
  const [copied, setCopied] = useState<string | null>(null);

  const origin = window.location.origin;

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(text);
      window.setTimeout(() => setCopied(null), 1500);
    } catch {
      // Clipboard access can be refused; the address is on screen either way.
    }
  }

  return (
    <>
      <TopBar title="API Gateway" subtitle="Manage your local inference API" />

      <div className="page">
        {gateway.error ? (
          <Card>
            <ErrorState error={gateway.error} onRetry={gateway.refresh} />
          </Card>
        ) : !gateway.data ? (
          <Card>
            <Loading what="the gateway" />
          </Card>
        ) : (
          <>
            <div
              className="grid"
              style={{ gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))" }}
            >
              <Card
                title="Status"
                action={<Pill tone="ok" dot>Running</Pill>}
              >
                <Row label="Version">{gateway.data.version}</Row>
                <Row label="Backend">{gateway.data.backend}</Row>
                <Row label="Engine">{gateway.data.engine.state}</Row>
                <Row label="Model">{gateway.data.model ?? "none loaded"}</Row>
                <Row label="Uptime">
                  {metrics.data ? duration(metrics.data.uptime_seconds) : "—"}
                </Row>
                <Row label="In flight">{metrics.data?.in_flight ?? "—"}</Row>
                <div className="card__note" style={{ marginTop: 10 }}>
                  In-flight counts requests the gateway is serving, not connected
                  clients — one client holds one connection across many requests,
                  and this page's own polling is excluded from the number.
                </div>
              </Card>

              <Card title="Configuration">
                {gateway.data.listeners.map((listener) => (
                  <Row key={listener.address} label="Serving on">
                    <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
                      {listener.address}
                      {listener.loopback ? (
                        <Pill tone="neutral">loopback</Pill>
                      ) : (
                        <Pill tone="warn">reachable</Pill>
                      )}
                    </span>
                  </Row>
                ))}
                {gateway.data.listeners.length === 0 && (
                  <Row label="Serving on">not recorded</Row>
                )}
                <Row label="Authentication">
                  <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                    {gateway.data.auth.required ? (
                      <>
                        <Lock size={14} /> key required
                      </>
                    ) : (
                      <>
                        <Unlock size={14} /> open on loopback
                      </>
                    )}
                  </span>
                </Row>
                <Row label="Concurrent requests">
                  {gateway.data.concurrency.max_concurrent_requests}
                  <span className="card__note" style={{ marginLeft: 8 }}>
                    {gateway.data.concurrency.requested === null
                      ? "derived from this machine"
                      : "as asked for at startup"}
                  </span>
                </Row>
                <Row label="Queue timeout">
                  {gateway.data.concurrency.queue_timeout_seconds}s
                </Row>

                <div className="notice notice--info" style={{ marginTop: 14 }}>
                  {gateway.data.restart_required.join(", ")} are fixed when the
                  gateway starts. Changing any of them means restarting the
                  listener, so they are shown here rather than offered as
                  controls.
                </div>
                <div className="card__note" style={{ marginTop: 10 }}>
                  The API key is never sent to this page, by design — it is kept
                  out of the log and out of the engine's command line for the same
                  reason.
                </div>
              </Card>

              <Card title="Paths">
                {gateway.data.paths ? (
                  <>
                    <Row label="Data">{gateway.data.paths.data}</Row>
                    <Row label="Models">{gateway.data.paths.models}</Row>
                    <Row label="Logs">{gateway.data.paths.logs}</Row>
                  </>
                ) : (
                  <div className="card__note">
                    This gateway was started without a data directory.
                  </div>
                )}
              </Card>
            </div>

            <div
              className="grid"
              style={{ gridTemplateColumns: "repeat(auto-fit, minmax(320px, 1fr))" }}
            >
              <Card title="Endpoints">
                <ul style={{ margin: 0, padding: 0, listStyle: "none" }}>
                  {ENDPOINTS.map((endpoint) => {
                    const full = `${origin}${endpoint.path}`;
                    return (
                      <li
                        key={endpoint.path}
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: 10,
                          padding: "8px 0",
                          borderBottom: "1px solid var(--rule)",
                          fontSize: 12.5,
                        }}
                      >
                        <Pill tone={endpoint.method === "GET" ? "accent" : "info"}>
                          {endpoint.method}
                        </Pill>
                        <span
                          style={{
                            flex: 1,
                            minWidth: 0,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          {full}
                        </span>
                        <button
                          type="button"
                          className="btn btn--ghost btn--icon"
                          style={{ width: 28, height: 28 }}
                          aria-label={`Copy ${full}`}
                          onClick={() => void copy(full)}
                        >
                          {copied === full ? <Check size={14} /> : <Copy size={14} />}
                        </button>
                      </li>
                    );
                  })}
                </ul>
              </Card>

              <Card title="Recent requests">
                {events.length === 0 ? (
                  <div className="card__note">
                    Nothing yet. Finished generations appear here as they happen.
                  </div>
                ) : (
                  <div className="scroll-x">
                    <table className="table">
                      <thead>
                        <tr>
                          <th>Time</th>
                          <th>Model</th>
                          <th>Prompt</th>
                          <th>Output</th>
                          <th>Took</th>
                          <th>Finish</th>
                        </tr>
                      </thead>
                      <tbody>
                        {events.map((event, index) => (
                          <tr key={`${event.at_unix_ms}-${index}`}>
                            <td className="tnum">
                              {new Date(event.at_unix_ms).toLocaleTimeString([], {
                                hour: "2-digit",
                                minute: "2-digit",
                                second: "2-digit",
                              })}
                            </td>
                            <td>{event.model ?? "—"}</td>
                            <td className="tnum">{event.prompt_tokens}</td>
                            <td className="tnum">{event.completion_tokens}</td>
                            <td className="tnum">{event.total_ms} ms</td>
                            <td>{event.finish_reason ?? "cancelled"}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </Card>
            </div>
          </>
        )}
      </div>
    </>
  );
}
