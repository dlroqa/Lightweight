import { Navigate, Route, Routes } from "react-router-dom";

import { Shell } from "./components/Shell";
import { Agent } from "./screens/Agent";
import { AgentTools } from "./screens/AgentTools";
import { Chat } from "./screens/Chat";
import { Dashboard } from "./screens/Dashboard";
import { AccessScreen } from "./screens/AccessScreen";
import { GatewayScreen } from "./screens/GatewayScreen";
import { Inference } from "./screens/Inference";
import { Logs } from "./screens/Logs";
import { Models } from "./screens/Models";
import { Performance } from "./screens/Performance";
import { SettingsScreen } from "./screens/SettingsScreen";

export function App() {
  return (
    <Routes>
      <Route element={<Shell />}>
        {/* The agent surface is the first viewport. The gateway dashboard keeps
            its place at an explicit path, and the old /agent link still lands
            on it, so no existing route is broken. */}
        <Route index element={<Agent />} />
        <Route path="agent" element={<Agent />} />
        <Route path="dashboard" element={<Dashboard />} />
        <Route path="agent/tools" element={<AgentTools />} />
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
