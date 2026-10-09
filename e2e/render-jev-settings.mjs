// Render Jev Settings on real routers (`hermes router --web-root`) and assert
// what an operator sees: the saved settings read from the router, the masked
// key, inline validation, the admin token, Save, refused and failed saves,
// Pending Restart, Test Connection on the *running* settings, every
// connection state in words — and that neither the typed key nor the admin
// token reaches a response body, a URL, browser storage or the page once saved.
//
// Two phases, with a real restart between them (scripts/render-panel.sh):
//   save       — a router running the Lightweight provider is switched to Jev
//                and given a key; it reports Pending Restart.
//   restarted  — the same router, restarted on the saved file: Jev is running.
//
// Environment:
//   PHASE              save | restarted
//   SETTINGS_BASE      the settings router's origin
//   MAIN_BASE          the main render router (store unavailable, env key)
//   ADMIN_TOKEN        the settings router's admin token (from its token file)
//   TYPED_KEY          the key the operator types (phase save)
//   EXPECT_BASE_URL    the scripted TypeSafe's URL
//   OUT_DIR            where screenshots land

import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";

const PHASE = process.env.PHASE ?? "save";
const BASE = (process.env.SETTINGS_BASE ?? "").replace(/\/+$/, "");
const MAIN = (process.env.MAIN_BASE ?? "").replace(/\/+$/, "");
const TOKEN = process.env.ADMIN_TOKEN ?? "";
const TYPED_KEY = process.env.TYPED_KEY ?? "";
const JEV_URL = process.env.EXPECT_BASE_URL ?? "";
const OUT_DIR = process.env.OUT_DIR ?? "screens";
const TIMEOUT = Number(process.env.RENDER_SETTLE_MS ?? 15000);

const failures = [];
function check(condition, message) {
  if (!condition) failures.push(message);
  console.log(`  [${condition ? "ok" : "FAIL"}] ${message}`);
}

const card = (page) => page.locator(".card", { has: page.locator("[data-jev-settings]") });

async function openSettings(page, base) {
  await page.goto(`${base}/#/classifier`, { waitUntil: "domcontentloaded" });
  await page.locator("[data-jev-settings]").waitFor({ timeout: TIMEOUT });
}

async function shot(page, name) {
  await card(page).screenshot({ path: `${OUT_DIR}/${name}.png` });
}

async function storage(page) {
  return page.evaluate(() => {
    const all = [];
    for (const store of [window.localStorage, window.sessionStorage]) {
      for (let i = 0; i < store.length; i += 1) {
        const key = store.key(i);
        all.push(`${key}=${store.getItem(key)}`);
      }
    }
    return all.join("\n");
  });
}

async function phaseSave(page, bodies, urls) {
  // --- a router with no credential store, its key in the environment -------------------
  await openSettings(page, MAIN);
  const main = card(page);
  check((await main.locator('[data-key-state="configured"]').count()) === 1, "main router: the key reads as configured");
  check((await main.locator('[data-key-source="environment"]').innerText()).includes("environment variable TYPESAFE_API_KEY"), "main router: the key is from the environment variable");
  check(await main.locator('[data-field="api_key"]').isDisabled(), "main router: with no credential store, the key field is disabled");
  check((await main.locator("[data-running-settings]").innerText()).includes(`Jev at ${JEV_URL}`), "main router: the running Jev endpoint is shown");
  await shot(page, "router-jev-settings-env");

  // --- initial load ----------------------------------------------------------------------
  await openSettings(page, BASE);
  const c = card(page);
  const provider = c.locator("[data-settings-provider]");
  check(await provider.getByLabel("Lightweight").isChecked(), "initial load: the saved provider (Lightweight) is selected");
  check((await c.locator("[data-jev-fields]").count()) === 0, "initial load: no Jev fields for Lightweight");
  check((await c.locator("[data-pending-restart]").count()) === 0, "initial load: nothing is pending");
  check((await c.locator("[data-running-settings]").innerText()).includes("Lightweight provider"), "initial load: the running provider is Lightweight");
  await shot(page, "router-jev-settings-initial");

  // --- switch to Jev: defaults, masked key -------------------------------------------------
  await provider.getByLabel("Jev / TypeSafe").check();
  await c.locator("[data-jev-fields]").waitFor({ timeout: TIMEOUT });
  const endpoint = c.locator('[data-field="base_url"]');
  const key = c.locator('[data-field="api_key"]');
  check((await endpoint.inputValue()) === "https://api.typesafe.ai", "Jev: the endpoint defaults to https://api.typesafe.ai");
  check((await c.locator('[data-field="model"]').inputValue()) === "jev-latest", "Jev: the model defaults to jev-latest");
  check((await key.inputValue()) === "", "Jev: the key field is never prefilled");
  check((await key.getAttribute("type")) === "password", "Jev: the key field is masked");
  check((await c.locator('[data-key-state="missing"]').count()) === 1, "Jev: with no key anywhere, the key reads as missing");

  // --- inline validation ------------------------------------------------------------------
  await endpoint.fill("http://api.typesafe.ai");
  await endpoint.blur();
  const endpointError = c.locator(".field", { has: page.locator('[data-field="base_url"]') }).locator(".field__error");
  check((await endpointError.innerText()).includes("https"), "editing: a plain-http endpoint is refused inline");
  await endpoint.fill(JEV_URL);
  await endpoint.blur();
  check((await endpointError.count()) === 0, "editing: a loopback endpoint clears the error");
  await c.locator('[data-field="timeout_ms"]').fill("5000");

  // --- the key: typed, shown, hidden ---------------------------------------------------------
  await key.fill(TYPED_KEY);
  await c.locator("[data-toggle-key]").click();
  check((await key.getAttribute("type")) === "text", "Show reveals the typed key");
  check((await c.locator("[data-toggle-key]").getAttribute("aria-label")) === "Hide API key", "the toggle is labelled for what it does");
  await c.locator("[data-toggle-key]").click();
  check((await key.getAttribute("type")) === "password", "Hide masks it again");
  await shot(page, "router-jev-settings-editing");

  // --- refused saves ----------------------------------------------------------------------
  await c.locator("[data-save-settings]").click();
  check((await c.locator('[data-settings-notice="warn"]').innerText()).includes("admin token"), "Save without the admin token asks for it");
  await c.locator("[data-admin-token]").fill("not-the-token-0000000000000000000000");
  await c.locator("[data-save-settings]").click();
  await c.locator('[data-settings-notice="danger"]').waitFor({ timeout: TIMEOUT });
  check((await c.locator('[data-settings-notice="danger"]').innerText()).includes("hermes router admin-token"), "a wrong admin token is refused, with the command to get the right one");
  check((await key.inputValue()) === TYPED_KEY, "a refused save keeps what was typed");
  await shot(page, "router-jev-settings-refused");

  // --- Save --------------------------------------------------------------------------------
  await c.locator("[data-admin-token]").fill(TOKEN);
  await c.locator("[data-save-settings]").click();
  await c.locator("[data-pending-restart]").waitFor({ timeout: TIMEOUT });
  check((await c.locator('[data-settings-notice="info"]').innerText()).includes("credential store"), "Save: the key went to the credential store");
  check((await c.locator("[data-restart-pill]").innerText()).includes("Pending Restart"), "Save: Pending Restart is shown");
  check((await c.locator("[data-pending-restart]").innerText()).includes("Test Connection checks the running settings"), "Pending Restart: Test Connection is said to check the running settings");
  check((await key.inputValue()) === "", "Save: the key field is cleared");
  check((await c.locator('[data-key-source="credential_store"]').count()) === 1, "Save: the key now reads as from the credential store");
  check((await c.locator('[data-key-state="configured"]').count()) === 1, "Save: the key reads as configured");
  check((await c.locator("[data-running-settings]").innerText()).includes("Lightweight provider"), "Save: the running provider is still Lightweight until a restart");
  await shot(page, "router-jev-settings-pending-restart");

  // --- Test Connection checks what is running ---------------------------------------------------
  await c.locator("[data-test-connection]").click();
  await c.locator("[data-check-status]").waitFor({ timeout: TIMEOUT });
  const checked = await c.locator("[data-check-status]").getAttribute("data-check-status");
  check(checked === "ok" || checked === "route_unavailable", `Test Connection checked the running Lightweight provider (got ${checked})`);

  // --- removing the key the saved settings need is refused ----------------------------------
  page.once("dialog", (dialog) => dialog.accept());
  await c.locator("[data-remove-key]").click();
  await c.locator('[data-settings-notice="danger"]').waitFor({ timeout: TIMEOUT });
  check((await c.locator('[data-settings-notice="danger"]').innerText()).includes("switch the provider or set the environment variable"), "removing a key the saved settings need is refused");

  // --- a reload reads the pending state from the router, not the browser ------------------------
  // A real reload: the token and key typed so far were page state only.
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.locator("[data-jev-settings]").waitFor({ timeout: TIMEOUT });
  check((await card(page).locator("[data-admin-token]").inputValue()) === "", "after a reload the admin token must be entered again");
  check((await card(page).locator("[data-pending-restart]").count()) === 1, "after a reload Pending Restart is still shown (read from the router)");
  check(await card(page).locator("[data-settings-provider]").getByLabel("Jev / TypeSafe").isChecked(), "after a reload the saved provider (Jev) is selected");
  check((await card(page).locator('[data-field="api_key"]').inputValue()) === "", "after a reload the key field is empty");

  // --- no secret leaves the router ---------------------------------------------------------------
  const dom = await page.content();
  check(!dom.includes(TYPED_KEY), "the saved key is in no part of the page");
  check(!dom.includes(TOKEN), "the admin token is in no part of the page after a reload");
  const stored = await storage(page);
  check(!stored.includes(TYPED_KEY) && !stored.includes(TOKEN), "neither the key nor the admin token is in browser storage");
  check(bodies.length > 0 && bodies.every((body) => !body.text.includes(TYPED_KEY) && !body.text.includes(TOKEN)), `neither appears in any of ${bodies.length} response bodies`);
  check(urls.every((url) => !url.includes(TYPED_KEY) && !url.includes(TOKEN)), "neither appears in any request URL");
}

async function phaseRestarted(page) {
  await openSettings(page, BASE);
  const c = card(page);
  check((await c.locator("[data-pending-restart]").count()) === 0, "after the restart nothing is pending");
  check((await c.locator("[data-running-settings]").innerText()).includes(`Jev at ${JEV_URL}`), "after the restart Jev is running on the saved endpoint");
  check((await c.locator('[data-field="base_url"]').inputValue()) === JEV_URL, "after the restart the saved endpoint is read back");

  await c.locator("[data-test-connection]").click();
  await c.locator('[data-check-status="ok"]').waitFor({ timeout: TIMEOUT });
  check((await c.locator('[data-check-status="ok"]').innerText()).includes("Connected"), "Test Connection on the running Jev settings: Connected");
  check((await c.locator("[data-connection-state]").innerText()).includes("Connected"), "the connection pill reads Connected");
  await shot(page, "router-jev-settings-connected");

  const states = {
    auth_error: "Authentication failed",
    rate_limited: "rate limited",
    timeout: "timed out",
    model_not_listed: "not listed",
    connection_error: "Could not reach",
  };
  for (const [status, words] of Object.entries(states)) {
    await page.route("**/api/router/v1/classifier/check", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ provider: "jev", status, model: "jev-latest", checked_at: Math.floor(Date.now() / 1000), duration_ms: 12, http_status: status === "auth_error" ? 401 : status === "rate_limited" ? 429 : undefined }),
      }),
    );
    await c.locator("[data-test-connection]").click();
    await c.locator(`[data-check-status="${status}"]`).waitFor({ timeout: TIMEOUT });
    const shown = await c.locator(`[data-check-status="${status}"]`).innerText();
    check(shown.toLowerCase().includes(words.toLowerCase()), `Test Connection ${status} reads “${words}”`);
    const pill = await c.locator("[data-connection-state]").innerText();
    check(status === "model_not_listed" ? pill.includes("not listed") : pill.startsWith("Error"), `the connection pill for ${status} reads ${pill}`);
    if (status === "auth_error") await shot(page, "router-jev-settings-auth-error");
    await page.unroute("**/api/router/v1/classifier/check");
  }

  // --- smoke: an Auto request classified by Jev with the saved settings ---------------------
  // The scripted TypeSafe sends a request about code to Coder and anything
  // else to General, and only with the right key: a failed classification
  // would fall back to General, so Coder proves Jev answered.
  const classified = async () => (await (await fetch(`${JEV_URL}/stats`)).json()).systemone;
  const before = await classified();
  const auto = async (text) => {
    const response = await fetch(`${BASE}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "Auto", messages: [{ role: "user", content: text }] }),
    });
    return { status: response.status, model: (await response.json()).model };
  };
  const coder = await auto("Write a Rust function that parses a config file");
  check(coder.status === 200 && coder.model === "Coder", `an Auto request about code is classified by Jev to Coder (got ${coder.status} ${coder.model})`);
  const general = await auto("Good morning! How are you today?");
  check(general.status === 200 && general.model === "General", `an Auto greeting is classified by Jev to General (got ${general.status} ${general.model})`);
  check((await classified()) === before + 2, "both Auto requests were answered by Jev, with the saved key");
  const traces = await (await fetch(`${BASE}/api/router/v1/traces?limit=5`)).json();
  const text = JSON.stringify(traces);
  check(text.includes("semantic") && text.includes("Coder"), "the routing trace names the semantic rule and the classified route");

  // A save the router refuses lists every problem; one it fails says nothing changed.
  await c.locator("[data-admin-token]").fill(TOKEN);
  await page.route("**/api/router/v1/classifier/settings", (route) =>
    route.request().method() === "PUT"
      ? route.fulfill({
          status: 422,
          contentType: "application/json",
          body: JSON.stringify({
            error: { message: "the router would refuse these settings", type: "invalid_request_error", code: "invalid_settings" },
            errors: ["auto_route.classifier: jev.timeout_ms is required"],
          }),
        })
      : route.continue(),
  );
  await c.locator("[data-save-settings]").click();
  await c.locator('[data-settings-notice="danger"]').waitFor({ timeout: TIMEOUT });
  check((await c.locator('[data-settings-notice="danger"]').innerText()).includes("jev.timeout_ms is required"), "a refused save lists the router's problems");
  await page.unroute("**/api/router/v1/classifier/settings");
  await page.route("**/api/router/v1/classifier/settings", (route) =>
    route.request().method() === "PUT"
      ? route.fulfill({
          status: 500,
          contentType: "application/json",
          body: JSON.stringify({
            error: { message: "the configuration file could not be written; the previous settings and key are unchanged", type: "invalid_request_error", code: "save_failed" },
          }),
        })
      : route.continue(),
  );
  await c.locator("[data-save-settings]").click();
  await page.waitForFunction(() => document.querySelector('[data-settings-notice="danger"]')?.textContent?.includes("unchanged"), null, { timeout: TIMEOUT });
  check((await c.locator('[data-settings-notice="danger"]').innerText()).includes("previous settings and key are unchanged"), "a failed save says the previous settings are intact");
  await shot(page, "router-jev-settings-save-failed");
  await page.unroute("**/api/router/v1/classifier/settings");
  check((await storage(page)).length === 0 || !(await storage(page)).includes(TOKEN), "the admin token is not in browser storage");
}

async function main() {
  if (!BASE || !TOKEN || !JEV_URL) throw new Error("SETTINGS_BASE, ADMIN_TOKEN and EXPECT_BASE_URL are required");
  if (PHASE === "save" && (!TYPED_KEY || !MAIN)) throw new Error("TYPED_KEY and MAIN_BASE are required");
  await mkdir(OUT_DIR, { recursive: true });
  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 2400 }, colorScheme: "dark", deviceScaleFactor: 2 });
  const page = await context.newPage();
  const errors = [];
  const bodies = [];
  const urls = [];
  page.on("pageerror", (err) => errors.push(String(err)));
  page.on("console", (message) => {
    if (TYPED_KEY && message.text().includes(TYPED_KEY)) errors.push("the typed key reached the console");
    if (message.text().includes(TOKEN)) errors.push("the admin token reached the console");
  });
  page.on("request", (request) => urls.push(request.url()));
  page.on("response", async (response) => {
    try {
      bodies.push({ url: response.url(), text: await response.text() });
    } catch {
      // A redirect or aborted response has no body to inspect.
    }
  });

  if (PHASE === "save") await phaseSave(page, bodies, urls);
  else await phaseRestarted(page);

  check(errors.length === 0, `no page errors or secrets in the console (${errors.join("; ") || "none"})`);
  await browser.close();
  if (failures.length > 0) {
    console.error(`\n${failures.length} Jev Settings check(s) failed:`);
    for (const failure of failures) console.error(`  - ${failure}`);
    process.exit(1);
  }
  console.log(`\nJev Settings render (${PHASE}) passed.`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
