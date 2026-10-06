import { Navigate, Route, Routes } from "react-router-dom";

import { Shell } from "./components/Shell";
import { Chat } from "./screens/Chat";
import { Dashboard } from "./screens/Dashboard";
import { AccessScreen } from "./screens/AccessScreen";
import { GatewayScreen } from "./screens/GatewayScreen";
import { Inference } from "./screens/Inference";
import { Logs } from "./screens/Logs";
import { Models } from "./screens/Models";
import { Performance } from "./screens/Performance";
import { SettingsScreen } from "./screens/SettingsScreen";
import { AutoRoutingScreen } from "./screens/router/AutoRoutingScreen";
import { ClassifierScreen } from "./screens/router/ClassifierScreen";
import { useBackend } from "./state/backend";

export function App() {
  // A router serves the same bundle; it answers only the router's API, so it
  // gets the router's screens. A gateway's screens are exactly as before.
  if (useBackend() === "router") {
    return (
      <Routes>
        <Route element={<Shell />}>
          <Route index element={<Navigate to="/auto" replace />} />
          <Route path="auto" element={<AutoRoutingScreen />} />
          <Route path="classifier" element={<ClassifierScreen />} />
          <Route path="*" element={<Navigate to="/auto" replace />} />
        </Route>
      </Routes>
    );
  }
  return (
    <Routes>
      <Route element={<Shell />}>
        <Route index element={<Dashboard />} />
        <Route path="chat" element={<Chat />} />
        <Route path="models" element={<Models />} />
        <Route path="inference" element={<Inference />} />
        <Route path="performance" element={<Performance />} />
        <Route path="gateway" element={<GatewayScreen />} />
        <Route path="access" element={<AccessScreen />} />
        <Route path="settings" element={<SettingsScreen />} />
        <Route path="logs" element={<Logs />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}
