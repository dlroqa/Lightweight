import { Activity, Cpu, Gauge, MemoryStick } from "lucide-react";

import { api } from "../api/client";
import { bytes, percent, rate } from "../api/format";
import { wasRead } from "../api/types";
import { Card } from "../components/Card";
import { Row } from "../components/Bits";
import { StatTile } from "../components/StatTile";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";
import { useSeries, useUtilization } from "../hooks/useSeries";

export function Performance() {
  const metrics = usePoll(api.metrics, 1000);
  const system = usePoll(api.system, 1000);

  const times = wasRead(system.data?.cpu_times) ? system.data.cpu_times : null;
  const utilization = useUtilization(times);
  const memory = wasRead(system.data?.memory) ? system.data.memory : null;

  const decodeRate =
    metrics.data && metrics.data.decode.total_ms > 0
      ? (metrics.data.tokens.decoded / metrics.data.decode.total_ms) * 1000
      : null;
  const prefillRate =
    metrics.data && metrics.data.prefill.total_ms > 0
      ? (metrics.data.tokens.prefilled / metrics.data.prefill.total_ms) * 1000
      : null;

  const decodeSeries = useSeries(decodeRate);
  const prefillSeries = useSeries(prefillRate);
  const cpuSeries = useSeries(utilization === null ? null : utilization * 100);
  const ramSeries = useSeries(memory ? memory.used / 1024 ** 3 : null);

  const cacheHitRate =
    metrics.data && metrics.data.tokens.prompt > 0
      ? metrics.data.tokens.cached / metrics.data.tokens.prompt
      : null;

  const ttft = metrics.data?.time_to_first_token;
  const queueWait = metrics.data?.queue_wait;

  return (
    <>
      <TopBar
        title="Performance"
        subtitle="Monitor system and inference performance"
      />

      <div className="page">
        <div className="tiles">
          <StatTile
            icon={<Activity size={16} />}
            tint="var(--series-1)"
            label="Tokens / Second"
            value={rate(decodeRate)}
            unit="tok/s"
            sub="Generation speed"
            series={decodeSeries}
          />
          <StatTile
            icon={<Gauge size={16} />}
            tint="var(--series-2)"
            label="Prompt Processing"
            value={rate(prefillRate)}
            unit="tok/s"
            sub="Prefill speed"
            series={prefillSeries}
          />
          <StatTile
            icon={<Cpu size={16} />}
            tint="var(--series-1)"
            label="CPU Usage"
            value={utilization === null ? "—" : percent(utilization)}
            sub={system.data ? `${system.data.cpu.physical_cores} cores` : ""}
            series={cpuSeries}
          />
          <StatTile
            icon={<MemoryStick size={16} />}
            tint="var(--series-3)"
            label="RAM Usage"
            value={memory ? bytes(memory.used) : "—"}
            sub={memory ? percent(memory.pressure) : "not measured"}
            series={ramSeries}
          />
        </div>

        <div
          className="grid"
          style={{ gridTemplateColumns: "repeat(auto-fit, minmax(300px, 1fr))" }}
        >
          <Card title="Detailed metrics">
            <Row label="Generations">
              {metrics.data?.generations.toLocaleString() ?? "—"}
            </Row>
            <Row label="Prompt tokens">
              {metrics.data?.tokens.prompt.toLocaleString() ?? "—"}
            </Row>
            <Row label="Generated tokens">
              {metrics.data?.tokens.completion.toLocaleString() ?? "—"}
            </Row>
            <Row label="Cached tokens">
              {metrics.data?.tokens.cached.toLocaleString() ?? "—"}
            </Row>
            <Row label="Cache hit rate">
              {cacheHitRate === null ? "—" : percent(cacheHitRate, 1)}
            </Row>
            <Row label="Mean time to first token">
              {ttft && ttft.count > 0
                ? `${Math.round(ttft.total_ms / ttft.count)} ms`
                : "no samples yet"}
            </Row>
            <Row label="Longest queue wait">
              {queueWait ? `${queueWait.max_ms} ms` : "—"}
            </Row>
          </Card>

          <Card title="Hardware">
            <Row label="CPU">{system.data?.cpu.model ?? "—"}</Row>
            <Row label="Cores / threads">
              {system.data
                ? `${system.data.cpu.physical_cores} / ${system.data.cpu.logical_cores}`
                : "—"}
            </Row>
            <Row label="Instruction sets">
              {system.data?.cpu.features.join(", ") || "none detected"}
            </Row>
            <Row label="Engine variant">
              {system.data?.cpu.expected_ggml_variant ?? "—"}
            </Row>
            <Row label="RAM">{memory ? bytes(memory.total) : "—"}</Row>
            <Row label="System">
              {system.data
                ? `${system.data.os.name} · ${system.data.os.architecture}`
                : "—"}
            </Row>
            {system.data && !system.data.cpu.has_avx_family && (
              <div className="notice notice--warn" style={{ marginTop: 12 }}>
                This processor has no AVX-family instructions, which is the single
                biggest predictor of low throughput. The engine has selected its{" "}
                <strong>{system.data.cpu.expected_ggml_variant}</strong> build
                accordingly.
              </div>
            )}
          </Card>

          <Card title="Queue">
            <Row label="Capacity">{metrics.data?.queue.capacity ?? "—"}</Row>
            <Row label="Running">{metrics.data?.queue.running ?? "—"}</Row>
            <Row label="Waiting">{metrics.data?.queue.waiting ?? "—"}</Row>
            <Row label="Admitted without waiting">
              {metrics.data?.queue.admitted_immediately.toLocaleString() ?? "—"}
            </Row>
            <Row label="Had to queue">
              {metrics.data?.queue.queued.toLocaleString() ?? "—"}
            </Row>
            <Row label="Overtakes">
              {metrics.data?.queue.overtakes.toLocaleString() ?? "—"}
            </Row>
            <Row label="Gave up waiting">
              {metrics.data?.queue.timed_out.toLocaleString() ?? "—"}
            </Row>
            <div className="card__note" style={{ marginTop: 10 }}>
              Overtakes are how often a short request was let past a waiting long
              one. On an uncontended gateway this stays at zero.
            </div>
          </Card>
        </div>

        <div className="notice notice--info">
          These lines are sampled by this page once a second and cover only the
          time since you opened it. The gateway keeps running totals, not a
          history.
        </div>
      </div>
    </>
  );
}
