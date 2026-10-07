// The cross-route fallback screen's rules, tested without a browser
// (`npm run test`). Each mirrors a rule the router enforces when it loads
// `router.json` (`crates/lightweight-router/src/fallback.rs`).

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import type {
  AutoView,
  CrossRouteFallbackView,
  RouterRouteView,
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
    assert.equal(reasonLabel("request_budget_exhausted"), "request_budget_exhausted");
  });

  it("counts a refused transition nowhere", () => {
    // The router never counts a transition to a route the budget refused, so
    // the counts the screen reads simply do not contain it.
    assert.deepEqual(transitionRows({ configured: true, counts: {}, exhausted: {} } as unknown as CrossRouteFallbackView), []);
  });
});
