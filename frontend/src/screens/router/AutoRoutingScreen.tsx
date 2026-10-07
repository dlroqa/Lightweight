import { Link } from "react-router-dom";
import { ArrowRight, Sparkles } from "lucide-react";

import { routerApi, type ApiError } from "../../api/client";
import type { AutoView, RouterRoutesBody } from "../../api/types";
import { Card } from "../../components/Card";
import { Empty, ErrorState, Loading, Pill, Row } from "../../components/Bits";
import { TopBar } from "../../components/Shell";
import { usePoll } from "../../hooks/usePoll";
import { isLogicalRoute, ruleAction } from "./classifierModel";
import { CrossRouteFallbackSection } from "./CrossRouteFallback";

/**
 * The router's `Auto` rules and its logical routes, read-only.
 *
 * Rules change only with `router.json`; this screen says what each one does in
 * an operator's words — in particular, which rules send a request straight to
 * a route and which ask the classifier.
 */
export function AutoRoutingScreen() {
  const auto = usePoll(routerApi.auto, 5000);
  const routes = usePoll(routerApi.routes, 10_000);
  const traces = usePoll(routerApi.traces, 10_000);

  return (
    <>
      <TopBar
        title="Auto Routing"
        subtitle="How the router chooses a route when a client asks for Auto"
      />
      <div className="page" style={{ display: "grid", gap: 18 }}>
        {auto.error ? (
          <RouterError error={auto.error} onRetry={auto.refresh} />
        ) : auto.loading && !auto.data ? (
          <Loading what="Auto routing" />
        ) : auto.data ? (
          <>
            <AutoCard auto={auto.data} />
            <RulesCard auto={auto.data} />
            {auto.data.configured && (
              <CrossRouteFallbackSection
                auto={auto.data}
                routes={routes.data}
                traces={traces.data}
                tracesError={traces.error}
              />
            )}
          </>
        ) : null}
        <RoutesCard routes={routes.data} error={routes.error} loading={routes.loading} />
      </div>
    </>
  );
}

/**
 * A failed read, with the one case the panel cannot fix itself said plainly:
 * a router with a client key wants it on every API request, and the panel
 * does not hold keys.
 */
export function RouterError({ error, onRetry }: { error: ApiError; onRetry: () => void }) {
  if (error.status === 401) {
    return (
      <div className="notice notice--warn" role="alert">
        This router requires its client key on every API request, and the panel does
        not store keys. Read its state with <code>curl -H &quot;Authorization: Bearer …&quot;</code>,
        or serve the panel from a router listening on loopback only, where no key is
        required.
      </div>
    );
  }
  return <ErrorState error={error} onRetry={onRetry} />;
}

function AutoCard({ auto }: { auto: AutoView }) {
  if (!auto.configured) {
    return (
      <Card title="Auto">
        <Empty
          title="Auto routing is not configured"
          hint="Add an auto_route section to router.json to let clients ask for Auto."
        />
      </Card>
    );
  }
  const classifier = auto.classifier ?? null;
  return (
    <Card
      title="Auto"
      action={
        <Pill tone={auto.enabled ? "ok" : "neutral"} dot>
          {auto.enabled ? "Enabled" : "Disabled"}
        </Pill>
      }
    >
      <Row label="Fallback route">{auto.fallback_route ?? "—"}</Row>
      <Row label="Requests that matched no rule">{auto.fallback_decisions ?? 0}</Row>
      <Row label="Semantic classifier">
        {classifier ? (
          <Link to="/classifier" style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
            {classifier.provider === "jev" ? "Jev / TypeSafe" : "Lightweight"}
            <ArrowRight size={13} />
          </Link>
        ) : (
          "Not configured"
        )}
      </Row>
    </Card>
  );
}

function RulesCard({ auto }: { auto: AutoView }) {
  const rules = auto.rules ?? [];
  const provider = auto.classifier?.provider;
  return (
    <Card title="Rules" action={<Pill tone="neutral">Tried in order</Pill>}>
      {rules.length === 0 ? (
        <Empty title="No rules" hint="Every Auto request goes to the fallback route." />
      ) : (
        <div className="scroll-x">
          <table className="table">
            <thead>
              <tr>
                <th>#</th>
                <th>Rule</th>
                <th>When</th>
                <th>Action</th>
                <th style={{ textAlign: "right" }}>Decisions</th>
              </tr>
            </thead>
            <tbody>
              {rules.map((rule) => (
                <tr key={rule.name} data-rule={rule.name}>
                  <td className="tnum">{rule.position}</td>
                  <td>{rule.name}</td>
                  <td style={{ color: "var(--text-muted)" }}>{rule.condition || "Any request"}</td>
                  <td>
                    {rule.classify ? (
                      <Pill tone="accent">
                        <Sparkles size={12} /> {ruleAction(rule)}
                      </Pill>
                    ) : (
                      <span>{ruleAction(rule)}</span>
                    )}
                  </td>
                  <td className="tnum" style={{ textAlign: "right" }}>
                    {rule.decisions}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {rules.some((rule) => rule.classify) && (
        <p className="card__note">
          A semantic classification rule asks the configured classifier
          {provider ? ` (currently ${provider === "jev" ? "Jev / TypeSafe" : "Lightweight"})` : ""} to
          choose among the candidate routes. Switching the classifier provider never changes
          these rules.
        </p>
      )}
    </Card>
  );
}

function RoutesCard({
  routes,
  error,
  loading,
}: {
  routes: RouterRoutesBody | null;
  error: ApiError | null;
  loading: boolean;
}) {
  return (
    <Card title="Logical routes">
      {error ? (
        <p className="card__note">{error.message}</p>
      ) : loading && !routes ? (
        <Loading what="routes" />
      ) : routes && routes.data.length > 0 ? (
        <div className="scroll-x">
          <table className="table">
            <thead>
              <tr>
                <th>Route</th>
                <th>Description</th>
                <th>Strategy</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {routes.data
                .filter((route) => isLogicalRoute(route.name))
                .map((route) => (
                  <tr key={route.name}>
                    <td>
                      {route.name}
                      {routes.default_route === route.name && (
                        <span style={{ color: "var(--text-muted)" }}> · default</span>
                      )}
                    </td>
                    <td style={{ color: "var(--text-muted)" }}>{route.description ?? "—"}</td>
                    <td>{route.strategy}</td>
                    <td>
                      <Pill tone={route.available ? "ok" : "warn"} dot>
                        {route.available ? "Available" : "Unavailable"}
                      </Pill>
                    </td>
                  </tr>
                ))}
            </tbody>
          </table>
        </div>
      ) : (
        <Empty title="No routes" />
      )}
    </Card>
  );
}
