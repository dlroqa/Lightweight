import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";

import { api } from "../api/client";
import type { PanelPreferences, Settings } from "../api/types";

/**
 * The panel's own preferences.
 *
 * They live in the `ui` half of the gateway's settings file, which the gateway
 * stores and never interprets. That means they follow the gateway rather than
 * the browser: opening the panel from another machine over the exposed bind
 * shows the same theme and the same sampling defaults.
 *
 * `localStorage` mirrors them only so the first paint is not a flash of the
 * wrong theme while the settings request is in flight.
 */

const MIRROR_KEY = "hermes.panel.preferences";

export const DEFAULT_PREFERENCES = {
  theme: "system" as "light" | "dark" | "system",
  translucent: true,
  compact: false,
  railCollapsed: false,
  sampling: {
    temperature: 0.7,
    top_p: 0.9,
    top_k: 40,
    min_p: 0.05,
    repeat_penalty: 1.1,
    presence_penalty: 0,
    frequency_penalty: 0,
    seed: -1,
    max_tokens: 512,
  },
};

export type Preferences = typeof DEFAULT_PREFERENCES;

interface PreferencesValue {
  preferences: Preferences;
  settings: Settings | null;
  /** Persisted to the gateway; falls back to the local mirror if that fails. */
  update: (patch: Partial<Preferences>) => void;
  saveGateway: (patch: Partial<Settings["gateway"]>) => Promise<void>;
  /** True when settings could not be reached, so screens can say so. */
  offline: boolean;
}

const PreferencesContext = createContext<PreferencesValue | null>(null);

function readMirror(): Preferences {
  try {
    const raw = window.localStorage.getItem(MIRROR_KEY);
    if (!raw) return DEFAULT_PREFERENCES;
    const parsed = JSON.parse(raw) as Partial<Preferences>;
    return {
      ...DEFAULT_PREFERENCES,
      ...parsed,
      sampling: { ...DEFAULT_PREFERENCES.sampling, ...(parsed.sampling ?? {}) },
    };
  } catch {
    // A private window, or storage the browser refuses. The defaults are a
    // complete answer.
    return DEFAULT_PREFERENCES;
  }
}

function writeMirror(preferences: Preferences) {
  try {
    window.localStorage.setItem(MIRROR_KEY, JSON.stringify(preferences));
  } catch {
    // Nothing here needs storage to work; it only makes the first paint calmer.
  }
}

export function PreferencesProvider({ children }: { children: ReactNode }) {
  const [preferences, setPreferences] = useState<Preferences>(readMirror);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [offline, setOffline] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void api
      .settings()
      .then((loaded) => {
        if (cancelled) return;
        setSettings(loaded);
        const stored = (loaded.ui ?? {}) as Partial<PanelPreferences>;
        setPreferences((current) => ({
          ...current,
          ...stored,
          sampling: {
            ...DEFAULT_PREFERENCES.sampling,
            ...(stored.sampling ?? {}),
          },
        }));
        setOffline(false);
      })
      .catch(() => {
        if (!cancelled) setOffline(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The theme is applied to the document rather than threaded through props:
  // it decides the token set, and every surface reads from that.
  useEffect(() => {
    const root = document.documentElement;
    if (preferences.theme === "system") {
      root.removeAttribute("data-theme");
    } else {
      root.setAttribute("data-theme", preferences.theme);
    }
    root.setAttribute("data-surfaces", preferences.translucent ? "glass" : "solid");
    root.setAttribute("data-density", preferences.compact ? "compact" : "comfortable");
  }, [preferences.theme, preferences.translucent, preferences.compact]);

  const update = useCallback((patch: Partial<Preferences>) => {
    setPreferences((current) => {
      const next = { ...current, ...patch };
      writeMirror(next);
      // Saved through the gateway so the preference follows the gateway. A
      // failure is not raised here: the change has already taken effect
      // locally, and the Settings screen is where a persistent failure is
      // reported.
      setSettings((existing) => {
        const base: Settings = existing ?? {
          gateway: { keep_history: true, default_n_ctx: null },
          ui: {},
        };
        const merged: Settings = { ...base, ui: { ...base.ui, ...next } };
        void api.saveSettings(merged).catch(() => setOffline(true));
        return merged;
      });
      return next;
    });
  }, []);

  const saveGateway = useCallback(
    async (patch: Partial<Settings["gateway"]>) => {
      const base: Settings = settings ?? {
        gateway: { keep_history: true, default_n_ctx: null },
        ui: preferences as unknown as Settings["ui"],
      };
      const merged: Settings = { ...base, gateway: { ...base.gateway, ...patch } };
      const saved = await api.saveSettings(merged);
      setSettings(saved);
      setOffline(false);
    },
    [settings, preferences],
  );

  const value = useMemo<PreferencesValue>(
    () => ({ preferences, settings, update, saveGateway, offline }),
    [preferences, settings, update, saveGateway, offline],
  );

  return (
    <PreferencesContext.Provider value={value}>{children}</PreferencesContext.Provider>
  );
}

export function usePreferences(): PreferencesValue {
  const value = useContext(PreferencesContext);
  if (!value) {
    throw new Error("usePreferences must be used inside PreferencesProvider");
  }
  return value;
}
