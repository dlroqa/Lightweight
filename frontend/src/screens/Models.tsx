import { useEffect, useState } from "react";
import { Download, HardDriveDownload, MoreVertical, RefreshCw } from "lucide-react";

import { api, ApiError } from "../api/client";
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
  const [failure, setFailure] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  // The list is polled; the detail is fetched only for the row in hand. That
  // split is the whole reason the detail endpoint exists — it opens the GGUF,
  // and doing that per row per poll would make watching the list cost more than
  // using it.
  useEffect(() => {
    if (!selected) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    setDetailError(null);
    void api
      .model(selected)
      .then((loaded) => !cancelled && setDetail(loaded))
      .catch((cause: ApiError) => !cancelled && setDetailError(cause));
    return () => {
      cancelled = true;
    };
  }, [selected]);

  async function act(work: () => Promise<unknown>) {
    setBusy(true);
    setFailure(null);
    try {
      await work();
      models.refresh();
      if (selected) setDetail(await api.model(selected).catch(() => null));
    } catch (cause) {
      setFailure(cause instanceof Error ? cause.message : String(cause));
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
        {failure && <div className="notice notice--danger">{failure}</div>}
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
                <button
                  type="button"
                  className="btn btn--primary"
                  disabled={busy || detail?.state === "missing"}
                  onClick={() => void act(() => api.loadModel(selected))}
                >
                  {busy ? "Working…" : "Load model"}
                </button>
              )
            }
          >
            {detailError ? (
              <ErrorState error={detailError} />
            ) : !detail ? (
              <Loading what="the model" />
            ) : (
              <ModelDetailBody detail={detail} />
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

function ModelDetailBody({ detail }: { detail: ModelDetail }) {
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
        <Row label="Confidence">{estimate.confidence}</Row>
      </div>

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
