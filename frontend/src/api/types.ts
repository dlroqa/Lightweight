/**
 * The gateway's payloads, as they actually arrive.
 *
 * Every type here was written from a response captured off a running gateway,
 * not from the Rust source and not from memory. Where a field can be absent the
 * type says so, because the panel has to render that case rather than crash on
 * it — `model: null` on a gateway with nothing loaded is the ordinary state at
 * first start, not an error.
 */

/** A probe that reports whether it managed to read anything. */
export type Probed<T> =
  | ({ state: "read" } & T)
  | { state: "unavailable"; code: string; message: string };

export function wasRead<T>(
  probed: Probed<T> | undefined,
): probed is { state: "read" } & T {
  return probed?.state === "read";
}

export interface CpuReport {
  model: string | null;
  architecture: string;
  physical_cores: number;
  logical_cores: number;
  features: string[];
  default_threads: number;
  thread_choices: number[];
  expected_ggml_variant: string;
  has_avx_family: boolean;
}

export interface CpuTimes {
  total: number;
  idle: number;
}

export interface MemoryReport {
  total: number;
  available: number;
  free: number;
  swap_total: number;
  swap_free: number;
  used: number;
  swap_used: number;
  pressure: number;
}

export interface FilesystemReport {
  path: string;
  total: number;
  available: number;
  free: number;
  used: number;
  pressure: number;
}

export interface DiskReport {
  downloads: Probed<FilesystemReport>;
  models: Probed<FilesystemReport>;
  same_filesystem: boolean | null;
}

export interface SystemReport {
  os: { name: string; family: string; architecture: string };
  cpu: CpuReport;
  cpu_times: Probed<CpuTimes>;
  memory: Probed<MemoryReport>;
  disk: Probed<DiskReport>;
}

export interface Tally {
  count: number;
  total_ms: number;
  max_ms: number;
  /**
   * Cumulative histogram buckets: each entry counts every observation at or
   * below its bound, and `le_ms: null` is the `+Inf` bucket.
   *
   * The bounds travel with the counts rather than being duplicated here, so
   * this file cannot drift out of step with the gateway's own ladder.
   */
  buckets: Bucket[];
}

export interface Bucket {
  le_ms: number | null;
  count: number;
}

export interface QueueSnapshot {
  capacity: number;
  running: number;
  waiting: number;
  waiting_interactive: number;
  waiting_bulk: number;
  admitted_immediately: number;
  queued: number;
  timed_out: number;
  abandoned: number;
  overtakes: number;
  wait_ms_total: number;
  wait_ms_max: number;
}

export interface Metrics {
  uptime_seconds: number;
  in_flight: number;
  requests: { endpoint: string; outcome: string; count: number }[];
  generations: number;
  finish_reasons: {
    stop: number;
    length: number;
    tool_calls: number;
    error: number;
    cancelled: number;
  };
  tokens: {
    prompt: number;
    completion: number;
    cached: number;
    prefilled: number;
    decoded: number;
  };
  queue_wait: Tally;
  time_to_first_token: Tally;
  prefill: Tally;
  decode: Tally;
  queue: QueueSnapshot;
  model: { id: string; n_ctx: number } | null;
  bands: {
    interactive_prompt_tokens: number;
    interactive_output_tokens: number;
  };
  bands_served: BandCount[];
  engine: Probed<EngineMemory>;
  engine_cpu: Probed<EngineCpu>;
  engine_counters: Probed<EngineCounters>;
}

export interface BandCount {
  band: string;
  generations: number;
  queue_wait: Tally;
}

/**
 * Processor time charged to the engine, in kernel clock ticks.
 *
 * A counter, so a rate needs two readings — the same discipline `cpu_times`
 * imposes for the machine as a whole, and the reason `useUtilization` exists.
 */
export interface EngineCpu {
  user_ticks: number;
  system_ticks: number;
}

/**
 * What the engine reports about its own work.
 *
 * Every field is optional because an engine publishes what its build
 * publishes: a counter this build does not have reads as absent, never as
 * zero.
 */
export interface EngineCounters {
  max_sequence_tokens?: number;
  decode_calls?: number;
  busy_slots_per_decode?: number;
  requests_processing?: number;
  requests_deferred?: number;
}

/**
 * What the engine process is holding.
 *
 * The number that makes a `coarse` estimate checkable: what a load actually
 * took, beside what it was predicted to take.
 */
export interface EngineMemory {
  rss: number;
  peak_rss: number;
  anon_rss?: number;
}

export interface Listener {
  address: string;
  port: number;
  loopback: boolean;
}

export interface GatewayReport {
  version: string;
  backend: string;
  engine: { state: string; detail?: string; error?: string };
  model: string | null;
  listeners: Listener[];
  restart_required: string[];
  auth: { required: boolean };
  concurrency: {
    max_concurrent_requests: number;
    queue_timeout_seconds: number;
    /** What was asked for on the command line; null when it was `auto`. */
    requested: number | null;
  };
  queue: QueueSnapshot;
  paths: { data: string; models: string; logs: string } | null;
  /** What this engine accepts. The source of the KV type choices offered. */
  engine_capabilities: {
    device: string;
    streaming: boolean;
    tool_calls: boolean;
    reasoning_content: boolean;
    max_concurrent_requests: number;
    kv_cache_types: string[];
  };
  /** What a load uses when a request names nothing. */
  defaults: {
    kv_type: string;
    threads?: number;
    concurrency: number;
    /** The physical batch a load uses when a request names none. */
    ubatch: number;
    /** The load modes this engine accepts, in its own spelling. */
    load_modes: string[];
  };
}

/** A row of `GET /api/v1/models`. */
export interface CatalogRow {
  id: string;
  name: string;
  path: string;
  bytes: number;
  sha256: string;
  architecture: string;
  supported: boolean;
  param_count?: number;
  quantization?: string;
  context_length?: number;
  weight_bytes?: number;
  added_at: number;
  last_loaded_at?: number;
  last_n_ctx?: number;
  state: "loaded" | "available" | "missing";
  verified: boolean;
  integrity_label: string;
}

export interface HeaderDetail {
  block_count: number | null;
  embedding_length: number | null;
  feed_forward_length: number | null;
  head_count: number | null;
  head_count_kv: number[] | null;
  vocab_size: number | null;
  context_length: number | null;
  rope_freq_base: number | null;
  sliding_window: number | null;
  tensor_count: number;
  gguf_version: number;
  /** Context sizes this model can be loaded at, smallest first. */
  context_presets: number[];
  missing: string[];
}

export interface Estimate {
  weights: number;
  kv_cache: number;
  compute: number;
  overhead: number;
  total: number;
  budget: number;
  margin: number;
  verdict: string;
  confidence: string;
  kv_bytes_per_token: number;
  max_context_that_fits: number | null;
  missing: string[];
  /**
   * The part of `budget` that is not free yet, because a model this load would
   * replace is still holding it. Zero for every load that replaces nothing.
   */
  reclaimable: number;
  params: { n_ctx: number; n_batch: number; n_ubatch: number; n_parallel: number };
}

export type ModelDetail = CatalogRow & {
  header?: HeaderDetail;
  /**
   * Absent only when there was no header to estimate against. An estimate the
   * gateway could not *compute* arrives as `unavailable` with the reason.
   */
  estimate?: Probed<Estimate>;
  /** Why the estimate is for the context it is for. */
  context_source?: "requested" | "setting" | "fitted";
};

/** A pinned model from `GET /api/v1/catalog`. */
export interface PinnedModel {
  id: string;
  name: string;
  repo: string;
  file: string;
  url: string;
  sha256: string;
  size: number;
  parameters: string;
  quantization: string;
  summary: string;
  installed: boolean;
}

export type JobState =
  | { state: "running"; of: string; [key: string]: unknown }
  | { state: "succeeded"; model: string | null }
  | {
      state: "failed";
      error: { message: string; code: string; remedies?: Remedy[] };
    }
  | { state: "cancelled" };

export interface Job {
  id: number;
  kind: string;
  started_at: number;
  status: JobState;
}

export interface LogRecord {
  timestamp: string;
  level: string;
  target: string;
  message: string;
  fields?: Record<string, unknown>;
}

export interface LogsBody {
  object: string;
  data: LogRecord[];
  truncated: boolean;
  files: string[];
}

export interface StoredMessage {
  role: string;
  content: string;
  reasoning_content?: string;
  at: number;
  prompt_tokens?: number;
  completion_tokens?: number;
  tokens_per_second?: number;
}

export interface Conversation {
  id: string;
  title: string;
  created_at: number;
  updated_at: number;
  model?: string | null;
  messages: StoredMessage[];
}

export interface ConversationSummary {
  id: string;
  title: string;
  created_at: number;
  updated_at: number;
  model: string | null;
  message_count: number;
  preview: string;
}

export interface Settings {
  gateway: {
    keep_history: boolean;
    default_n_ctx: number | null;
  };
  /** Opaque to the gateway; this is the panel's half. */
  ui: PanelPreferences & Record<string, unknown>;
}

/** What the panel itself remembers. Stored inside `settings.ui`. */
export interface PanelPreferences {
  theme?: "light" | "dark" | "system";
  translucent?: boolean;
  compact?: boolean;
  railCollapsed?: boolean;
  sampling?: SamplingPreferences;
}

export interface SamplingPreferences {
  temperature: number;
  top_p: number;
  top_k: number;
  min_p: number;
  repeat_penalty: number;
  presence_penalty: number;
  frequency_penalty: number;
  seed: number;
  max_tokens: number;
}

/** A finished generation, from `GET /api/v1/events`. */
export interface RequestEvent {
  at_unix_ms: number;
  id: string | null;
  model: string | null;
  prompt_tokens: number;
  completion_tokens: number;
  cached_tokens: number;
  finish_reason: string | null;
  queue_wait_ms: number;
  time_to_first_token_ms: number | null;
  total_ms: number;
  /** Which queue served it: "interactive" or "bulk". */
  band: string | null;
}

/** One request holding a slot right now. */
export interface RunningRequest {
  id: string | null;
  model: string | null;
  band: string;
  running_ms: number;
  prompt_tokens: number;
}

/** One request still waiting for a slot. */
export interface WaitingRequest {
  /** The scheduler's ticket number: a queued request has no completion id yet. */
  ticket: number;
  band: string;
  waited_ms: number;
  position: number;
}

/** Who is being served, and who is waiting, at one instant. */
export interface RequestRoster {
  capacity: number;
  running: RunningRequest[];
  waiting: WaitingRequest[];
}

/** The error envelope every failing endpoint returns. */
/**
 * A remedy as the gateway sends it.
 *
 * `label` is the sentence to show; `action` is the tag the UI switches on to
 * offer a button that applies the fix. Both are carried rather than just the
 * sentence, because a remedy the user can only read is half of what the error
 * taxonomy promises.
 */
export interface Remedy {
  label: string;
  /**
   * Absent only on the one remedy the panel makes up itself, for the case where
   * the gateway never answered at all and so cannot have sent one. Everything
   * that crosses the wire carries an action.
   */
  action?: string;
  [field: string]: unknown;
}

export interface ApiErrorBody {
  error: {
    message: string;
    type: string;
    code: string;
    param?: string | null;
    hermes?: { remedies?: Remedy[] } | null;
  };
}

/**
 * One saved benchmark run.
 *
 * A record of what this machine did, not a claim about the software: the
 * machine and engine fingerprints travel with every run precisely so that a
 * number is never read as a property of the product.
 */
export interface BenchmarkRun {
  id: string;
  at_unix: number;
  machine: {
    cpu_model: string | null;
    physical_cores: number;
    logical_cores: number;
    isa_features: string[];
    total_memory: number;
    os: string;
    architecture: string;
  };
  engine: {
    backend: string;
    build: string | null;
    ggml_variant: string | null;
  };
  model: {
    id: string;
    architecture: string;
    quantization: string;
    parameters: number | null;
  };
  samples: BenchmarkSample[];
}

export interface BenchmarkSample {
  scenario: "cold_prefill" | "cached_prefill" | "decode";
  params: { n_ctx: number; n_batch: number; n_ubatch: number };
  threads: number;
  repetition: number;
  prompt_tokens: number;
  cached_tokens: number;
  /** Tokens the engine actually evaluated, which a cache hit makes far smaller. */
  prefilled_tokens: number;
  generated_tokens: number;
  prefill_ms: number | null;
  decode_ms: number | null;
  time_to_first_token_ms: number | null;
  wall_ms: number;
  engine_ticks: number | null;
  machine_ticks: number | null;
  peak_rss: number | null;
}
