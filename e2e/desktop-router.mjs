// The real desktop app, headless under xvfb, with a real Gateway and a real
// Router: Router & Jev Settings opens the Router's own panel in its own window,
// and nothing is joined that should stay apart.
//
// Driven through the app's own menu items, clicked from the main process by id
// (`router-start`, `router-settings`, …), so no dialog is involved and no
// test-only code is in the product. The app is launched as it ships: the
// Electron binary with the sandbox on (the app refuses `--no-sandbox`), and no
// loader injected.
//
// Two modes, one per run (scripts/render-desktop.sh runs both):
//
//   MODE=start   The app starts the Gateway itself; the Router is started from
//                the Router menu, from its own config file found through
//                `hermes router config-path`. Quit must stop both.
//   MODE=attach  A Gateway and a Router are already serving, started outside
//                the app. The app attaches to both; quit must leave both.
//
// Properties checked in both: the Gateway window shows the Gateway's screens
// and calls only the Gateway; the Router window shows Auto Routing and the
// Classifier with its Jev Settings card and calls only the Router; Test
// Connection reaches only the Router; and the Jev key and the Router's admin
// token appear in no DOM, browser storage, response body, request URL, app log
// or artifact.
//
// Environment:
//   MODE, ELECTRON_BIN, APP_DIR, HERMES_BIN, GATEWAY_PORT, ROUTER_PORT,
//   ROUTER_CONFIG (the file the Router reads), JEV_KEY, OUT_DIR, and the
//   app's own environment (HERMES_GATEWAY_HOME, …) passed straight through.

import { execFile } from "node:child_process";
import { createWriteStream } from "node:fs";
import { mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";
import { _electron as electron } from "playwright";

const MODE = process.env.MODE ?? "start";
const ELECTRON_BIN = required("ELECTRON_BIN");
const APP_DIR = required("APP_DIR");
const HERMES_BIN = required("HERMES_BIN");
const ROUTER_CONFIG = required("ROUTER_CONFIG");
const JEV_KEY = required("JEV_KEY");
const GATEWAY_PORT = Number(required("GATEWAY_PORT"));
const ROUTER_PORT = Number(required("ROUTER_PORT"));
const OUT_DIR = process.env.OUT_DIR ?? "desktop-screens";
const TIMEOUT = Number(process.env.RENDER_SETTLE_MS ?? 30000);

const GATEWAY = `http://127.0.0.1:${GATEWAY_PORT}`;
const ROUTER = `http://127.0.0.1:${ROUTER_PORT}`;

const failures = [];
function check(condition, message) {
  if (!condition) failures.push(message);
  console.log(`  [${condition ? "ok" : "FAIL"}] ${message}`);
}

function required(name) {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}

const run = promisify(execFile);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** What is on a port, as the app's own supervisors judge it. */
async function holder(port) {
  try {
    const response = await fetch(`http://127.0.0.1:${port}/version`, { signal: AbortSignal.timeout(2000) });
    const body = await response.json().catch(() => ({}));
    return typeof body.build === "string" && body.build.startsWith("lightweight-router-") ? "router" : "gateway";
  } catch {
    return "nothing";
  }
}

/** Every request a page makes, and every same-origin response body, for the leak and origin checks. */
function record(page) {
  const seen = { urls: [], bodies: [] };
  page.on("request", (request) => seen.urls.push(request.url()));
  page.on("response", async (response) => {
    try {
      seen.bodies.push({ url: response.url(), text: await response.text() });
    } catch {
      // A redirect or an aborted response has no body.
    }
  });
  return seen;
}

/** Requests that left the page's own origin. Only http(s): data:/blob: URLs are the page's own. */
function offOrigin(urls, origin) {
  return urls.filter((url) => /^https?:/.test(url) && new URL(url).origin !== origin);
}

async function storage(page) {
  return page.evaluate(() => {
    const dump = (area) => Object.keys(area).map((key) => `${key}=${area.getItem(key)}`).join("\n");
    return `${dump(window.localStorage)}\n${dump(window.sessionStorage)}`;
  });
}

const menuLabel = (app, id) =>
  app.evaluate(({ Menu }, itemId) => Menu.getApplicationMenu()?.getMenuItemById(itemId)?.label ?? null, id);
const menuEnabled = (app, id) =>
  app.evaluate(({ Menu }, itemId) => Menu.getApplicationMenu()?.getMenuItemById(itemId)?.enabled ?? null, id);
const clickMenu = (app, id) =>
  app.evaluate(({ Menu }, itemId) => {
    const item = Menu.getApplicationMenu()?.getMenuItemById(itemId);
    if (!item) throw new Error(`no menu item ${itemId}`);
    item.click();
  }, id);

async function waitForStatus(app, pattern) {
  const deadline = Date.now() + TIMEOUT * 2;
  let label = null;
  while (Date.now() < deadline) {
    label = await menuLabel(app, "router-status");
    if (label && pattern.test(label)) return label;
    await sleep(250);
  }
  throw new Error(`the Router status never matched ${pattern}; last: ${JSON.stringify(label)}`);
}

async function main() {
  await mkdir(OUT_DIR, { recursive: true });
  const logPath = join(OUT_DIR, `desktop-${MODE}.log`);
  const log = createWriteStream(logPath);

  console.log(`== desktop (${MODE}) ==`);
  // An editor's integrated terminal (VS Code's, for one) exports
  // ELECTRON_RUN_AS_NODE, which turns the Electron binary into plain Node.
  const env = { ...process.env };
  delete env.ELECTRON_RUN_AS_NODE;
  const app = await electron.launch({
    executablePath: ELECTRON_BIN,
    args: [APP_DIR],
    env,
    timeout: TIMEOUT * 2,
  });
  app.process().stdout?.pipe(log);
  app.process().stderr?.pipe(log);

  let routerPage = null;
  let gatewaySeen = null;
  let routerSeen = null;
  let adminToken = "";
  try {
    // --- the Gateway window, exactly as before ----------------------------------------
    const gatewayPage = await app.firstWindow({ timeout: TIMEOUT * 2 });
    gatewaySeen = record(gatewayPage);
    await gatewayPage.waitForURL(`${GATEWAY}/**`, { timeout: TIMEOUT });
    check(gatewayPage.url().startsWith(`${GATEWAY}/`), `the main window is the Gateway's origin (${gatewayPage.url()})`);
    await gatewayPage.getByText("Dashboard", { exact: true }).first().waitFor({ timeout: TIMEOUT });
    check((await gatewayPage.getByText("Auto Routing", { exact: true }).count()) === 0, "the Gateway window offers no Router screen");
    await gatewayPage.getByRole("link", { name: "Models" }).first().click();
    await gatewayPage.waitForURL(/#\/models/, { timeout: TIMEOUT });
    check(true, "the Gateway panel navigates (Dashboard → Models)");
    check(await gatewayPage.evaluate(() => "hermesShell" in window), "the Gateway window keeps its shell bridge");
    await gatewayPage.screenshot({ path: join(OUT_DIR, `desktop-${MODE}-gateway.png`) });

    // --- the Router, started on request or attached ---------------------------------
    if (MODE === "start") {
      const initial = await waitForStatus(app, /Router — /);
      check(/Not started/.test(initial), `no Router is started at launch (“${initial}”)`);
      check((await holder(ROUTER_PORT)) === "nothing", "nothing listens on the Router port before Start Router");
      check((await menuEnabled(app, "router-stop")) === false, "Stop Router is offered only for a Router the app owns");
      await clickMenu(app, "router-start");
      const running = await waitForStatus(app, /Running on port/);
      check(running.includes(String(ROUTER_PORT)), `Start Router runs it on its own port (“${running}”)`);
      check((await menuEnabled(app, "router-restart")) === true, "Restart Router is offered for the Router the app owns");
    } else {
      const attached = await waitForStatus(app, /Attached to port/);
      check(attached.includes(String(ROUTER_PORT)), `an independently started Router is attached (“${attached}”)`);
      check((await menuEnabled(app, "router-restart")) === false, "an attached Router cannot be restarted from the app");
      check((await menuEnabled(app, "router-stop")) === false, "an attached Router cannot be stopped from the app");
    }
    check((await holder(GATEWAY_PORT)) === "gateway", "the Gateway is still serving beside the Router");

    // The token exists only while the Router runs; read it the documented way,
    // to prove it never reaches the app's pages.
    const token = await run(HERMES_BIN, ["router", "admin-token", "--config", ROUTER_CONFIG], { env: process.env });
    adminToken = token.stdout.trim();
    check(adminToken.length >= 16, "the running Router has an admin token to keep out of every page");

    // --- Router & Jev Settings… -------------------------------------------------------
    const opened = app.waitForEvent("window", { timeout: TIMEOUT });
    await clickMenu(app, "router-settings");
    routerPage = await opened;
    routerSeen = record(routerPage);
    await routerPage.waitForURL(`${ROUTER}/**`, { timeout: TIMEOUT });
    check(routerPage.url().startsWith(`${ROUTER}/`), `Router & Jev Settings loads the Router's origin (${routerPage.url()})`);
    await routerPage.getByRole("heading", { name: "Auto Routing", exact: true }).waitFor({ timeout: TIMEOUT });
    check(true, "the Router window opens on Auto Routing");
    check((await routerPage.getByText("Dashboard", { exact: true }).count()) === 0, "the Router window offers no Gateway screen");
    check(await routerPage.evaluate(() => !("hermesShell" in window)), "the Router window has no shell bridge");
    check((await gatewayPage.url()).startsWith(`${GATEWAY}/`), "the Gateway window still shows the Gateway");
    await routerPage.screenshot({ path: join(OUT_DIR, `desktop-${MODE}-router-auto.png`), fullPage: true });

    await routerPage.getByRole("link", { name: "Classifier" }).first().click();
    await routerPage.waitForURL(/#\/classifier/, { timeout: TIMEOUT });
    await routerPage.locator("[data-jev-settings]").waitFor({ timeout: TIMEOUT });
    check(true, "the Classifier screen shows the Jev Settings card");
    check((await routerPage.locator('[data-panel="jev"]').count()) === 1, "the classifier's Jev panel is shown");
    check((await routerPage.locator("[data-admin-token]").count()) === 1, "Jev Settings still asks for the admin token");
    check((await routerPage.locator("[data-admin-unavailable]").count()) === 0, "settings are writable: the Router listens on loopback only");

    // --- Test Connection, both buttons, against the Router only -------------------------
    const before = routerSeen.urls.length;
    const providerCheck = routerPage.waitForResponse((response) => response.url().endsWith("/api/router/v1/classifier/check"), { timeout: TIMEOUT });
    await routerPage.locator(".card", { hasText: "Provider status" }).getByRole("button", { name: "Test Connection" }).click();
    const providerResponse = await providerCheck;
    check(providerResponse.url().startsWith(`${ROUTER}/`), "the provider's Test Connection went to the Router");
    await routerPage.locator('[data-check-status="ok"]').first().waitFor({ timeout: TIMEOUT });
    check((await routerPage.locator('[data-check-status="ok"]').first().innerText()).includes("Connected"), "Test Connection: Connected (the Router reached scripted Jev)");

    const settings = routerPage.locator(".card", { has: routerPage.locator("[data-jev-settings]") });
    const settingsCheck = routerPage.waitForResponse((response) => response.url().endsWith("/api/router/v1/classifier/check"), { timeout: TIMEOUT });
    await settings.locator("[data-test-connection]").click();
    check((await settingsCheck).url().startsWith(`${ROUTER}/`), "Jev Settings' Test Connection went to the Router");
    await settings.locator("[data-connection-state]").filter({ hasText: "Connected" }).waitFor({ timeout: TIMEOUT });
    check(true, "the Jev Settings connection pill reads Connected");
    const during = routerSeen.urls.slice(before);
    check(during.length > 0 && offOrigin(during, ROUTER).length === 0, `Test Connection made requests to the Router only (${during.length} requests)`);
    check(!during.some((url) => url.includes(String(GATEWAY_PORT))), "Test Connection made no request to the Gateway");
    await routerPage.screenshot({ path: join(OUT_DIR, `desktop-${MODE}-router-classifier.png`), fullPage: true });

    // --- an owned Router restarts (how saved settings apply); the Gateway is untouched ---
    if (MODE === "start") {
      const reloaded = routerPage.waitForEvent("load", { timeout: TIMEOUT * 2 });
      await clickMenu(app, "router-restart");
      await reloaded;
      await waitForStatus(app, /Running on port/);
      check((await holder(ROUTER_PORT)) === "router", "Restart Router brings the Router back");
      check((await holder(GATEWAY_PORT)) === "gateway", "Restart Router did not disturb the Gateway");
      await routerPage.getByRole("heading", { name: /Auto Routing|Classifier/ }).first().waitFor({ timeout: TIMEOUT });
      check(true, "the Router window reloads onto the restarted Router");
    }

    // --- origins, and no key anywhere --------------------------------------------------
    check(offOrigin(gatewaySeen.urls, GATEWAY).length === 0, `the Gateway window called only the Gateway (${gatewaySeen.urls.length} requests)${offOrigin(gatewaySeen.urls, GATEWAY).slice(0, 3).join(" ")}`);
    check(offOrigin(routerSeen.urls, ROUTER).length === 0, `the Router window called only the Router (${routerSeen.urls.length} requests)${offOrigin(routerSeen.urls, ROUTER).slice(0, 3).join(" ")}`);
    const secrets = [
      ["the Jev key", JEV_KEY],
      ["the admin token", adminToken],
    ];
    for (const [page, seen, name] of [
      [gatewayPage, gatewaySeen, "Gateway"],
      [routerPage, routerSeen, "Router"],
    ]) {
      const dom = await page.content();
      const stored = await storage(page);
      for (const [label, secret] of secrets) {
        check(!dom.includes(secret), `${label} is not in the ${name} window's page`);
        check(!stored.includes(secret), `${label} is not in the ${name} window's browser storage`);
        check(seen.bodies.every((body) => !body.text.includes(secret)), `${label} is in none of the ${name} window's ${seen.bodies.length} response bodies`);
        check(seen.urls.every((url) => !url.includes(secret)), `${label} is in no ${name} window request URL`);
      }
    }
  } finally {
    // --- quit: only what the app started is stopped ----------------------------------
    await app.close().catch((error) => console.log(`  (close: ${error.message})`));
    log.end();
  }

  await sleep(500);
  if (MODE === "start") {
    check((await holder(ROUTER_PORT)) === "nothing", "quitting stopped the Router the app started");
    check((await holder(GATEWAY_PORT)) === "nothing", "quitting stopped the Gateway the app started");
  } else {
    check((await holder(ROUTER_PORT)) === "router", "quitting left the attached Router serving");
    check((await holder(GATEWAY_PORT)) === "gateway", "quitting left the attached Gateway serving");
  }

  // The app's own log, and every artifact this run leaves, hold neither secret.
  const appLog = await readFile(logPath, "utf8");
  check(appLog.length > 0, `the app's log was captured (${appLog.length} bytes)`);
  for (const file of await readdir(OUT_DIR)) {
    const bytes = await readFile(join(OUT_DIR, file));
    for (const [label, secret] of [["the Jev key", JEV_KEY], ["the admin token", adminToken]]) {
      if (secret && bytes.includes(Buffer.from(secret))) {
        failures.push(`${label} is in the artifact ${file}`);
        // Never upload it: a text file is redacted in place, anything else removed.
        if (file.endsWith(".log")) await writeFile(join(OUT_DIR, file), bytes.toString("utf8").split(secret).join("[REDACTED]"));
        else await rm(join(OUT_DIR, file), { force: true });
      }
    }
  }
  check(!failures.some((failure) => failure.includes("artifact")), "no artifact holds the Jev key or the admin token");

  if (failures.length) {
    console.error(`\n${failures.length} check(s) failed:\n  - ${failures.join("\n  - ")}`);
    // Redacted: a failure may be a leak, and the CI log must not repeat it.
    let shown = appLog;
    for (const secret of [JEV_KEY, adminToken]) if (secret) shown = shown.split(secret).join("[REDACTED]");
    console.error(`\n== app log (${logPath}, secrets redacted) ==\n${shown}`);
    process.exit(1);
  }
  console.log(`desktop (${MODE}): all checks passed`);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
