import { useEffect, useState } from "react";
import { Download, HardDriveDownload, MoreVertical, RefreshCw } from "lucide-react";

import { api, ApiError, followJob } from "../api/client";
import { bytes, contextLength, parameters } from "../api/format";
import type { CatalogRow, Estimate, ModelDetail, PinnedModel } from "../api/types";
import { wasRead } from "../api/types";
import { Card } from "../components/Card";
import { Empty, ErrorState, Loading, Pill, Row } from "../components/Bits";
import { TopBar } from "../components/Shell";
import { usePoll } from "../hooks/usePoll";

export function Models() {
  const models = usePoll(api.models, 4000);
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState<ModelDetail | null>(null);
  const [detailError, setDetailError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  // An `ApiError` rather than a string, so the remedies the gateway sends
  // reach the screen. A refused load is the case that matters: it arrives with
  // "Reduce the context to N tokens" and "Quantize the KV cache" attached.
  const [failure, setFailure] = useState<ApiError | null>(null);
  const [adding, setAdding] = useState(false);

  // The list is polled; the detail is fetched only for the row in hand. That
  // split is the whole reason the detail endpoint exists — it opens the GGUF,
  // and doing that per row per poll would make watching the list cost more than
  // using it.
  // What the user is weighing. Both are per-load: a context is judged by the
  // estimate beside it, and a KV type trades output quality, which no estimate
  // judges — so neither is stored as a gateway-wide default here.
  const [wanted, setWanted] = useState<LoadChoice>({});

  // What this engine accepts, and what a load would use if asked nothing. Both
  // come from the gateway rather than from a list kept here: the engine's real
  // set is read from its own `--help` at the pinned build, and a copy in the
  // panel would be a second answer waiting to go stale.
  const gateway = usePoll(api.gateway, 0);
  // `thread_choices` has been served by /api/v1/system since M6b.1 and read by
  // nothing. This is what reads it.
  const system = usePoll(api.system, 0);
  const kvTypes = gateway.data?.engine_capabilities.kv_cache_types ?? [];
  const refused =
    wasRead(detail?.estimate) && detail.estimate.verdict === "insufficient";

  useEffect(() => {
    if (!selected) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    setDetailError(null);
    void api
      .model(selected, wanted)
      .then((loaded) => !cancelled && setDetail(loaded))
      .catch((cause: ApiError) => !cancelled && setDetailError(cause));
    return () => {
      cancelled = true;
    };
  }, [selected, wanted]);

  // A different model is a different set of choices; carrying the last one over
  // would price a context this model may not support.
  useEffect(() => setWanted({}), [selected]);

  async function act(work: () => Promise<unknown>) {
    setBusy(true);
    setFailure(null);
    try {
      await work();
      models.refresh();
      if (selected) setDetail(await api.model(selected, wanted).catch(() => null));
    } catch (cause) {
      setFailure(
        cause instanceof ApiError
          ? cause
          : new ApiError(
              0,
              "unexpected",
              cause instanceof Error ? cause.message : String(cause),
              [],
            ),
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <TopBar
        title="Models"
        subtitle="Manage your local models"
        actions={
          <>
            <button
              type="button"
              className="btn"
              onClick={() => setAdding((current) => !current)}
            >
              <Download size={16} />
              Add model
            </button>
            <button
              type="button"
              className="btn btn--icon"
              onClick={models.refresh}
              aria-label="Refresh the model list"
            >
              <RefreshCw size={15} />
            </button>
          </>
        }
      />

      <div className="page">
        {failure && (
          <div className="notice notice--danger">
            <ErrorState error={failure} />
          </div>
        )}
        {adding && <AddModel onDone={() => { setAdding(false); models.refresh(); }} />}

        <Card flush>
          {models.loading && !models.data ? (
            <Loading what="models" />
          ) : models.error ? (
            <ErrorState error={models.error} onRetry={models.refresh} />
          ) : (models.data?.length ?? 0) === 0 ? (
            <Empty
              title="No models installed"
              hint="Add one from the pinned list, paste a direct .gguf link, or import a file already on this machine."
            />
          ) : (
            <div className="scroll-x">
              <table className="table">
                <thead>
                  <tr>
                    <th>Model</th>
                    <th>Parameters</th>
                    <th>Quantization</th>
                    <th>Size</th>
                    <th>Context</th>
                    <th>Integrity</th>
                    <th>Status</th>
                    <th aria-label="Actions" />
                  </tr>
                </thead>
                <tbody>
                  {models.data?.map((model) => (
                    <ModelRow
                      key={model.id}
                      model={model}
                      selected={model.id === selected}
                      onSelect={() =>
                        setSelected((current) =>
                          current === model.id ? null : model.id,
                        )
                      }
                    />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Card>

        {selected && (
          <Card
            title="Model details"
            action={
              detail?.state === "loaded" ? (
                <button
                  type="button"
                  className="btn btn--danger"
                  disabled={busy}
                  onClick={() => void act(api.unloadModel)}
                >
                  Unload model
                </button>
              ) : (
                <div style={{ display: "flex", gap: 8 }}>
                  {refused && (
                    // Only when the estimate says it will not fit. A control
                    // that is never needed is never shown.
                    <button
                      type="button"
                      className="btn btn--danger"
                      disabled={busy}
                      onClick={() =>
                        void act(async () => {
                          const job = await api.loadModel(selected, {
                            ...wanted,
                            force: true,
                          });
                          await followJob(job.job);
                        })
                      }
                    >
                      Load anyway
                    </button>
                  )}
                  <button
                    type="button"
                    className="btn btn--primary"
                    disabled={busy || detail?.state === "missing"}
                    onClick={() =>
                      void act(async () => {
                        // Followed to the end: the 202 only says the job
                        // started, and a refusal arrives inside it.
                        const job = await api.loadModel(selected, wanted);
                        await followJob(job.job);
                      })
                    }
                  >
                    {busy ? "Working…" : "Load model"}
                  </button>
                </div>
              )
            }
          >
            {detailError ? (
              <ErrorState error={detailError} />
            ) : !detail ? (
              <Loading what="the model" />
            ) : (
              <ModelDetailBody
                detail={detail}
                wanted={wanted}
                onWant={setWanted}
                kvTypes={kvTypes}
                defaultKvType={gateway.data?.defaults.kv_type}
                threadChoices={system.data?.cpu.thread_choices ?? []}
                defaultThreads={
                  gateway.data?.defaults.threads ??
                  system.data?.cpu.default_threads
                }
                loadModes={gateway.data?.defaults.load_modes ?? []}
                defaultUbatch={gateway.data?.defaults.ubatch}
              />
            )}
          </Card>
        )}
      </div>
    </>
  );
}

function ModelRow({
  model,
  selected,
  onSelect,
}: {
  model: CatalogRow;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <tr
      className={selected ? "is-selected" : undefined}
      onClick={onSelect}
      style={{ cursor: "pointer" }}
    >
      <td>
        <div style={{ fontWeight: 600 }}>{model.name}</div>
        <div style={{ fontSize: 11.5, color: "var(--text-muted)" }}>{model.id}</div>
      </td>
      <td className="tnum">{parameters(model.param_count)}</td>
      <td>{model.quantization ?? "—"}</td>
      <td className="tnum">{bytes(model.bytes)}</td>
      <td className="tnum">{contextLength(model.context_length)}</td>
      <td>
        {/* `recorded, not verified` is said in words, as the CLI says it. */}
        <span style={{ fontSize: 12, color: "var(--text-muted)" }}>
          {model.integrity_label}
        </span>
      </td>
      <td>
        {model.state === "loaded" ? (
          <Pill tone="ok">Loaded</Pill>
        ) : model.state === "missing" ? (
          <Pill tone="danger">Missing</Pill>
        ) : (
          <Pill tone="neutral">Available</Pill>
        )}
      </td>
      <td style={{ width: 40 }}>
        <MoreVertical size={16} color="var(--text-faint)" />
      </td>
    </tr>
  );
}

/**
 * What the user has chosen for the next load.
 *
 * Every field optional and absent by default: an unset control means "whatever
 * the gateway would do", which is exactly what the request body means too.
 */
export interface LoadChoice {
  ctx?: number;
  kv_type?: string;
  threads?: number;
  ubatch?: number;
  load_mode?: string;
}

function ModelDetailBody({
  detail,
  wanted,
  onWant,
  kvTypes,
  defaultKvType,
  threadChoices,
  defaultThreads,
  loadModes,
  defaultUbatch,
}: {
  detail: ModelDetail;
  wanted: LoadChoice;
  onWant: (wanted: LoadChoice) => void;
  kvTypes: string[];
  defaultKvType?: string;
  threadChoices: number[];
  defaultThreads?: number;
  loadModes: string[];
  defaultUbatch?: number;
}) {
  const presets = detail.header?.context_presets ?? [];
  const chosenCtx = wasRead(detail.estimate)
    ? detail.estimate.params.n_ctx
    : undefined;
  return (
    <>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 10,
          flexWrap: "wrap",
          marginBottom: 16,
        }}
      >
        <span style={{ fontSize: 16, fontWeight: 600 }}>{detail.name}</span>
        <Pill tone="accent">GGUF</Pill>
        {detail.state === "loaded" && <Pill tone="ok">Loaded</Pill>}
        {!detail.supported && (
          <Pill tone="warn">The pinned engine cannot run this architecture</Pill>
        )}
      </div>

      {detail.state === "missing" && (
        <div className="notice notice--warn" style={{ marginBottom: 16 }}>
          The file is not where the catalog recorded it. The entry is kept rather
          than forgotten — the drive may simply not be mounted.
        </div>
      )}

      <div
        className="grid"
        style={{ gridTemplateColumns: "repeat(auto-fit, minmax(240px, 1fr))" }}
      >
        <div>
          <Row label="Architecture">{detail.architecture.toUpperCase()}</Row>
          <Row label="Parameters">{parameters(detail.param_count)}</Row>
          <Row label="Quantization">{detail.quantization ?? "—"}</Row>
          <Row label="File size">{bytes(detail.bytes)}</Row>
          {detail.header && (
            <>
              <Row label="Vocab size">
                {detail.header.vocab_size?.toLocaleString() ?? "—"}
              </Row>
              <Row label="Hidden size">
                {detail.header.embedding_length?.toLocaleString() ?? "—"}
              </Row>
            </>
          )}
        </div>
        <div>
          {detail.header && (
            <>
              <Row label="Layers">{detail.header.block_count ?? "—"}</Row>
              <Row label="Attention heads">{detail.header.head_count ?? "—"}</Row>
              <Row label="KV heads">
                {detail.header.head_count_kv?.[0] ?? "—"}
              </Row>
              <Row label="Trained context">
                {contextLength(detail.header.context_length)}
              </Row>
              <Row label="Tensors">
                {detail.header.tensor_count.toLocaleString()}
              </Row>
            </>
          )}
          <Row label="Integrity">{detail.integrity_label}</Row>
        </div>
      </div>

      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
          gap: 12,
          marginTop: 16,
        }}
      >
          {presets.length > 0 && (
            <div className="field">
              <label className="field__label" htmlFor="load-ctx">
                Context
              </label>
              <select
                id="load-ctx"
                className="select tnum"
                value={wanted.ctx ?? ""}
                onChange={(event) =>
                  onWant({
                    ...wanted,
                    ctx: event.target.value ? Number(event.target.value) : undefined,
                  })
                }
              >
                <option value="">
                  {sourceLabel(detail.context_source, chosenCtx)}
                </option>
                {presets.map((preset) => (
                  <option key={preset} value={preset}>
                    {contextLength(preset)}
                  </option>
                ))}
              </select>
            </div>
          )}
          {kvTypes.length > 0 && (
            <div className="field">
              <label className="field__label" htmlFor="load-kv">
                KV cache type
              </label>
              <select
                id="load-kv"
                className="select"
                value={wanted.kv_type ?? ""}
                onChange={(event) =>
                  onWant({ ...wanted, kv_type: event.target.value || undefined })
                }
              >
                <option value="">{defaultKvType ?? "default"} (default)</option>
                {kvTypes.map((kind) => (
                  <option key={kind} value={kind}>
                    {kind}
                  </option>
                ))}
              </select>
            </div>
          )}
          {threadChoices.length > 0 && (
            <div className="field">
              <label className="field__label" htmlFor="load-threads">
                Threads
              </label>
              <select
                id="load-threads"
                className="select tnum"
                value={wanted.threads ?? ""}
                onChange={(event) =>
                  onWant({
                    ...wanted,
                    threads: event.target.value
                      ? Number(event.target.value)
                      : undefined,
                  })
                }
              >
                <option value="">
                  {defaultThreads ?? "physical cores"} (default)
                </option>
                {threadChoices.map((count) => (
                  <option key={count} value={count}>
                    {count}
                  </option>
                ))}
              </select>
            </div>
          )}
          <div className="field">
            <label className="field__label" htmlFor="load-ubatch">
              Physical batch
            </label>
            <select
              id="load-ubatch"
              className="select tnum"
              value={wanted.ubatch ?? ""}
              onChange={(event) =>
                onWant({
                  ...wanted,
                  ubatch: event.target.value
                    ? Number(event.target.value)
                    : undefined,
                })
              }
            >
              <option value="">{defaultUbatch ?? 512} (default)</option>
              {UBATCH_CHOICES.map((size) => (
                <option key={size} value={size}>
                  {size}
                </option>
              ))}
            </select>
          </div>
          {loadModes.length > 0 && (
            <div className="field">
              <label className="field__label" htmlFor="load-mode">
                Weight loading
              </label>
              <select
                id="load-mode"
                className="select"
                value={wanted.load_mode ?? ""}
                onChange={(event) =>
                  onWant({
                    ...wanted,
                    load_mode: event.target.value || undefined,
                  })
                }
              >
                <option value="">auto (default)</option>
                {loadModes
                  .filter((mode) => mode !== "auto")
                  .map((mode) => (
                    <option key={mode} value={mode}>
                      {mode}
                    </option>
                  ))}
              </select>
            </div>
          )}
      </div>
      {wanted.load_mode?.includes("mlock") && (
        <div className="notice notice--warn" style={{ marginTop: 10 }}>
          Locking keeps the weights out of swap, and locked pages cannot be
          reclaimed — so this load is refused unless the estimate is
          comfortable, and it is checked against this user&rsquo;s
          locked-memory allowance before the engine starts.
        </div>
      )}
      {wanted.ubatch !== undefined && (
        <div className="card__note" style={{ marginTop: 8 }}>
          A larger physical batch raises prompt-processing throughput and the
          compute buffers with it. The estimate below is for the size chosen.
        </div>
      )}

      {wasRead(detail.estimate) ? (
        <EstimatePanel estimate={detail.estimate} />
      ) : (
        detail.estimate && (
          <div className="notice notice--warn">
            No memory verdict: {detail.estimate.message}
          </div>
        )
      )}
    </>
  );
}

function EstimatePanel({ estimate }: { estimate: Estimate }) {
  const tone =
    estimate.verdict === "safe"
      ? "ok"
      : estimate.verdict === "insufficient"
        ? "danger"
        : "warn";

  const parts = [
    { label: "Weights", value: estimate.weights, color: "var(--series-1)" },
    { label: "KV cache", value: estimate.kv_cache, color: "var(--series-2)" },
    { label: "Compute", value: estimate.compute, color: "var(--series-3)" },
    { label: "Overhead", value: estimate.overhead, color: "var(--series-4)" },
  ];

  return (
    <div
      style={{
        marginTop: 20,
        paddingTop: 16,
        borderTop: "1px solid var(--rule)",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 12,
          marginBottom: 12,
          flexWrap: "wrap",
        }}
      >
        <span style={{ fontWeight: 600, fontSize: 14 }}>
          Memory estimate at {estimate.params.n_ctx.toLocaleString()} tokens
        </span>
        <Pill tone={tone}>{estimate.verdict}</Pill>
      </div>

      <div
        style={{
          display: "flex",
          height: 10,
          borderRadius: 999,
          overflow: "hidden",
          background: "var(--neutral-soft)",
          marginBottom: 10,
        }}
        role="img"
        aria-label={`Estimated ${bytes(estimate.total)} against a budget of ${bytes(estimate.budget)}`}
      >
        {parts.map((part) => (
          <div
            key={part.label}
            style={{
              width: `${Math.min(100, (part.value / Math.max(estimate.total, estimate.budget)) * 100)}%`,
              background: part.color,
            }}
          />
        ))}
      </div>

      <div style={{ display: "flex", flexWrap: "wrap", gap: 16, fontSize: 12 }}>
        {parts.map((part) => (
          <span key={part.label} style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <span
              className="dot"
              style={{ color: part.color, width: 8, height: 8 }}
            />
            {part.label}
            <strong className="tnum">{bytes(part.value)}</strong>
          </span>
        ))}
      </div>

      <div style={{ marginTop: 12 }}>
        <Row label="Total required">{bytes(estimate.total)}</Row>
        <Row label="Available to spend">{bytes(estimate.budget)}</Row>
        {estimate.reclaimable > 0 && (
          <Row label="Of which reclaimable">
            {bytes(estimate.reclaimable)} held by the model this would replace
          </Row>
        )}
        <Row label="Confidence">{estimate.confidence}</Row>
      </div>

      {estimate.verdict === "tight" && (
        <div className="notice notice--warn" style={{ marginTop: 12 }}>
          This fits, but inside the safety margin. It will probably work;
          another application growing could push it into an OOM kill.
        </div>
      )}
      {estimate.confidence !== "measured" && (
        <div className="notice notice--info" style={{ marginTop: 12 }}>
          This estimate uses the shipped coefficients rather than a measurement
          from this machine, so it is an upper bound rather than a prediction.
        </div>
      )}
      {estimate.missing.length > 0 && (
        <div className="notice notice--warn" style={{ marginTop: 8 }}>
          Incomplete: the header did not carry {estimate.missing.join(", ")}.
        </div>
      )}
    </div>
  );
}

/**
 * Why the context on offer is the one on offer.
 *
 * The same three sentences `hermes serve` prints, so the CLI and the panel
 * cannot describe the same decision differently.
 */
function sourceLabel(
  source: ModelDetail["context_source"],
  n_ctx?: number,
): string {
  const size = n_ctx === undefined ? "" : ` (${contextLength(n_ctx)})`;
  switch (source) {
    case "setting":
      return `Your default context length${size}`;
    case "requested":
      return `As requested${size}`;
    default:
      return `Fit to this machine${size}`;
  }
}

/** Adding a model: the pinned list, a direct link, or a file already here. */
function AddModel({ onDone }: { onDone: () => void }) {
  const pinned = usePoll<PinnedModel[]>(api.catalog, 0);
  const [url, setUrl] = useState("");
  const [sha256, setSha256] = useState("");
  const [path, setPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [started, setStarted] = useState<string | null>(null);

  async function start(work: () => Promise<{ job: number }>, what: string) {
    setBusy(true);
    setFailure(null);
    try {
      const job = await work();
      setStarted(`${what} started as job ${job.job}. Progress is on the Logs screen.`);
      onDone();
    } catch (cause) {
      setFailure(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card title="Add a model">
      {failure && <div className="notice notice--danger">{failure}</div>}
      {started && <div className="notice notice--info">{started}</div>}

      <div style={{ marginBottom: 18 }}>
        <div className="card__note" style={{ marginBottom: 8 }}>
          Pinned models — each one is downloaded and checked against a recorded
          digest.
        </div>
        <div className="scroll-x">
          <table className="table">
            <tbody>
              {pinned.data?.map((model) => (
                <tr key={model.id}>
                  <td>
                    <div style={{ fontWeight: 600 }}>{model.name}</div>
                    <div style={{ fontSize: 11.5, color: "var(--text-muted)" }}>
                      {model.summary}
                    </div>
                  </td>
                  <td className="tnum">{model.parameters}</td>
                  <td>{model.quantization}</td>
                  <td className="tnum">{bytes(model.size)}</td>
                  <td style={{ textAlign: "right" }}>
                    {model.installed ? (
                      <Pill tone="ok">Installed</Pill>
                    ) : (
                      <button
                        type="button"
                        className="btn"
                        disabled={busy}
                        onClick={() =>
                          void start(() => api.downloadModel({ id: model.id }), "Download")
                        }
                      >
                        <Download size={15} />
                        Download
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      <div
        className="grid"
        style={{ gridTemplateColumns: "repeat(auto-fit, minmax(260px, 1fr))" }}
      >
        <div>
          <div className="field">
            <label className="field__label" htmlFor="model-url">
              Direct .gguf link
            </label>
            <input
              id="model-url"
              className="input"
              placeholder="https://…/model.gguf"
              value={url}
              onChange={(event) => setUrl(event.target.value)}
            />
          </div>
          <div className="field" style={{ marginTop: 10 }}>
            <label className="field__label" htmlFor="model-sha">
              Expected sha256 (optional)
            </label>
            <input
              id="model-sha"
              className="input"
              placeholder="Leave empty if the host publishes one"
              value={sha256}
              onChange={(event) => setSha256(event.target.value)}
            />
          </div>
          <button
            type="button"
            className="btn btn--primary"
            style={{ marginTop: 10 }}
            disabled={busy || url.trim() === ""}
            onClick={() =>
              void start(
                () =>
                  api.downloadModel(
                    sha256.trim() ? { url, sha256: sha256.trim() } : { url },
                  ),
                "Download",
              )
            }
          >
            Download from link
          </button>
        </div>

        <div>
          <div className="field">
            <label className="field__label" htmlFor="model-path">
              Import a file already on this machine
            </label>
            <input
              id="model-path"
              className="input"
              placeholder="/path/to/model.gguf"
              value={path}
              onChange={(event) => setPath(event.target.value)}
            />
          </div>
          <button
            type="button"
            className="btn"
            style={{ marginTop: 10 }}
            disabled={busy || path.trim() === ""}
            onClick={() => void start(() => api.importModel(path), "Import")}
          >
            <HardDriveDownload size={15} />
            Import
          </button>
          <div className="card__note" style={{ marginTop: 8 }}>
            The file is registered where it lies and hashed in place; it is not
            copied.
          </div>
        </div>
      </div>
    </Card>
  );
}

/**
 * Physical batch sizes worth offering.
 *
 * Powers of two around the engine's default of 512. Not derived from the
 * machine, because this one is not a machine limit: it is a throughput and
 * memory trade whose right answer is measured, which is what `hermes bench`
 * is for.
 */
const UBATCH_CHOICES = [64, 128, 256, 512, 1024, 2048];
