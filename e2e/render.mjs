// Render the panel against a live gateway and assert the screens that broke.
//
// The panel is one bundle over two backends: the gateway's own control API
// under `/api/v1`, and the agent API under `/api/lightagent/v1` that the
// gateway proxies to a separate `lightagent serve`. When that proxy is missing
// an agent screen falls to the SPA fallback and renders `index.html`, so the
// client chokes on the leading `<` of `<!doctype …>` and the page shows
// "Could not reach the agent API: … is not valid JSON". That is the exact
// failure this render exists to catch, in a real browser, before a human hits
// it — and every route is screenshotted so a person can eyeball the panel on a
// pull request the way the icons job lets them eyeball the icons.
//
// It asserts *properties*, not pixels: the agent screens must not carry a
// fallback/parse-error string, the Tools screen must list a built-in tool, and
// no route may log an uncaught exception. A pixel diff would be a flake the
// moment a runner's font rendering differs from a contributor's.

import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";

const BASE = (process.env.PANEL_BASE ?? "http://127.0.0.1:11434").replace(/\/+$/, "");
const OUT_DIR = process.env.OUT_DIR ?? "screens";
// A route settles when the panel has either rendered its data or surfaced its
// own error; this is the ceiling on waiting for whichever comes first.
const SETTLE_MS = Number(process.env.RENDER_SETTLE_MS ?? 15000);

// The strings a broken agent screen shows. The first two are the panel's own
// wording; the last two are the raw document leaking through when a request
// that should have been JSON was answered with `index.html`.
const FALLBACK_SIGNS = [
  "Could not reach the agent API",
  "is not valid JSON",
  "<!doctype",
  "<!DOCTYPE",
];

// One entry per hash route. `mustContain` is the positive proof a screen
// rendered real data; `agentBacked` marks the screens served by the proxied
// agent API, which are the ones a missing proxy breaks and so are checked for
// the fallback signs above.
const ROUTES = [
  { name: "dashboard", hash: "#/" },
  { name: "agent", hash: "#/agent", agentBacked: true, mustContain: "No run yet" },
  {
    name: "agent-tools",
    hash: "#/agent/tools",
    agentBacked: true,
    // A built-in tool the agent server always lists; its presence proves the
    // proxy delivered real JSON rather than the document.
    mustContain: "datetime.now",
  },
  { name: "chat", hash: "#/chat" },
  { name: "models", hash: "#/models" },
  { name: "inference", hash: "#/inference" },
  { name: "performance", hash: "#/performance" },
  { name: "gateway", hash: "#/gateway" },
  { name: "access", hash: "#/access" },
  { name: "settings", hash: "#/settings" },
  { name: "logs", hash: "#/logs" },
];

/** Wait until `mustContain` shows, or a fallback sign does, or time runs out. */
async function settle(page, route) {
  const deadline = Date.now() + SETTLE_MS;
  // Give the SPA a beat to mount and fire its fetches before the first read.
  await page.waitForLoadState("networkidle").catch(() => {});
  while (Date.now() < deadline) {
    const text = await page.evaluate(() => document.body.innerText).catch(() => "");
    const failed = FALLBACK_SIGNS.some((s) => text.includes(s));
    const arrived = route.mustContain ? text.includes(route.mustContain) : true;
    if (failed || arrived) return text;
    await page.waitForTimeout(250);
  }
  return page.evaluate(() => document.body.innerText).catch(() => "");
}

async function main() {
  await mkdir(OUT_DIR, { recursive: true });
  const browser = await chromium.launch();
  // A fixed, generous viewport so every screenshot frames the whole panel the
  // same way, and dark because the panel is dark by default.
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    colorScheme: "dark",
    deviceScaleFactor: 2,
  });

  const failures = [];
  for (const route of ROUTES) {
    const page = await context.newPage();
    // An uncaught exception on any screen is a failure of that screen, even one
    // whose visible text looks fine.
    const errors = [];
    page.on("pageerror", (err) => errors.push(String(err)));

    try {
      await page.goto(`${BASE}/${route.hash}`, { waitUntil: "domcontentloaded" });
      const text = await settle(page, route);
      await page.screenshot({ path: `${OUT_DIR}/${route.name}.png`, fullPage: true });

      if (route.agentBacked) {
        const sign = FALLBACK_SIGNS.find((s) => text.includes(s));
        if (sign) failures.push(`${route.name}: agent screen shows the fallback/parse error (“${sign}”)`);
      }
      if (route.mustContain && !text.includes(route.mustContain)) {
        failures.push(`${route.name}: expected to see “${route.mustContain}”, never appeared`);
      }
      if (errors.length) {
        failures.push(`${route.name}: uncaught page error — ${errors[0]}`);
      }
      const mark = failures.some((f) => f.startsWith(`${route.name}:`)) ? "FAIL" : "ok";
      console.log(`  [${mark}] ${route.name.padEnd(12)} ${route.hash}`);
    } catch (err) {
      failures.push(`${route.name}: ${err instanceof Error ? err.message : String(err)}`);
      console.log(`  [FAIL] ${route.name.padEnd(12)} ${route.hash} — ${err}`);
    } finally {
      await page.close();
    }
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
