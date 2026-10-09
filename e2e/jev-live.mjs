// Live Jev validation (scripts/jev-live.sh): a router whose classifier is Jev
// on the real TypeSafe API. Proves, through the router's own API and panel:
//
// 1. Test Connection (POST /classifier/check): the key is accepted and the
//    model is listed (an alias must be; a pinned version may not be).
// 2. Auto requests are classified *by Jev*: each trace's classifier is
//    `jev`, its outcome `chosen` (never a fallback), its chosen route the
//    expected logical route, and the request is served there (200, `model`
//    = the route).
// 3. The panel, served by the router, shows the key as configured from the
//    environment and Test Connection as Connected.
// 4. The key is in no API answer, page, browser storage or request URL.
//
// Writes a summary (routes, outcomes, confidences, timings — no request text,
// no key) and screenshots to OUT_DIR.
//
// Environment: ROUTER_BASE, JEV_MODEL, OUT_DIR, LIVE_KEY_SENTINEL (the key,
// compared and never printed).

import { mkdir, writeFile } from "node:fs/promises";
import { chromium } from "playwright";

const BASE = (process.env.ROUTER_BASE ?? "").replace(/\/+$/, "");
const MODEL = process.env.JEV_MODEL ?? "jev-latest";
const OUT_DIR = process.env.OUT_DIR ?? "jev-live";
const KEY = process.env.LIVE_KEY_SENTINEL ?? "";
const TIMEOUT = 60_000;

// What each request is, and the route it must land on. Unambiguous on
// purpose: this validates the integration, not the classifier's judgement at
// the margin.
const CASES = [
  { id: "code-rust", expect: "Coder", text: "Write a Rust function that reverses a singly linked list, and explain how it satisfies the borrow checker." },
  { id: "code-debug", expect: "Coder", text: "My Python parser crashes with TypeError: 'NoneType' object is not subscriptable on line 42. How do I debug and fix it?" },
  { id: "general-pet", expect: "General", text: "What are some friendly names for a golden retriever puppy?" },
  { id: "general-book", expect: "General", text: "Summarize the plot of Pride and Prejudice in two sentences." },
];

const failures = [];
const bodies = [];
function check(condition, message) {
  if (!condition) failures.push(message);
  console.log(`  [${condition ? "ok" : "FAIL"}] ${message}`);
}

async function api(path, init) {
  const response = await fetch(`${BASE}${path}`, { ...init, signal: AbortSignal.timeout(TIMEOUT) });
  const text = await response.text();
  bodies.push(text);
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // Not JSON: left null, and the check that needed it fails.
  }
  return { status: response.status, json };
}

async function main() {
  if (!BASE || !KEY) throw new Error("ROUTER_BASE and LIVE_KEY_SENTINEL are required");
  await mkdir(OUT_DIR, { recursive: true });
  const summary = { model: MODEL, check: null, cases: [] };

  // --- 1. Test Connection against the live API ----------------------------------------------
  const report = await api("/api/router/v1/classifier/check", { method: "POST" });
  summary.check = {
    status: report.json?.status,
    http_status: report.json?.http_status ?? null,
    model_listed: report.json?.model_listed ?? null,
    duration_ms: report.json?.duration_ms,
  };
  const pinned = /\d+\.\d+/.test(MODEL);
  check(report.status === 200 && report.json?.provider === "jev", "Test Connection reaches the Jev provider");
  check(
    report.json?.status === "ok" || (pinned && report.json?.status === "model_not_listed"),
    `TypeSafe accepts the key and ${pinned ? "answers for" : "lists"} ${MODEL} (status ${report.json?.status}, HTTP ${report.json?.http_status ?? "-"})`,
  );

  const auto = await api("/api/router/v1/auto");
  check(auto.json?.classifier?.provider === "jev", "the running classifier is Jev");
  check(auto.json?.classifier?.jev?.api_key_configured === true, "the router found the key");
  check(auto.json?.classifier?.jev?.api_key_source === "environment", "the key came from the environment (the protected secret)");
  check(auto.json?.classifier?.jev?.base_url === "https://api.typesafe.ai", "the endpoint is the real TypeSafe API");

  // --- 2. Auto requests, classified by Jev ----------------------------------------------------------
  for (const item of CASES) {
    const requestId = `jev-live-${item.id}-${Date.now()}`;
    const answer = await api("/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", "x-request-id": requestId },
      body: JSON.stringify({ model: "Auto", messages: [{ role: "user", content: item.text }] }),
    });
    const traces = await api("/api/router/v1/traces?limit=50");
    const trace = (traces.json?.data ?? []).find((t) => t.request_id === requestId) ?? null;
    const classifier = trace?.classifier ?? null;
    const row = {
      case: item.id,
      expected: item.expect,
      status: answer.status,
      served_by: answer.json?.model ?? null,
      auto_rule: trace?.auto_rule ?? null,
      provider: classifier?.provider ?? null,
      jev_model: classifier?.model ?? null,
      outcome: classifier?.outcome ?? null,
      chosen_route: classifier?.chosen_route ?? null,
      confidence: classifier?.confidence ?? null,
      classification_ms: classifier?.duration_ms ?? null,
    };
    summary.cases.push(row);
    check(trace !== null, `${item.id}: the request's trace is found by its id`);
    check(row.provider === "jev" && row.auto_rule === "semantic", `${item.id}: Auto asked Jev (rule ${row.auto_rule}, provider ${row.provider})`);
    check(row.outcome === "chosen", `${item.id}: Jev classified it, not a fallback (outcome ${row.outcome}, confidence ${row.confidence})`);
    check(row.chosen_route === item.expect, `${item.id}: Jev chose ${item.expect} (got ${row.chosen_route})`);
    check(row.status === 200 && row.served_by === item.expect, `${item.id}: served by ${item.expect} (got ${row.status} ${row.served_by})`);
  }
  const outcomes = (await api("/api/router/v1/auto")).json?.classifier?.outcomes ?? {};
  summary.outcomes = outcomes;
  check((outcomes.chosen ?? 0) >= CASES.length, `the router counted ${outcomes.chosen ?? 0} Jev classifications as chosen`);

  // --- 3. The panel ----------------------------------------------------------------------------
  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 2400 }, colorScheme: "dark" });
  const page = await context.newPage();
  const urls = [];
  const pageErrors = [];
  page.on("request", (request) => urls.push(request.url()));
  page.on("pageerror", (err) => pageErrors.push(String(err)));
  page.on("response", async (response) => {
    try {
      bodies.push(await response.text());
    } catch {
      // No body to inspect.
    }
  });
  await page.goto(`${BASE}/#/classifier`, { waitUntil: "domcontentloaded" });
  await page.locator("[data-jev-settings]").waitFor({ timeout: TIMEOUT });
  const card = page.locator(".card", { has: page.locator("[data-jev-settings]") });
  check((await card.locator('[data-key-state="configured"]').count()) === 1, "panel: the key reads as configured");
  check((await card.locator('[data-key-source="environment"]').count()) === 1, "panel: the key reads as from the environment");
  await card.locator("[data-test-connection]").click();
  await card.locator("[data-check-status]").waitFor({ timeout: TIMEOUT });
  const shown = await card.locator("[data-check-status]").getAttribute("data-check-status");
  check(shown === "ok" || (pinned && shown === "model_not_listed"), `panel: Test Connection against the live API reads ${shown}`);
  await card.screenshot({ path: `${OUT_DIR}/jev-live-settings.png` });
  await page.goto(`${BASE}/#/auto`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(1500);
  await page.screenshot({ path: `${OUT_DIR}/jev-live-auto.png`, fullPage: true });

  // --- 4. The key went nowhere ---------------------------------------------------------------------
  const dom = await page.content();
  const stored = await page.evaluate(() => JSON.stringify({ ...window.localStorage, ...window.sessionStorage }));
  check(!dom.includes(KEY), "the key is in no page");
  check(!stored.includes(KEY), "the key is in no browser storage");
  check(urls.every((url) => !url.includes(KEY)), "the key is in no request URL");
  check(bodies.every((body) => !body.includes(KEY)), `the key is in none of ${bodies.length} API answers`);
  check(pageErrors.length === 0, `no page errors (${pageErrors.length})`);
  await browser.close();

  await writeFile(`${OUT_DIR}/summary.json`, `${JSON.stringify(summary, null, 2)}\n`);
  console.log("\nSummary (no request text, no key):");
  console.log(JSON.stringify(summary, null, 2));
  if (failures.length > 0) {
    console.error(`\n${failures.length} live Jev check(s) failed:`);
    for (const failure of failures) console.error(`  - ${failure}`);
    process.exit(1);
  }
  console.log("\nLive Jev validation passed.");
}

main().catch((err) => {
  // An error message is the router's or the browser's, never the key: the key
  // is only ever compared, and GitHub masks it besides.
  console.error(String(err?.message ?? err));
  process.exit(1);
});
