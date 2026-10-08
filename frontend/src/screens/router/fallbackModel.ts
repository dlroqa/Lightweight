/**
 * The cross-route fallback screen's rules (R9.3.1), kept free of React so they
 * can be tested on their own — as `classifierModel.ts` is for the classifier.
 *
 * The router is authoritative: it refuses a bad `router.json` when it loads.
 * These checks mirror its rules so an operator sees a mistake while drafting,
 * not after a restart; `hermes router validate-config` is still the last word.
 */

import type {
  AutoView,
  CrossRouteFallbackView,
  RouterRouteView,
  RoutingTraceView,
  TracesBody,
} from "../../api/types";
import { isLogicalRoute } from "./classifierModel.ts";

/** The most fallback routes one list may name. Fixed by the router. */
export const MAX_FALLBACK_ROUTES = 3;

/** The three reasons that can move a request to the next listed route. */
export const TRIGGERS: { reason: string; label: string; description: string }[] = [
  {
    reason: "route_unavailable",
    label: "Route unavailable",
    description: "No usable deployment is available for the logical route.",
  },
  {
    reason: "route_exhausted",
    label: "Route exhausted",
    description:
      "Every deployment of the route was tried, and the route ended on a 502/503/504 refusal sent before any answer.",
  },
  {
    reason: "route_capability_mismatch",
    label: "Capability mismatch",
    description: "The logical route cannot satisfy the original request's requirements.",
  },
];

/** What never moves a request to another route. */
export const EXCLUSIONS = [
  "Explicit route requests (a client that names a route gets that route or its error)",
  "A 500 or any other answer the route committed",
  "Context overflow (context_length_exceeded)",
  "A stream that fails after it started",
];

/** The trace outcome and attempt reason the pre-commit request budget (R9.3.2) ends a request with. */
export const BUDGET_EXHAUSTED = "request_budget_exhausted";

/** The node's structured code for a prompt longer than its context. */
export const CONTEXT_OVERFLOW = "context_length_exceeded";

export function reasonLabel(reason: string | undefined | null): string {
  if (!reason) return "";
  return TRIGGERS.find((t) => t.reason === reason)?.label
    ?? (reason === CONTEXT_OVERFLOW
      ? "Context overflow"
      : reason === BUDGET_EXHAUSTED
        ? "Budget expired"
        : reason);
}

/** How a request that changed route ended, in the card's words. */
export interface TraceVerdict {
  kind: "budget" | "context_overflow" | "exhausted" | "served" | "unsuccessful";
  label: string;
}

/**
 * The most specific terminal state of a trace, in this order:
 *
 * 1. The pre-commit request budget ended it (`outcome:
 *    "request_budget_exhausted"`, or its `request_budget` block says so). The
 *    client got a 504 before any response started, so nothing was served —
 *    even though R9.3's own block keeps `exhausted: false` (time ran out, not
 *    the list). Named after what the trace actually holds: the route a start
 *    check refused (`next_unattempted_route`, never attempted), the
 *    classification, or the route whose attempt was cut. No route is invented.
 * 2. The last route attempted refused the prompt as longer than its context
 *    (`context_length_exceeded`), and no larger deployment was left: the
 *    client got that route's own `400`. R9.3's block keeps `exhausted: false`
 *    for it (the chain stopped, the list did not run out), so it is read from
 *    the structured reason the router recorded, never from `client_error`.
 * 3. The fallback list ran out: the last route's own error.
 * 4. The request succeeded (`outcome: "ok"`: a 2xx/3xx answer, or a stream
 *    that ran to its end). Only this reads "Served by"; `exhausted: false`
 *    is not success, and neither is a final route or an attempt.
 * 5. Anything else (another client or server error, a stream the node broke
 *    off, a client that left, an unknown outcome) is named neutrally, with
 *    no cause guessed.
 *
 * A trace without a `request_budget` block (no budget configured, or a router
 * from before R9.3.2) reads exactly as before.
 */
export function traceVerdict(trace: RoutingTraceView): TraceVerdict {
  const budget = trace.request_budget;
  const block = trace.cross_route_fallback;
  if (trace.outcome === BUDGET_EXHAUSTED || budget?.exhausted === true) {
    if (budget?.next_unattempted_route) {
      return { kind: "budget", label: `Budget expired before attempting ${budget.next_unattempted_route}` };
    }
    if (budget?.stage === "classifier") {
      return { kind: "budget", label: "Budget expired during classification" };
    }
    const route = block?.final_route ?? trace.route;
    return route && route !== AUTO
      ? { kind: "budget", label: `Budget expired while attempting ${route}` }
      : { kind: "budget", label: "Budget expired before any route was attempted" };
  }
  const overflowed = contextOverflowRoute(trace);
  if (overflowed) {
    return { kind: "context_overflow", label: `Context limit exceeded while attempting ${overflowed}` };
  }
  if (block?.exhausted) return { kind: "exhausted", label: "Exhausted" };
  const last = block?.final_route ?? trace.route;
  if (trace.outcome === SUCCESS) return { kind: "served", label: `Served by ${last}` };
  return last && last !== AUTO
    ? { kind: "unsuccessful", label: `Request ended while attempting ${last}` }
    : { kind: "unsuccessful", label: "Request ended without a successful response" };
}

/** The one trace outcome that means a response was served (`Outcome::Ok`). */
const SUCCESS = "ok";

/**
 * The route whose context overflow ended the request, if one did. With a
 * fallback block, its last route attempt says so (`failed` /
 * `context_length_exceeded`). Without one (an `Auto` request that never moved,
 * or an explicit route), the request's last deployment attempt was refused as
 * `context_overflow` and the client got that `client_error`.
 */
function contextOverflowRoute(trace: RoutingTraceView): string | undefined {
  const block = trace.cross_route_fallback;
  if (block) {
    const last = block.attempts.at(-1);
    return last?.outcome === "failed" && last.reason === CONTEXT_OVERFLOW ? last.route : undefined;
  }
  const last = trace.attempts?.at(-1);
  return trace.outcome === "client_error" && last?.outcome === "context_overflow"
    ? last.route ?? trace.route
    : undefined;
}

/** `Auto`'s fixed label: never a route that was attempted. */
const AUTO = "Auto";

/** One list being drafted: an initial route and its fallback routes, as typed. */
export interface ChainDraft {
  id: number;
  source: string;
  /** Comma-separated route names, in order. */
  targets: string;
}

/** What the drafted names are checked against, read from the running router. */
export interface FallbackContext {
  /** Every configured logical route, as the router spells it. */
  routes: string[];
  /** Routes `Auto` can resolve to: only these may have a list. */
  reachable: string[];
  /** The Lightweight classifier's route(s): never a source or a target. */
  classifierRoutes: string[];
}

function same(a: string, b: string): boolean {
  return a.trim().toLowerCase() === b.trim().toLowerCase();
}

/** The routes and roles the running router reports. */
export function contextFrom(auto: AutoView | null, routes: RouterRouteView[]): FallbackContext {
  const names = routes.map((route) => route.name).filter(isLogicalRoute);
  const reachable = new Set<string>();
  for (const rule of auto?.rules ?? []) reachable.add(rule.route);
  if (auto?.fallback_route) reachable.add(auto.fallback_route);
  const classifier = auto?.classifier ?? null;
  for (const candidate of classifier?.candidates ?? []) reachable.add(candidate.route);
  if (classifier?.fallback_route) reachable.add(classifier.fallback_route);
  const classifierRoutes = [classifier?.route, classifier?.lightweight?.route].filter(
    (route): route is string => typeof route === "string" && route.length > 0,
  );
  return { routes: names, reachable: [...reachable], classifierRoutes };
}

/** The running configuration as a draft, one row per list. */
export function draftFrom(view: CrossRouteFallbackView | null | undefined): ChainDraft[] {
  return Object.entries(view?.chains ?? {}).map(([source, targets], index) => ({
    id: index + 1,
    source,
    targets: targets.join(", "),
  }));
}

/** `" General ,Reasoning,, "` → `["General", "Reasoning"]`. */
export function parseTargets(text: string): string[] {
  return text
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name.length > 0);
}

export type Problem = string | null;

export interface ChainProblems {
  /** By draft row id: the first problem with that row. */
  rows: Record<number, Problem>;
  /** A cycle across the lists, naming its routes. */
  cycle: Problem;
}

/** The configured route `name` refers to, or why it refers to none. */
function concrete(name: string, ctx: FallbackContext): { route?: string; problem?: string } {
  const trimmed = name.trim();
  if (!trimmed) return { problem: "is empty" };
  if (same(trimmed, "auto")) {
    return { problem: `"${trimmed}" is Auto itself; fallback works between concrete routes` };
  }
  if (same(trimmed, "default")) return { problem: `"${trimmed}" is reserved; name a configured route` };
  const route = ctx.routes.find((candidate) => same(candidate, trimmed));
  if (!route) return { problem: `"${trimmed}" is not one of the router's routes` };
  if (ctx.classifierRoutes.some((c) => same(c, route))) {
    return { problem: `"${route}" is the classifier's route, not a route that answers clients` };
  }
  return { route };
}

/** Check every drafted list, as the router will when it loads the file. */
export function validateChains(rows: ChainDraft[], ctx: FallbackContext): ChainProblems {
  const problems: Record<number, Problem> = {};
  const resolved: [string, string[]][] = [];
  const seen: string[] = [];
  for (const row of rows) {
    const fail = (problem: string) => {
      problems[row.id] = problem;
    };
    const source = concrete(row.source, ctx);
    if (!row.source.trim()) {
      fail("Choose the initial route.");
      continue;
    }
    if (source.problem || !source.route) {
      fail(`Initial route ${source.problem}.`);
      continue;
    }
    if (!ctx.reachable.some((r) => same(r, source.route!))) {
      fail(`Auto never resolves to "${source.route}", so this list would never be used.`);
      continue;
    }
    if (seen.some((s) => same(s, source.route!))) {
      fail(`"${source.route}" already has a list (names are compared ignoring case).`);
      continue;
    }
    seen.push(source.route);
    const names = parseTargets(row.targets);
    if (names.length === 0) {
      fail("List at least one fallback route.");
      continue;
    }
    if (names.length > MAX_FALLBACK_ROUTES) {
      fail(`At most ${MAX_FALLBACK_ROUTES} fallback routes; this list has ${names.length}.`);
      continue;
    }
    const targets: string[] = [];
    let ok = true;
    for (const name of names) {
      const target = concrete(name, ctx);
      if (target.problem || !target.route) {
        fail(`Fallback route ${target.problem}.`);
        ok = false;
        break;
      }
      if (same(target.route, source.route)) {
        fail("A route cannot fall back to itself.");
        ok = false;
        break;
      }
      if (targets.some((t) => same(t, target.route!))) {
        fail(`"${target.route}" is listed more than once (names are compared ignoring case).`);
        ok = false;
        break;
      }
      targets.push(target.route);
    }
    if (ok) resolved.push([source.route, targets]);
  }
  return { rows: problems, cycle: findCycle(resolved) };
}

/**
 * A cycle across every list (each `route → entry` is an edge), as
 * "Fallback cycle detected: A → B → A". Requests never follow this graph —
 * only the initial route's list is used — but the router refuses a cycle.
 */
export function findCycle(chains: [string, string[]][]): Problem {
  const edges = new Map(chains.map(([source, targets]) => [source, targets]));
  const done = new Set<string>();
  const visit = (node: string, path: string[]): string[] | null => {
    if (path.includes(node)) return [...path.slice(path.indexOf(node)), node];
    if (done.has(node)) return null;
    for (const next of edges.get(node) ?? []) {
      const cycle = visit(next, [...path, node]);
      if (cycle) return cycle;
    }
    done.add(node);
    return null;
  };
  for (const [source] of chains) {
    const cycle = visit(source, []);
    if (cycle) return `Fallback cycle detected: ${cycle.join(" → ")}.`;
  }
  return null;
}

export function hasChainProblems(problems: ChainProblems): boolean {
  return problems.cycle !== null || Object.values(problems.rows).some((p) => p !== null);
}

/** The section, spelled as the router spells its routes. */
export function fallbackSection(rows: ChainDraft[], ctx: FallbackContext): Record<string, string[]> {
  const section: Record<string, string[]> = {};
  for (const row of rows) {
    const source = concrete(row.source, ctx).route;
    if (!source) continue;
    section[source] = parseTargets(row.targets)
      .map((name) => concrete(name, ctx).route)
      .filter((route): route is string => !!route);
  }
  return section;
}

/** `"cross_route_fallback": { … }`, ready to paste into `auto_route`. */
export function fallbackSnippet(rows: ChainDraft[], ctx: FallbackContext): string {
  const body = JSON.stringify(fallbackSection(rows, ctx), null, 2);
  return `"cross_route_fallback": ${body}`;
}

/** One `from → to` transition and how often each reason took it. */
export interface TransitionRow {
  from: string;
  to: string;
  total: number;
  reasons: [string, number][];
}

export function transitionRows(view: CrossRouteFallbackView | null | undefined): TransitionRow[] {
  const rows: TransitionRow[] = [];
  for (const [from, targets] of Object.entries(view?.counts ?? {})) {
    for (const [to, reasons] of Object.entries(targets)) {
      const entries = Object.entries(reasons);
      rows.push({
        from,
        to,
        total: entries.reduce((sum, [, n]) => sum + n, 0),
        reasons: entries,
      });
    }
  }
  return rows;
}

/** Initial routes whose whole list failed, and the last reason. */
export function exhaustedRows(
  view: CrossRouteFallbackView | null | undefined,
): { route: string; total: number; reasons: [string, number][] }[] {
  return Object.entries(view?.exhausted ?? {}).map(([route, reasons]) => {
    const entries = Object.entries(reasons);
    return { route, total: entries.reduce((sum, [, n]) => sum + n, 0), reasons: entries };
  });
}

/** Recent requests that moved to a fallback route, newest first. */
export function fallbackTraces(body: TracesBody | null | undefined): RoutingTraceView[] {
  return (body?.data ?? []).filter((trace) => !!trace.cross_route_fallback);
}

/**
 * A trace's deployment attempts grouped by logical route, in order: same-route
 * failover (several deployments under one route) stays distinct from the
 * cross-route steps between routes.
 */
export function deploymentsByRoute(trace: RoutingTraceView): { route: string; deployments: string[] }[] {
  const groups: { route: string; deployments: string[] }[] = [];
  for (const attempt of trace.attempts ?? []) {
    const route = attempt.route ?? trace.route;
    const last = groups[groups.length - 1];
    const label = `${attempt.deployment} (${attempt.outcome}${attempt.upstream_status ? ` ${attempt.upstream_status}` : ""})`;
    if (last && last.route === route) last.deployments.push(label);
    else groups.push({ route, deployments: [label] });
  }
  return groups;
}
