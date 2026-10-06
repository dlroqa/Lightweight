/**
 * The classifier settings, as the panel edits them — with no React in it, so
 * the rules can be tested on their own (`npm run test`).
 *
 * Every rule here mirrors one the router enforces when it loads `router.json`
 * (`crates/lightweight-router/src/classifier/`). They exist to say what is
 * wrong next to the field while a person types; the router remains the
 * authority, and `hermes router validate-config` is the last word.
 *
 * Nothing in this module ever holds a secret. The TypeSafe key is read by the
 * router from the environment variable `api_key_env` names; the panel edits
 * that *name* and shows only whether the router found a value behind it.
 */

import type {
  AutoView,
  ClassifierCheckReport,
  ClassifierView,
  RouterRouteView,
} from "../../api/types";

// --- the router's own limits (classifier/mod.rs, classifier/jev.rs) ------------------

export const MAX_TIMEOUT_MS = 120_000;
export const DEFAULT_MIN_CONFIDENCE = 0.65;
export const DEFAULT_MAX_INPUT_CHARS = 2_000;
export const MAX_INPUT_CHARS = 32_000;
export const MAX_CANDIDATES = 16;
export const MAX_DESCRIPTION_CHARS = 200;
export const MAX_MODEL_CHARS = 128;
export const DEFAULT_BASE_URL = "https://api.typesafe.ai";
export const DEFAULT_API_KEY_ENV = "TYPESAFE_API_KEY";
/**
 * Offered as a starting point for Jev, never inserted silently: the router has
 * no default timeout, because what is safe depends on the network.
 */
export const SUGGESTED_JEV_TIMEOUT_MS = 5_000;

export type ProviderKind = "lightweight" | "jev";

/** Form state: numbers stay strings until validated, as typed. */
export interface LightweightDraft {
  route: string;
  timeout_ms: string;
  min_confidence: string;
  max_input_chars: string;
}

export interface JevDraft {
  base_url: string;
  api_key_env: string;
  model: string;
  timeout_ms: string;
  min_confidence: string;
  max_input_chars: string;
  include_user_text: boolean;
}

export interface ClassifierDraft {
  provider: ProviderKind;
  /** Candidate routes, in the order they are described to the classifier. */
  candidates: string[];
  fallback_route: string;
  /** Route name -> description, as edited. */
  descriptions: Record<string, string>;
  lightweight: LightweightDraft;
  jev: JevDraft;
  /** Also write the provider that is not selected, as a standby block. */
  keep_standby: boolean;
}

// --- reading the running router into a draft --------------------------------------

/** `Auto` itself is never a classifier choice, nor `default`. */
export function isLogicalRoute(name: string): boolean {
  const lower = name.trim().toLowerCase();
  return lower !== "auto" && lower !== "default" && lower !== "";
}

function text(value: number | null | undefined, fallback: string): string {
  return value === null || value === undefined ? fallback : String(value);
}

/**
 * The draft a running router's configuration reads back as. Blocks the router
 * does not have start from the router's defaults — and Jev's timeout starts
 * empty, because the router has none.
 */
export function draftFrom(auto: AutoView | null, routes: RouterRouteView[]): ClassifierDraft {
  const classifier = auto?.classifier ?? null;
  const descriptions: Record<string, string> = {};
  for (const route of routes) {
    if (route.description) descriptions[route.name] = route.description;
  }
  for (const candidate of classifier?.candidates ?? []) {
    if (candidate.description) descriptions[candidate.route] = candidate.description;
  }
  const lw = classifier?.lightweight;
  const jev = classifier?.jev;
  const firstRoute = routes.find((route) => isLogicalRoute(route.name))?.name ?? "";
  return {
    provider: classifier?.provider ?? "lightweight",
    candidates: classifier?.candidates.map((candidate) => candidate.route) ?? [],
    fallback_route: classifier?.fallback_route ?? auto?.fallback_route ?? firstRoute,
    descriptions,
    lightweight: {
      route: lw?.route ?? "",
      timeout_ms: text(lw?.timeout_ms, ""),
      min_confidence: text(lw?.min_confidence, String(DEFAULT_MIN_CONFIDENCE)),
      max_input_chars: text(lw?.max_input_chars, String(DEFAULT_MAX_INPUT_CHARS)),
    },
    jev: {
      base_url: jev?.base_url ?? DEFAULT_BASE_URL,
      api_key_env: jev?.api_key_env ?? DEFAULT_API_KEY_ENV,
      model: jev?.model ?? "",
      timeout_ms: text(jev?.timeout_ms, ""),
      min_confidence: text(jev?.min_confidence, String(DEFAULT_MIN_CONFIDENCE)),
      max_input_chars: text(jev?.max_input_chars, String(DEFAULT_MAX_INPUT_CHARS)),
      include_user_text: jev?.include_user_text ?? true,
    },
    keep_standby: Boolean(classifier && lw && jev),
  };
}

// --- field rules --------------------------------------------------------------------

/** A problem with one field, or `null`. Messages say how to fix it. */
export type Problem = string | null;

export function checkTimeout(raw: string): Problem {
  const value = raw.trim();
  if (value === "") {
    return `Required. Choose a bound between 1 and ${MAX_TIMEOUT_MS} ms for your hardware or network.`;
  }
  if (!/^\d+$/.test(value)) return "Whole milliseconds only.";
  const ms = Number(value);
  if (ms < 1 || ms > MAX_TIMEOUT_MS) return `Must be between 1 and ${MAX_TIMEOUT_MS} ms.`;
  return null;
}

export function checkConfidence(raw: string): Problem {
  const value = raw.trim();
  if (value === "") return "Required.";
  const number = Number(value);
  if (!Number.isFinite(number) || number < 0 || number > 1) {
    return "Must be a number between 0 and 1.";
  }
  return null;
}

export function checkMaxInput(raw: string): Problem {
  const value = raw.trim();
  if (value === "") return "Required.";
  if (!/^\d+$/.test(value)) return "Whole characters only.";
  const chars = Number(value);
  if (chars < 1 || chars > MAX_INPUT_CHARS) return `Must be between 1 and ${MAX_INPUT_CHARS}.`;
  return null;
}

function isLoopbackHost(hostname: string): boolean {
  const host = hostname.replace(/^\[|\]$/g, "").toLowerCase();
  if (host === "localhost" || host === "::1") return true;
  return /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host);
}

/** Jev's base URL: https (http only for loopback), no credentials, query or fragment. */
export function checkBaseUrl(raw: string): Problem {
  const value = raw.trim();
  if (value === "") return "Required.";
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return "Not a valid URL.";
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") return "Must use https.";
  if (!url.hostname) return "Has no host.";
  if (url.username || url.password) {
    return "Must not carry credentials. The key belongs in the environment variable below.";
  }
  if (url.search || value.includes("?")) return "Must not have a query string.";
  if (url.hash || value.includes("#")) return "Must not have a fragment.";
  if (url.protocol === "http:" && !isLoopbackHost(url.hostname)) {
    return "Must use https: the API key and request content travel over it (http is accepted only for a loopback address).";
  }
  return null;
}

/** The router strips one trailing slash and keeps any proxy path prefix. */
export function normalizeBaseUrl(raw: string): string {
  return raw.trim().replace(/\/+$/, "");
}

export function checkEnvName(raw: string): Problem {
  const value = raw.trim();
  if (value === "") return "Required.";
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(value)) {
    return "Letters, digits and underscores only, not starting with a digit.";
  }
  return null;
}

// Built from a code point so the source carries no control character itself.
const CONTROL = new RegExp(`[${String.fromCharCode(0)}-${String.fromCharCode(0x1f)}${String.fromCharCode(0x7f)}-${String.fromCharCode(0x9f)}]`);

export function checkModel(raw: string): Problem {
  const value = raw.trim();
  if (value === "") return "Required: an alias such as jev-latest, or a pinned version.";
  if (value.length > MAX_MODEL_CHARS || CONTROL.test(value)) {
    return `At most ${MAX_MODEL_CHARS} characters, with no control character.`;
  }
  return null;
}

/** Empty is allowed (the route then has no description); anything else is checked. */
export function checkDescription(raw: string): Problem {
  const value = raw.trim();
  if (value === "") return null;
  if ([...value].length > MAX_DESCRIPTION_CHARS) {
    return `At most ${MAX_DESCRIPTION_CHARS} characters.`;
  }
  if (CONTROL.test(value)) return "No line breaks or control characters.";
  return null;
}

export interface DraftProblems {
  candidates: Problem;
  fallback_route: Problem;
  descriptions: Record<string, Problem>;
  lightweight: Partial<Record<keyof LightweightDraft, Problem>>;
  jev: Partial<Record<keyof JevDraft, Problem>>;
}

function routeProblem(name: string, routes: string[], what: string): Problem {
  if (name.trim() === "") return `Choose the ${what}.`;
  if (!isLogicalRoute(name)) return "Auto and default cannot be chosen; pick a configured route.";
  if (!routes.some((route) => route.toLowerCase() === name.toLowerCase())) {
    return "Not one of the configured routes.";
  }
  return null;
}

function checkLightweight(draft: LightweightDraft, routes: string[]) {
  return {
    route: routeProblem(draft.route, routes, "route that classifies"),
    timeout_ms: checkTimeout(draft.timeout_ms),
    min_confidence: checkConfidence(draft.min_confidence),
    max_input_chars: checkMaxInput(draft.max_input_chars),
  };
}

function checkJev(draft: JevDraft) {
  return {
    base_url: checkBaseUrl(draft.base_url),
    api_key_env: checkEnvName(draft.api_key_env),
    model: checkModel(draft.model),
    timeout_ms: checkTimeout(draft.timeout_ms),
    min_confidence: checkConfidence(draft.min_confidence),
    max_input_chars: checkMaxInput(draft.max_input_chars),
  };
}

/** Every field's problem. `routes` are the configured route names. */
export function validateDraft(draft: ClassifierDraft, routes: string[]): DraftProblems {
  let candidates: Problem = null;
  if (draft.candidates.length === 0) candidates = "Choose at least one route to classify among.";
  else if (draft.candidates.length > MAX_CANDIDATES) {
    candidates = `At most ${MAX_CANDIDATES} routes.`;
  } else {
    const bad = draft.candidates.find((name) => routeProblem(name, routes, "route") !== null);
    if (bad !== undefined) candidates = `${bad} cannot be a candidate.`;
  }
  const descriptions: Record<string, Problem> = {};
  for (const [route, description] of Object.entries(draft.descriptions)) {
    descriptions[route] = checkDescription(description);
  }
  const standby = draft.keep_standby;
  return {
    candidates,
    fallback_route: routeProblem(draft.fallback_route, routes, "fallback route"),
    descriptions,
    lightweight:
      draft.provider === "lightweight" || standby ? checkLightweight(draft.lightweight, routes) : {},
    jev: draft.provider === "jev" || standby ? checkJev(draft.jev) : {},
  };
}

export function hasProblems(problems: DraftProblems): boolean {
  return (
    problems.candidates !== null ||
    problems.fallback_route !== null ||
    Object.values(problems.descriptions).some((p) => p !== null) ||
    Object.values(problems.lightweight).some((p) => p !== null) ||
    Object.values(problems.jev).some((p) => p !== null)
  );
}

// --- the configuration the draft becomes --------------------------------------------

function lightweightBlock(draft: LightweightDraft) {
  return {
    route: draft.route.trim(),
    timeout_ms: Number(draft.timeout_ms.trim()),
    min_confidence: Number(draft.min_confidence.trim()),
    max_input_chars: Number(draft.max_input_chars.trim()),
  };
}

function jevBlock(draft: JevDraft) {
  return {
    base_url: normalizeBaseUrl(draft.base_url),
    api_key_env: draft.api_key_env.trim(),
    model: draft.model.trim(),
    timeout_ms: Number(draft.timeout_ms.trim()),
    min_confidence: Number(draft.min_confidence.trim()),
    max_input_chars: Number(draft.max_input_chars.trim()),
    include_user_text: draft.include_user_text,
  };
}

/**
 * The `auto_route.classifier` section, in the canonical (R9.1a) shape only:
 * `provider` plus one block per provider. Never the R9.1 flat shorthand —
 * the router refuses a file that has both — and never a key.
 */
export function classifierSection(draft: ClassifierDraft): Record<string, unknown> {
  const section: Record<string, unknown> = {
    provider: draft.provider,
    routes: draft.candidates,
    fallback_route: draft.fallback_route,
  };
  if (draft.provider === "lightweight" || draft.keep_standby) {
    section.lightweight = lightweightBlock(draft.lightweight);
  }
  if (draft.provider === "jev" || draft.keep_standby) {
    section.jev = jevBlock(draft.jev);
  }
  return section;
}

/** The section as it is pasted under `"auto_route"` in `router.json`. */
export function classifierSnippet(draft: ClassifierDraft): string {
  return JSON.stringify({ classifier: classifierSection(draft) }, null, 2);
}

/**
 * Route descriptions that differ from what the router is running, as the
 * `routes[]` entries to change. Descriptions live on each route, not in the
 * classifier section, because both providers read them.
 */
export function descriptionChanges(
  draft: ClassifierDraft,
  running: Record<string, string>,
): { name: string; description: string | null }[] {
  const changes: { name: string; description: string | null }[] = [];
  for (const [name, raw] of Object.entries(draft.descriptions)) {
    const next = raw.trim();
    const before = (running[name] ?? "").trim();
    if (next !== before) changes.push({ name, description: next === "" ? null : next });
  }
  return changes.sort((a, b) => a.name.localeCompare(b.name));
}

// --- what the router reports, in words ----------------------------------------------

export type Tone = "ok" | "warn" | "danger" | "info" | "neutral";

export interface Explained {
  tone: Tone;
  title: string;
  detail: string;
}

/**
 * A `POST /api/router/v1/classifier/check` result, for an operator. Says what
 * the router observed, never a provider's error body (the router never
 * returns one).
 */
export function explainCheck(report: ClassifierCheckReport): Explained {
  switch (report.status) {
    case "ok":
      return report.provider === "jev"
        ? {
            tone: "ok",
            title: "Connected",
            detail: `TypeSafe accepted the key and lists the model ${report.model ?? ""}.`.replace(" .", "."),
          }
        : {
            tone: "ok",
            title: "Connected",
            detail: "The classifier route has a deployment available.",
          };
    case "model_not_listed":
      return {
        tone: "warn",
        title: "Model is not listed by the provider",
        detail:
          "The configured model was not listed by the provider's model discovery endpoint. " +
          "Pinned versions may still be accepted. An alias that is not listed is likely misspelled.",
      };
    case "api_key_missing":
      return {
        tone: "danger",
        title: "API key missing",
        detail:
          "The environment variable named for the key was not set (or was empty) when the router started. " +
          "Set it in the router's environment and restart the router.",
      };
    case "auth_error":
      return {
        tone: "danger",
        title: "Authentication failed",
        detail: "The provider refused the API key. Check the key's value, then restart the router.",
      };
    case "rate_limited":
      return {
        tone: "warn",
        title: "Provider rate limited the request",
        detail: "The provider is rate limiting or overloaded. Classifications fall back until it recovers.",
      };
    case "provider_error":
      return {
        tone: "danger",
        title: "Provider error",
        detail: "The provider answered with an error status. Classifications fall back meanwhile.",
      };
    case "connection_error":
      return {
        tone: "danger",
        title: "Could not reach the provider",
        detail: "The connection failed. Check the base URL and this machine's network.",
      };
    case "timeout":
      return {
        tone: "danger",
        title: "Connection timed out",
        detail: "No answer within the configured timeout. Consider a longer timeout for this network.",
      };
    case "invalid_response":
      return {
        tone: "danger",
        title: "Unexpected response",
        detail: "The provider answered, but not in the shape expected. Check the base URL points at TypeSafe.",
      };
    case "route_unavailable":
      return {
        tone: "danger",
        title: "Classifier route unavailable",
        detail: "No deployment of the classifier route is available right now.",
      };
    default:
      return { tone: "neutral", title: report.status, detail: "The router reported this status." };
  }
}

/** What a recorded classification outcome means. Shared by both providers. */
export const OUTCOME_LABELS: Record<string, string> = {
  chosen: "Chosen",
  low_confidence: "Low confidence (fallback)",
  invalid: "Invalid answer (fallback)",
  unavailable: "Classifier route unavailable (fallback)",
  timeout: "Timed out (fallback)",
  auth_error: "Authentication failed (fallback)",
  rate_limited: "Rate limited (fallback)",
  connection_error: "Connection error (fallback)",
  provider_error: "Provider error (fallback)",
  nested: "Nested request (prevented)",
};

export function outcomeLabel(kind: string): string {
  return OUTCOME_LABELS[kind] ?? kind;
}

/** Whether the classifier's key is set, in words, for one provider block. */
export function keyStatus(view: ClassifierView | null): "configured" | "missing" | "unknown" {
  const jev = view?.jev;
  if (!jev) return "unknown";
  return jev.api_key_configured ? "configured" : "missing";
}

/** An Auto rule's action, as an operator reads it. */
export function ruleAction(rule: { route: string; classify: boolean }): string {
  return rule.classify ? "Semantic classification" : `Route directly to ${rule.route}`;
}
