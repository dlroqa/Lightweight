import { useState } from "react";
import { RefreshCw, Search } from "lucide-react";

import { api } from "../api/client";
import { clockWithSeconds } from "../api/format";
import type { LogRecord } from "../api/types";
import { Card } from "../components/Card";
import { Empty, ErrorState, Loading, Pill } from "../components/Bits";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";

const LEVELS = ["", "trace", "debug", "info", "warn", "error"];

export function Logs() {
  const [level, setLevel] = useState("");
  const [target, setTarget] = useState("");
  const [search, setSearch] = useState("");
  const [limit, setLimit] = useState(200);

  const logs = usePoll(
    () => api.logs({ level: level || undefined, target: target || undefined, search: search || undefined, limit }),
    5000,
  );

  const targets = Array.from(
    new Set((logs.data?.data ?? []).map((record) => record.target)),
  ).sort();

  return (
    <>
      <TopBar
        title="Logs"
        subtitle="What the gateway has recorded about itself"
        actions={
          <button
            type="button"
            className="btn btn--icon"
            onClick={logs.refresh}
            aria-label="Refresh the log"
          >
            <RefreshCw size={15} />
          </button>
        }
      />

      <div className="page">
        <Card>
          <div
            style={{
              display: "flex",
              flexWrap: "wrap",
              gap: 12,
              alignItems: "flex-end",
            }}
          >
            <div className="field" style={{ width: 130 }}>
              <label className="field__label" htmlFor="log-level">
                Level
              </label>
              <select
                id="log-level"
                className="select"
                value={level}
                onChange={(event) => setLevel(event.target.value)}
              >
                {LEVELS.map((option) => (
                  <option key={option || "all"} value={option}>
                    {option === "" ? "All" : option}
                  </option>
                ))}
              </select>
            </div>

            <div className="field" style={{ width: 190 }}>
              <label className="field__label" htmlFor="log-target">
                Source
              </label>
              <select
                id="log-target"
                className="select"
                value={target}
                onChange={(event) => setTarget(event.target.value)}
              >
                <option value="">All</option>
                {targets.map((option) => (
                  <option key={option} value={option}>
                    {option}
                  </option>
                ))}
              </select>
            </div>

            <div className="field" style={{ flex: 1, minWidth: 200 }}>
              <label className="field__label" htmlFor="log-search">
                Search
              </label>
              <div style={{ position: "relative" }}>
                <Search
                  size={15}
                  style={{
                    position: "absolute",
                    left: 11,
                    top: "50%",
                    transform: "translateY(-50%)",
                    color: "var(--text-faint)",
                  }}
                />
                <input
                  id="log-search"
                  className="input"
                  style={{ paddingLeft: 34 }}
                  placeholder="Search messages…"
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                />
              </div>
            </div>

            <div className="field" style={{ width: 120 }}>
              <label className="field__label" htmlFor="log-limit">
                Records
              </label>
              <select
                id="log-limit"
                className="select"
                value={limit}
                onChange={(event) => setLimit(Number(event.target.value))}
              >
                {[50, 200, 500, 2000].map((option) => (
                  <option key={option} value={option}>
                    {option}
                  </option>
                ))}
              </select>
            </div>
          </div>
        </Card>

        <Card flush>
          {logs.loading && !logs.data ? (
            <Loading what="the log" />
          ) : logs.error ? (
            <ErrorState error={logs.error} onRetry={logs.refresh} />
          ) : (logs.data?.data.length ?? 0) === 0 ? (
            <Empty
              title="Nothing recorded yet"
              hint="The gateway writes a line per request and one for every lifecycle event. Prompts and keys are never among them."
            />
          ) : (
            <>
              <div className="scroll-x">
                <table className="table">
                  <thead>
                    <tr>
                      <th style={{ width: 110 }}>Time</th>
                      <th style={{ width: 80 }}>Level</th>
                      <th style={{ width: 160 }}>Source</th>
                      <th>Message</th>
                    </tr>
                  </thead>
                  <tbody>
                    {logs.data?.data.map((record, index) => (
                      <LogRow key={`${record.timestamp}-${index}`} record={record} />
                    ))}
                  </tbody>
                </table>
              </div>
              {logs.data?.truncated && (
                <div
                  className="card__note"
                  style={{ padding: "10px 16px", borderTop: "1px solid var(--rule)" }}
                >
                  Older matching records were left out to honour the limit. The
                  full record is in {logs.data.files.join(", ")}.
                </div>
              )}
            </>
          )}
        </Card>
      </div>
    </>
  );
}

function LogRow({ record }: { record: LogRecord }) {
  const extras = Object.entries(record.fields ?? {});
  return (
    <tr>
      <td className="tnum" style={{ color: "var(--text-muted)" }}>
        {clockWithSeconds(record.timestamp)}
      </td>
      <td>
        <Pill tone={toneFor(record.level)}>{record.level}</Pill>
      </td>
      <td style={{ color: "var(--text-muted)", fontSize: 12 }}>
        {record.target.replace(/^hermes::/, "")}
      </td>
      <td>
        <div>{record.message}</div>
        {extras.length > 0 && (
          <div
            style={{
              display: "flex",
              flexWrap: "wrap",
              gap: 10,
              marginTop: 4,
              fontSize: 11.5,
              color: "var(--text-muted)",
            }}
            className="tnum"
          >
            {extras.map(([key, value]) => (
              <span key={key}>
                {key}={String(value)}
              </span>
            ))}
          </div>
        )}
      </td>
    </tr>
  );
}

function toneFor(level: string): "ok" | "warn" | "danger" | "neutral" | "accent" {
  switch (level.toUpperCase()) {
    case "ERROR":
      return "danger";
    case "WARN":
      return "warn";
    case "INFO":
      return "ok";
    case "DEBUG":
      return "accent";
    default:
      return "neutral";
  }
}
