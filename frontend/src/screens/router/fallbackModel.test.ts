// The cross-route fallback screen's rules, tested without a browser
// (`npm run test`). Each mirrors a rule the router enforces when it loads
// `router.json` (`crates/lightweight-router/src/fallback.rs`).

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import type {
  AutoView,
  CrossRouteFallbackView,
  RouterRouteView,
  RoutingTraceView,
  TracesBody,
} from "../../api/types.ts";
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
  findCycle,
  hasChainProblems,
  parseTargets,
  reasonLabel,
  traceVerdict,
  transitionRows,
  validateChains,
  type ChainDraft,
} from "./fallbackModel.ts";

function route(name: string): RouterRouteView {
  return { name, description: null, strategy: "priority", available: true, deployments: [] } as unknown as RouterRouteView;
}

const routes = ["General", "Coder", "Research", "Reasoning", "ToolAgent", "Orphan", "RouterClassifier"].map(route);

const auto = {
  configured: true,
  enabled: true,
  fallback_route: "General",
  rules: [
    { position: 1, name: "forced", when: {}, condition: "", route: "ToolAgent", classify: false, decisions: 0 },
    { position: 2, name: "semantic", when: {}, condition: "", route: "General", classify: true, decisions: 0 },
  ],
  classifier: {
    provider: "lightweight",
    route: "RouterClassifier",
    candidates: [{ route: "General" }, { route: "Coder" }, { route: "Research" }, { route: "Reasoning" }],
    fallback_route: "General",
  },
} as unknown as AutoView;

const ctx = contextFrom(auto, routes);

function rows(...lists: [string, string][]): ChainDraft[] {
  return lists.map(([source, targets], index) => ({ id: index + 1, source, targets }));
}

function problemOf(source: string, targets: string): string {
  return validateChains(rows([source, targets]), ctx).rows[1] ?? "";
}

describe("what the router allows", () => {
  it("reads the routes, which ones Auto reaches, and the classifier's route", () => {
    assert.ok(ctx.routes.includes("General") && ctx.routes.includes("Orphan"));
    assert.deepEqual(new Set(ctx.reachable), new Set(["ToolAgent", "General", "Coder", "Research", "Reasoning"]));
    assert.deepEqual(ctx.classifierRoutes, ["RouterClassifier"]);
  });

  it("states the fixed bound, the three triggers and the exclusions", () => {
    assert.equal(MAX_FALLBACK_ROUTES, 3);
    assert.deepEqual(
      TRIGGERS.map((t) => t.reason),
      ["route_unavailable", "route_exhausted", "route_capability_mismatch"],
    );
    for (const trigger of TRIGGERS) assert.ok(trigger.description.length > 20);
    const text = EXCLUSIONS.join(" ");
    for (const words of ["Explicit route", "500", "Context overflow", "stream"]) {
      assert.ok(text.includes(words), words);
    }
    assert.equal(reasonLabel("route_exhausted"), "Route exhausted");
    assert.equal(reasonLabel("context_length_exceeded"), "Context overflow");
  });
});

describe("drafting lists", () => {
  it("reads the running lists into a draft", () => {
    const view = { chains: { Coder: ["General", "Reasoning"], Research: ["General"] } } as unknown as CrossRouteFallbackView;
    assert.deepEqual(draftFrom(view), [
      { id: 1, source: "Coder", targets: "General, Reasoning" },
      { id: 2, source: "Research", targets: "General" },
    ]);
    assert.deepEqual(draftFrom(null), []);
  });

  it("reads comma-separated names, ignoring blanks", () => {
    assert.deepEqual(parseTargets(" General ,Reasoning,, "), ["General", "Reasoning"]);
    assert.deepEqual(parseTargets(""), []);
  });

  it("accepts valid lists and produces the canonical snippet", () => {
    const draft = rows(["coder", "general, REASONING"], ["Research", "General"]);
    const problems = validateChains(draft, ctx);
    assert.equal(hasChainProblems(problems), false, JSON.stringify(problems));
    assert.equal(
      fallbackSnippet(draft, ctx),
      `"cross_route_fallback": {\n  "Coder": [\n    "General",\n    "Reasoning"\n  ],\n  "Research": [\n    "General"\n  ]\n}`,
    );
  });

  it("accepts the maximum of three and refuses four", () => {
    assert.equal(problemOf("Coder", "General, Research, Reasoning"), "");
    assert.match(problemOf("Coder", "General, Research, Reasoning, ToolAgent"), /At most 3 fallback routes; this list has 4/);
  });
});

describe("what the router refuses", () => {
  const cases: [string, string, RegExp][] = [
    ["Missing", "General", /Initial route "Missing" is not one of the router's routes/],
    ["Coder", "Missing", /Fallback route "Missing" is not one of the router's routes/],
    ["Auto", "General", /Initial route "Auto" is Auto itself/],
    ["Coder", "auto", /Fallback route "auto" is Auto itself/],
    ["Coder", "default", /"default" is reserved/],
    ["Coder", "RouterClassifier", /is the classifier's route/],
    ["RouterClassifier", "General", /is the classifier's route/],
    ["Orphan", "General", /Auto never resolves to "Orphan"/],
    ["", "General", /Choose the initial route/],
    ["Coder", "", /List at least one fallback route/],
    ["Coder", "Coder", /cannot fall back to itself/],
    ["Coder", "coder", /cannot fall back to itself/],
    ["Coder", "General, General", /"General" is listed more than once/],
    ["Coder", "General, GENERAL", /"General" is listed more than once \(names are compared ignoring case\)/],
  ];
  for (const [source, targets, expected] of cases) {
    it(`${JSON.stringify(source)}: ${JSON.stringify(targets)}`, () => {
      assert.match(problemOf(source, targets), expected);
    });
  }

  it("refuses a second list for the same route, ignoring case", () => {
    const problems = validateChains(rows(["Coder", "General"], ["coder", "Research"]), ctx);
    assert.match(problems.rows[2] ?? "", /"Coder" already has a list/);
  });

  it("detects a two-route cycle", () => {
    const problems = validateChains(rows(["Coder", "General"], ["General", "Coder"]), ctx);
    assert.equal(problems.cycle, "Fallback cycle detected: Coder → General → Coder.");
    assert.ok(hasChainProblems(problems));
  });

  it("detects a three-route cycle and names its routes", () => {
    const problems = validateChains(
      rows(["Coder", "Research"], ["Research", "Reasoning"], ["Reasoning", "Coder"]),
      ctx,
    );
    assert.equal(problems.cycle, "Fallback cycle detected: Coder → Research → Reasoning → Coder.");
  });

  it("finds a cycle reached through a later entry, and none in a plain chain", () => {
    assert.match(
      findCycle([["Coder", ["General", "Research"]], ["Research", ["Reasoning"]], ["Reasoning", ["Research"]]]) ?? "",
      /Research → Reasoning → Research/,
    );
    assert.equal(findCycle([["Coder", ["General", "Reasoning"]], ["General", ["Research"]], ["Reasoning", ["Research"]]]), null);
  });
});

describe("what the router reports", () => {
  const view = {
    counts: { Coder: { General: { route_unavailable: 8, route_exhausted: 12, route_capability_mismatch: 3 } } },
    exhausted: { Research: { route_unavailable: 2 } },
  } as unknown as CrossRouteFallbackView;

  it("totals each transition by reason", () => {
    const row = transitionRows(view)[0]!;
    assert.equal(row.from, "Coder");
    assert.equal(row.to, "General");
    assert.equal(row.total, 23);
    assert.deepEqual(new Map(row.reasons).get("route_exhausted"), 12);
    assert.deepEqual(transitionRows(null), []);
  });

  it("totals exhausted lists", () => {
    assert.deepEqual(exhaustedRows(view), [{ route: "Research", total: 2, reasons: [["route_unavailable", 2]] }]);
  });

  it("keeps only traces that fell back, and groups deployment attempts by route", () => {
    const body = {
      object: "list",
      capacity: 200,
      data: [
        { request_id: "a", received_at: 1, route: "General", requested_route: "Coder", outcome: "ok" },
        {
          request_id: "b",
          received_at: 2,
          route: "General",
          requested_route: "Auto",
          outcome: "ok",
          attempts: [
            { route: "Coder", deployment: "coder/A", reason: "priority", outcome: "failed", upstream_status: 503 },
            { route: "Coder", deployment: "coder/B", reason: "failover", outcome: "failed", upstream_status: 503 },
            { route: "General", deployment: "general/G", reason: "priority", outcome: "committed", upstream_status: 200 },
          ],
          cross_route_fallback: {
            initial_route: "Coder",
            final_route: "General",
            exhausted: false,
            attempts: [
              { route: "Coder", outcome: "failed", reason: "route_exhausted" },
              { route: "General", outcome: "committed" },
            ],
          },
        },
      ],
    } as unknown as TracesBody;
    const traces = fallbackTraces(body);
    assert.deepEqual(traces.map((t) => t.request_id), ["b"]);
    assert.deepEqual(deploymentsByRoute(traces[0]!), [
      { route: "Coder", deployments: ["coder/A (failed 503)", "coder/B (failed 503)"] },
      { route: "General", deployments: ["general/G (committed 200)"] },
    ]);
  });
});

// R9.3.2: a request the pre-commit budget ended while on a fallback route. The
// frozen screen predates these values; it must show them as written and never
// fail on them (design test B44).
describe("request-budget values the screen predates", () => {
  it("lists a budget-ended fallback with its raw outcome and reason", () => {
    const body = {
      object: "list",
      data: [
        {
          request_id: "budget",
          route: "General",
          requested_route: "Auto",
          status: 504,
          outcome: "request_budget_exhausted",
          request_budget: {
            configured_ms: 2000,
            elapsed_ms: 2004,
            remaining_ms: 0,
            exhausted: true,
            stage: "cross_route_fallback",
          },
          attempts: [
            { route: "Coder", deployment: "coder/A", reason: "priority", outcome: "failed", upstream_status: 503 },
            { route: "General", deployment: "general/G", reason: "priority", outcome: "request_budget_exhausted" },
          ],
          cross_route_fallback: {
            initial_route: "Coder",
            final_route: "General",
            exhausted: false,
            attempts: [
              { route: "Coder", outcome: "failed", reason: "route_exhausted" },
              { route: "General", outcome: "failed", reason: "request_budget_exhausted" },
            ],
          },
        },
      ],
    } as unknown as TracesBody;
    const traces = fallbackTraces(body);
    assert.deepEqual(traces.map((t) => t.request_id), ["budget"]);
    assert.deepEqual(deploymentsByRoute(traces[0]!), [
      { route: "Coder", deployments: ["coder/A (failed 503)"] },
      { route: "General", deployments: ["general/G (request_budget_exhausted)"] },
    ]);
    // Since the budget wording follow-up the reason reads in words.
    assert.equal(reasonLabel("request_budget_exhausted"), "Budget expired");
  });

  it("counts a refused transition nowhere", () => {
    // The router never counts a transition to a route the budget refused, so
    // the counts the screen reads simply do not contain it.
    assert.deepEqual(transitionRows({ configured: true, counts: {}, exhausted: {} } as unknown as CrossRouteFallbackView), []);
  });
});

// The budget wording follow-up: a request the pre-commit budget (R9.3.2)
// ended must never read as served. Fixtures are the trace shapes the merged
// router writes (crates/lightweight-router/tests/request_budget.rs).
describe("what a trace's terminal state reads as", () => {
  const trace = (fields: Record<string, unknown>) =>
    ({ request_id: "r", received_at: 0, requested_route: "Auto", ...fields }) as unknown as RoutingTraceView;
  const fallback = (final: string, attempts: unknown[], exhausted = false) => ({
    initial_route: "Coder",
    final_route: final,
    exhausted,
    attempts,
  });
  const coderFailed = { route: "Coder", outcome: "failed", reason: "route_exhausted" };

  it("1. a fallback route that answered is served", () => {
    const verdict = traceVerdict(trace({
      route: "General", outcome: "ok", status: 200,
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "committed" }]),
    }));
    assert.deepEqual(verdict, { kind: "served", label: "Served by General" });
  });

  it("2. a budget cut on General reads as an attempt, never as served", () => {
    const verdict = traceVerdict(trace({
      route: "General", outcome: "request_budget_exhausted", status: 504,
      request_budget: { configured_ms: 2000, elapsed_ms: 2004, remaining_ms: 0, exhausted: true, stage: "cross_route_fallback" },
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "failed", reason: "request_budget_exhausted" }]),
    }));
    assert.deepEqual(verdict, { kind: "budget", label: "Budget expired while attempting General" });
    assert.ok(!verdict.label.includes("Served"));
  });

  it("3. a budget spent before General started names General as not attempted", () => {
    const verdict = traceVerdict(trace({
      route: "Coder", outcome: "request_budget_exhausted", status: 504,
      request_budget: { configured_ms: 1000, elapsed_ms: 1102, remaining_ms: 0, exhausted: true,
                        stage: "cross_route_fallback", next_unattempted_route: "General" },
      cross_route_fallback: fallback("Coder", [coderFailed]),
    }));
    assert.deepEqual(verdict, { kind: "budget", label: "Budget expired before attempting General" });
    assert.ok(!verdict.label.includes("Served") && !verdict.label.includes("while attempting General"));
  });

  it("4. classification ended by the budget invents no route", () => {
    const verdict = traceVerdict(trace({
      route: "Auto", outcome: "request_budget_exhausted", status: 504, attempts: [],
      request_budget: { configured_ms: 1000, elapsed_ms: 1001, remaining_ms: 0, exhausted: true, stage: "classifier" },
    }));
    assert.deepEqual(verdict, { kind: "budget", label: "Budget expired during classification" });
  });

  it("5. an explicit Coder request cut by the budget names Coder, with no fallback", () => {
    const verdict = traceVerdict(trace({
      route: "Coder", requested_route: "Coder", outcome: "request_budget_exhausted", status: 504,
      request_budget: { configured_ms: 1000, elapsed_ms: 1003, remaining_ms: 0, exhausted: true, stage: "same_route_attempt" },
    }));
    assert.deepEqual(verdict, { kind: "budget", label: "Budget expired while attempting Coder" });
    assert.ok(!verdict.label.includes("General"));
  });

  it("6. an exhausted list (route_unavailable) reads as before", () => {
    const verdict = traceVerdict(trace({
      route: "General", outcome: "unavailable", status: 503,
      request_budget: { configured_ms: 30000, elapsed_ms: 4, remaining_ms: 29996, exhausted: false },
      cross_route_fallback: fallback("General", [
        { route: "Coder", outcome: "failed", reason: "route_unavailable" },
        { route: "General", outcome: "failed", reason: "route_unavailable" },
      ], true),
    }));
    assert.deepEqual(verdict, { kind: "exhausted", label: "Exhausted" });
  });

  it("7. a route error that completed before the deadline (server_busy) is not budget wording", () => {
    const verdict = traceVerdict(trace({
      route: "General", outcome: "server_error", status: 503,
      request_budget: { configured_ms: 1500, elapsed_ms: 1600, remaining_ms: 0, exhausted: false },
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "failed", reason: "route_exhausted" }], true),
    }));
    assert.equal(verdict.kind, "exhausted");
    assert.ok(!verdict.label.includes("Budget"));
  });

  it("8. a stream that committed before the deadline is served, whatever came after", () => {
    const verdict = traceVerdict(trace({
      route: "General", outcome: "ok", status: 200,
      request_budget: { configured_ms: 1000, elapsed_before_commit_ms: 12, remaining_at_commit_ms: 988, exhausted: false },
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "committed" }]),
    }));
    assert.deepEqual(verdict, { kind: "served", label: "Served by General" });
  });

  it("9. the terminal outcome alone is enough: budget wins over the generic served wording", () => {
    // Defensive: a trace whose outcome says the budget ended it reads so,
    // even if its block were missing or did not say `exhausted`.
    for (const request_budget of [undefined, { configured_ms: 1000, exhausted: false }]) {
      const verdict = traceVerdict(trace({
        route: "General", outcome: "request_budget_exhausted", status: 504, request_budget,
        cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "failed", reason: "request_budget_exhausted" }]),
      }));
      assert.deepEqual(verdict, { kind: "budget", label: "Budget expired while attempting General" });
    }
  });

  it("10/11. a trace with no budget block (no budget, or a v0.6.0 router) reads exactly as before", () => {
    const served = traceVerdict(trace({
      route: "General", outcome: "ok", status: 200,
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "committed" }]),
    }));
    assert.deepEqual(served, { kind: "served", label: "Served by General" });
    const exhausted = traceVerdict(trace({
      route: "General", outcome: "unavailable", status: 503,
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "failed", reason: "route_unavailable" }], true),
    }));
    assert.deepEqual(exhausted, { kind: "exhausted", label: "Exhausted" });
  });

  it("12. Auto stays Auto when no route started", () => {
    const verdict = traceVerdict(trace({
      route: "Auto", outcome: "request_budget_exhausted", status: 504,
      request_budget: { configured_ms: 1000, elapsed_ms: 1100, remaining_ms: 0, exhausted: true, stage: "route_planning" },
    }));
    assert.deepEqual(verdict, { kind: "budget", label: "Budget expired before any route was attempted" });
    const named = traceVerdict(trace({
      route: "Auto", outcome: "request_budget_exhausted", status: 504,
      request_budget: { configured_ms: 1000, exhausted: true, stage: "route_planning", next_unattempted_route: "General" },
    }));
    assert.deepEqual(named, { kind: "budget", label: "Budget expired before attempting General" });
  });

  it("the budget-cut step reads in words, with the code beside it", () => {
    assert.equal(reasonLabel("request_budget_exhausted"), "Budget expired");
    assert.equal(reasonLabel("route_unavailable"), "Route unavailable");
  });
});

describe("a request a context overflow ended", () => {
  const trace = (fields: Record<string, unknown>) =>
    ({ request_id: "r", received_at: 0, requested_route: "Auto", ...fields }) as unknown as RoutingTraceView;
  const fallback = (final: string, attempts: unknown[], exhausted = false) => ({
    initial_route: "Coder",
    final_route: final,
    exhausted,
    attempts,
  });
  const coderFailed = { route: "Coder", outcome: "failed", reason: "route_exhausted" };
  const coderDeployment = { route: "Coder", deployment: "coder/Coder", reason: "priority", outcome: "failed", upstream_status: 503 };
  const generalOverflow = { route: "General", deployment: "general/General", reason: "priority", outcome: "context_overflow", upstream_status: 400 };
  // Exactly what the router records for Coder (503) → General (400 context_length_exceeded).
  const overflowOnGeneral = (fields: Record<string, unknown> = {}) => trace({
    route: "General", outcome: "client_error", status: 400,
    attempts: [coderDeployment, generalOverflow],
    cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "failed", reason: "context_length_exceeded" }]),
    ...fields,
  });

  it("Coder → General, General overflows: names General as attempted, never served, never exhausted", () => {
    const verdict = traceVerdict(overflowOnGeneral());
    assert.deepEqual(verdict, { kind: "context_overflow", label: "Context limit exceeded while attempting General" });
    assert.ok(!verdict.label.includes("Served") && !verdict.label.includes("Exhausted"));
    assert.ok(!verdict.label.includes("Coder") && !verdict.label.includes("Auto") && !verdict.label.includes("Budget"));
  });

  it("an Auto request that overflowed on its first route reads the same, with no fallback invented", () => {
    const verdict = traceVerdict(trace({
      route: "General", outcome: "client_error", status: 400, attempts: [generalOverflow],
    }));
    assert.deepEqual(verdict, { kind: "context_overflow", label: "Context limit exceeded while attempting General" });
  });

  it("an explicit General request that overflowed names General", () => {
    const verdict = traceVerdict(trace({
      route: "General", requested_route: "General", outcome: "client_error", status: 400, attempts: [generalOverflow],
    }));
    assert.deepEqual(verdict, { kind: "context_overflow", label: "Context limit exceeded while attempting General" });
  });

  it("another client error is never read as a context overflow", () => {
    const committed400 = { route: "General", deployment: "general/General", reason: "priority", outcome: "committed", upstream_status: 400 };
    const withFallback = traceVerdict(trace({
      route: "General", outcome: "client_error", status: 400, attempts: [coderDeployment, committed400],
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "committed" }]),
    }));
    assert.notEqual(withFallback.kind, "context_overflow");
    assert.ok(!withFallback.label.includes("Context"));
    const explicit = traceVerdict(trace({
      route: "General", requested_route: "General", outcome: "client_error", status: 400, attempts: [committed400],
    }));
    assert.notEqual(explicit.kind, "context_overflow");
  });

  it("an overflow a larger deployment then answered is served", () => {
    const verdict = traceVerdict(trace({
      route: "General", outcome: "ok", status: 200,
      attempts: [generalOverflow, { route: "General", deployment: "general/Large", reason: "context_overflow_failover", outcome: "committed", upstream_status: 200 }],
    }));
    assert.deepEqual(verdict, { kind: "served", label: "Served by General" });
  });

  it("the request budget still wins, even over a defensive overflow-shaped trace", () => {
    const verdict = traceVerdict(overflowOnGeneral({
      outcome: "request_budget_exhausted", status: 504,
      request_budget: { configured_ms: 1500, elapsed_ms: 1600, remaining_ms: 0, exhausted: true, stage: "cross_route_fallback" },
    }));
    assert.deepEqual(verdict, { kind: "budget", label: "Budget expired while attempting General" });
  });

  it("a request budget that did not end it changes nothing", () => {
    const verdict = traceVerdict(overflowOnGeneral({
      request_budget: { configured_ms: 30000, elapsed_before_commit_ms: 9, remaining_at_commit_ms: 29991, exhausted: false },
    }));
    assert.equal(verdict.kind, "context_overflow");
  });

  it("success, an exhausted list and a committed stream read exactly as before", () => {
    assert.deepEqual(traceVerdict(trace({
      route: "General", outcome: "ok", status: 200, stream: true,
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "committed" }]),
    })), { kind: "served", label: "Served by General" });
    assert.deepEqual(traceVerdict(trace({
      route: "General", outcome: "unavailable", status: 503,
      cross_route_fallback: fallback("General", [coderFailed, { route: "General", outcome: "failed", reason: "route_unavailable" }], true),
    })), { kind: "exhausted", label: "Exhausted" });
  });

  it("a context-overflow step still reads Context overflow, with the code beside it", () => {
    assert.equal(reasonLabel("context_length_exceeded"), "Context overflow");
  });
});
