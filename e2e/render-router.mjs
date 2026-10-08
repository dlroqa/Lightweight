// Render the panel as a router serves it (`hermes router --web-root`) and
// assert the router screens: Auto rules, the classifier provider selector and
// panels, Test Connection against the real router, every check status in
// words, field validation, and that the TypeSafe key never reaches the browser.
//
// As render.mjs: properties, not pixels. Each screen is screenshotted for a
// person to eyeball on the pull request.
//
// Environment:
//   PANEL_BASE       the router's origin (default http://127.0.0.1:11500)
//   OUT_DIR          where screenshots land (default screens)
//   SECRET_SENTINEL  the TypeSafe key the router was started with; it must
//                    appear in no response body, DOM or browser storage

import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";

const BASE = (process.env.PANEL_BASE ?? "http://127.0.0.1:11500").replace(/\/+$/, "");
const OUT_DIR = process.env.OUT_DIR ?? "screens";
const SECRET = process.env.SECRET_SENTINEL ?? "";
const TIMEOUT = Number(process.env.RENDER_SETTLE_MS ?? 15000);

const failures = [];
function check(condition, message) {
  if (!condition) failures.push(message);
  console.log(`  [${condition ? "ok" : "FAIL"}] ${message}`);
}

async function main() {
  if (!SECRET) throw new Error("SECRET_SENTINEL is required, to prove the key never reaches the browser");
  await mkdir(OUT_DIR, { recursive: true });
  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    colorScheme: "dark",
    deviceScaleFactor: 2,
  });
  const page = await context.newPage();
  const errors = [];
  const bodies = [];
  page.on("pageerror", (err) => errors.push(String(err)));
  page.on("response", async (response) => {
    if (!response.url().startsWith(BASE)) return;
    try {
      bodies.push({ url: response.url(), text: await response.text() });
    } catch {
      // A redirect or aborted response has no body to inspect.
    }
  });

  // --- Auto Routing -----------------------------------------------------------------
  await page.goto(`${BASE}/`, { waitUntil: "domcontentloaded" });
  await page.getByRole("heading", { name: "Auto Routing", exact: true }).waitFor({ timeout: TIMEOUT });
  check(page.url().endsWith("#/auto"), "a router's panel opens on Auto Routing");
  const semantic = page.locator('tr[data-rule="semantic"]');
  await semantic.waitFor({ timeout: TIMEOUT });
  check((await semantic.innerText()).includes("Semantic classification"), "a classify rule reads as Semantic classification");
  check(
    (await page.locator('tr[data-rule="tools"]').innerText()).includes("Route directly to Coder"),
    "a direct rule reads as Route directly to Coder",
  );
  check(!(await page.getByText("Dashboard", { exact: true }).count()), "gateway-only sections are not offered on a router");
  await page.screenshot({ path: `${OUT_DIR}/router-auto.png`, fullPage: true });

  // --- Cross-route fallback (R9.3.1) ---------------------------------------------------
  // Every route here is unavailable (the gateway serves none of these models),
  // so a tools request to Auto goes Coder → General and exhausts the list:
  // real counters, a real exhausted chain and a real trace to render.
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: BASE });
  const fallbackRequest = await fetch(`${BASE}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      model: "Auto",
      tools: [{ type: "function", function: { name: "f", parameters: { type: "object" } } }],
      messages: [{ role: "user", content: "render fallback" }],
    }),
  });
  check(fallbackRequest.status === 503, "an Auto request whose list is exhausted gets the last route's own 503");
  await page.reload({ waitUntil: "domcontentloaded" });
  const summary = page.locator("[data-fallback-summary]");
  await summary.waitFor({ timeout: TIMEOUT });
  check(await page.getByRole("heading", { name: "Cross-Route Fallback", exact: true }).isVisible(), "the Cross-Route Fallback card renders on Auto Routing");
  check((await summary.innerText()).includes("Auto-selected routes only"), "it says it applies to Auto-selected routes only");
  check((await page.locator("[data-max-routes]").innerText()).trim() === "3", "the maximum of 3 fallback routes is shown");
  for (const reason of ["route_unavailable", "route_exhausted", "route_capability_mismatch"]) {
    check((await page.locator(`[data-trigger="${reason}"]`).count()) === 1, `trigger ${reason} is listed`);
  }
  check((await page.locator("[data-explicit-warning]").innerText()).includes("Explicit route requests return that route's error"), "the explicit-route warning is shown");
  const exclusions = await page.locator("[data-exclusions]").innerText();
  check(exclusions.includes("500") && exclusions.includes("Context overflow"), "500 and context overflow are listed as never falling back");
  const chain = await page.locator('[data-chain="Coder"]').innerText();
  check(chain.includes("Coder") && chain.includes("General"), "the Coder → General list renders");
  check((await page.locator("[data-non-transitive]").innerText()).includes("do not recursively apply their own fallback lists"), "the non-transitive rule is explained");
  check((await page.locator("[data-same-vs-cross]").innerText()).includes("Coder/A → Coder/B"), "same-route failover is distinguished from cross-route fallback");
  const transition = await page.locator('[data-transition="Coder->General"]').innerText();
  check(transition.includes("route_unavailable: 1"), "the Coder → General fallback is counted by reason");
  check((await page.locator('[data-exhausted-route="Coder"]').innerText()).includes("route_unavailable: 1"), "the exhausted list is counted");
  check((await page.locator("[data-identity-help]").innerText()).includes('model: "General"'), "response identity is explained");
  check((await page.locator("[data-requests-total-help]").innerText()).includes("counts each client request once"), "router_requests_total's final-route meaning is explained");
  const trace = page.locator("[data-fallback-trace]").first();
  await trace.waitFor({ timeout: TIMEOUT });
  const traceText = await trace.innerText();
  check(traceText.includes("Requested Auto") && traceText.includes("initial Coder") && traceText.includes("final General"), "the trace shows requested, initial and final routes");
  check((await trace.locator('[data-trace-step="Coder"]').innerText()).includes("route_unavailable"), "the trace shows Coder failing as route_unavailable");
  check((await trace.locator("[data-trace-exhausted]").count()) === 1 && traceText.includes("Exhausted"), "the exhausted chain is shown as exhausted");

  // The draft: valid as running, then every refusal, then the snippet again.
  check((await page.locator("[data-fallback-snippet]").innerText()).includes('"Coder": ['), "the running lists produce a snippet");
  await page.locator("[data-fallback-snippet]").getByRole("button", { name: "Copy to clipboard" }).click();
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  check(copied.startsWith('"cross_route_fallback": {') && copied.includes('"General"'), "Copy puts the canonical snippet on the clipboard");
  check((await page.locator("[data-fallback-snippet]").innerText()).includes("hermes router validate-config"), "the validate-config step is shown");
  check(await page.locator("[data-restart-required]").isVisible(), "the restart-required step is shown");
  await page.locator("[data-draft-add]").click();
  const newRow = page.locator("[data-draft-row]").last();
  await newRow.locator("[data-draft-source]").selectOption("General");
  await newRow.locator("[data-draft-targets]").fill("Coder");
  check((await page.locator("[data-draft-cycle]").innerText()).includes("Fallback cycle detected: Coder → General → Coder"), "a two-route cycle is refused and named");
  check((await page.locator("[data-fallback-snippet]").count()) === 0, "a cycle produces no snippet");
  for (const [targets, words] of [
    ["Missing", "is not one of the router's routes"],
    ["Research", "is the classifier's route"],
    ["Auto", "is Auto itself"],
    ["General", "cannot fall back to itself"],
    ["Coder, coder", "listed more than once"],
    ["Coder, Research, Missing, Other", "At most 3 fallback routes"],
  ]) {
    await newRow.locator("[data-draft-targets]").fill(targets);
    check((await newRow.locator("[data-draft-problem]").innerText()).includes(words), `“${targets}” is refused: ${words}`);
  }
  await newRow.getByRole("button", { name: /Remove list/ }).click();
  check((await page.locator("[data-fallback-snippet]").count()) === 1, "removing the bad list brings the snippet back");
  await page.screenshot({ path: `${OUT_DIR}/router-fallback.png`, fullPage: true });
  // The panel scrolls inside its own container, so the cards are captured whole.
  await page.locator(".card", { has: page.locator("[data-fallback-summary]") }).screenshot({ path: `${OUT_DIR}/router-fallback-summary.png` });
  await page.locator(".card", { has: page.locator("[data-fallback-traces]") }).screenshot({ path: `${OUT_DIR}/router-fallback-traces.png` });

  // The approved R9.3 wording, held in place: exactly the three triggers, every
  // exclusion, and the final-route meaning of the response and the counter.
  check((await page.locator("[data-trigger]").count()) === 3, "exactly three triggers are listed, none the router lacks");
  check(exclusions.includes("Explicit route requests") && exclusions.includes("after it started"), "explicit routes and failures after the stream started are listed as never falling back");
  check((await page.locator("[data-identity-help]").innerText()).includes("Auto → Coder → General"), "response identity is explained with Auto → Coder → General");
  check((await page.locator("[data-requests-total-help]").innerText()).includes("not the route Auto first chose"), "router_requests_total is not the route Auto first chose");

  // --- Cross-route fallback that succeeds ------------------------------------------------
  // Load the scripted nodes: Coder's second deployment now refuses with 503
  // (same-route failover has nowhere left to go: route_exhausted) and General's
  // second deployment answers. The router sees them on its next probe.
  const nodeUrls = (process.env.MOCK_NODE_URLS ?? "").split(",").filter(Boolean);
  check(nodeUrls.length === 2, "the two scripted nodes are given to the render");
  for (const url of nodeUrls) await fetch(`${url}/control/load`, { method: "POST" });
  const ready = Date.now() + TIMEOUT;
  let available = [];
  while (Date.now() < ready) {
    const routes = await (await fetch(`${BASE}/api/router/v1/routes`)).json();
    available = routes.data.filter((route) => route.available).map((route) => route.name);
    if (available.includes("Coder") && available.includes("General")) break;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  check(available.includes("Coder") && available.includes("General"), "the router sees the loaded Coder and General deployments");
  const servedRequest = await fetch(`${BASE}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      model: "Auto",
      tools: [{ type: "function", function: { name: "f", parameters: { type: "object" } } }],
      messages: [{ role: "user", content: "render successful fallback" }],
    }),
  });
  const served = await servedRequest.json();
  const servedId = servedRequest.headers.get("x-request-id") ?? "";
  check(servedRequest.status === 200, "an Auto request whose Coder route is exhausted is answered by General");
  check(served.model === "General", `the response names the final route: model "General" (got ${JSON.stringify(served.model)})`);
  check(servedId.length > 0, "the response carries its request id");
  await page.reload({ waitUntil: "domcontentloaded" });
  const success = page.locator(`[data-fallback-trace="${servedId}"]`);
  await success.waitFor({ timeout: TIMEOUT });
  const successText = await success.innerText();
  check(successText.includes("Requested Auto") && successText.includes("initial Coder") && successText.includes("final General"), "the successful trace shows requested Auto, initial Coder, final General");
  check(successText.includes("Served by General"), "the successful trace reads Served by General");
  check((await success.locator("[data-trace-exhausted]").count()) === 0 && !successText.includes("Exhausted"), "the successful trace is not marked exhausted");
  const steps = await success.locator("[data-trace-step]").evaluateAll((items) => items.map((item) => item.getAttribute("data-trace-step")));
  check(JSON.stringify(steps) === JSON.stringify(["Coder", "General"]), `the steps are Coder then General (got ${JSON.stringify(steps)})`);
  const coderStep = await success.locator('[data-trace-step="Coder"]').innerText();
  const generalStep = await success.locator('[data-trace-step="General"]').innerText();
  check(coderStep.includes("Route exhausted (route_exhausted)"), "Coder's step renders its fallback reason, route_exhausted");
  check(generalStep.includes("answered (200)"), "General's step renders it answered (ok, 200)");
  check(coderStep.includes("coder-b") && coderStep.includes("503") && !coderStep.includes("general-b"), "Coder's same-route attempt (coder-b, 503) stays with Coder");
  check(generalStep.includes("general-b") && !generalStep.includes("coder-b"), "General's same-route attempt (general-b) stays with General");
  check((await page.locator("[data-identity-help]").innerText()).includes('model: "General"'), "the response identity help agrees with the served response");
  const transitions = await page.locator('[data-transition="Coder->General"]').innerText();
  check(transitions.includes("route_unavailable: 1") && transitions.includes("route_exhausted: 1"), "the Coder → General counter now holds both reasons");
  check((await page.locator('[data-exhausted-route="Coder"]').innerText()).includes("route_unavailable: 1") && !(await page.locator('[data-exhausted-route="Coder"]').innerText()).includes("route_exhausted"), "a fallback that succeeded is not counted as exhausted");
  const exhaustedTrace = page.locator("[data-fallback-trace]", { has: page.locator("[data-trace-exhausted]") });
  check((await exhaustedTrace.count()) === 1, "the earlier exhausted trace is still listed, still exhausted");
  await success.screenshot({ path: `${OUT_DIR}/router-fallback-success-trace.png` });

  // --- Classifier: Jev, as running -----------------------------------------------------
  await page.goto(`${BASE}/#/classifier`, { waitUntil: "domcontentloaded" });
  await page.getByText("Classifier Provider", { exact: true }).waitFor({ timeout: TIMEOUT });
  const jevRadio = page.getByRole("radio", { name: /Jev \/ TypeSafe/ });
  const lwRadio = page.getByRole("radio", { name: /Lightweight/ });
  check(await jevRadio.isChecked(), "the provider selector shows the running provider (Jev)");
  const jevPanel = page.locator('[data-panel="jev"]');
  check(await jevPanel.isVisible(), "the Jev panel is shown for Jev");
  check(
    (await page.locator("[data-privacy-notice]").innerText()).includes("Jev is an external classifier provider"),
    "the external-provider privacy notice is visible",
  );
  check((await page.getByLabel("Base URL").inputValue()) === process.env.EXPECT_BASE_URL, "the base URL is read from the router");
  check((await page.getByLabel("API Key Environment Variable").inputValue()) === "TYPESAFE_API_KEY", "the key's variable name is shown");
  check((await page.locator('[data-key-status="configured"]').count()) === 1, "the key status reads Configured");
  check((await page.getByLabel("Model — required").inputValue()) === "jev-latest", "the model is read from the router");
  check((await page.getByLabel("Timeout (ms) — required").inputValue()) === "5000", "the timeout is read from the router");
  check(
    (await page.locator("[data-include-user-text]").innerText()).includes("structural request traits"),
    "include-user-text Off explains what Jev still receives",
  );
  await page.getByRole("switch", { name: "Include user message text in classifier request" }).click();
  check(
    (await page.locator("[data-include-user-text]").innerText()).includes("bounded last user message may be sent"),
    "include-user-text On explains what is sent",
  );
  for (const route of ["General", "Coder", "Research"]) {
    check((await page.locator(`[data-candidate="${route}"]`).count()) === 1, `candidate ${route} is offered`);
  }
  check((await page.locator('[data-candidate="Auto"]').count()) === 0, "Auto is never a candidate");
  check(
    (await page.getByLabel("Description — Coder").inputValue()).startsWith("Programming"),
    "route descriptions are shown and editable",
  );
  check((await page.getByLabel("Classifier Fallback Route").inputValue()) === "General", "the fallback route is shown");
  check(
    (await page.locator("[data-config-snippet]").innerText()).includes('"provider": "jev"'),
    "a valid draft produces the canonical configuration",
  );
  await page.screenshot({ path: `${OUT_DIR}/router-classifier-jev.png`, fullPage: true });

  // --- Test Connection, against the real router and scripted TypeSafe -----------------
  await page.getByRole("button", { name: "Test Connection" }).click();
  await page.locator('[data-check-status="ok"]').waitFor({ timeout: TIMEOUT });
  check((await page.locator('[data-check-status="ok"]').innerText()).includes("Connected"), "Test Connection reaches the real provider: Connected");

  // Every other status, as the router would report it.
  const statuses = {
    api_key_missing: "API key missing",
    auth_error: "Authentication failed",
    rate_limited: "Provider rate limited the request",
    provider_error: "Provider error",
    connection_error: "Could not reach the provider",
    timeout: "Connection timed out",
    invalid_response: "Unexpected response",
    model_not_listed: "Pinned versions may still be accepted",
  };
  for (const [status, words] of Object.entries(statuses)) {
    await page.route("**/api/router/v1/classifier/check", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          provider: "jev",
          status,
          model: "jev-1.13.0",
          model_listed: status === "model_not_listed" ? false : undefined,
          checked_at: Math.floor(Date.now() / 1000),
          duration_ms: 42,
        }),
      }),
    );
    await page.getByRole("button", { name: "Test Connection" }).click();
    const shown = page.locator(`[data-check-status="${status}"]`);
    await shown.waitFor({ timeout: TIMEOUT });
    const text = await shown.innerText();
    check(text.includes(words) && !/invalid model/i.test(text), `${status} reads as “${words}”`);
    if (status === "model_not_listed") {
      await page.screenshot({ path: `${OUT_DIR}/router-check-model-not-listed.png`, fullPage: true });
    }
    await page.unroute("**/api/router/v1/classifier/check");
  }

  // --- validation ------------------------------------------------------------------------
  const timeout = page.getByLabel("Timeout (ms) — required");
  await timeout.fill("");
  check(await page.getByText(/Required\. Choose a bound/).isVisible(), "an empty timeout is refused as required");
  check(await page.getByRole("button", { name: "Use 5000" }).isVisible(), "5000 ms is offered as a suggestion, not inserted");
  await timeout.fill("999999");
  check(await page.getByText("Must be between 1 and 120000 ms.").isVisible(), "a timeout over 120000 ms is refused");
  check((await page.locator("[data-config-snippet]").count()) === 0, "an invalid draft produces no configuration");
  await timeout.fill("5000");
  await page.getByLabel("Minimum Confidence").fill("1.5");
  check(await page.getByText("Must be a number between 0 and 1.").isVisible(), "a confidence above 1 is refused");
  await page.getByLabel("Minimum Confidence").fill("0.65");
  await page.getByLabel("Maximum Input Characters").fill("0");
  check(await page.getByText("Must be between 1 and 32000.").isVisible(), "a maximum input of 0 is refused");
  await page.getByLabel("Maximum Input Characters").fill("2000");
  await page.getByLabel("Base URL").fill("http://api.typesafe.ai");
  check(await page.getByText(/Must use https/).isVisible(), "a plain-http remote base URL is refused");
  await page.getByLabel("Base URL").fill(process.env.EXPECT_BASE_URL);
  await page.getByLabel("Model — required").fill("jev-1.13.0");
  check(
    (await page.locator("[data-config-snippet]").innerText()).includes('"model": "jev-1.13.0"'),
    "a manually pinned model is accepted",
  );

  // --- switching provider ---------------------------------------------------------------
  await lwRadio.check();
  check(await page.locator('[data-panel="lightweight"]').isVisible(), "choosing Lightweight shows the Lightweight panel");
  check(!(await jevPanel.isVisible()), "and hides the Jev panel");
  check((await page.getByLabel("Classifier Route").inputValue()) === "Research", "the standby Lightweight settings are read");
  check(
    (await page.locator("[data-config-snippet]").innerText()).includes('"provider": "lightweight"'),
    "switching changes only the classifier section",
  );
  await page.screenshot({ path: `${OUT_DIR}/router-classifier-lightweight.png`, fullPage: true });
  await jevRadio.check();
  check(await jevPanel.isVisible(), "choosing Jev again shows the Jev panel");

  // --- a request the pre-commit budget ended (R9.3.2) -------------------------------------
  // A second router, with `pre_commit_budget_ms: 1500`: Coder refuses with 503
  // and General accepts and never answers. The R9.3 trace card must never call
  // such a request served. The two shapes the binary cannot be timed into
  // (a fallback refused just as the budget ran out, and classification ended
  // by it) are rendered from fixtures shaped exactly like the merged router's
  // traces (crates/lightweight-router/tests/request_budget.rs, b11 and b08).
  const BUDGET_BASE = (process.env.BUDGET_PANEL_BASE ?? "").replace(/\/+$/, "");
  check(BUDGET_BASE.length > 0, "the budget router is given to the render");
  const ask = async (body) => {
    const response = await fetch(`${BUDGET_BASE}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: response.status, id: response.headers.get("x-request-id") ?? "", body: await response.json() };
  };
  const cut = await ask({
    model: "Auto",
    tools: [{ type: "function", function: { name: "f", parameters: { type: "object" } } }],
    messages: [{ role: "user", content: "render a budget cut" }],
  });
  check(cut.status === 504 && cut.body?.error?.code === "request_budget_exhausted", `Auto → Coder → General, General cut: 504 request_budget_exhausted (got ${cut.status})`);
  const explicit = await ask({ model: "General", messages: [{ role: "user", content: "render an explicit cut" }] });
  check(explicit.status === 504 && explicit.body?.error?.code === "request_budget_exhausted", `an explicit General request is cut too: 504 (got ${explicit.status})`);

  const refusedId = "render-budget-refused";
  const classifierId = "render-budget-classifier";
  const fixtures = [
    {
      request_id: refusedId, received_at: 0, route: "Coder", requested_route: "Auto",
      outcome: "request_budget_exhausted", status: 504,
      attempts: [{ route: "Coder", deployment: "coder/Coder", reason: "priority", outcome: "failed", upstream_status: 503 }],
      request_budget: { configured_ms: 1500, elapsed_ms: 1602, remaining_ms: 0, exhausted: true,
                        stage: "cross_route_fallback", next_unattempted_route: "General" },
      cross_route_fallback: { initial_route: "Coder", final_route: "Coder", exhausted: false,
                              attempts: [{ route: "Coder", outcome: "failed", reason: "route_exhausted" }] },
    },
    {
      request_id: classifierId, received_at: 0, route: "Auto", requested_route: "Auto",
      outcome: "request_budget_exhausted", status: 504, attempts: [],
      request_budget: { configured_ms: 1500, elapsed_ms: 1501, remaining_ms: 0, exhausted: true, stage: "classifier" },
    },
  ];
  const budgetPage = await context.newPage();
  budgetPage.on("pageerror", (err) => errors.push(String(err)));
  await budgetPage.route(/\/api\/router\/v1\/traces/, async (route) => {
    const response = await route.fetch();
    const body = await response.json();
    body.data = [...fixtures, ...(body.data ?? [])];
    await route.fulfill({ response, json: body });
  });
  await budgetPage.goto(`${BUDGET_BASE}/#/auto`, { waitUntil: "domcontentloaded" });
  const cutTrace = budgetPage.locator(`[data-fallback-trace="${cut.id}"]`);
  await cutTrace.waitFor({ timeout: TIMEOUT });
  const cutText = await cutTrace.innerText();
  check(cutText.includes("Budget expired while attempting General"), "a budget cut on General reads: Budget expired while attempting General");
  check(!cutText.includes("Served by"), "a budget cut on General never reads as served");
  check((await cutTrace.locator('[data-trace-verdict="budget"]').count()) === 1, "its verdict is the budget, not served or exhausted");
  check((await cutTrace.locator("[data-trace-budget]").innerText()).includes("504 request_budget_exhausted before any response started"), "it says the client got 504 before any response started");
  check((await cutTrace.locator("[data-trace-exhausted]").count()) === 0 && !cutText.includes("Exhausted"), "a budget cut is not shown as an exhausted list");
  check((await cutTrace.locator('[data-trace-step="General"]').innerText()).includes("Budget expired (request_budget_exhausted)"), "General's step reads Budget expired");
  check((await cutTrace.locator('[data-trace-step="Coder"]').innerText()).includes("Route exhausted (route_exhausted)"), "Coder's step keeps its own fallback reason");
  await cutTrace.screenshot({ path: `${OUT_DIR}/router-budget-cut-trace.png` });

  const refused = budgetPage.locator(`[data-fallback-trace="${refusedId}"]`);
  const refusedText = await refused.innerText();
  check(refusedText.includes("Budget expired before attempting General"), "a fallback refused by the budget reads: Budget expired before attempting General");
  check(!refusedText.includes("Served by") && !refusedText.includes("while attempting General"), "the refused route is never shown as served or attempted");
  const refusedSteps = await refused.locator("[data-trace-step]").evaluateAll((items) => items.map((item) => item.getAttribute("data-trace-step")));
  check(JSON.stringify(refusedSteps) === JSON.stringify(["Coder"]), `only Coder is listed as a step (got ${JSON.stringify(refusedSteps)})`);
  await refused.screenshot({ path: `${OUT_DIR}/router-budget-refused-trace.png` });

  check((await budgetPage.locator(`[data-fallback-trace="${explicit.id}"]`).count()) === 0, "an explicit request never appears as a cross-route fallback");
  check((await budgetPage.locator(`[data-fallback-trace="${classifierId}"]`).count()) === 0, "classification ended by the budget invents no route and no fallback");
  check((await budgetPage.locator('[data-trace-verdict="served"]').count()) === 0, "nothing on the budget router reads as served");
  await budgetPage.close();
  // The first router's traces are untouched by any of this: success still reads
  // served and the exhausted list still reads exhausted (checked above).

  // --- a request a context overflow ended, and a stream that outlived its budget --------
  // Two more routers, both Auto → Coder (503) → General. On the first, General
  // refuses the prompt with `400 context_length_exceeded`: R9.3's block keeps
  // `exhausted: false` (the chain stopped, the list did not run out), and the
  // card must not call it served. On the second, General streams for about
  // two seconds past a 1500 ms pre-commit budget: committed, so served.
  const OVERFLOW_BASE = (process.env.OVERFLOW_PANEL_BASE ?? "").replace(/\/+$/, "");
  const STREAM_BASE = (process.env.STREAM_PANEL_BASE ?? "").replace(/\/+$/, "");
  check(OVERFLOW_BASE.length > 0 && STREAM_BASE.length > 0, "the overflow and stream routers are given to the render");
  const send = async (base, body) => {
    const started = Date.now();
    const response = await fetch(`${base}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const text = await response.text();
    return { status: response.status, id: response.headers.get("x-request-id") ?? "", text, ms: Date.now() - started };
  };
  const toolRequest = (content, extra = {}) => ({
    model: "Auto",
    tools: [{ type: "function", function: { name: "f", parameters: { type: "object" } } }],
    messages: [{ role: "user", content }],
    ...extra,
  });

  const overflow = await send(OVERFLOW_BASE, toolRequest("render a context overflow"));
  check(overflow.status === 400 && overflow.text.includes("context_length_exceeded"), `Auto → Coder → General, General overflows: 400 context_length_exceeded (got ${overflow.status})`);
  const overflowPage = await context.newPage();
  overflowPage.on("pageerror", (err) => errors.push(String(err)));
  await overflowPage.goto(`${OVERFLOW_BASE}/#/auto`, { waitUntil: "domcontentloaded" });
  const overflowTrace = overflowPage.locator(`[data-fallback-trace="${overflow.id}"]`);
  await overflowTrace.waitFor({ timeout: TIMEOUT });
  const overflowText = await overflowTrace.innerText();
  check(overflowText.includes("Context limit exceeded while attempting General"), "a context overflow on General reads: Context limit exceeded while attempting General");
  check(!overflowText.includes("Served by"), "a context overflow on General never reads as served");
  check(!overflowText.includes("Exhausted") && (await overflowTrace.locator("[data-trace-exhausted]").count()) === 0, "a context overflow is not shown as an exhausted list");
  check(!overflowText.includes("Budget"), "a context overflow is not shown as a budget expiry");
  check((await overflowTrace.locator('[data-trace-verdict="context_overflow"]').count()) === 1, "its verdict is the context overflow");
  check((await overflowTrace.locator("[data-trace-context-overflow]").innerText()).includes("the client got General's own 400 context_length_exceeded"), "it says the client got General's own 400");
  check((await overflowTrace.locator('[data-trace-step="General"]').innerText()).includes("Context overflow (context_length_exceeded)"), "General's step reads Context overflow");
  check((await overflowTrace.locator('[data-trace-step="Coder"]').innerText()).includes("Route exhausted (route_exhausted)"), "Coder's step keeps its own fallback reason");
  await overflowTrace.screenshot({ path: `${OUT_DIR}/router-context-overflow-trace.png` });
  const explicitOverflow = await send(OVERFLOW_BASE, { model: "General", messages: [{ role: "user", content: "render an explicit overflow" }] });
  check(explicitOverflow.status === 400, `an explicit General request overflows too: 400 (got ${explicitOverflow.status})`);
  await overflowPage.reload({ waitUntil: "domcontentloaded" });
  await overflowPage.locator(`[data-fallback-trace="${overflow.id}"]`).waitFor({ timeout: TIMEOUT });
  check((await overflowPage.locator(`[data-fallback-trace="${explicitOverflow.id}"]`).count()) === 0, "an explicit overflow never appears as a cross-route fallback");
  check((await overflowPage.locator('[data-trace-verdict="served"]').count()) === 0, "nothing on the overflow router reads as served");
  await overflowPage.close();

  const streamed = await send(STREAM_BASE, toolRequest("render a stream", { stream: true }));
  check(streamed.status === 200 && streamed.text.includes("data: [DONE]") && streamed.ms > 1500, `Auto → Coder → General streams 200 to [DONE] past the 1500 ms budget (${streamed.ms} ms)`);
  const streamPage = await context.newPage();
  streamPage.on("pageerror", (err) => errors.push(String(err)));
  await streamPage.goto(`${STREAM_BASE}/#/auto`, { waitUntil: "domcontentloaded" });
  const streamTrace = streamPage.locator(`[data-fallback-trace="${streamed.id}"]`);
  await streamTrace.waitFor({ timeout: TIMEOUT });
  const streamText = await streamTrace.innerText();
  check(streamText.includes("Served by General") && (await streamTrace.locator('[data-trace-verdict="served"]').count()) === 1, "a committed stream reads: Served by General");
  check(!streamText.includes("Budget") && !streamText.includes("Context limit"), "a committed stream has no budget or context-overflow wording");
  await streamTrace.screenshot({ path: `${OUT_DIR}/router-stream-served-trace.png` });
  await streamPage.close();

  // --- an error that is neither a fallback reason nor a context overflow -----------------
  // General commits a plain 400 on one router and a 500 on another (neither
  // moves a request on): `exhausted: false`, final route General, and nothing
  // served. "Served by" needs `outcome: "ok"`; these read neutrally instead.
  for (const [name, envName, status, outcome] of [
    ["client-error", "CLIENT_ERROR_PANEL_BASE", 400, "client_error"],
    ["server-error", "SERVER_ERROR_PANEL_BASE", 500, "server_error"],
  ]) {
    const base = (process.env[envName] ?? "").replace(/\/+$/, "");
    check(base.length > 0, `the ${name} router is given to the render`);
    const answer = await send(base, toolRequest(`render a ${name}`));
    check(answer.status === status, `Auto → Coder → General, General answers ${status} (got ${answer.status})`);
    const page = await context.newPage();
    page.on("pageerror", (err) => errors.push(String(err)));
    await page.goto(`${base}/#/auto`, { waitUntil: "domcontentloaded" });
    const card = page.locator(`[data-fallback-trace="${answer.id}"]`);
    await card.waitFor({ timeout: TIMEOUT });
    const text = await card.innerText();
    check(text.includes("Request ended while attempting General"), `a ${status} on General reads: Request ended while attempting General`);
    check(!text.includes("Served by"), `a ${status} on General never reads as served, though the list was not exhausted`);
    check(!text.includes("Exhausted") && !text.includes("Budget") && !text.includes("Context limit"), `a ${status} on General guesses no exhaustion, budget or context cause`);
    check((await card.locator('[data-trace-verdict="unsuccessful"]').count()) === 1, `its verdict is unsuccessful`);
    check((await card.locator("[data-trace-unsuccessful]").innerText()).includes(`outcome ${outcome}, status ${status}`), `it names the outcome ${outcome} and status ${status}`);
    await card.screenshot({ path: `${OUT_DIR}/router-${name}-trace.png` });
    await page.close();
  }

  // --- the key never reaches the browser ------------------------------------------------
  const html = await page.content();
  const storage = await page.evaluate(() => JSON.stringify({ ...localStorage, ...sessionStorage }));
  check(!html.includes(SECRET), "the TypeSafe key is not in the page");
  check(!storage.includes(SECRET), "the TypeSafe key is not in browser storage");
  const leaked = bodies.filter((body) => body.text.includes(SECRET)).map((body) => body.url);
  check(leaked.length === 0, `the TypeSafe key is in no response (${bodies.length} inspected)${leaked.length ? `: ${leaked.join(", ")}` : ""}`);
  check(errors.length === 0, `no uncaught page error${errors.length ? `: ${errors[0]}` : ""}`);

  await context.close();
  await browser.close();

  console.log(`\nScreenshots written to ${OUT_DIR}/`);
  if (failures.length) {
    console.error(`\n${failures.length} router render check(s) failed:`);
    for (const f of failures) console.error(`  - ${f}`);
    process.exit(1);
  }
  console.log("All router render checks passed.");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
