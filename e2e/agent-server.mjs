// Exercise Settings against real processes in isolated homes. Build the panel,
// `lightweight`, and `lightagent` first, then run `npm run agent-server` here.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const root = fileURLToPath(new URL("../", import.meta.url));
const gatewayBinary = resolve(process.env.GATEWAY_BIN ?? join(root, "target/debug/lightweight"));
const agentBinary = resolve(process.env.LIGHTAGENT_BIN ?? join(root, "target/debug/lightagent"));
const home = await mkdtemp(join(tmpdir(), "lightagent-settings-"));
const env = { ...process.env, HERMES_GATEWAY_HOME: join(home, "gateway"),
  LIGHTAGENT_HOME: join(home, "agent"), LIGHTAGENT_BIN: agentBinary };

async function unusedPort() {
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const port = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}

async function until(check, description) {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`Timed out: ${description}`);
}

const gatewayPort = await unusedPort();
let agentPort = await unusedPort();
while (agentPort === gatewayPort) agentPort = await unusedPort();
const base = `http://127.0.0.1:${gatewayPort}`;
const upstream = `http://127.0.0.1:${agentPort}`;
const gateway = spawn(gatewayBinary, ["serve", "--port", String(gatewayPort),
  "--agent-upstream", upstream, "--web-root", join(root, "frontend/dist")],
  { env, stdio: ["ignore", "pipe", "pipe"] });
let output = "";
for (const stream of [gateway.stdout, gateway.stderr]) {
  stream.on("data", (chunk) => { output = (output + chunk).slice(-8192); });
}
const gatewayClosed = once(gateway, "close");
let browser;
try {
  await until(async () => {
    if (gateway.exitCode !== null) throw new Error(output);
    return fetch(`${base}/health`).then((r) => r.ok).catch(() => false);
  }, "gateway ready");
  browser = await chromium.launch({ headless: true, args: ["--no-sandbox"] });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, colorScheme: "dark" });
  const pageErrors = [];
  page.on("pageerror", (error) => pageErrors.push(String(error)));
  await page.goto(`${base}/#/agent/tools`);
  await page.getByRole("link", { name: "Server settings" }).click();
  const card = page.locator("section").filter({ has: page.getByRole("heading", { name: "Lightagent server", exact: true }) });
  await until(() => card.getByRole("button", { name: "Start server", exact: true }).isEnabled(), "start button enabled");
  // Missing setup must surface the child's own remedy and permit retry.
  await card.getByRole("button", { name: "Start server", exact: true }).click();
  await card.getByRole("alert").filter({ hasText: "lightagent init" }).waitFor({ timeout: 20_000 });
  assert(await card.getByRole("button", { name: "Start server", exact: true }).isEnabled());

  const init = spawn(agentBinary, ["init", "--base-url", `${base}/v1`], { env, stdio: "pipe" });
  const [code] = await once(init, "close");
  assert.equal(code, 0, "initialize a temporary agent profile");
  await card.getByRole("button", { name: "Start server", exact: true }).click();
  await card.getByRole("button", { name: "Server running", exact: true }).waitFor({ timeout: 20_000 });
  assert(await card.getByRole("button", { name: "Server running", exact: true }).isDisabled());
  assert.equal(await card.getByRole("alert").count(), 0, "old startup error cleared");
  await page.screenshot({ path: process.env.SETTINGS_SCREENSHOT ?? join(home, "settings-running.png"), fullPage: true });
  // Concurrent requests against an existing agent are harmless.
  await Promise.all(Array.from({ length: 3 }, async () => {
    const response = await fetch(`${base}/api/v1/agent-server/start`, { method: "POST" });
    assert.equal(response.status, 202);
    assert.equal((await response.json()).status, "running");
  }));
  await page.goto(`${base}/#/agent/tools`);
  await page.getByText("datetime.now", { exact: true }).waitFor();
  assert.equal(await page.getByRole("alert").count(), 0);
  assert.deepEqual(pageErrors, []);
  gateway.kill("SIGINT");
  await gatewayClosed;
  await until(() => fetch(`${upstream}/health`).then(() => false).catch(() => true), "owned agent stops with gateway");
  console.log("Passed: Settings startup error, retry, running status, duplicate starts, Tools recovery, and shutdown cleanup.");
} finally {
  await browser?.close();
  if (gateway.exitCode === null && gateway.signalCode === null) {
    gateway.kill("SIGINT");
    await gatewayClosed;
  }
  await rm(home, { recursive: true, force: true });
}
