// A scripted TypeSafe endpoint for the router render: just enough of
// `GET /v1/models` for the router's real classifier check to run end to end
// without ever calling the real API, and of `POST /v1/systemone` for a real
// Auto classification when the user text is sent (a request mentioning code
// goes to Coder, anything else to General). Bearer auth as TypeSafe does it: a wrong or missing key is a
// 401. `GET /stats` counts the classifications answered, never a key.
//
// Environment: MOCK_JEV_PORT (required), MOCK_JEV_KEY (required).

import { createServer } from "node:http";

const port = Number(process.env.MOCK_JEV_PORT);
const key = process.env.MOCK_JEV_KEY;
if (!port || !key) {
  console.error("MOCK_JEV_PORT and MOCK_JEV_KEY are required");
  process.exit(2);
}

let classified = 0;

const server = createServer((request, response) => {
  const json = (status, body) => {
    response.writeHead(status, { "content-type": "application/json" });
    response.end(JSON.stringify(body));
  };
  if (request.method === "GET" && request.url === "/health") return json(200, { ok: true });
  if (request.method === "GET" && request.url === "/stats") return json(200, { systemone: classified });
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
  if (request.method === "POST" && request.url === "/v1/systemone") {
    let raw = "";
    request.on("data", (chunk) => (raw += chunk));
    request.on("end", () => {
      let body = {};
      try {
        body = JSON.parse(raw);
      } catch {
        return json(422, { detail: "invalid body" });
      }
      // Only a classifier that sends user text is answered; one that does not
      // (the main render router) keeps the 404 its checks were written for.
      if (typeof body?.state?.request !== "string") return json(404, { detail: "not found" });
      const text = body.state.request;
      const options = Object.keys(body?.questions?.route?.criteria ?? {});
      const choice = /rust|code|function|debug/i.test(text) ? "Coder" : "General";
      if (!options.includes(choice)) return json(422, { detail: "not a candidate" });
      classified += 1;
      return json(200, {
        answers: { route: { type: "choice", choice, confidence: 0.92, probabilities: { [choice]: 0.92 } } },
      });
    });
    return;
  }
  return json(404, { detail: "not found" });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`mock TypeSafe listening on http://127.0.0.1:${port}`);
});
