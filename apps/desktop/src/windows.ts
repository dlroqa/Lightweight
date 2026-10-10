/**
 * What each window loads, and what it is allowed to do.
 *
 * Free of any `electron` import so the two promises that matter can be tested
 * without a display: the Gateway window shows the Gateway's own origin, the
 * Router window shows the Router's own origin, and neither can be steered onto
 * the other. Each panel then talks only to the server it was loaded from, which
 * is the whole of the same-origin model the panel and Jev Settings rely on.
 */

/** A panel's URL: the server's own loopback origin, where it serves the bundle. */
export function panelUrl(port: number): string {
  return `http://127.0.0.1:${port}/`;
}

/** The Gateway window's URL. Exactly what the shell has always loaded. */
export function gatewayPanelUrl(gatewayPort: number): string {
  return panelUrl(gatewayPort);
}

/** The Router window's URL: the Router's origin, never the Gateway's. */
export function routerPanelUrl(routerPort: number): string {
  return panelUrl(routerPort);
}

export interface SecureWebPreferences {
  contextIsolation: true;
  nodeIntegration: false;
  sandbox: true;
}

/**
 * The Router window's web preferences.
 *
 * The same three flags as the Gateway window, and **no preload**: the Router's
 * page gets no `hermesShell` bridge, so nothing it runs can reach the shell's
 * IPC — not `gateway:restart`, not anything added later for the Gateway.
 */
export function routerWindowPreferences(): SecureWebPreferences {
  return { contextIsolation: true, nodeIntegration: false, sandbox: true };
}

/** Whether `url` is on exactly `origin` (scheme, host and port). */
export function isSameOrigin(url: string, origin: string): boolean {
  try {
    return new URL(url).origin === new URL(origin).origin;
  } catch {
    return false;
  }
}

/** Whether a link may be handed to the system browser: web pages only. */
export function isExternalWebLink(url: string): boolean {
  try {
    const { protocol } = new URL(url);
    return protocol === "http:" || protocol === "https:";
  } catch {
    return false;
  }
}
