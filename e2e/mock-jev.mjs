// A scripted TypeSafe endpoint for the router render: just enough of
// `GET /v1/models` for the router's real classifier check to run end to end
// without ever calling the real API. Bearer auth as TypeSafe does it: a wrong
// or missing key is a 401.
//
// Environment: MOCK_JEV_PORT (required), MOCK_JEV_KEY (required).

import { createServer } from "node:http";

const port = Number(process.env.MOCK_JEV_PORT);
const key = process.env.MOCK_JEV_KEY;
if (!port || !key) {
  console.error("MOCK_JEV_PORT and MOCK_JEV_KEY are required");
  process.exit(2);
}

const server = createServer((request, response) => {
  const json = (status, body) => {
    response.writeHead(status, { "content-type": "application/json" });
    response.end(JSON.stringify(body));
  };
  if (request.method === "GET" && request.url === "/health") return json(200, { ok: true });
  if (request.headers.authorization !== `Bearer ${key}`) {
    return json(401, { detail: "Missing or invalid API key" });
  }
  if (request.method === "GET" && request.url === "/v1/models") {
    // Aliases only, as the real service lists them: a pinned version is
    // accepted for classification without appearing here.
    return json(200, {
      models: [
        { name: "jev-latest", description: "latest", release_date: "2026-09-01" },
        { name: "jev-preview", description: "preview", release_date: "2026-09-20" },
      ],
    });
  }
  return json(404, { detail: "not found" });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`mock TypeSafe listening on http://127.0.0.1:${port}`);
});
