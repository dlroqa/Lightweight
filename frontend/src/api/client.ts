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
  readonly remedies: string[];

  constructor(status: number, code: string, message: string, remedies: string[]) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.remedies = remedies;
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
      ["Check that `hermes serve` is running, then try again."],
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
      error?.hermes?.remedies?.map((remedy) => remedy.message) ?? [],
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
  model: (id: string) =>
    request<ModelDetail>(`/api/v1/models/${encodeURIComponent(id)}`),
  catalog: () =>
    request<ListBody<PinnedModel>>("/api/v1/catalog").then((body) => body.data),

  loadModel: (id: string, options: { ctx?: number; force?: boolean } = {}) =>
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
