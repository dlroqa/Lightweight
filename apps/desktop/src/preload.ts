/**
 * The only bridge between the panel and the shell.
 *
 * Deliberately tiny. The panel is a web page that works unchanged in a browser,
 * so anything exposed here has to be optional — a feature the panel can do
 * without is a feature it can have in both builds.
 */

import { contextBridge, ipcRenderer } from "electron";

import type { GatewayState } from "./gateway.js";

contextBridge.exposeInMainWorld("hermesShell", {
  /** True only inside the desktop shell, so the panel can tell. */
  present: true,
  current: (): Promise<GatewayState> => ipcRenderer.invoke("gateway:current"),
  /** Restart the gateway this shell owns, so a new bind configuration applies. */
  restart: (): Promise<GatewayState> => ipcRenderer.invoke("gateway:restart"),
  onState: (listener: (state: GatewayState) => void): (() => void) => {
    const handler = (_event: unknown, state: GatewayState) => listener(state);
    ipcRenderer.on("gateway:state", handler);
    return () => ipcRenderer.removeListener("gateway:state", handler);
  },
});
