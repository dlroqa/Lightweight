/**
 * Talking to the Lightagent HTTP API (`/api/lightagent/v1`).
 *
 * Same-origin, like the rest of the panel: in development Vite proxies
 * `/api/lightagent` to the agent server, and in production `lightagent serve
 * --web-root` serves this bundle itself. Distinct from the inference gateway's
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
  if (!response.ok) {
    const body = await response.text();
    throw new Error(`${response.status}: ${body || response.statusText}`);
  }
  return (await response.json()) as T;
}

export const agentApi = {
  tools: () => jsonRequest<{ tools: ToolInfo[] }>("/tools"),
  createRun: (message: string, profile?: string) =>
    jsonRequest<{ id: string; status: string }>("/runs", {
      method: "POST",
      body: JSON.stringify({ message, profile }),
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
