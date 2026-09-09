import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { Wrench } from "lucide-react";

import { agentApi, type ToolInfo } from "../api/agent";
import { Card } from "../components/Card";
import { Empty, Pill } from "../components/Bits";
import { TopBar } from "../components/Shell";

type Tone = "ok" | "warn" | "danger" | "info" | "accent" | "neutral";

function riskTone(risk: string): Tone {
  switch (risk) {
    case "observe":
      return "ok";
    case "external":
      return "info";
    case "sensitive":
    case "mutating":
      return "warn";
    case "executable":
    case "privileged":
      return "danger";
    default:
      return "neutral";
  }
}

export function AgentTools() {
  const [tools, setTools] = useState<ToolInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let live = true;
    setError(null);
    setTools(null);
    agentApi
      .tools()
      .then((response) => {
        if (live) setTools(response.tools);
      })
      .catch((cause) => {
        if (live) setError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      live = false;
    };
  }, [attempt]);

  return (
    <>
      <TopBar title="Tools" subtitle="What the agent can call, and how each is gated" />
      <div className="page">
        <Card title="Enabled tools">
          {error ? (
            <div role="alert">
              <p className="muted">Could not reach the agent API: {error}</p>
              <button type="button" className="btn" onClick={() => setAttempt((value) => value + 1)}>
                Retry
              </button>
              <Link className="btn" to="/settings" style={{ marginLeft: 8 }}>
                Server settings
              </Link>
            </div>
          ) : !tools ? (
            <span className="muted">Loading…</span>
          ) : tools.length === 0 ? (
            <Empty title="No tools" />
          ) : (
            <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
              {tools.map((tool) => (
                <div
                  key={tool.name}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 10,
                    padding: "10px 12px",
                    border: "1px solid var(--border, #2a2a2a)",
                    borderRadius: 8,
                  }}
                >
                  <Wrench size={15} />
                  <strong style={{ minWidth: 140 }}>{tool.name}</strong>
                  <Pill tone={riskTone(tool.risk)}>{tool.risk}</Pill>
                  <span className="muted" style={{ fontSize: 13 }}>
                    {tool.description}
                  </span>
                </div>
              ))}
            </div>
          )}
        </Card>
      </div>
    </>
  );
}
