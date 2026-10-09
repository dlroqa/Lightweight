/**
 * Jev Settings: what the panel sends when it saves, and how it reads the
 * router's settings view. Run with `npm run test`.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import type { ClassifierCheckReport, ClassifierSettingsView } from "../../api/types.ts";
import {
  DEFAULT_BASE_URL,
  DEFAULT_JEV_MODEL,
  checkApiKey,
  connectionState,
  explainSaveError,
  hasSettingsProblems,
  keySourceLabel,
  restartReasons,
  savedKeyState,
  settingsDraftFrom,
  settingsRequest,
  validateSettings,
} from "./classifierModel.ts";

function view(patch: Partial<ClassifierSettingsView> = {}): ClassifierSettingsView {
  return {
    file: "router.json",
    revision: "a".repeat(64),
    configured: true,
    providers: ["lightweight", "jev"],
    saved: {
      provider: "jev",
      candidates: ["General", "Coder"],
      fallback_route: "General",
      jev: {
        base_url: "https://api.typesafe.ai",
        model: "jev-latest",
        api_key_env: "TYPESAFE_API_KEY",
        timeout_ms: 5000,
        min_confidence: null,
      },
    },
    active: {
      provider: "jev",
      candidates: ["General", "Coder"],
      fallback_route: "General",
      jev: null,
      key_source: "credential_store",
    },
    restart_required: false,
    restart_reasons: [],
    key: {
      api_key_env: "TYPESAFE_API_KEY",
      source: "credential_store",
      environment: false,
      stored: true,
      store: { available: true, backend: "secret-service" },
    },
    admin: { available: true, token_command: "hermes router admin-token" },
    ...patch,
  };
}

function report(status: string): ClassifierCheckReport {
  return { provider: "jev", status, checked_at: 1, duration_ms: 3 } as ClassifierCheckReport;
}

describe("the Jev Settings draft", () => {
  it("is read from the saved file and never holds a key", () => {
    const draft = settingsDraftFrom(view());
    assert.equal(draft.provider, "jev");
    assert.equal(draft.base_url, "https://api.typesafe.ai");
    assert.equal(draft.model, "jev-latest");
    assert.equal(draft.timeout_ms, "5000");
    assert.equal(draft.min_confidence, "");
    assert.equal(draft.api_key, "");
  });

  it("starts a new Jev configuration from the defaults", () => {
    const draft = settingsDraftFrom(
      view({ saved: { provider: "lightweight", candidates: [], fallback_route: null, jev: null } }),
    );
    assert.equal(draft.provider, "lightweight");
    assert.equal(draft.base_url, DEFAULT_BASE_URL);
    assert.equal(draft.model, DEFAULT_JEV_MODEL);
    assert.equal(DEFAULT_JEV_MODEL, "jev-latest");
    assert.equal(draft.timeout_ms, "");
  });
});

describe("the save request", () => {
  it("sends a key only when one was typed", () => {
    const draft = settingsDraftFrom(view());
    assert.equal("api_key" in settingsRequest(draft), false);
    assert.equal("api_key" in settingsRequest({ ...draft, api_key: "   " }), false);
    assert.equal(settingsRequest({ ...draft, api_key: " k-1 " }).api_key, "k-1");
  });

  it("sends the Jev fields only for Jev, and only the numbers that were given", () => {
    const draft = settingsDraftFrom(view());
    assert.deepEqual(settingsRequest(draft), {
      provider: "jev",
      jev: { base_url: "https://api.typesafe.ai", model: "jev-latest", timeout_ms: 5000 },
    });
    assert.deepEqual(settingsRequest({ ...draft, provider: "lightweight" }), {
      provider: "lightweight",
    });
    const withConfidence = settingsRequest({ ...draft, min_confidence: "0.8", timeout_ms: "" });
    assert.deepEqual(withConfidence.jev, {
      base_url: "https://api.typesafe.ai",
      model: "jev-latest",
      min_confidence: 0.8,
    });
  });
});

describe("validation", () => {
  it("refuses what the router refuses", () => {
    const draft = settingsDraftFrom(view());
    const problems = validateSettings(
      { ...draft, base_url: "http://api.typesafe.ai", model: "", min_confidence: "2", api_key: "a b" },
      view(),
    );
    assert.ok(problems.base_url);
    assert.ok(problems.model);
    assert.ok(problems.min_confidence);
    assert.ok(problems.api_key);
    assert.equal(hasSettingsProblems(problems), true);
    assert.ok(validateSettings({ ...draft, base_url: "https://u:p@x.example" }, view()).base_url);
    assert.ok(validateSettings({ ...draft, base_url: "https://x.example/?q=1" }, view()).base_url);
  });

  it("needs a timeout only when the file has none", () => {
    const draft = { ...settingsDraftFrom(view()), timeout_ms: "" };
    assert.equal(validateSettings(draft, view()).timeout_ms, null);
    const fresh = view({
      saved: { ...view().saved, jev: { ...view().saved.jev!, timeout_ms: null } },
    });
    assert.ok(validateSettings(draft, fresh).timeout_ms);
  });

  it("checks nothing Jev-specific for the Lightweight provider", () => {
    const draft = { ...settingsDraftFrom(view()), provider: "lightweight" as const, model: "" };
    assert.equal(hasSettingsProblems(validateSettings(draft, view())), false);
  });

  it("accepts an empty key, and a one-line key", () => {
    assert.equal(checkApiKey(""), null);
    assert.equal(checkApiKey("ts_abc123"), null);
    assert.ok(checkApiKey("two\nlines"));
    assert.ok(checkApiKey("x".repeat(4097)));
  });
});

describe("status in words", () => {
  it("says where the key comes from, never what it is", () => {
    assert.deepEqual(savedKeyState(view()).state, "configured");
    assert.match(savedKeyState(view()).label, /credential store/);
    const env = view({ key: { ...view().key, source: "environment", environment: true } });
    assert.match(savedKeyState(env).label, /environment variable TYPESAFE_API_KEY/);
    const missing = view({ key: { ...view().key, source: "missing", stored: false } });
    assert.equal(savedKeyState(missing).state, "missing");
    assert.match(keySourceLabel("credential_store", "X"), /credential store/);
    assert.match(keySourceLabel(null, "X"), /not running Jev/);
  });

  it("names every connection state of the running settings", () => {
    assert.deepEqual(connectionState(null), { tone: "neutral", label: "Not checked" });
    assert.deepEqual(connectionState(report("ok")), { tone: "ok", label: "Connected" });
    assert.equal(connectionState(report("model_not_listed")).tone, "warn");
    for (const [status, words] of [
      ["auth_error", "Authentication failed"],
      ["rate_limited", "rate limited"],
      ["timeout", "timed out"],
      ["connection_error", "Could not reach"],
    ] as const) {
      const state = connectionState(report(status));
      assert.equal(state.tone, "danger", status);
      assert.match(state.label, new RegExp(`^Error — .*${words}`, "i"), status);
    }
  });

  it("explains a pending restart and a refused save", () => {
    const pending = view({ restart_required: true, restart_reasons: ["settings_changed", "key_changed"] });
    assert.equal(restartReasons(pending).length, 2);
    assert.match(explainSaveError("admin_token_invalid", "x"), /hermes router admin-token/);
    assert.match(explainSaveError("revision_conflict", "x"), /reloaded/);
    assert.equal(explainSaveError("invalid_settings", "the router said so"), "the router said so");
  });
});
