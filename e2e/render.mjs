// Render the panel against a live gateway and assert the control-panel screens
// render real control data. Every route is screenshotted so a person can eyeball
// the panel on a pull request.
//
// It asserts *properties*, not pixels: a screen must render its own positive
// heading, its control API requests must be successful JSON responses, and it
// must not throw an uncaught exception. A pixel diff would be a flake the moment
// a runner's font rendering differs from a contributor's.

import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";

const BASE = (process.env.PANEL_BASE ?? "http://127.0.0.1:11434").replace(/\/+$/, "");
const OUT_DIR = process.env.OUT_DIR ?? "screens";
const SETTLE_MS = Number(process.env.RENDER_SETTLE_MS ?? 15000);

const ROUTES = [
  { name: "dashboard", hash: "#/", mustContain: "Dashboard", api: true },
  { name: "chat", hash: "#/chat", mustContain: "Chat", api: true },
  { name: "models", hash: "#/models", mustContain: "Models", api: true },
  { name: "inference", hash: "#/inference", mustContain: "Inference", api: false },
  { name: "performance", hash: "#/performance", mustContain: "Performance", api: true },
  { name: "gateway", hash: "#/gateway", mustContain: "API Gateway", api: true },
  { name: "access", hash: "#/access", mustContain: "Access & Keys", api: true },
  { name: "settings", hash: "#/settings", mustContain: "Settings", api: true },
  { name: "logs", hash: "#/logs", mustContain: "Logs", api: true },
];

function isControlApi(url) {
  // Dashboard and Gateway subscribe to this SSE stream. It is deliberately not
  // JSON; every other `/api/v1/*` request made by this render suite is control
  // data and must be a successful JSON response.
  return url.startsWith(`${BASE}/api/v1/`) && !url.startsWith(`${BASE}/api/v1/events`);
}

/** Wait for a screen's own positive proof rather than merely React mounting. */
async function settle(page, route) {
  await page.waitForLoadState("networkidle").catch(() => {});
  await page.getByText(route.mustContain, { exact: true }).first().waitFor({ timeout: SETTLE_MS });
  // Give startup fetches a short chance to complete after the visible heading
  // mounts, then let response assertions below prove their content type/status.
  await page.waitForTimeout(250);
  return page.evaluate(() => document.body.innerText);
}

async function checkInvalidJsonRecovery(context) {
  const page = await context.newPage();
  try {
    await page.route("**/api/v1/models", (route) =>
      route.fulfill({
        status: 200,
        contentType: "text/html",
        body: "<!doctype html><title>Panel fallback</title>",
      }),
    );
    await page.goto(`${BASE}/#/models`, { waitUntil: "domcontentloaded" });
    await page
      .getByText("The gateway returned invalid JSON for /api/v1/models.", { exact: true })
      .waitFor({ timeout: SETTLE_MS });
    const text = await page.evaluate(() => document.body.innerText);
    if (text.includes("<!doctype") || text.includes("Panel fallback")) {
      throw new Error("Models exposed an HTML fallback instead of a clean API error");
    }
  } finally {
    await page.close();
  }
  console.log("  [ok] HTML API fallback is rejected and shown as a clean error");
}

async function main() {
  await mkdir(OUT_DIR, { recursive: true });
  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    colorScheme: "dark",
    deviceScaleFactor: 2,
  });

  const failures = [];
  for (const route of ROUTES) {
    const page = await context.newPage();
    const errors = [];
    const apiResponses = [];
    page.on("pageerror", (err) => errors.push(String(err)));
    page.on("response", (response) => {
      if (isControlApi(response.url())) {
        apiResponses.push({
          url: response.url(),
          status: response.status(),
          contentType: response.headers()["content-type"] ?? "",
        });
      }
    });

    try {
      await page.goto(`${BASE}/${route.hash}`, { waitUntil: "domcontentloaded" });
      const text = await settle(page, route);
      await page.screenshot({ path: `${OUT_DIR}/${route.name}.png`, fullPage: true });

      if (!text.includes(route.mustContain)) {
        failures.push(`${route.name}: expected “${route.mustContain}” never appeared`);
      }
      if (route.api && apiResponses.length === 0) {
        failures.push(`${route.name}: made no /api/v1 control API request`);
      }
      for (const response of apiResponses) {
        if (response.status < 200 || response.status >= 300) {
          failures.push(`${route.name}: ${response.url} returned HTTP ${response.status}`);
        }
        if (!response.contentType.toLowerCase().includes("application/json")) {
          failures.push(
            `${route.name}: ${response.url} returned non-JSON content-type ${response.contentType || "(missing)"}`,
          );
        }
      }
      if (errors.length) failures.push(`${route.name}: uncaught page error — ${errors[0]}`);
      const mark = failures.some((f) => f.startsWith(`${route.name}:`)) ? "FAIL" : "ok";
      console.log(`  [${mark}] ${route.name.padEnd(12)} ${route.hash}`);
    } catch (err) {
      failures.push(`${route.name}: ${err instanceof Error ? err.message : String(err)}`);
      console.log(`  [FAIL] ${route.name.padEnd(12)} ${route.hash} — ${err}`);
    } finally {
      await page.close();
    }
  }

  try {
    await checkInvalidJsonRecovery(context);
  } catch (err) {
    failures.push(`invalid JSON recovery: ${err instanceof Error ? err.message : String(err)}`);
  }

  await context.close();
  await browser.close();

  console.log(`\nScreenshots written to ${OUT_DIR}/`);
  if (failures.length) {
    console.error(`\n${failures.length} render check(s) failed:`);
    for (const f of failures) console.error(`  - ${f}`);
    process.exit(1);
  }
  console.log("All render checks passed.");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
