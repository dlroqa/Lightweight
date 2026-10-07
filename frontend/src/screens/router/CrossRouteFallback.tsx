import { useEffect, useMemo, useState } from "react";
import { ArrowDown, ArrowRight, Plus, Trash2 } from "lucide-react";

import type { ApiError } from "../../api/client";
import type { AutoView, RouterRoutesBody, RoutingTraceView, TracesBody } from "../../api/types";
import { Card } from "../../components/Card";
import { Empty, Loading, Pill, Row } from "../../components/Bits";
import { CodeBlock } from "../AccessScreen";
import { Field } from "./ClassifierScreen";
import {
  EXCLUSIONS,
  MAX_FALLBACK_ROUTES,
  TRIGGERS,
  contextFrom,
  deploymentsByRoute,
  draftFrom,
  exhaustedRows,
  fallbackSnippet,
  fallbackTraces,
  hasChainProblems,
  reasonLabel,
  transitionRows,
  validateChains,
  type ChainDraft,
} from "./fallbackModel";

/**
 * Explicit cross-route fallback (R9.3.1) on the Auto Routing screen: what is
 * configured and what it has done, a draft of the lists that produces the
 * snippet to paste, and the recent requests that changed route.
 *
 * Read-only, like the classifier screen: the router has no config write API,
 * so a draft becomes a validated snippet for `router.json`, never a save.
 */
export function CrossRouteFallbackSection({
  auto,
  routes,
  traces,
  tracesError,
}: {
  auto: AutoView;
  routes: RouterRoutesBody | null;
  traces: TracesBody | null;
  tracesError: ApiError | null;
}) {
  const view = auto.cross_route_fallback;
  if (!view) {
    return (
      <Card title="Cross-Route Fallback">
        <p className="card__note" data-fallback-unsupported>
          This router does not report cross-route fallback. Upgrade it to use this section.
        </p>
      </Card>
    );
  }
  return (
    <>
      <SummaryCard auto={auto} />
      <DraftCard auto={auto} routes={routes} />
      <RecentCard traces={traces} error={tracesError} />
    </>
  );
}

function SummaryCard({ auto }: { auto: AutoView }) {
  const view = auto.cross_route_fallback!;
  const chains = Object.entries(view.chains);
  const transitions = transitionRows(view);
  const exhausted = exhaustedRows(view);
  return (
    <Card
      title="Cross-Route Fallback"
      action={
        <Pill tone={view.configured ? "ok" : "neutral"} dot>
          {view.configured ? "Configured" : "Not configured"}
        </Pill>
      }
    >
      <div data-fallback-summary>
        <Row label="Configured">{view.configured ? "Yes" : "No"}</Row>
        <Row label="Applies to">Auto-selected routes only</Row>
        <Row label="Maximum fallback routes">
          <span className="tnum" data-max-routes>
            {view.max_routes}
          </span>
        </Row>
      </div>

      <div className="notice notice--info" role="note" style={{ margin: "12px 0" }} data-explicit-warning>
        Cross-route fallback applies only when the original request uses <code>Auto</code>.
        Explicit route requests return that route's error and do not fall back.
      </div>

      <h3 className="card__title" style={{ fontSize: 14, margin: "14px 0 6px" }}>
        Triggers
      </h3>
      <ul className="card__note" style={{ margin: 0, paddingLeft: 18 }} data-triggers>
        {TRIGGERS.filter((trigger) => view.triggers.includes(trigger.reason)).map((trigger) => (
          <li key={trigger.reason} data-trigger={trigger.reason}>
            <strong>{trigger.label}</strong> (<code>{trigger.reason}</code>): {trigger.description}
          </li>
        ))}
      </ul>
      <p className="card__note" style={{ marginTop: 8 }}>
        Each applies only before the response is committed, and only after the route's own
        deployments were tried (same-route failover comes first).
      </p>
      <h3 className="card__title" style={{ fontSize: 14, margin: "14px 0 6px" }}>
        Never falls back on
      </h3>
      <ul className="card__note" style={{ margin: 0, paddingLeft: 18 }} data-exclusions>
        {EXCLUSIONS.map((exclusion) => (
          <li key={exclusion}>{exclusion}</li>
        ))}
      </ul>

      <h3 className="card__title" style={{ fontSize: 14, margin: "18px 0 6px" }}>
        Fallback lists
      </h3>
      {chains.length === 0 ? (
        <Empty
          title="No fallback lists"
          hint="A route Auto chooses returns its own error when it cannot execute. Draft lists below."
        />
      ) : (
        <ul style={{ listStyle: "none", margin: 0, padding: 0, display: "grid", gap: 6 }} data-chains>
          {chains.map(([source, targets]) => (
            <li key={source} data-chain={source} style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: 6 }}>
              <Pill tone="accent">{source}</Pill>
              {targets.map((target) => (
                <span key={target} style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                  <ArrowRight size={13} aria-label="then" />
                  <Pill tone="neutral">{target}</Pill>
                </span>
              ))}
            </li>
          ))}
        </ul>
      )}
      <p className="card__note" style={{ marginTop: 10 }} data-non-transitive>
        Only the initial route's configured list is used for a request, in order. Fallback routes
        do not recursively apply their own fallback lists.
      </p>
      <p className="card__note" data-same-vs-cross>
        Same-route failover (for example <code>Coder/A → Coder/B</code>) is a different mechanism: it
        happens first, inside a route. Cross-route fallback (<code>Coder → General</code>) happens only
        after the whole route failed for one of the triggers above.
      </p>

      <h3 className="card__title" style={{ fontSize: 14, margin: "18px 0 6px" }}>
        Fallbacks taken since the router started
      </h3>
      {transitions.length === 0 ? (
        <p className="card__note" data-no-transitions>
          None yet.
        </p>
      ) : (
        <div className="scroll-x">
          <table className="table" data-transitions>
            <thead>
              <tr>
                <th>From</th>
                <th>To</th>
                <th>Reasons</th>
                <th style={{ textAlign: "right" }}>Fallbacks</th>
              </tr>
            </thead>
            <tbody>
              {transitions.map((row) => (
                <tr key={`${row.from}->${row.to}`} data-transition={`${row.from}->${row.to}`}>
                  <td>{row.from}</td>
                  <td>{row.to}</td>
                  <td style={{ color: "var(--text-muted)" }}>
                    {row.reasons.map(([reason, count]) => `${reason}: ${count}`).join(" · ")}
                  </td>
                  <td className="tnum" style={{ textAlign: "right" }}>
                    {row.total}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {exhausted.length > 0 && (
        <div className="scroll-x" style={{ marginTop: 10 }}>
          <table className="table" data-exhausted>
            <thead>
              <tr>
                <th>Initial route</th>
                <th>Last reason</th>
                <th style={{ textAlign: "right" }}>Exhausted lists</th>
              </tr>
            </thead>
            <tbody>
              {exhausted.map((row) => (
                <tr key={row.route} data-exhausted-route={row.route}>
                  <td>{row.route}</td>
                  <td style={{ color: "var(--text-muted)" }}>
                    {row.reasons.map(([reason, count]) => `${reason}: ${count}`).join(" · ")}
                  </td>
                  <td className="tnum" style={{ textAlign: "right" }}>
                    {row.total}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <p className="card__note" style={{ marginTop: 10 }} data-identity-help>
        When a fallback route serves the answer, the response's <code>model</code> names that final
        route: <code>Auto → Coder → General</code> answers with <code>model: &quot;General&quot;</code>. An
        exhausted list returns the last attempted route's own error.
      </p>
      <p className="card__note" data-requests-total-help>
        <code>router_requests_total</code> counts each client request once, under its final serving (or
        last failing) logical route, not the route Auto first chose. Fallback transitions are counted in{" "}
        <code>router_cross_route_fallback_total</code> and{" "}
        <code>router_cross_route_fallback_exhausted_total</code>.
      </p>
    </Card>
  );
}

function DraftCard({ auto, routes }: { auto: AutoView; routes: RouterRoutesBody | null }) {
  const ctx = useMemo(() => contextFrom(auto, routes?.data ?? []), [auto, routes]);
  const [draft, setDraft] = useState<ChainDraft[] | null>(null);
  // Seed the draft from the running configuration once, then leave it alone:
  // polling must never overwrite what the operator is typing.
  useEffect(() => {
    if (draft === null && auto.cross_route_fallback) {
      setDraft(draftFrom(auto.cross_route_fallback));
    }
  }, [auto, draft]);
  const rows = draft ?? [];
  const problems = validateChains(rows, ctx);
  const invalid = hasChainProblems(problems);
  const nextId = rows.reduce((max, row) => Math.max(max, row.id), 0) + 1;
  const update = (id: number, change: Partial<ChainDraft>) =>
    setDraft(rows.map((row) => (row.id === id ? { ...row, ...change } : row)));
  const sources = ctx.reachable.filter((route) => !ctx.classifierRoutes.includes(route));

  return (
    <Card title="Draft fallback lists">
      <p className="card__note" style={{ marginTop: 0 }}>
        One list per initial route, in the order routes are tried, at most {MAX_FALLBACK_ROUTES}. Only
        routes Auto can choose may have a list. The router checks everything again when it loads the
        file.
      </p>
      {rows.length === 0 && (
        <p className="card__note" data-draft-empty>
          No lists drafted. With none, nothing falls back.
        </p>
      )}
      <div style={{ display: "grid", gap: 12 }}>
        {rows.map((row) => (
          <fieldset
            key={row.id}
            className="field"
            data-draft-row={row.id}
            style={{ border: "1px solid var(--rule)", borderRadius: "var(--radius)", padding: 12, margin: 0 }}
          >
            <legend className="field__label" style={{ padding: "0 4px" }}>
              List {row.id}
            </legend>
            <div style={{ display: "grid", gap: 12, gridTemplateColumns: "minmax(160px, 220px) 1fr auto", alignItems: "start" }}>
              <Field label="Initial route">
                {(props) => (
                  <select
                    {...props}
                    className="select"
                    value={row.source}
                    data-draft-source
                    onChange={(e) => update(row.id, { source: e.target.value })}
                  >
                    <option value="">Choose a route…</option>
                    {sources.map((route) => (
                      <option key={route} value={route}>
                        {route}
                      </option>
                    ))}
                    {row.source && !sources.includes(row.source) && (
                      <option value={row.source}>{row.source}</option>
                    )}
                  </select>
                )}
              </Field>
              <Field
                label="Fallback routes, in order"
                help={`Comma-separated, at most ${MAX_FALLBACK_ROUTES}. For example: General, Reasoning`}
              >
                {(props) => (
                  <input
                    {...props}
                    className="input"
                    value={row.targets}
                    data-draft-targets
                    placeholder="General, Reasoning"
                    onChange={(e) => update(row.id, { targets: e.target.value })}
                  />
                )}
              </Field>
              <button
                type="button"
                className="btn btn--ghost"
                style={{ marginTop: 26 }}
                aria-label={`Remove list ${row.id}`}
                onClick={() => setDraft(rows.filter((other) => other.id !== row.id))}
              >
                <Trash2 size={14} /> Remove
              </button>
            </div>
            {problems.rows[row.id] && (
              <span className="field__error" role="alert" data-draft-problem>
                {problems.rows[row.id]}
              </span>
            )}
          </fieldset>
        ))}
      </div>
      <div style={{ display: "flex", gap: 8, marginTop: 12 }}>
        <button
          type="button"
          className="btn"
          data-draft-add
          onClick={() => setDraft([...rows, { id: nextId, source: "", targets: "" }])}
        >
          <Plus size={14} /> Add a list
        </button>
        <button
          type="button"
          className="btn btn--ghost"
          onClick={() => setDraft(draftFrom(auto.cross_route_fallback))}
        >
          Reset to running
        </button>
      </div>
      {problems.cycle && (
        <div className="notice notice--danger" role="alert" style={{ marginTop: 12 }} data-draft-cycle>
          {problems.cycle} The router refuses a cycle anywhere in the lists, even though a request only
          ever follows its initial route's list.
        </div>
      )}

      <h3 className="card__title" style={{ fontSize: 14, margin: "18px 0 6px" }}>
        Configuration to apply
      </h3>
      {invalid ? (
        <div className="notice notice--warn" role="status" data-fallback-snippet-blocked>
          Fix the lists marked above to produce a configuration the router will accept.
        </div>
      ) : (
        <div style={{ display: "grid", gap: 12 }} data-fallback-snippet>
          <p className="card__note" style={{ margin: 0 }}>
            {rows.length === 0 ? (
              <>
                Remove <code>cross_route_fallback</code> from <code>auto_route</code> in{" "}
                <code>router.json</code>, or set it to:
              </>
            ) : (
              <>
                Set <code>cross_route_fallback</code> in <code>auto_route</code> in{" "}
                <code>router.json</code> to:
              </>
            )}
          </p>
          <CodeBlock text={fallbackSnippet(rows, ctx)} />
          <ol className="card__note" style={{ margin: 0, paddingLeft: 18 }}>
            <li>
              Check the file: <code>hermes router validate-config</code>
            </li>
            <li data-restart-required>Restart the router to load it. Nothing is saved from this panel.</li>
          </ol>
        </div>
      )}
    </Card>
  );
}

function RecentCard({ traces, error }: { traces: TracesBody | null; error: ApiError | null }) {
  const recent = fallbackTraces(traces).slice(0, 10);
  return (
    <Card title="Recent cross-route fallbacks">
      {error ? (
        <p className="card__note">{error.message}</p>
      ) : !traces ? (
        <Loading what="recent requests" />
      ) : recent.length === 0 ? (
        <Empty
          title="No recent fallbacks"
          hint="Requests that changed logical route appear here, from the router's recent traces."
        />
      ) : (
        <div style={{ display: "grid", gap: 14 }} data-fallback-traces>
          {recent.map((trace) => (
            <TraceSteps key={trace.request_id} trace={trace} />
          ))}
        </div>
      )}
    </Card>
  );
}

function TraceSteps({ trace }: { trace: RoutingTraceView }) {
  const block = trace.cross_route_fallback!;
  const groups = deploymentsByRoute(trace);
  return (
    <div
      data-fallback-trace={trace.request_id}
      style={{ border: "1px solid var(--rule)", borderRadius: "var(--radius)", padding: 12 }}
    >
      <div style={{ display: "flex", flexWrap: "wrap", gap: 8, alignItems: "center", marginBottom: 8 }}>
        <span className="card__note" style={{ margin: 0 }}>
          Requested <strong>{trace.requested_route}</strong> · initial <strong>{block.initial_route}</strong> ·
          final <strong data-trace-final>{block.final_route}</strong>
        </span>
        {block.exhausted ? (
          <Pill tone="danger">Exhausted</Pill>
        ) : (
          <Pill tone="ok">Served by {block.final_route}</Pill>
        )}
        <code style={{ fontSize: 12, color: "var(--text-muted)" }}>{trace.request_id}</code>
      </div>
      <ol style={{ listStyle: "none", margin: 0, padding: 0, display: "grid", gap: 4 }}>
        {block.attempts.map((attempt, index) => {
          const deployments = groups.find((group) => group.route === attempt.route)?.deployments ?? [];
          return (
            <li key={`${attempt.route}-${index}`} data-trace-step={attempt.route}>
              {index > 0 && <ArrowDown size={13} aria-label="then" style={{ margin: "2px 0 2px 6px" }} />}
              <div style={{ display: "flex", flexWrap: "wrap", gap: 8, alignItems: "center" }}>
                <Pill tone={attempt.outcome === "committed" ? "ok" : "warn"}>{attempt.route}</Pill>
                <span>
                  {attempt.outcome === "committed"
                    ? `answered${trace.status ? ` (${trace.status})` : ""}`
                    : `${reasonLabel(attempt.reason)}${attempt.reason ? ` (${attempt.reason})` : ""}`}
                </span>
                {deployments.length > 0 && (
                  <span className="card__note" style={{ margin: 0 }}>
                    same-route attempts: {deployments.join(" → ")}
                  </span>
                )}
              </div>
            </li>
          );
        })}
        {block.exhausted && (
          <li data-trace-exhausted>
            <ArrowDown size={13} aria-label="then" style={{ margin: "2px 0 2px 6px" }} />
            <div>Exhausted: the client got {block.final_route}&apos;s own error.</div>
          </li>
        )}
      </ol>
    </div>
  );
}
