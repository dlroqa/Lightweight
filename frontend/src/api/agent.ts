/**
 * Talking to the Lightagent HTTP API (`/api/lightagent/v1`).
 *
 * Same-origin, like the rest of the panel: in development Vite proxies
 * `/api/lightagent` to the agent server, and in production the gateway proxies
 * it via `--agent-upstream` (or `lightagent serve --web-root` serves the panel
 * itself). Distinct from the inference gateway's
 * `/api/v1`, which the other screens use.
 */

const BASE = "/api/lightagent/v1";

export interface ToolInfo {
  name: string;
  risk: string;
  description: string;
}

export interface PendingApproval {
  approval_id: string;
  tool: string;
  risk: string;
}

export interface RunView {
  id: string;
  status: string;
  events: number;
  pending_approval: PendingApproval | null;
}

export interface SessionSummary {
  id: string;
  profile: string;
  title: string;
  updated_at: SystemTime;
  message_count: number;
  run_count: number;
}

export interface SystemTime {
  secs_since_epoch: number;
  nanos_since_epoch: number;
}

export interface SessionMessage {
  role: string;
  content: string;
}

export interface ToolHistoryEntry {
  id: string;
  tool: string;
  arguments_preview: string;
  result_excerpt: string;
  source?: string;
  truncated: boolean;
  outcome: string;
  duration_ms?: number;
}

export interface SessionRun {
  run_id: string;
  started_at: SystemTime;
  ended_at?: SystemTime;
  stop_reason?: string;
  tools: ToolHistoryEntry[];
}

export interface AgentSession {
  id: string;
  profile: string;
  cwd?: string;
  title: string;
  created_at: SystemTime;
  updated_at: SystemTime;
  messages: SessionMessage[];
  approvals_unrestricted: boolean;
  runs: SessionRun[];
}

export interface LightagentSettings {
  max_turns: number;
  max_tool_calls: number;
  wall_clock_secs: number | null;
  approval_policy: "permissive" | "balanced" | "strict";
  web_enabled: boolean;
  filesystem_tools_enabled: boolean;
  terminal_enabled: boolean;
  memory_enabled: boolean;
  show_reasoning_in_tui: boolean;
}

export interface ApprovalRow {
  run: string;
  pending: PendingApproval | null;
}

async function jsonRequest<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${BASE}${path}`, {
    headers: { "Content-Type": "application/json" },
    ...init,
  });
  const contentType = response.headers.get("content-type") ?? "";
  if (!contentType.toLowerCase().includes("application/json")) {
    throw new Error(
      "This gateway does not expose the agent API. Restart it with the current Lightweight build, then try again.",
    );
  }
  const body = await response.json();
  if (!response.ok) {
    const message =
      typeof body?.error === "string"
        ? body.error
        : typeof body?.error?.message === "string"
          ? body.error.message
          : response.statusText;
    throw new Error(`${response.status}: ${message}`);
  }
  return body as T;
}

export const agentApi = {
  tools: () => jsonRequest<{ tools: ToolInfo[] }>("/tools"),
  settings: () => jsonRequest<LightagentSettings>("/settings"),
  saveSettings: (settings: LightagentSettings) =>
    jsonRequest<LightagentSettings>("/settings", {
      method: "PUT",
      body: JSON.stringify(settings),
    }),
  createRun: (message: string, profile?: string, sessionId?: string) =>
    jsonRequest<{ id: string; status: string; session_id: string | null }>("/runs", {
      method: "POST",
      body: JSON.stringify({ message, profile, session_id: sessionId }),
    }),
  createSession: () => jsonRequest<{ id: string }>("/sessions", { method: "POST", body: "{}" }),
  session: (id: string) =>
    jsonRequest<AgentSession>(`/sessions/${encodeURIComponent(id)}`),
  deleteSession: (id: string) =>
    jsonRequest<{ deleted: boolean }>(`/sessions/${encodeURIComponent(id)}`, {
      method: "DELETE",
    }),
  run: (id: string) => jsonRequest<RunView>(`/runs/${id}`),
  cancelRun: (id: string) =>
    jsonRequest<{ id: string; cancelled: boolean }>(`/runs/${id}/cancel`, {
      method: "POST",
      body: "{}",
    }),
  sessions: () => jsonRequest<{ sessions: SessionSummary[] }>("/sessions"),
  approvals: () => jsonRequest<{ approvals: ApprovalRow[] }>("/approvals"),
  respondApproval: (run: string, approve: boolean) =>
    jsonRequest<{ run: string; delivered: boolean }>(`/approvals/${run}`, {
      method: "POST",
      body: JSON.stringify({ approve }),
    }),
  eventsUrl: (id: string) => `${BASE}/runs/${id}/events`,
};
