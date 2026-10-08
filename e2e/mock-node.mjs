// Scripted Lightweight nodes for the router render: just enough of the node
// contract (`GET /v1/capabilities`, `POST /v1/chat/completions`) for the real
// router to route to them, so a cross-route fallback that *succeeds* can be
// rendered from a real trace without loading a model.
//
// Each node serves one model, as a gateway does, and starts with nothing
// loaded — so it is unavailable to the router, exactly like the empty gateway.
// `POST /control/load` loads it; the router sees the change on its next probe.
// A loaded node answers every chat request with its scripted status: 200 is a
// small chat completion, anything else is a refusal sent before any answer.
//
// Environment: MOCK_NODES (required), a comma-separated list of
// `port:model:status[:loaded]`, for example `11502:Coder:503,11503:General:200`.
// `status` may be `hang`: the node accepts the request and never answers (a
// queue that never moves, a generation that never ends), which is what a
// router's pre-commit request budget cuts. `overflow` refuses the prompt as
// longer than the node's context (400, `context_length_exceeded`, the
// structured code the router reads). `stream` answers every request with a
// server-sent-events stream spread over about two seconds. `:loaded` starts
// the node loaded.

import { createServer } from "node:http";

const specs = (process.env.MOCK_NODES ?? "")
  .split(",")
  .filter(Boolean)
  .map((spec) => {
    const [port, model, status, initially] = spec.split(":");
    const hang = status === "hang";
    const overflow = status === "overflow";
    const stream = status === "stream";
    const scripted = hang || overflow || stream;
    return { port: Number(port), model, status: scripted ? 0 : Number(status), hang, overflow, stream, loaded: initially === "loaded" };
  });
if (!specs.length || specs.some((s) => !s.port || !s.model || (!s.status && !s.hang && !s.overflow && !s.stream))) {
  console.error("MOCK_NODES must list port:model:status entries");
  process.exit(2);
}

const FEATURES = {
  streaming: true, sse_done: true, usage_chunk: true, chat_completions: true, completions: true,
  tools: true, tool_call_deltas: true, tool_choice: true, parallel_tool_calls: true, reasoning_content: true,
};

for (const spec of specs) {
  let loaded = spec.loaded;
  const server = createServer((request, response) => {
    const json = (status, body) => {
      response.writeHead(status, { "content-type": "application/json" });
      response.end(JSON.stringify(body));
    };
    // Drain the body before answering, as a real node reads the request first.
    request.resume();
    request.on("end", () => {
      if (request.method === "GET" && request.url === "/health") return json(200, { ok: true });
      if (request.method === "POST" && request.url === "/control/load") {
        loaded = true;
        return json(200, { model: spec.model, loaded });
      }
      if (request.method === "GET" && request.url === "/v1/capabilities") {
        return json(200, {
          object: "capability.list",
          protocol: { name: "lightweight-public-inference", version: 1, compatible_versions: [1] },
          server: { name: "Lightweight", version: "0.5.0" },
          endpoints: { models: "/v1/models", chat_completions: "/v1/chat/completions", completions: "/v1/completions" },
          features: FEATURES,
          state: loaded ? { model_loaded: true, model: { id: spec.model, context_length: 8192 } } : { model_loaded: false },
          limits: { max_concurrent_requests: 1 },
        });
      }
      if (request.method === "POST" && request.url === "/v1/chat/completions") {
        if (!loaded) return json(404, { error: { message: "no model is loaded", type: "invalid_request_error", code: "model_not_found" } });
        if (spec.hang) return; // never answered: the client gives up first
        if (spec.overflow) {
          return json(400, { error: { message: "the prompt is longer than the model's context", type: "invalid_request_error", code: "context_length_exceeded" } });
        }
        if (spec.stream) {
          response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
          const chunk = (delta, finish_reason = null) => response.write(`data: ${JSON.stringify({
            id: "chatcmpl-render", object: "chat.completion.chunk", created: Math.floor(Date.now() / 1000),
            model: spec.model, choices: [{ index: 0, delta, finish_reason }],
          })}\n\n`);
          chunk({ role: "assistant" });
          let sent = 0;
          const timer = setInterval(() => {
            if (sent < 4) return chunk({ content: `part ${sent++} ` });
            clearInterval(timer);
            chunk({}, "stop");
            response.end("data: [DONE]\n\n");
          }, 500);
          return;
        }
        if (spec.status !== 200) {
          return json(spec.status, { error: { message: `${spec.model} is scripted to refuse`, type: "server_error", code: "unavailable" } });
        }
        return json(200, {
          id: "chatcmpl-render",
          object: "chat.completion",
          created: Math.floor(Date.now() / 1000),
          model: spec.model,
          choices: [{ index: 0, message: { role: "assistant", content: `answered by ${spec.model}` }, finish_reason: "stop" }],
          usage: { prompt_tokens: 4, completion_tokens: 3, total_tokens: 7 },
        });
      }
      return json(404, { error: { message: "not found" } });
    });
  });
  server.listen(spec.port, "127.0.0.1", () => {
    console.log(`mock node ${spec.model} (${spec.status}) listening on http://127.0.0.1:${spec.port}`);
  });
}
