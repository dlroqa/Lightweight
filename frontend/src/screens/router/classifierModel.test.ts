/**
 * The classifier settings' rules, against the router's own (see
 * `crates/lightweight-router/src/classifier/`). Run with `npm run test`.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import type { AutoView, ClassifierCheckReport, RouterRouteView } from "../../api/types.ts";
import {
  DEFAULT_API_KEY_ENV,
  DEFAULT_BASE_URL,
  type ClassifierDraft,
  checkBaseUrl,
  checkConfidence,
  checkDescription,
  checkEnvName,
  checkMaxInput,
  checkModel,
  checkTimeout,
  classifierSection,
  classifierSnippet,
  descriptionChanges,
  draftFrom,
  explainCheck,
  hasProblems,
  isLogicalRoute,
  keyStatus,
  normalizeBaseUrl,
  outcomeLabel,
  ruleAction,
  validateDraft,
} from "./classifierModel.ts";

const ROUTES: RouterRouteView[] = ["General", "Coder", "Research", "Reasoning"].map((name) => ({
  name,
  description: name === "Coder" ? "Programming and debugging" : null,
  strategy: "priority",
  available: true,
  deployments: [],
}));
const NAMES = ROUTES.map((route) => route.name);

/** What `GET /api/router/v1/auto` says for a router running Jev. */
function jevAuto(overrides: Partial<NonNullable<AutoView["classifier"]>> = {}): AutoView {
  return {
    configured: true,
    enabled: true,
    fallback_route: "General",
    rules: [],
    classifier: {
      provider: "jev",
      route: null,
      model: "jev-1.13.0",
      candidates: [
        { route: "General", description: "Everyday conversation" },
        { route: "Coder", description: "Programming and debugging" },
      ],
      fallback_route: "General",
      min_confidence: 0.7,
      timeout_ms: 5000,
      max_input_chars: 1500,
      invoked_by: ["semantic"],
      outcomes: {},
      status: { last_success_at: null, last_failure_at: null, last_failure_kind: null, last_check: null },
      jev: {
        base_url: "https://api.typesafe.ai",
        model: "jev-1.13.0",
        api_key_env: "TYPESAFE_API_KEY",
        api_key_configured: true,
        timeout_ms: 5000,
        min_confidence: 0.7,
        max_input_chars: 1500,
        include_user_text: false,
        active: true,
      },
      ...overrides,
    },
  };
}

function validDraft(provider: "lightweight" | "jev"): ClassifierDraft {
  const draft = draftFrom(jevAuto(), ROUTES);
  draft.provider = provider;
  draft.lightweight = { route: "Reasoning", timeout_ms: "30000", min_confidence: "0.65", max_input_chars: "2000" };
  return draft;
}

describe("reading the running router", () => {
  it("reads a Jev configuration without inventing anything", () => {
    const draft = draftFrom(jevAuto(), ROUTES);
    assert.equal(draft.provider, "jev");
    assert.deepEqual(draft.candidates, ["General", "Coder"]);
    assert.equal(draft.fallback_route, "General");
    assert.equal(draft.jev.model, "jev-1.13.0");
    assert.equal(draft.jev.timeout_ms, "5000");
    assert.equal(draft.jev.include_user_text, false);
    assert.equal(draft.descriptions.General, "Everyday conversation");
    // No lightweight block was running: it starts empty, with no timeout.
    assert.equal(draft.lightweight.timeout_ms, "");
    assert.equal(draft.keep_standby, false);
  });

  it("starts a router with no classifier from the router's defaults and no timeout", () => {
    const draft = draftFrom({ configured: true, enabled: true, fallback_route: "General" }, ROUTES);
    assert.equal(draft.provider, "lightweight");
    assert.equal(draft.jev.base_url, DEFAULT_BASE_URL);
    assert.equal(draft.jev.api_key_env, DEFAULT_API_KEY_ENV);
    assert.equal(draft.jev.min_confidence, "0.65");
    assert.equal(draft.jev.max_input_chars, "2000");
    assert.equal(draft.jev.include_user_text, true);
    // Required, so never silently filled.
    assert.equal(draft.jev.timeout_ms, "");
    assert.equal(draft.jev.model, "");
  });

  it("reads an R9.1 Lightweight classifier (reported in the canonical shape)", () => {
    const auto: AutoView = {
      configured: true,
      enabled: true,
      fallback_route: "General",
      classifier: {
        ...jevAuto().classifier!,
        provider: "lightweight",
        route: "Reasoning",
        model: null,
        jev: undefined,
        lightweight: { route: "Reasoning", timeout_ms: 30000, min_confidence: 0.6, max_input_chars: 2000, active: true },
      },
    };
    const draft = draftFrom(auto, ROUTES);
    assert.equal(draft.provider, "lightweight");
    assert.equal(draft.lightweight.route, "Reasoning");
    assert.equal(draft.lightweight.timeout_ms, "30000");
    assert.equal(draft.lightweight.min_confidence, "0.6");
  });

  it("never offers Auto or default as a classifier choice", () => {
    assert.equal(isLogicalRoute("Auto"), false);
    assert.equal(isLogicalRoute("auto"), false);
    assert.equal(isLogicalRoute("default"), false);
    assert.equal(isLogicalRoute("Coder"), true);
  });
});

describe("field rules", () => {
  it("timeout: required, whole milliseconds, 1 to 120000", () => {
    assert.match(checkTimeout("") ?? "", /Required/);
    assert.notEqual(checkTimeout("0"), null);
    assert.notEqual(checkTimeout("120001"), null);
    assert.notEqual(checkTimeout("1.5"), null);
    assert.notEqual(checkTimeout("-5"), null);
    assert.equal(checkTimeout("1"), null);
    assert.equal(checkTimeout("120000"), null);
    assert.equal(checkTimeout("5000"), null);
  });

  it("minimum confidence: a finite number from 0 to 1", () => {
    assert.equal(checkConfidence("0"), null);
    assert.equal(checkConfidence("0.65"), null);
    assert.equal(checkConfidence("1"), null);
    assert.notEqual(checkConfidence("1.01"), null);
    assert.notEqual(checkConfidence("-0.1"), null);
    assert.notEqual(checkConfidence("NaN"), null);
    assert.notEqual(checkConfidence("Infinity"), null);
    assert.notEqual(checkConfidence(""), null);
  });

  it("maximum input: whole characters, 1 to 32000", () => {
    assert.equal(checkMaxInput("2000"), null);
    assert.equal(checkMaxInput("32000"), null);
    assert.notEqual(checkMaxInput("0"), null);
    assert.notEqual(checkMaxInput("32001"), null);
    assert.notEqual(checkMaxInput("10.5"), null);
  });

  it("base URL: https, http only for loopback, no credentials, query or fragment", () => {
    assert.equal(checkBaseUrl("https://api.typesafe.ai"), null);
    assert.equal(checkBaseUrl("https://proxy.example/typesafe/"), null);
    assert.equal(checkBaseUrl("http://127.0.0.1:8080"), null);
    assert.equal(checkBaseUrl("http://localhost:8080"), null);
    assert.equal(checkBaseUrl("http://[::1]:8080"), null);
    assert.match(checkBaseUrl("http://api.typesafe.ai") ?? "", /https/);
    assert.match(checkBaseUrl("https://user:pass@api.typesafe.ai") ?? "", /credentials/);
    assert.match(checkBaseUrl("https://api.typesafe.ai?x=1") ?? "", /query/);
    assert.match(checkBaseUrl("https://api.typesafe.ai#top") ?? "", /fragment/);
    assert.notEqual(checkBaseUrl("ftp://api.typesafe.ai"), null);
    assert.notEqual(checkBaseUrl("not a url"), null);
  });

  it("base URL: a trailing slash is removed and a proxy prefix kept", () => {
    assert.equal(normalizeBaseUrl("https://api.typesafe.ai/"), "https://api.typesafe.ai");
    assert.equal(normalizeBaseUrl(" https://proxy.example/typesafe/ "), "https://proxy.example/typesafe");
  });

  it("API key environment variable: a usable name, never a value", () => {
    assert.equal(checkEnvName("TYPESAFE_API_KEY"), null);
    assert.equal(checkEnvName("_KEY2"), null);
    assert.notEqual(checkEnvName("2KEY"), null);
    assert.notEqual(checkEnvName("MY-KEY"), null);
    assert.notEqual(checkEnvName("sk live key"), null);
    assert.notEqual(checkEnvName(""), null);
  });

  it("model: required, any alias or pinned version, bounded", () => {
    assert.equal(checkModel("jev-latest"), null);
    assert.equal(checkModel("jev-1.13.0"), null);
    assert.equal(checkModel("some-future-model"), null);
    assert.notEqual(checkModel(""), null);
    assert.notEqual(checkModel("   "), null);
    assert.notEqual(checkModel("x".repeat(129)), null);
    assert.notEqual(checkModel(`jev${String.fromCharCode(10)}latest`), null);
  });

  it("route description: optional, at most 200 characters, one line", () => {
    assert.equal(checkDescription(""), null);
    assert.equal(checkDescription("Programming, debugging, architecture."), null);
    assert.equal(checkDescription("x".repeat(200)), null);
    assert.notEqual(checkDescription("x".repeat(201)), null);
    assert.notEqual(checkDescription(`two${String.fromCharCode(10)}lines`), null);
  });
});

describe("the draft as a whole", () => {
  it("a complete Jev draft is valid", () => {
    assert.equal(hasProblems(validateDraft(validDraft("jev"), NAMES)), false);
  });

  it("a Jev draft without a timeout or model is refused", () => {
    const draft = validDraft("jev");
    draft.jev.timeout_ms = "";
    draft.jev.model = "";
    const problems = validateDraft(draft, NAMES);
    assert.notEqual(problems.jev.timeout_ms, null);
    assert.notEqual(problems.jev.model, null);
  });

  it("candidates must be configured logical routes, at least one", () => {
    const draft = validDraft("jev");
    draft.candidates = [];
    assert.notEqual(validateDraft(draft, NAMES).candidates, null);
    draft.candidates = ["Auto"];
    assert.notEqual(validateDraft(draft, NAMES).candidates, null);
    draft.candidates = ["Nowhere"];
    assert.notEqual(validateDraft(draft, NAMES).candidates, null);
    draft.candidates = ["Coder", "Research"];
    assert.equal(validateDraft(draft, NAMES).candidates, null);
  });

  it("the fallback route must be a configured logical route", () => {
    const draft = validDraft("jev");
    draft.fallback_route = "Auto";
    assert.notEqual(validateDraft(draft, NAMES).fallback_route, null);
    draft.fallback_route = "";
    assert.notEqual(validateDraft(draft, NAMES).fallback_route, null);
    draft.fallback_route = "Research";
    assert.equal(validateDraft(draft, NAMES).fallback_route, null);
  });

  it("the unselected provider is checked only when kept as standby", () => {
    const draft = validDraft("jev");
    draft.lightweight.timeout_ms = "";
    assert.equal(hasProblems(validateDraft(draft, NAMES)), false);
    draft.keep_standby = true;
    assert.notEqual(validateDraft(draft, NAMES).lightweight.timeout_ms, null);
  });
});

describe("the configuration it produces", () => {
  it("writes the canonical shape only, never the R9.1 shorthand", () => {
    const section = classifierSection(validDraft("lightweight"));
    assert.deepEqual(Object.keys(section).sort(), ["fallback_route", "lightweight", "provider", "routes"]);
    for (const flat of ["route", "timeout_ms", "min_confidence", "max_input_chars"]) {
      assert.equal(flat in section, false, flat);
    }
    assert.deepEqual(section.lightweight, {
      route: "Reasoning",
      timeout_ms: 30000,
      min_confidence: 0.65,
      max_input_chars: 2000,
    });
  });

  it("switching provider changes only the classifier section", () => {
    const jev = classifierSection(validDraft("jev"));
    assert.equal(jev.provider, "jev");
    assert.deepEqual(jev.routes, ["General", "Coder"]);
    assert.equal(jev.fallback_route, "General");
    assert.deepEqual(jev.jev, {
      base_url: "https://api.typesafe.ai",
      api_key_env: "TYPESAFE_API_KEY",
      model: "jev-1.13.0",
      timeout_ms: 5000,
      min_confidence: 0.7,
      max_input_chars: 1500,
      include_user_text: false,
    });
    assert.equal("lightweight" in jev, false);
  });

  it("keeps the other provider as a standby block when asked", () => {
    const draft = validDraft("jev");
    draft.keep_standby = true;
    const section = classifierSection(draft);
    assert.ok(section.jev);
    assert.ok(section.lightweight);
  });

  it("never carries a key: only the variable's name", () => {
    const snippet = classifierSnippet(validDraft("jev"));
    assert.match(snippet, /"api_key_env": "TYPESAFE_API_KEY"/);
    assert.doesNotMatch(snippet, /"api_key"\s*:/);
    assert.doesNotMatch(snippet, /bearer|authorization/i);
  });

  it("lists only the route descriptions that changed", () => {
    const draft = validDraft("jev");
    const running = { General: "Everyday conversation", Coder: "Programming and debugging" };
    assert.deepEqual(descriptionChanges(draft, running), []);
    draft.descriptions.Coder = "Programming, debugging, architecture and code generation.";
    draft.descriptions.Research = "Finding and summarising sources.";
    draft.descriptions.General = "  ";
    assert.deepEqual(descriptionChanges(draft, running), [
      { name: "Coder", description: "Programming, debugging, architecture and code generation." },
      { name: "General", description: null },
      { name: "Research", description: "Finding and summarising sources." },
    ]);
  });
});

describe("what the router reports, in words", () => {
  const report = (status: string, extra: Partial<ClassifierCheckReport> = {}): ClassifierCheckReport => ({
    provider: "jev",
    status,
    checked_at: 1,
    duration_ms: 12,
    ...extra,
  });

  it("maps every check status to an operator message", () => {
    const expected: Record<string, [string, RegExp]> = {
      ok: ["ok", /^Connected$/],
      api_key_missing: ["danger", /^API key missing$/],
      auth_error: ["danger", /^Authentication failed$/],
      rate_limited: ["warn", /^Provider rate limited the request$/],
      provider_error: ["danger", /^Provider error$/],
      connection_error: ["danger", /reach the provider/],
      timeout: ["danger", /^Connection timed out$/],
      invalid_response: ["danger", /^Unexpected response$/],
      model_not_listed: ["warn", /^Model is not listed by the provider$/],
      route_unavailable: ["danger", /route unavailable/],
    };
    for (const [status, [tone, title]] of Object.entries(expected)) {
      const explained = explainCheck(report(status));
      assert.equal(explained.tone, tone, status);
      assert.match(explained.title, title, status);
    }
  });

  it("does not call an unlisted model invalid: a pinned version may still be accepted", () => {
    const explained = explainCheck(report("model_not_listed", { model: "jev-1.13.0", model_listed: false }));
    assert.doesNotMatch(`${explained.title} ${explained.detail}`, /invalid model/i);
    assert.match(explained.detail, /not listed by the provider's model discovery endpoint/);
    assert.match(explained.detail, /Pinned versions may still be accepted/);
  });

  it("reports the key as configured, missing or unknown, never its value", () => {
    assert.equal(keyStatus(jevAuto().classifier!), "configured");
    const missing = jevAuto();
    missing.classifier!.jev!.api_key_configured = false;
    assert.equal(keyStatus(missing.classifier!), "missing");
    assert.equal(keyStatus(null), "unknown");
  });

  it("tells a semantic rule from a direct one", () => {
    assert.equal(ruleAction({ route: "Coder", classify: false }), "Route directly to Coder");
    assert.equal(ruleAction({ route: "General", classify: true }), "Semantic classification");
  });
});

// R9.3.2 adds a classification outcome this frozen screen was built before.
// It must show it as written, never fail on it (design test B44).
describe("a request-budget outcome the screen predates", () => {
  it("is shown as the router wrote it", () => {
    assert.equal(outcomeLabel("request_budget_exhausted"), "request_budget_exhausted");
    assert.equal(outcomeLabel("timeout"), "Timed out (fallback)");
  });
});
