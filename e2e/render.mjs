// Render the panel against a live gateway and assert the control-panel screens
// render. Every route is screenshotted so a person can eyeball the panel on a
// pull request.
//
// It asserts *properties*, not pixels: a screen must not fall to the SPA
// fallback (the raw `index.html` document leaking through) and must not throw
// an uncaught exception. A pixel diff would be a flake the moment a runner's
// font rendering differs from a contributor's.

import { mkdir } from "node:fs/promises";
import { chromium } from "playwright";

const BASE = (process.env.PANEL_BASE ?? "http://127.0.0.1:11434").replace(/\/+$/, "");
const OUT_DIR = process.env.OUT_DIR ?? "screens";
// A route settles when the panel has either rendered its data or surfaced its
// own error; this is the ceiling on waiting for whichever comes first.
const SETTLE_MS = Number(process.env.RENDER_SETTLE_MS ?? 15000);

// The raw document leaking through when a request that should have been the SPA
// was answered with `index.html`.
const FALLBACK_SIGNS = ["<!doctype", "<!DOCTYPE"];

// One entry per hash route. The index route is the dashboard.
const ROUTES = [
  { name: "dashboard", hash: "#/" },
  { name: "chat", hash: "#/chat" },
  { name: "models", hash: "#/models" },
  { name: "inference", hash: "#/inference" },
  { name: "performance", hash: "#/performance" },
  { name: "gateway", hash: "#/gateway" },
  { name: "access", hash: "#/access" },
  { name: "settings", hash: "#/settings" },
  { name: "logs", hash: "#/logs" },
];

/** Wait until the SPA has rendered something, or a fallback sign shows, or time runs out. */
async function settle(page) {
  const deadline = Date.now() + SETTLE_MS;
  // Give the SPA a beat to mount and fire its fetches before the first read.
  await page.waitForLoadState("networkidle").catch(() => {});
  while (Date.now() < deadline) {
    const text = await page.evaluate(() => document.body.innerText).catch(() => "");
    if (FALLBACK_SIGNS.some((s) => text.includes(s))) return text;
    if (text.trim().length > 0) return text;
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
      const text = await settle(page);
      await page.screenshot({ path: `${OUT_DIR}/${route.name}.png`, fullPage: true });

      const sign = FALLBACK_SIGNS.find((s) => text.includes(s));
      if (sign) failures.push(`${route.name}: SPA fallback leaked (“${sign}”)`);
      if (!text.trim()) failures.push(`${route.name}: rendered an empty document`);
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
