import { useEffect, useId, useMemo, useState, type ReactNode } from "react";
import { Loader2, PlugZap, ShieldAlert } from "lucide-react";

import { routerApi } from "../../api/client";
import type {
  AutoView,
  ClassifierCheckReport,
  ClassifierView,
  RouterRouteView,
} from "../../api/types";
import { Card } from "../../components/Card";
import { Empty, Loading, Pill, Row, Switch } from "../../components/Bits";
import { TopBar } from "../../components/Shell";
import { usePoll } from "../../hooks/usePoll";
import { CodeBlock } from "../AccessScreen";
import { RouterError } from "./AutoRoutingScreen";
import {
  DEFAULT_API_KEY_ENV,
  DEFAULT_BASE_URL,
  DEFAULT_MAX_INPUT_CHARS,
  DEFAULT_MIN_CONFIDENCE,
  MAX_CANDIDATES,
  MAX_DESCRIPTION_CHARS,
  MAX_TIMEOUT_MS,
  SUGGESTED_JEV_TIMEOUT_MS,
  type ClassifierDraft,
  type DraftProblems,
  type JevDraft,
  type LightweightDraft,
  type ProviderKind,
  classifierSnippet,
  descriptionChanges,
  draftFrom,
  explainCheck,
  hasProblems,
  isLogicalRoute,
  keyStatus,
  outcomeLabel,
  validateDraft,
} from "./classifierModel";

const PROVIDER_NAMES: Record<ProviderKind, string> = {
  lightweight: "Lightweight",
  jev: "Jev / TypeSafe",
};

function when(unixSeconds: number | null | undefined): string {
  if (!unixSeconds) return "never";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  const ago =
    seconds < 60
      ? "just now"
      : seconds < 3600
        ? `${Math.floor(seconds / 60)}m ago`
        : seconds < 86_400
          ? `${Math.floor(seconds / 3600)}h ago`
          : `${Math.floor(seconds / 86_400)}d ago`;
  return `${ago} · ${new Date(unixSeconds * 1000).toLocaleString()}`;
}

/**
 * The semantic classifier: which provider chooses a route for the `Auto`
 * rules that ask for classification, how it is doing, and its settings.
 *
 * The status and Test Connection are the running router's. The settings are a
 * draft: the router reads `router.json` once at start and has no API that
 * writes it, so the panel produces the validated configuration to paste
 * rather than pretending to save it.
 */
export function ClassifierScreen() {
  const auto = usePoll(routerApi.auto, 5000);
  const routes = usePoll(routerApi.routes, 15_000);

  return (
    <>
      <TopBar
        title="Classifier"
        subtitle="Which provider chooses a route for Auto's semantic classification rules"
      />
      <div className="page" style={{ display: "grid", gap: 18 }}>
        {auto.error ? (
          <RouterError error={auto.error} onRetry={auto.refresh} />
        ) : (auto.loading && !auto.data) || (routes.loading && !routes.data) ? (
          <Loading what="the classifier" />
        ) : auto.data ? (
          <>
            <StatusCard auto={auto.data} onChecked={auto.refresh} />
            <Settings auto={auto.data} routes={routes.data?.data ?? []} />
          </>
        ) : null}
      </div>
    </>
  );
}

// --- status and Test Connection ------------------------------------------------------

function StatusCard({ auto, onChecked }: { auto: AutoView; onChecked: () => void }) {
  const classifier = auto.classifier ?? null;
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<ClassifierCheckReport | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  async function check() {
    setChecking(true);
    setFailure(null);
    try {
      setResult(await routerApi.checkClassifier());
      onChecked();
    } catch (cause) {
      setResult(null);
      setFailure(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setChecking(false);
    }
  }

  if (!classifier) {
    return (
      <Card title="Provider status">
        <Empty
          title="No classifier is configured"
          hint="Auto rules route directly. Use the settings below to write a classifier section, then add a rule with semantic classification."
        />
      </Card>
    );
  }

  const status = classifier.status;
  const key = keyStatus(classifier);
  const lastCheck = result ?? status.last_check;
  const outcomes = Object.entries(classifier.outcomes);

  return (
    <Card
      title="Provider status"
      action={
        <button
          type="button"
          className="btn btn--primary"
          onClick={check}
          disabled={checking}
          aria-busy={checking}
        >
          {checking ? <Loader2 size={15} className="spin" /> : <PlugZap size={15} />}
          Test Connection
        </button>
      }
    >
      <Row label="Provider">{PROVIDER_NAMES[classifier.provider]}</Row>
      <Row label="Active">Yes</Row>
      {classifier.provider === "jev" ? (
        <>
          <Row label="API key">
            <KeyPill status={key} variable={classifier.jev?.api_key_env ?? DEFAULT_API_KEY_ENV} />
          </Row>
          <Row label="Model">{classifier.model ?? "—"}</Row>
          <Row label="Endpoint">{classifier.jev?.base_url ?? "—"}</Row>
        </>
      ) : (
        <Row label="Classifier route">{classifier.route ?? "—"}</Row>
      )}
      <Row label="Used by rules">
        {classifier.invoked_by.length > 0 ? classifier.invoked_by.join(", ") : "none yet"}
      </Row>
      <Row label="Last check">{lastCheck ? when(lastCheck.checked_at) : "never"}</Row>
      <Row label="Last success">{when(status.last_success_at)}</Row>
      <Row label="Last failure">{when(status.last_failure_at)}</Row>
      <Row label="Last failure kind">
        {status.last_failure_kind ? outcomeLabel(status.last_failure_kind) : "—"}
      </Row>
      {outcomes.length > 0 && (
        <Row label="Outcomes since start">
          {outcomes.map(([kind, count]) => `${outcomeLabel(kind)}: ${count}`).join(" · ")}
        </Row>
      )}

      <div aria-live="polite" style={{ marginTop: 14 }}>
        {failure && (
          <div className="notice notice--danger" role="alert">
            {failure}
          </div>
        )}
        {lastCheck && !failure && <CheckResult report={lastCheck} fresh={result !== null} />}
      </div>
      <p className="card__note">
        Test Connection asks the running router to check the provider it is using now, with
        the settings it loaded at start — not the draft below. For Jev it lists TypeSafe&apos;s
        models with the configured key; no request text is sent, and the key and any error body
        stay on the router.
      </p>
    </Card>
  );
}

function KeyPill({ status, variable }: { status: "configured" | "missing" | "unknown"; variable: string }) {
  if (status === "configured") {
    return (
      <Pill tone="ok" dot>
        Configured ({variable})
      </Pill>
    );
  }
  if (status === "missing") {
    return (
      <Pill tone="danger" dot>
        Missing ({variable})
      </Pill>
    );
  }
  return <Pill tone="neutral">Unknown</Pill>;
}

function CheckResult({ report, fresh }: { report: ClassifierCheckReport; fresh: boolean }) {
  const explained = explainCheck(report);
  // The panel has info, warn and danger notices; success reads as info.
  const notice = explained.tone === "warn" || explained.tone === "danger" ? explained.tone : "info";
  return (
    <div
      className={`notice notice--${notice}`}
      data-check-status={report.status}
      style={{ display: "grid", gap: 6 }}
    >
      <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
        <Pill tone={explained.tone} dot>
          {explained.title}
        </Pill>
        <span className="tnum" style={{ fontSize: 12, color: "var(--text-muted)" }}>
          {fresh ? "just now" : "last check"} · {Math.round(report.duration_ms)} ms
          {report.http_status ? ` · HTTP ${report.http_status}` : ""}
        </span>
      </div>
      <span>{explained.detail}</span>
    </div>
  );
}

// --- the settings draft --------------------------------------------------------------

function Settings({ auto, routes }: { auto: AutoView; routes: RouterRouteView[] }) {
  const logical = useMemo(
    () => routes.filter((route) => isLogicalRoute(route.name)).map((route) => route.name),
    [routes],
  );
  const running = useMemo(() => {
    const descriptions: Record<string, string> = {};
    for (const route of routes) if (route.description) descriptions[route.name] = route.description;
    return descriptions;
  }, [routes]);

  // Seeded once from the running router; later polls never overwrite edits.
  const [draft, setDraft] = useState<ClassifierDraft>(() => draftFrom(auto, routes));
  const [seeded, setSeeded] = useState(routes.length > 0);
  useEffect(() => {
    if (!seeded && routes.length > 0) {
      setDraft(draftFrom(auto, routes));
      setSeeded(true);
    }
  }, [seeded, auto, routes]);

  const problems = useMemo(() => validateDraft(draft, logical), [draft, logical]);
  const valid = !hasProblems(problems);
  const runningProvider = auto.classifier?.provider ?? null;
  const other: ProviderKind = draft.provider === "jev" ? "lightweight" : "jev";

  const update = (patch: Partial<ClassifierDraft>) => setDraft((d) => ({ ...d, ...patch }));
  const updateLightweight = (patch: Partial<LightweightDraft>) =>
    setDraft((d) => ({ ...d, lightweight: { ...d.lightweight, ...patch } }));
  const updateJev = (patch: Partial<JevDraft>) => setDraft((d) => ({ ...d, jev: { ...d.jev, ...patch } }));

  return (
    <>
      <Card
        title="Classifier settings"
        action={
          runningProvider && draft.provider !== runningProvider ? (
            <Pill tone="warn">Draft: switching from {PROVIDER_NAMES[runningProvider]}</Pill>
          ) : (
            <Pill tone="neutral">Draft</Pill>
          )
        }
      >
        <div className="notice notice--info" style={{ marginBottom: 16 }}>
          These settings are not saved to the router from here. The router reads{" "}
          <code>router.json</code> once at start and has no API that writes it, so the panel
          produces the validated configuration below to paste in. Switching provider changes
          only this section; Auto rules keep asking for semantic classification.
        </div>

        <fieldset className="choice-group" aria-label="Classifier Provider" style={{ marginBottom: 18 }}>
          <legend className="field__label" style={{ marginBottom: 8, width: "100%" }}>
            Classifier Provider
          </legend>
          {(["lightweight", "jev"] as const).map((kind) => (
            <label className="choice" key={kind}>
              <input
                type="radio"
                name="classifier-provider"
                value={kind}
                checked={draft.provider === kind}
                onChange={() => update({ provider: kind })}
              />
              {PROVIDER_NAMES[kind]}
              {runningProvider === kind && (
                <span style={{ color: "var(--text-muted)", fontSize: 12 }}>(running)</span>
              )}
            </label>
          ))}
        </fieldset>

        {draft.provider === "lightweight" ? (
          <LightweightPanel
            draft={draft.lightweight}
            problems={problems.lightweight}
            routes={logical}
            onChange={updateLightweight}
          />
        ) : (
          <JevPanel
            draft={draft.jev}
            problems={problems.jev}
            view={auto.classifier ?? null}
            onChange={updateJev}
          />
        )}

        <label
          style={{ display: "flex", gap: 8, alignItems: "center", marginTop: 16, fontSize: 13 }}
        >
          <input
            type="checkbox"
            checked={draft.keep_standby}
            onChange={(e) => update({ keep_standby: e.target.checked })}
          />
          Also keep the {PROVIDER_NAMES[other]} settings in the file as a standby block (never
          asked; lets the router report whether switching would work)
        </label>
      </Card>

      <CandidatesCard
        draft={draft}
        problems={problems}
        routes={logical}
        onChange={update}
      />

      <ConfigurationCard draft={draft} valid={valid} running={running} />
    </>
  );
}

export function Field({
  label,
  help,
  problem,
  children,
}: {
  label: string;
  help?: ReactNode;
  problem?: string | null;
  children: (props: { id: string; "aria-invalid": boolean; "aria-describedby": string }) => ReactNode;
}) {
  const id = useId();
  return (
    <div className="field">
      <label className="field__label" htmlFor={id}>
        {label}
      </label>
      {children({ id, "aria-invalid": Boolean(problem), "aria-describedby": `${id}-help ${id}-error` })}
      {help && (
        <span className="field__help" id={`${id}-help`}>
          {help}
        </span>
      )}
      {problem && (
        <span className="field__error" id={`${id}-error`} role="alert">
          {problem}
        </span>
      )}
    </div>
  );
}

const grid = {
  display: "grid",
  gap: 16,
  gridTemplateColumns: "repeat(auto-fit, minmax(240px, 1fr))",
} as const;

function LimitFields<T extends { timeout_ms: string; min_confidence: string; max_input_chars: string }>({
  draft,
  problems,
  onChange,
  timeoutHelp,
  timeoutAction,
}: {
  draft: T;
  problems: Partial<Record<"timeout_ms" | "min_confidence" | "max_input_chars", string | null>>;
  onChange: (patch: Partial<T>) => void;
  timeoutHelp: ReactNode;
  timeoutAction?: ReactNode;
}) {
  return (
    <>
      <Field label="Timeout (ms) — required" help={timeoutHelp} problem={problems.timeout_ms}>
        {(props) => (
          <div style={{ display: "flex", gap: 8 }}>
            <input
              {...props}
              className="input"
              inputMode="numeric"
              placeholder={`1 – ${MAX_TIMEOUT_MS}`}
              value={draft.timeout_ms}
              onChange={(e) => onChange({ timeout_ms: e.target.value } as Partial<T>)}
            />
            {timeoutAction}
          </div>
        )}
      </Field>
      <Field
        label="Minimum Confidence"
        help={`Classifier results below this threshold use the deterministic fallback route. Default ${DEFAULT_MIN_CONFIDENCE}.`}
        problem={problems.min_confidence}
      >
        {(props) => (
          <input
            {...props}
            className="input"
            inputMode="decimal"
            value={draft.min_confidence}
            onChange={(e) => onChange({ min_confidence: e.target.value } as Partial<T>)}
          />
        )}
      </Field>
      <Field
        label="Maximum Input Characters"
        help={`Bounds the user text supplied to semantic classification: only the last user message, cut to this length. Default ${DEFAULT_MAX_INPUT_CHARS}.`}
        problem={problems.max_input_chars}
      >
        {(props) => (
          <input
            {...props}
            className="input"
            inputMode="numeric"
            value={draft.max_input_chars}
            onChange={(e) => onChange({ max_input_chars: e.target.value } as Partial<T>)}
          />
        )}
      </Field>
    </>
  );
}

function LightweightPanel({
  draft,
  problems,
  routes,
  onChange,
}: {
  draft: LightweightDraft;
  problems: DraftProblems["lightweight"];
  routes: string[];
  onChange: (patch: Partial<LightweightDraft>) => void;
}) {
  return (
    <section aria-label="Lightweight classifier" data-panel="lightweight">
      <h3 className="card__title" style={{ fontSize: 14, margin: "0 0 6px" }}>
        Lightweight Classifier
      </h3>
      <p className="card__note" style={{ marginTop: 0 }}>
        A configured route on your own gateways classifies, through the router&apos;s own
        pipeline. Nothing leaves your machines.
      </p>
      <div style={grid}>
        <Field
          label="Classifier Route"
          help="The logical route whose model answers the classification question. Never Auto."
          problem={problems.route}
        >
          {(props) => (
            <select
              {...props}
              className="select"
              value={draft.route}
              onChange={(e) => onChange({ route: e.target.value })}
            >
              <option value="">Choose a route…</option>
              {routes.map((route) => (
                <option key={route} value={route}>
                  {route}
                </option>
              ))}
            </select>
          )}
        </Field>
        <LimitFields
          draft={draft}
          problems={problems}
          onChange={onChange}
          timeoutHelp="How long one classification may take. There is no default: a small model on a CPU can need tens of seconds."
        />
      </div>
    </section>
  );
}

function JevPanel({
  draft,
  problems,
  view,
  onChange,
}: {
  draft: JevDraft;
  problems: DraftProblems["jev"];
  view: ClassifierView | null;
  onChange: (patch: Partial<JevDraft>) => void;
}) {
  const running = view?.jev ?? null;
  const key: "configured" | "missing" | "unknown" =
    running && running.api_key_env === draft.api_key_env.trim()
      ? running.api_key_configured
        ? "configured"
        : "missing"
      : "unknown";
  const lastCheck = view?.provider === "jev" ? view.status.last_check : null;
  const checkedModel = lastCheck?.model === draft.model.trim() ? lastCheck : null;

  return (
    <section aria-label="Jev / TypeSafe classifier" data-panel="jev">
      <h3 className="card__title" style={{ fontSize: 14, margin: "0 0 10px" }}>
        Jev / TypeSafe Classifier
      </h3>

      <div
        className="notice notice--warn"
        data-privacy-notice
        style={{ marginBottom: 16, display: "flex", gap: 10, alignItems: "flex-start" }}
      >
        <ShieldAlert size={17} style={{ flex: "none", marginTop: 1 }} />
        <span>
          Jev is an external classifier provider. When &quot;Include user text&quot; is enabled,
          bounded user-message content is sent to the configured TypeSafe endpoint for
          classification.
        </span>
      </div>

      <div style={grid}>
        <Field
          label="Base URL"
          help={`Default ${DEFAULT_BASE_URL}. https is required (http only for a loopback address). A proxy path prefix is kept and a trailing slash removed; no credentials, query or fragment.`}
          problem={problems.base_url}
        >
          {(props) => (
            <input
              {...props}
              className="input"
              type="url"
              spellCheck={false}
              value={draft.base_url}
              onChange={(e) => onChange({ base_url: e.target.value })}
            />
          )}
        </Field>

        <Field
          label="API Key Environment Variable"
          help={
            <>
              The key itself is never entered or shown here. Set it where the router runs, then
              restart it: <code>export {draft.api_key_env.trim() || DEFAULT_API_KEY_ENV}=&quot;…&quot;</code>
            </>
          }
          problem={problems.api_key_env}
        >
          {(props) => (
            <input
              {...props}
              className="input"
              spellCheck={false}
              autoComplete="off"
              value={draft.api_key_env}
              onChange={(e) => onChange({ api_key_env: e.target.value })}
            />
          )}
        </Field>

        <div className="field">
          <span className="field__label">API Key Status</span>
          <div data-key-status={key}>
            <KeyPill status={key} variable={draft.api_key_env.trim() || DEFAULT_API_KEY_ENV} />
          </div>
          <span className="field__help">
            {key === "unknown"
              ? "The router has not loaded this variable name; its status appears after the router restarts with it."
              : "Whether the router found a value in this variable when it started. The value is never sent to the panel."}
          </span>
        </div>

        <Field
          label="Model — required"
          help="An alias such as jev-latest, or a pinned version such as jev-1.13.0. Pinned versions can be accepted even when the provider lists only aliases."
          problem={problems.model}
        >
          {(props) => (
            <>
              <input
                {...props}
                className="input"
                list="jev-models"
                spellCheck={false}
                value={draft.model}
                onChange={(e) => onChange({ model: e.target.value })}
              />
              <datalist id="jev-models">
                {[running?.model, "jev-latest"]
                  .filter((model): model is string => Boolean(model))
                  .filter((model, index, all) => all.indexOf(model) === index)
                  .map((model) => (
                    <option key={model} value={model} />
                  ))}
              </datalist>
            </>
          )}
        </Field>

        <div className="field">
          <span className="field__label">Model Discovery</span>
          <div data-model-discovery>
            {checkedModel?.model_listed === true ? (
              <Pill tone="ok" dot>
                Listed by the provider
              </Pill>
            ) : checkedModel?.model_listed === false ? (
              <Pill tone="warn" dot>
                Not listed
              </Pill>
            ) : (
              <Pill tone="neutral">Not checked</Pill>
            )}
          </div>
          <span className="field__help">
            {checkedModel?.model_listed === false
              ? "The configured model was not listed by the provider's model discovery endpoint. Pinned versions may still be accepted."
              : "Test Connection checks whether the running model is listed by GET /v1/models."}
          </span>
        </div>

        <LimitFields
          draft={draft}
          problems={problems}
          onChange={onChange}
          timeoutHelp={`Required, there is no default. ${SUGGESTED_JEV_TIMEOUT_MS} ms is a recommended starting point for a network call, not a guarantee; choose what your network needs.`}
          timeoutAction={
            draft.timeout_ms.trim() === "" ? (
              <button
                type="button"
                className="btn"
                onClick={() => onChange({ timeout_ms: String(SUGGESTED_JEV_TIMEOUT_MS) })}
              >
                Use {SUGGESTED_JEV_TIMEOUT_MS}
              </button>
            ) : undefined
          }
        />
      </div>

      <div
        style={{
          marginTop: 16,
          padding: "12px 14px",
          border: "1px solid var(--rule)",
          borderRadius: "var(--radius)",
          display: "grid",
          gap: 8,
        }}
      >
        <div style={{ display: "flex", gap: 12, alignItems: "center" }}>
          <Switch
            checked={draft.include_user_text}
            onChange={(next) => onChange({ include_user_text: next })}
            label="Include user message text in classifier request"
          />
          <strong style={{ fontSize: 13.5 }}>
            Include user message text in classifier request{" "}
            <span style={{ color: "var(--text-muted)", fontWeight: 400 }}>
              ({draft.include_user_text ? "On" : "Off"})
            </span>
          </strong>
        </div>
        <span className="field__help" data-include-user-text={draft.include_user_text ? "on" : "off"}>
          {draft.include_user_text
            ? "On: the bounded last user message may be sent to the configured TypeSafe endpoint."
            : "Off: Jev receives structural request traits and route descriptions, but no user-message text."}
        </span>
      </div>
    </section>
  );
}

function CandidatesCard({
  draft,
  problems,
  routes,
  onChange,
}: {
  draft: ClassifierDraft;
  problems: DraftProblems;
  routes: string[];
  onChange: (patch: Partial<ClassifierDraft>) => void;
}) {
  function toggle(route: string, on: boolean) {
    const candidates = on
      ? [...draft.candidates, route]
      : draft.candidates.filter((name) => name !== route);
    onChange({ candidates });
  }

  return (
    <Card title="Candidates and fallback">
      <p className="card__note" style={{ marginTop: 0 }}>
        Shared by both providers. Only logical routes can be chosen — never Auto, a node, a
        deployment or a model file. Each route&apos;s description is what the classifier is told
        the route is for.
      </p>

      <fieldset style={{ border: "none", padding: 0, margin: "0 0 16px" }} aria-describedby="candidates-error">
        <legend className="field__label" style={{ marginBottom: 8 }}>
          Classifier Candidates (at most {MAX_CANDIDATES})
        </legend>
        <div style={{ display: "grid", gap: 10 }}>
          {routes.map((route) => {
            const checked = draft.candidates.includes(route);
            const description = draft.descriptions[route] ?? "";
            const problem = problems.descriptions[route] ?? null;
            return (
              <div
                key={route}
                data-candidate={route}
                style={{
                  padding: "10px 12px",
                  border: "1px solid var(--rule)",
                  borderRadius: "var(--radius)",
                  display: "grid",
                  gap: 8,
                }}
              >
                <label style={{ display: "flex", gap: 8, alignItems: "center", fontWeight: 500 }}>
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={(e) => toggle(route, e.target.checked)}
                  />
                  {route}
                </label>
                <Field
                  label={`Description — ${route}`}
                  help={`${[...description.trim()].length}/${MAX_DESCRIPTION_CHARS} characters, one line. Saved on the route itself.`}
                  problem={problem}
                >
                  {(props) => (
                    <textarea
                      {...props}
                      className="input"
                      rows={2}
                      value={description}
                      placeholder="What this route is for, e.g. programming, debugging and code generation."
                      onChange={(e) =>
                        onChange({ descriptions: { ...draft.descriptions, [route]: e.target.value } })
                      }
                    />
                  )}
                </Field>
              </div>
            );
          })}
        </div>
        {problems.candidates && (
          <span className="field__error" id="candidates-error" role="alert">
            {problems.candidates}
          </span>
        )}
      </fieldset>

      <Field
        label="Classifier Fallback Route"
        help="Used whenever classification does not produce a confident candidate: low confidence, timeout, authentication or connection errors, rate limiting, an unavailable classifier route, an invalid answer, or any other provider error. Auto never fails because the classifier did."
        problem={problems.fallback_route}
      >
        {(props) => (
          <select
            {...props}
            className="select"
            value={draft.fallback_route}
            onChange={(e) => onChange({ fallback_route: e.target.value })}
            style={{ maxWidth: 320 }}
          >
            <option value="">Choose a route…</option>
            {routes.map((route) => (
              <option key={route} value={route}>
                {route}
              </option>
            ))}
          </select>
        )}
      </Field>
    </Card>
  );
}

function ConfigurationCard({
  draft,
  valid,
  running,
}: {
  draft: ClassifierDraft;
  valid: boolean;
  running: Record<string, string>;
}) {
  const changes = descriptionChanges(draft, running);
  return (
    <Card title="Configuration to apply">
      {!valid ? (
        <div className="notice notice--warn" role="status">
          Fix the fields marked above to produce a configuration the router will accept.
        </div>
      ) : (
        <div style={{ display: "grid", gap: 14 }} data-config-snippet>
          <p className="card__note" style={{ margin: 0 }}>
            Replace <code>auto_route.classifier</code> in <code>router.json</code> with:
          </p>
          <CodeBlock text={classifierSnippet(draft)} />
          {changes.length > 0 && (
            <>
              <p className="card__note" style={{ margin: 0 }}>
                And set these descriptions on the matching entries in <code>routes</code> (
                <code>null</code> means remove the description):
              </p>
              <CodeBlock text={JSON.stringify(changes, null, 2)} />
            </>
          )}
          <ol className="card__note" style={{ margin: 0, paddingLeft: 18 }}>
            <li>
              Check the file: <code>hermes router validate-config</code>
            </li>
            <li>Restart the router to load it.</li>
            <li>Come back here and press Test Connection.</li>
          </ol>
        </div>
      )}
    </Card>
  );
}

