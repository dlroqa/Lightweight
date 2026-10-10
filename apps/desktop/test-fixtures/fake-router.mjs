// A scripted stand-in for `hermes router`, for the supervisor's unit tests.
//
// Run as `node fake-router.mjs <the arguments hermes would get>`:
//
//   router config-path [--config P]       prints P, or $FAKE_ROUTER_DEFAULT_CONFIG
//   router validate-config --config P     exit 0 if P has a non-empty "nodes"
//                                         array, else the router's own wording, exit 1
//   router --config P --listen H:PORT ... serves /version and /health as a Router
//
// FAKE_ROUTER_MODE changes the serving behaviour: "exit" says the port is taken
// and exits 1 without listening; "crash" names a missing variable and exits 1;
// "hang" never listens. On SIGINT it writes "SIGINT" to $FAKE_ROUTER_SIGNAL_FILE
// (when set) and exits 0, which is how the tests see the clean stop was used.

import { readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";

const args = process.argv.slice(2);
const value = (flag) => {
  const index = args.indexOf(flag);
  return index >= 0 ? args[index + 1] : undefined;
};

if (args[0] !== "router") {
  console.error(`fake router: unexpected arguments ${JSON.stringify(args)}`);
  process.exit(2);
}

if (args[1] === "config-path") {
  console.log(value("--config") ?? process.env.FAKE_ROUTER_DEFAULT_CONFIG ?? "/nowhere/router.json");
  process.exit(0);
}

if (args[1] === "validate-config") {
  const path = value("--config");
  let nodes = [];
  try {
    nodes = JSON.parse(readFileSync(path, "utf8")).nodes ?? [];
  } catch {
    // Reported below like any other refusal.
  }
  if (Array.isArray(nodes) && nodes.length > 0) {
    console.log(`${path} is valid.`);
    process.exit(0);
  }
  console.error(`${path} was refused:\n  - the configuration lists no nodes`);
  process.exit(1);
}

const listen = value("--listen") ?? "";
const port = Number(listen.split(":").pop());
const mode = process.env.FAKE_ROUTER_MODE ?? "serve";

process.on("SIGINT", () => {
  if (process.env.FAKE_ROUTER_SIGNAL_FILE) writeFileSync(process.env.FAKE_ROUTER_SIGNAL_FILE, "SIGINT");
  process.exit(0);
});

if (mode === "exit") {
  console.error(`error: could not listen on ${listen}: Address already in use (os error 98)`);
  process.exit(1);
}
if (mode === "crash") {
  console.error("error: node \"n\": the variable NODE_KEY it names is not set");
  process.exit(1);
}
if (mode === "hang") {
  setInterval(() => {}, 1000);
} else {
  const server = createServer((request, response) => {
    const json = (body) => {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify(body));
    };
    if (request.url === "/version") return json({ version: "0.0.0", build: "lightweight-router-0.0.0" });
    if (request.url === "/health") return json({ status: "unavailable", routes_available: 0, routes: 1 });
    response.writeHead(404);
    response.end();
  });
  server.on("error", (error) => {
    console.error(`error: could not listen on ${listen}: ${error.message}`);
    process.exit(1);
  });
  server.listen(port, "127.0.0.1", () => console.log(`fake router listening on ${listen}`));
}
