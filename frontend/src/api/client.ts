/**
 * Talking to the gateway.
 *
 * Same-origin, always: in development Vite proxies these paths through, and in
 * production the gateway serves this bundle itself. There is no base URL to
 * configure and no CORS to negotiate.
 */

import type {
  ApiErrorBody,
  Conversation,
  ConversationSummary,
  GatewayReport,
  Job,
  LogsBody,
  Metrics,
  ModelDetail,
  PinnedModel,
  CatalogRow,
  Remedy,
  Settings,
  SystemReport,
} from "./types";

/**
 * A failure the panel can show a person.
 *
 * Carries the gateway's own `code` so a screen can react to a specific
 * condition — `no_data_directory` is a different thing to say than a network
 * failure — and its remedies, which the error taxonomy has attached since M0
 * precisely so a UI can offer a next step rather than an apology.
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly remedies: Remedy[];

  constructor(status: number, code: string, message: string, remedies: Remedy[]) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.remedies = remedies;
  }
}

/**
 * Wait for a long operation to finish, and throw what it failed with.
 *
 * `POST .../load` answers 202 and a refusal lands in the *job*, not in the
 * response. A caller that stopped at the status code counted every refusal as a
 * success — which is how `insufficient_memory`, the one condition the whole RAM
 * estimator exists to report, stayed invisible on screen until M7.3.
 */
export async function followJob(id: number): Promise<void> {
  for (;;) {
    const job = await api.job(id);
    if (job.status.state === "succeeded") return;
    if (job.status.state === "cancelled") {
      throw new ApiError(0, "job_cancelled", "The operation was cancelled.", []);
    }
    if (job.status.state === "failed") {
      const { code, message, remedies } = job.status.error;
      throw new ApiError(0, code, message, remedies ?? []);
    }
    await new Promise((resolve) => setTimeout(resolve, 400));
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      ...init,
      headers: {
        ...(init?.body ? { "content-type": "application/json" } : {}),
        ...init?.headers,
      },
    });
  } catch (cause) {
    // The gateway is not answering at all. Said as such, rather than as a
    // status code that never arrived.
    throw new ApiError(
      0,
      "gateway_unreachable",
      "The gateway is not responding. Is it still running?",
      [{ label: "Check that `hermes serve` is running, then try again." }],
    );
  }

  if (response.status === 204) {
    return undefined as T;
  }

  const text = await response.text();
  const parsed: unknown = text ? safeParse(text) : null;

  if (!response.ok) {
    const body = parsed as ApiErrorBody | null;
    const error = body?.error;
    throw new ApiError(
      response.status,
      error?.code ?? "http_error",
      error?.message ?? `${response.status} ${response.statusText}`,
      error?.hermes?.remedies ?? [],
    );
  }

  return parsed as T;
}

function safeParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

interface ListBody<T> {
  object: string;
  data: T[];
}

export const api = {
  system: () => request<SystemReport>("/api/v1/system"),
  metrics: () => request<Metrics>("/api/v1/metrics"),
  gateway: () => request<GatewayReport>("/api/v1/gateway"),

  models: () =>
    request<ListBody<CatalogRow>>("/api/v1/models").then((body) => body.data),
  /**
   * One model in full, optionally priced for options the user is weighing.
   *
   * The arithmetic stays on the gateway: changing the KV type changes bytes per
   * token, and doing that here would mean a second implementation of ggml block
   * geometry waiting to disagree with what the engine allocates.
   */
  model: (id: string, options: { ctx?: number; kv_type?: string } = {}) => {
    const query = new URLSearchParams();
    if (options.ctx !== undefined) query.set("ctx", String(options.ctx));
    if (options.kv_type !== undefined) query.set("kv_type", options.kv_type);
    const suffix = query.size > 0 ? `?${query}` : "";
    return request<ModelDetail>(
      `/api/v1/models/${encodeURIComponent(id)}${suffix}`,
    );
  },
  catalog: () =>
    request<ListBody<PinnedModel>>("/api/v1/catalog").then((body) => body.data),

  loadModel: (
    id: string,
    options: { ctx?: number; kv_type?: string; force?: boolean } = {},
  ) =>
    request<{ job: number; events: string }>(
      `/api/v1/models/${encodeURIComponent(id)}/load`,
      { method: "POST", body: JSON.stringify(options) },
    ),
  unloadModel: () =>
    request<{ unloaded: string | null }>("/api/v1/models/unload", {
      method: "POST",
    }),
  removeModel: (id: string, deleteFile: boolean) =>
    request<{ removed: string; file_deleted: boolean }>(
      `/api/v1/models/${encodeURIComponent(id)}?delete_file=${deleteFile}`,
      { method: "DELETE" },
    ),
  downloadModel: (body: { id?: string; url?: string; sha256?: string }) =>
    request<{ job: number; events: string }>("/api/v1/models/download", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  importModel: (path: string) =>
    request<{ job: number; events: string }>("/api/v1/models/import", {
      method: "POST",
      body: JSON.stringify({ path }),
    }),

  jobs: () => request<ListBody<Job>>("/api/v1/jobs").then((body) => body.data),
  job: (id: number) => request<Job>(`/api/v1/jobs/${id}`),

  logs: (query: {
    level?: string;
    target?: string;
    search?: string;
    limit?: number;
  }) => {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(query)) {
      if (value !== undefined && value !== "") params.set(key, String(value));
    }
    const suffix = params.toString();
    return request<LogsBody>(`/api/v1/logs${suffix ? `?${suffix}` : ""}`);
  },

  conversations: () =>
    request<ListBody<ConversationSummary>>("/api/v1/conversations").then(
      (body) => body.data,
    ),
  conversation: (id: string) =>
    request<Conversation>(`/api/v1/conversations/${encodeURIComponent(id)}`),
  createConversation: () =>
    request<Conversation>("/api/v1/conversations", { method: "POST" }),
  saveConversation: (
    id: string,
    body: Pick<Conversation, "title" | "messages"> & {
      model?: string | null;
      created_at?: number;
    },
  ) =>
    request<Conversation>(`/api/v1/conversations/${encodeURIComponent(id)}`, {
      method: "PUT",
      body: JSON.stringify(body),
    }),
  deleteConversation: (id: string) =>
    request<{ deleted: string }>(
      `/api/v1/conversations/${encodeURIComponent(id)}`,
      { method: "DELETE" },
    ),

  settings: () => request<Settings>("/api/v1/settings"),
  saveSettings: (settings: Settings) =>
    request<Settings>("/api/v1/settings", {
      method: "PUT",
      body: JSON.stringify(settings),
    }),
};
