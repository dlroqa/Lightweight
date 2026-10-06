import { createContext, useContext, useEffect, useState, type ReactNode } from "react";

/**
 * Which server this panel was loaded from.
 *
 * The same bundle is served by a gateway (`hermes serve --web-root`) and by a
 * router (`hermes router --web-root`), each from its own origin, so the page
 * and the API it calls always share one and nothing cross-origin is ever
 * permitted. The two servers answer different APIs, so the panel asks once,
 * through the one endpoint both answer without a credential: `GET /version`,
 * whose `build` names the server.
 *
 * Anything other than a router's answer — including no answer — is treated
 * as a gateway, which is what the panel has always assumed.
 */
export type Backend = "gateway" | "router";

const BackendContext = createContext<Backend>("gateway");

export const ROUTER_BUILD_PREFIX = "lightweight-router-";

export function BackendProvider({ children }: { children: ReactNode }) {
  const [backend, setBackend] = useState<Backend | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch("/version")
      .then((response) => (response.ok ? response.json() : null))
      .then((body: { build?: unknown } | null) => {
        if (cancelled) return;
        const build = typeof body?.build === "string" ? body.build : "";
        setBackend(build.startsWith(ROUTER_BUILD_PREFIX) ? "router" : "gateway");
      })
      .catch(() => {
        if (!cancelled) setBackend("gateway");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // One local request; rendering nothing until it answers avoids drawing the
  // gateway's screens on a router for a frame and then swapping them out.
  if (backend === null) return null;
  return <BackendContext.Provider value={backend}>{children}</BackendContext.Provider>;
}

export function useBackend(): Backend {
  return useContext(BackendContext);
}
