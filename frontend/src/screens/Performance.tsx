import { Activity, Cpu, Gauge, MemoryStick } from "lucide-react";

import { api } from "../api/client";
import { bytes, percent, quantile, rate } from "../api/format";
import { wasRead } from "../api/types";
import { Card } from "../components/Card";
import { Row } from "../components/Bits";
import { StatTile } from "../components/StatTile";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";
import { useEngineCores, useSeries, useUtilization } from "../hooks/useSeries";

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

  // What the engine is doing with the processor, as opposed to how much of it
  // is held. Both counters are cumulative, so this is a difference across two
  // polls, and it is `null` rather than zero until there are two.
  const engineCpu = wasRead(metrics.data?.engine_cpu)
    ? metrics.data.engine_cpu
    : null;
  const engineCores = useEngineCores(
    engineCpu,
    times,
    system.data?.cpu.logical_cores ?? null,
  );
  const counters = wasRead(metrics.data?.engine_counters)
    ? metrics.data.engine_counters
    : null;

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
            sub={
              engineCores === null
                ? system.data
                  ? `${system.data.cpu.physical_cores} cores`
                  : ""
                : `engine using ${engineCores.toFixed(1)} of ${
                    system.data?.cpu.logical_cores
                  } cores`
            }
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
            <Row label="Median time to first token">{quantile(ttft, 0.5)}</Row>
            <Row label="95th percentile first token">
              {quantile(ttft, 0.95)}
            </Row>
            <Row label="Longest queue wait">
              {queueWait ? `${queueWait.max_ms} ms` : "—"}
            </Row>
            <div className="card__note" style={{ marginTop: 10 }}>
              Percentiles are bucket bounds, not interpolated values: at least
              that share of requests finished within the figure shown.
            </div>
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

          <Card title="Engine">
            <Row label="Processor time">
              {engineCpu
                ? `${engineCpu.user_ticks + engineCpu.system_ticks} ticks`
                : "not reported"}
            </Row>
            <Row label="Cores in use">
              {engineCores === null ? "—" : engineCores.toFixed(2)}
            </Row>
            <Row label="Longest sequence served">
              {counters?.max_sequence_tokens === undefined
                ? "not reported"
                : `${counters.max_sequence_tokens.toLocaleString()} tokens`}
            </Row>
            <Row label="Decode steps">
              {counters?.decode_calls === undefined
                ? "not reported"
                : counters.decode_calls.toLocaleString()}
            </Row>
            <Row label="Slots busy per decode">
              {counters?.busy_slots_per_decode === undefined
                ? "not reported"
                : counters.busy_slots_per_decode.toFixed(2)}
            </Row>
            <Row label="Deferred by the engine">
              {counters?.requests_deferred === undefined
                ? "not reported"
                : counters.requests_deferred.toLocaleString()}
            </Row>
            <div className="card__note" style={{ marginTop: 10 }}>
              The longest sequence served against the loaded context is the
              cheapest signal that a model is holding a window nobody is using.
            </div>
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
