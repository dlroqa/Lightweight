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
  message_count: number;
  run_count: number;
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
      "The agent API returned a non-JSON response. Start `lightagent serve` and connect the gateway with `--agent-upstream http://127.0.0.1:8735` (use your agent's address if different). If the gateway does not support this option, update it first.",
    );
  }
  const body = await response.json();
  if (!response.ok) {
    const message = typeof body?.error === "string" ? body.error : response.statusText;
    throw new Error(`${response.status}: ${message}`);
  }
  return body as T;
}

export const agentApi = {
  tools: () => jsonRequest<{ tools: ToolInfo[] }>("/tools"),
  createRun: (message: string, profile?: string, sessionId?: string) =>
    jsonRequest<{ id: string; status: string; session_id: string | null }>("/runs", {
      method: "POST",
      body: JSON.stringify({ message, profile, session_id: sessionId }),
    }),
  createSession: () => jsonRequest<{ id: string }>("/sessions", { method: "POST", body: "{}" }),
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
