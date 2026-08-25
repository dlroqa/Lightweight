import { useMemo } from "react";
import { Link } from "react-router-dom";
import {
  ChevronRight,
  Clock,
  Cpu,
  Layers,
  MemoryStick,
  Zap,
} from "lucide-react";

import { api } from "../api/client";
import { bytes, contextLength, duration, percent, rate } from "../api/format";
import { wasRead } from "../api/types";
import { Card } from "../components/Card";
import { Meter } from "../components/Meter";
import { ModelSelector } from "../components/ModelSelector";
import { Pill, Row } from "../components/Bits";
import { StatTile } from "../components/StatTile";
import { TopBar } from "../components/Shell";
import { useRequestEvents } from "../hooks/useEventStream";
import { usePoll } from "../hooks/usePoll";
import { useSeries, useUtilization } from "../hooks/useSeries";

export function Dashboard() {
  // One second, which is the tick the whole panel's charts are built on: the
  // gateway publishes counters and the client differences them.
  const system = usePoll(api.system, 1000);
  const metrics = usePoll(api.metrics, 1000);
  const models = usePoll(api.models, 5000);
  const gateway = usePoll(api.gateway, 5000);
  const events = useRequestEvents(12);
  // The same one-second tick: what is running now changes while a request is
  // still open, which is exactly when the finished-generation feed says
  // nothing.
  const roster = usePoll(api.requests, 1000);
  const waiting = roster.data?.waiting ?? [];
  // The one at the front, which is the only wait a reader can act on.
  const next = waiting[0];

  const times = wasRead(system.data?.cpu_times) ? system.data.cpu_times : null;
  const utilization = useUtilization(times);
  const memory = wasRead(system.data?.memory) ? system.data.memory : null;
  // What the engine is holding, when there is one and this platform can say.
  // Shown beside the machine's total because a Coarse estimate is only
  // checkable against the number it was trying to predict.
  const engine = wasRead(metrics.data?.engine) ? metrics.data.engine : null;
  const disk = wasRead(system.data?.disk) ? system.data.disk : null;
  const diskModels = wasRead(disk?.models) ? disk.models : null;

  const decodeRate = useMemo(() => {
    const decode = metrics.data?.decode;
    const decoded = metrics.data?.tokens.decoded;
    if (!decode || decoded === undefined || decode.total_ms === 0) return null;
    return (decoded / decode.total_ms) * 1000;
  }, [metrics.data]);

  const cpuSeries = useSeries(utilization === null ? null : utilization * 100);
  const ramSeries = useSeries(memory ? memory.used / 1024 ** 3 : null);
  const rateSeries = useSeries(decodeRate);

  const loadedId = metrics.data?.model?.id ?? null;
  const loadedModel = models.data?.find(
    (model) => loadedId !== null && loadedId.startsWith(model.id),
  );

  return (
    <>
      <TopBar
        title="Dashboard"
        subtitle="System overview and live statistics"
        actions={
          <ModelSelector
            models={models.data ?? []}
            loadedId={loadedModel?.id ?? null}
            onChanged={() => {
              models.refresh();
              metrics.refresh();
            }}
          />
        }
      />

      <div className="page">
        <div className="tiles">
          <StatTile
            icon={<Cpu size={16} />}
            tint="var(--series-1)"
            label="CPU Usage"
            value={utilization === null ? "—" : percent(utilization)}
            sub={
              system.data
                ? `${system.data.cpu.physical_cores} cores`
                : "measuring…"
            }
            series={cpuSeries}
          />
          <StatTile
            icon={<MemoryStick size={16} />}
            tint="var(--series-2)"
            label="RAM Usage"
            value={memory ? bytes(memory.used) : "—"}
            sub={
              engine
                ? `engine ${bytes(engine.rss)} of ${bytes(memory?.total ?? 0)}`
                : memory
                  ? `of ${bytes(memory.total)}`
                  : "not measured"
            }
            series={ramSeries}
          />
          <StatTile
            icon={<Zap size={16} />}
            tint="var(--series-3)"
            label="Tokens / Sec"
            value={rate(decodeRate)}
            sub="Generation"
            series={rateSeries}
          />
          <StatTile
            icon={<Layers size={16} />}
            tint="var(--series-1)"
            label="Context Length"
            value={
              metrics.data?.model
                ? metrics.data.model.n_ctx.toLocaleString()
                : "—"
            }
            sub={
              loadedModel?.context_length
                ? `of ${contextLength(loadedModel.context_length)}`
                : "no model loaded"
            }
          />
          <StatTile
            icon={<Clock size={16} />}
            tint="var(--series-4)"
            label="Uptime"
            value={metrics.data ? duration(metrics.data.uptime_seconds) : "—"}
            sub="Session"
          />
        </div>

        <div
          className="grid"
          style={{ gridTemplateColumns: "repeat(auto-fit, minmax(300px, 1fr))" }}
        >
          <Card
            title="Current Model"
            action={
              loadedId ? (
                <Pill tone="ok">Loaded</Pill>
              ) : (
                <Pill tone="neutral">None</Pill>
              )
            }
          >
            {loadedId ? (
              <>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 10,
                    marginBottom: 12,
                  }}
                >
                  <span style={{ fontSize: 15, fontWeight: 600 }}>
                    {loadedModel?.name ?? loadedId}
                  </span>
                  <Pill tone="accent">GGUF</Pill>
                </div>
                <Row label="Parameters">
                  {loadedModel?.param_count
                    ? `${(loadedModel.param_count / 1e9).toFixed(1)}B`
                    : "—"}
                </Row>
                <Row label="Quantization">{loadedModel?.quantization ?? "—"}</Row>
                <Row label="Model Size">{bytes(loadedModel?.bytes)}</Row>
                <Row label="Context Length">
                  {metrics.data?.model?.n_ctx.toLocaleString() ?? "—"}
                </Row>
                <Row label="Architecture">
                  {loadedModel?.architecture.toUpperCase() ?? "—"}
                </Row>
                <Link
                  to="/models"
                  className="btn"
                  style={{
                    marginTop: 16,
                    width: "100%",
                    justifyContent: "space-between",
                  }}
                >
                  View model details
                  <ChevronRight size={16} />
                </Link>
              </>
            ) : (
              <div className="empty">
                <strong>No model loaded</strong>
                <span>
                  The gateway is running and answering, but nothing is resident
                  yet.
                </span>
                <Link to="/models" className="btn btn--primary" style={{ marginTop: 8 }}>
                  Choose a model
                </Link>
              </div>
            )}
          </Card>

          <Card title="System Resources">
            <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
              {memory ? (
                <Meter
                  label="RAM"
                  value={bytes(memory.used)}
                  of={bytes(memory.total)}
                  fraction={memory.pressure}
                  color="var(--series-2)"
                  foot={
                    memory.swap_used > 0
                      ? `${bytes(memory.swap_used)} swap in use`
                      : "no swap in use"
                  }
                />
              ) : (
                <Unavailable what="Memory" />
              )}

              {diskModels ? (
                <Meter
                  label="Disk"
                  value={bytes(diskModels.used)}
                  of={bytes(diskModels.total)}
                  fraction={diskModels.pressure}
                  color="var(--series-1)"
                  foot={`${bytes(diskModels.available)} available to write`}
                />
              ) : (
                <Unavailable what="Disk" />
              )}

              {utilization !== null ? (
                <Meter
                  label="CPU"
                  value={percent(utilization)}
                  fraction={utilization}
                  color="var(--series-1)"
                  foot={
                    system.data
                      ? `${system.data.cpu.physical_cores} cores / ${system.data.cpu.logical_cores} threads`
                      : undefined
                  }
                />
              ) : (
                <Meter
                  label="CPU"
                  value="measuring…"
                  fraction={null}
                  color="var(--series-1)"
                  foot="a rate needs two readings"
                />
              )}
            </div>
          </Card>

          <Card
            title="Running Now"
            action={
              <span className="card__note tnum">
                {roster.data
                  ? `${roster.data.running.length} of ${roster.data.capacity} slots`
                  : "—"}
              </span>
            }
          >
            {roster.data && roster.data.running.length > 0 ? (
              <ul style={{ margin: 0, padding: 0, listStyle: "none" }}>
                {roster.data.running.map((request, index) => (
                  <li
                    key={request.id ?? `slot-${index}`}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 10,
                      padding: "8px 10px",
                      borderRadius: "var(--radius)",
                      background:
                        index % 2 === 0 ? "var(--surface-sunken)" : "transparent",
                      fontSize: 12.5,
                    }}
                  >
                    <Pill tone={bandTone(request.band)}>{request.band}</Pill>
                    <span className="tnum" style={{ color: "var(--text-muted)" }}>
                      {duration(request.running_ms)}
                    </span>
                    <span
                      style={{
                        flex: 1,
                        minWidth: 0,
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {request.prompt_tokens} prompt tokens
                      {request.model ? ` · ${request.model}` : ""}
                    </span>
                  </li>
                ))}
              </ul>
            ) : (
              <div className="empty">
                <strong>Nothing running</strong>
                <span>
                  Requests appear here while they are being served, and in the
                  feed below once they finish.
                </span>
              </div>
            )}

            {next && (
              <div className="card__note" style={{ marginTop: 10 }}>
                {waiting.length === 1
                  ? "1 request is waiting for a slot"
                  : `${waiting.length} requests are waiting for a slot`}
                , the next after {duration(next.waited_ms)} so far.
              </div>
            )}
          </Card>

          <Card
            title="Inference Live Feed"
            action={
              <span className="card__note tnum">
                {decodeRate === null ? "idle" : `${rate(decodeRate)} tok/s`}
              </span>
            }
          >
            {events.length === 0 ? (
              <div className="empty">
                <strong>Nothing yet</strong>
                <span>
                  Finished generations appear here as they happen, newest first.
                </span>
              </div>
            ) : (
              <ul style={{ margin: 0, padding: 0, listStyle: "none" }}>
                {events.map((event, index) => (
                  <li
                    key={`${event.at_unix_ms}-${event.id ?? index}`}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 10,
                      padding: "8px 10px",
                      borderRadius: "var(--radius)",
                      background:
                        index % 2 === 0 ? "var(--surface-sunken)" : "transparent",
                      fontSize: 12.5,
                    }}
                  >
                    <Pill tone={toneFor(event.finish_reason)}>
                      {event.finish_reason ?? "cancelled"}
                    </Pill>
                    {event.band && (
                      <Pill tone={bandTone(event.band)}>{event.band}</Pill>
                    )}
                    <span className="tnum" style={{ color: "var(--text-muted)" }}>
                      {new Date(event.at_unix_ms).toLocaleTimeString([], {
                        hour: "2-digit",
                        minute: "2-digit",
                        second: "2-digit",
                      })}
                    </span>
                    <span
                      style={{
                        flex: 1,
                        minWidth: 0,
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {event.completion_tokens} tokens in {event.total_ms} ms
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        </div>

        <Card>
          <div
            style={{
              display: "flex",
              flexWrap: "wrap",
              alignItems: "center",
              gap: 24,
              fontSize: 13,
            }}
          >
            <span style={{ fontWeight: 600 }}>API Gateway</span>
            <Pill tone={gateway.data ? "ok" : "neutral"} dot>
              {gateway.data ? "Running" : "unknown"}
            </Pill>
            <Stat label="Port" value={gateway.data?.listeners[0]?.port ?? "—"} />
            <Stat label="In flight" value={metrics.data?.in_flight ?? "—"} />
            <Stat
              label="Requests"
              value={
                metrics.data
                  ? metrics.data.requests.reduce((sum, row) => sum + row.count, 0)
                  : "—"
              }
            />
            <Stat label="Auth" value={gateway.data?.auth.required ? "key required" : "loopback"} />
          </div>
        </Card>
      </div>
    </>
  );
}

function Stat({ label, value }: { label: string; value: string | number }) {
  return (
    <span style={{ display: "flex", alignItems: "center", gap: 8 }}>
      <span style={{ color: "var(--text-muted)" }}>{label}</span>
      <span className="tnum" style={{ fontWeight: 600 }}>
        {value}
      </span>
    </span>
  );
}

function Unavailable({ what }: { what: string }) {
  return (
    <div style={{ fontSize: 12.5, color: "var(--text-muted)" }}>
      {what} is not measured on this platform yet.
    </div>
  );
}

/// Which band a request was served in, as a tone.
///
/// Never the only signal: the band's name is always beside it, because a
/// reader who cannot separate the two tones must still be able to read which
/// queue served the request.
function bandTone(band: string): "info" | "neutral" {
  return band === "interactive" ? "info" : "neutral";
}

function toneFor(
  reason: string | null,
): "ok" | "warn" | "danger" | "neutral" {
  switch (reason) {
    case "stop":
      return "ok";
    case "length":
      return "warn";
    case "error":
      return "danger";
    default:
      return "neutral";
  }
}
