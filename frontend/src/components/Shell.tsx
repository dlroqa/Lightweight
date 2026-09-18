import { useEffect, useState } from "react";
import { NavLink, Outlet, useLocation } from "react-router-dom";
import {
  Bot,
  ChevronLeft,
  Cpu,
  FileText,
  Gauge,
  KeyRound,
  LayoutDashboard,
  Menu as MenuIcon,
  MessageSquare,
  Moon,
  Network,
  Package,
  RotateCw,
  Settings as SettingsIcon,
  Sliders,
  Sun,
  Wrench,
} from "lucide-react";

import { api } from "../api/client";
import { bytes, percent } from "../api/format";
import { usePoll } from "../hooks/usePoll";
import { useMediaQuery } from "../hooks/useMediaQuery";
import { useUtilization } from "../hooks/useSeries";
import { usePreferences } from "../state/preferences";
import { wasRead } from "../api/types";

const NAV = [
  { to: "/", label: "Agent", icon: Bot, end: true },
  { to: "/agent/tools", label: "Agent Tools", icon: Wrench },
  { to: "/dashboard", label: "Dashboard", icon: LayoutDashboard },
  { to: "/chat", label: "Chat", icon: MessageSquare },
  { to: "/models", label: "Models", icon: Package },
  { to: "/inference", label: "Inference", icon: Sliders },
  { to: "/performance", label: "Performance", icon: Gauge },
  { to: "/gateway", label: "API Gateway", icon: Network },
  { to: "/access", label: "Access & Keys", icon: KeyRound },
  { to: "/settings", label: "Settings", icon: SettingsIcon },
  { to: "/logs", label: "Logs", icon: FileText },
];

/**
 * The frame every screen sits in: the rail on the left, the page on the right.
 *
 * The rail carries the machine's own state at the bottom, as the reference
 * does, because it is true on every screen and belongs where it is always
 * visible rather than repeated on each one.
 */
export function Shell() {
  const { preferences, update } = usePreferences();
  const location = useLocation();

  // Layout follows the same breakpoints the stylesheet uses. On mobile the rail
  // is a drawer and shows its full self; on tablet it is always compact so the
  // working surface keeps its room; on desktop it honours the saved preference.
  const mobile = useMediaQuery("(max-width: 760px)");
  const tablet = useMediaQuery("(max-width: 1100px) and (min-width: 761px)");
  const collapsed = mobile ? false : tablet ? true : preferences.railCollapsed;

  const [drawerOpen, setDrawerOpen] = useState(false);
  // A route change or a return to a wider viewport closes the drawer, so it is
  // never left open behind the page it navigated to.
  useEffect(() => setDrawerOpen(false), [location.pathname]);
  useEffect(() => {
    if (!mobile) setDrawerOpen(false);
  }, [mobile]);

  // One poll for the whole rail. Two seconds rather than one: this is ambient
  // context, and the screens that need a faster reading take their own.
  const system = usePoll(api.system, 2000);
  const times = wasRead(system.data?.cpu_times) ? system.data.cpu_times : null;
  const utilization = useUtilization(times);
  const memory = wasRead(system.data?.memory) ? system.data.memory : null;

  const nextTheme = preferences.theme === "dark" ? "light" : "dark";

  return (
    <div className={`shell${collapsed ? " is-collapsed" : ""}`}>
      <div className="shell__frame" aria-hidden="true" />

      {mobile && drawerOpen && (
        <button
          type="button"
          className="drawer-scrim"
          aria-label="Close the menu"
          onClick={() => setDrawerOpen(false)}
        />
      )}

      <nav
        className={`rail${mobile && drawerOpen ? " is-open" : ""}`}
        aria-label="Sections"
        aria-hidden={mobile && !drawerOpen}
      >
        <div className="rail__brand">
          {/*
            The application's own icon, from `public/`, rather than a glyph
            standing in for one - the same file the browser puts in the tab, so
            it is fetched once and the rail and the tab cannot drift apart. It
            is served by the gateway beside the panel, so it needs no network,
            and `alt` is empty because the name it stands for is spelled out
            immediately to its right.
          */}
          <img className="rail__mark" src="/icon.png" alt="" width={38} height={38} />
          {!collapsed && (
            <span className="rail__name">
              <strong>Lightweight</strong>
              <span>CPU Inference Gateway</span>
            </span>
          )}
        </div>

        <div className="rail__nav">
          {NAV.map(({ to, label, icon: Icon, end }) => (
            <NavLink
              key={to}
              to={to}
              end={end}
              className={({ isActive }) => `navitem${isActive ? " is-active" : ""}`}
              title={collapsed ? label : undefined}
            >
              <Icon size={18} strokeWidth={1.9} />
              {!collapsed && <span>{label}</span>}
            </NavLink>
          ))}
        </div>

        <div className="rail__spacer" />

        {!collapsed && (
          <div className="railcard">
            <span className="railcard__label">CPU Mode</span>
            <span
              style={{
                display: "flex",
                alignItems: "center",
                gap: 7,
                color: "var(--ok)",
                fontSize: 13,
                fontWeight: 500,
              }}
            >
              <span className="dot" />
              Active
            </span>
            <span className="railcard__line">
              {system.data
                ? `${system.data.cpu.physical_cores} cores · ${
                    system.data.cpu.has_avx_family ? "AVX" : "no AVX"
                  }`
                : "CPU only · no GPU"}
            </span>
            <span className="railcard__line">Local inference</span>

            {(utilization !== null || memory) && <hr />}
            {memory && (
              <MiniMeter
                label="RAM"
                value={`${bytes(memory.used)} / ${bytes(memory.total)}`}
                fraction={memory.pressure}
                color="var(--series-2)"
              />
            )}
            {utilization !== null && (
              <MiniMeter
                label="CPU"
                value={percent(utilization)}
                fraction={utilization}
                color="var(--series-1)"
              />
            )}
          </div>
        )}

        <div className="rail__controls">
          <button
            type="button"
            className="btn btn--icon"
            title={`Switch to ${nextTheme} theme`}
            aria-label={`Switch to ${nextTheme} theme`}
            onClick={() => update({ theme: nextTheme })}
          >
            {preferences.theme === "dark" ? <Sun size={17} /> : <Moon size={17} />}
          </button>
          <button
            type="button"
            className="btn btn--icon"
            title="Refresh"
            aria-label="Refresh the page"
            onClick={() => window.location.reload()}
          >
            <RotateCw size={16} />
          </button>
          <button
            type="button"
            className="btn btn--icon"
            title={collapsed ? "Expand the sidebar" : "Collapse the sidebar"}
            aria-label={collapsed ? "Expand the sidebar" : "Collapse the sidebar"}
            onClick={() => update({ railCollapsed: !collapsed })}
          >
            <ChevronLeft
              size={17}
              style={{ transform: collapsed ? "rotate(180deg)" : undefined }}
            />
          </button>
        </div>
      </nav>

      <main className="main">
        <div className="mobilebar">
          <button
            type="button"
            className="btn btn--icon"
            aria-label="Open the menu"
            aria-expanded={drawerOpen}
            onClick={() => setDrawerOpen(true)}
          >
            <MenuIcon size={18} />
          </button>
          <img className="rail__mark" src="/icon.png" alt="" width={26} height={26}
            style={{ width: 26, height: 26 }} />
          <span className="mobilebar__name">Lightweight</span>
        </div>
        <Outlet />
      </main>
    </div>
  );
}

function MiniMeter({
  label,
  value,
  fraction,
  color,
}: {
  label: string;
  value: string;
  fraction: number;
  color: string;
}) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 5 }}>
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          fontSize: 11.5,
          color: "var(--text-muted)",
        }}
      >
        <span>{label}</span>
        <span className="tnum">{value}</span>
      </div>
      <div className="meter__track" style={{ height: 5 }}>
        <div
          className="meter__fill"
          style={{ width: `${Math.min(100, fraction * 100)}%`, background: color }}
        />
      </div>
    </div>
  );
}

export function TopBar({
  title,
  subtitle,
  actions,
}: {
  title: string;
  subtitle: string;
  actions?: React.ReactNode;
}) {
  return (
    <header className="topbar">
      <div className="topbar__titles">
        <h1>{title}</h1>
        <p>{subtitle}</p>
      </div>
      {actions && <div className="topbar__actions">{actions}</div>}
    </header>
  );
}

export { Cpu };
