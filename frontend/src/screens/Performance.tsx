import { useState } from "react";

import { Activity, Cpu, Gauge, MemoryStick, Timer } from "lucide-react";

import { api, ApiError, followJob } from "../api/client";
import { bytes, clock, percent, quantile, rate } from "../api/format";
import type { BenchmarkRun, BenchmarkSample } from "../api/types";
import { wasRead } from "../api/types";
import { Card } from "../components/Card";
import { ErrorState, Row } from "../components/Bits";
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

  const runs = usePoll(api.benchmarks, 10_000);
  const [running, setRunning] = useState(false);
  const [failure, setFailure] = useState<ApiError | null>(null);

  // The panel's benchmark measures the model that is already loaded, at the
  // parameters it is already loaded with. Varying those means reloading the
  // engine, which is `hermes bench` — and doing it here would interrupt
  // whoever this gateway is serving.
  async function runBenchmark() {
    setRunning(true);
    setFailure(null);
    try {
      const job = await api.runBenchmark({});
      await followJob(job.job);
      await runs.refresh();
    } catch (error) {
      setFailure(error instanceof ApiError ? error : null);
    } finally {
      setRunning(false);
    }
  }

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
            <Row label="Waiting (short first)">
              {metrics.data
                ? `${metrics.data.queue.waiting_interactive} interactive · ${metrics.data.queue.waiting_bulk} bulk`
                : "—"}
            </Row>
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
            <Row label="Client disconnected while queued">
              {metrics.data?.queue.abandoned.toLocaleString() ?? "—"}
            </Row>
            <div className="card__note" style={{ marginTop: 10 }}>
              Overtakes are how often a short request was let past a waiting long
              one. On an uncontended gateway this stays at zero. Giving up and
              disconnecting are counted apart: the first is a wait that ran out,
              the second is a client that walked away.
            </div>
          </Card>
        </div>

        <Card title="Benchmark">
          <div
            style={{
              display: "flex",
              gap: 12,
              alignItems: "center",
              flexWrap: "wrap",
            }}
          >
            <button
              type="button"
              className="btn btn--primary"
              onClick={() => void runBenchmark()}
              disabled={running || !metrics.data?.model}
            >
              <Timer size={15} />
              {running ? "Measuring…" : "Run benchmark"}
            </button>
            <span className="card__note">
              {metrics.data?.model
                ? `Measures ${metrics.data.model.id} as it is loaded now.`
                : "Load a model first: there is nothing resident to measure."}
            </span>
          </div>

          {failure && (
            <div style={{ marginTop: 12 }}>
              <ErrorState error={failure} />
            </div>
          )}

          {runs.data && runs.data.length > 0 && (
            <div className="scroll-x" style={{ marginTop: 14 }}>
              <table className="table">
                <thead>
                  <tr>
                    <th>When</th>
                    <th>Model</th>
                    <th>Prefill</th>
                    <th>Decode</th>
                    <th>Cached reuse</th>
                  </tr>
                </thead>
                <tbody>
                  {runs.data.slice(0, 8).map((run) => (
                    <tr key={run.id}>
                      <td>{clock(run.at_unix)}</td>
                      <td>{run.model.id}</td>
                      <td className="tnum">
                        {rate(best(run, "cold_prefill"))} tok/s
                      </td>
                      <td className="tnum">
                        {rate(best(run, "decode"))} tok/s
                      </td>
                      <td className="tnum">{cachedSpeedup(run)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}

          <div className="card__note" style={{ marginTop: 10 }}>
            These figures describe this machine and this engine build. They are
            not a property of the software and do not transfer to other
            hardware. To vary context, batch size or thread count, use{" "}
            <code>hermes bench</code>, which brings its own engine rather than
            interrupting this one.
          </div>
        </Card>

        <div className="notice notice--info">
          These lines are sampled by this page once a second and cover only the
          time since you opened it. The gateway keeps running totals, not a
          history.
        </div>
      </div>
    </>
  );
}

/**
 * The best rate a run measured for one scenario.
 *
 * The best rather than the mean: repetitions on a contended machine include
 * whatever else it was doing, and the fastest is the closest thing to what the
 * hardware can do. It is still one machine's number.
 */
function best(run: BenchmarkRun, scenario: BenchmarkSample["scenario"]): number | null {
  const rates = run.samples
    .filter((sample) => sample.scenario === scenario)
    .map((sample) =>
      scenario === "decode"
        ? sample.decode_ms && sample.generated_tokens > 1
          ? (sample.generated_tokens / sample.decode_ms) * 1000
          : null
        : sample.prefill_ms && sample.prefilled_tokens > 0
          ? (sample.prefilled_tokens / sample.prefill_ms) * 1000
          : null,
    )
    .filter((value): value is number => value !== null);
  return rates.length > 0 ? Math.max(...rates) : null;
}

/**
 * What the prefix cache saved, as the ratio of the slowest cold first token to
 * the fastest cached one.
 *
 * Time to first token rather than throughput, because that is what a cache hit
 * actually changes and what a person waiting actually feels.
 */
function cachedSpeedup(run: BenchmarkRun): string {
  const cold = run.samples
    .filter((sample) => sample.scenario === "cold_prefill")
    .map((sample) => sample.time_to_first_token_ms)
    .filter((value): value is number => value !== null);
  const cached = run.samples
    .filter((sample) => sample.scenario === "cached_prefill" && sample.cached_tokens > 0)
    .map((sample) => sample.time_to_first_token_ms)
    .filter((value): value is number => value !== null);
  if (cold.length === 0 || cached.length === 0) return "—";
  const slowest = Math.max(...cold);
  const fastest = Math.min(...cached);
  if (fastest <= 0) return "—";
  return `${Math.round(slowest / fastest)}x faster`;
}
