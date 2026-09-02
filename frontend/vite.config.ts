import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";

/**
 * The dev server proxies the gateway's own paths to the gateway.
 *
 * This is what makes the panel same-origin in development, exactly as serving
 * it from `--web-root` makes it same-origin in production. Neither needs a CORS
 * policy on the API, and adding one would be a decision about who may call this
 * gateway taken in order to answer a question about where a file is served
 * from.
 *
 * `11434` is the port `hermes serve` uses by default. Set `HERMES_DEV_ORIGIN`
 * in the environment or a `.env` file when the gateway is elsewhere.
 */
const PROXIED = ["/api", "/v1", "/health", "/props", "/version", "/metrics"];

export default defineConfig(({ mode }) => {
  // Read through Vite rather than `process.env`, which does not exist in the
  // browser-facing type world this project compiles against.
  const env = loadEnv(mode, process.cwd(), "HERMES_");
  const gateway = env.HERMES_DEV_ORIGIN ?? "http://127.0.0.1:11434";
  // The Lightagent API is a separate server (`lightagent serve`, default 8735).
  // Its prefix is listed first so it wins over the gateway's `/api`.
  const agent = env.HERMES_AGENT_ORIGIN ?? "http://127.0.0.1:8735";

  return {
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api/lightagent": { target: agent, changeOrigin: true },
      ...Object.fromEntries(
        PROXIED.map((path) => [path, { target: gateway, changeOrigin: true }]),
      ),
    },
  },
  build: {
    outDir: "dist",
    // Hashed file names, so the gateway can serve them with a long cache life
    // while `index.html` stays uncached.
    assetsDir: "assets",
    // The panel is served from a local gateway, so a source map costs nothing
    // at run time and makes a stack trace in the wild readable.
    sourcemap: true,
  },
  };
});
