import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { HashRouter } from "react-router-dom";

import { App } from "./App";
import { PreferencesProvider } from "./state/preferences";
import "./styles/app.css";

/**
 * `HashRouter` rather than `BrowserRouter`.
 *
 * The gateway serves `index.html` for any path that has no extension, so
 * history routing would work — but the Electron shell in M6b.4 loads this from
 * a file URL where it would not, and a router that works in one build and not
 * the other is a bug waiting for the packaging step.
 */
const container = document.getElementById("root");
if (!container) {
  throw new Error("the document has no #root to mount into");
}

createRoot(container).render(
  <StrictMode>
    <PreferencesProvider>
      <HashRouter>
        <App />
      </HashRouter>
    </PreferencesProvider>
  </StrictMode>,
);
