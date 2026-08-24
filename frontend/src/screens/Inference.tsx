import { RotateCcw } from "lucide-react";

import { Card } from "../components/Card";
import { TopBar } from "../components/Shell";
import { DEFAULT_PREFERENCES, usePreferences } from "../state/preferences";

/**
 * Sampling parameters.
 *
 * Every control here is a value sent with each chat request, which is where
 * OpenAI's API puts them — so they are the panel's defaults rather than gateway
 * state, and they follow the gateway because they live in the settings file's
 * `ui` half.
 *
 * The reference also shows a Batch Size control. It is deliberately absent: the
 * engine is always given the `RuntimeParams` defaults for `--batch-size` and
 * `--ubatch-size`, and no request or load option can vary them, so a slider
 * here would change nothing — which is worse than no slider.
 */
export function Inference() {
  const { preferences, update } = usePreferences();
  const sampling = preferences.sampling;

  function set(patch: Partial<typeof sampling>) {
    update({ sampling: { ...sampling, ...patch } });
  }

  return (
    <>
      <TopBar
        title="Inference"
        subtitle="Defaults applied to every message you send"
        actions={
          <button
            type="button"
            className="btn"
            onClick={() => update({ sampling: DEFAULT_PREFERENCES.sampling })}
          >
            <RotateCcw size={15} />
            Reset to defaults
          </button>
        }
      />

      <div className="page">
        <div
          className="grid"
          style={{ gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))" }}
        >
          <Card title="Sampling">
            <Slider
              label="Temperature"
              value={sampling.temperature}
              min={0}
              max={2}
              step={0.05}
              onChange={(temperature) => set({ temperature })}
              hint="Higher is more varied. 0 makes the model pick its most likely token every time."
            />
            <Slider
              label="Top P"
              value={sampling.top_p}
              min={0}
              max={1}
              step={0.01}
              onChange={(top_p) => set({ top_p })}
              hint="Considers only the most likely tokens whose probabilities sum to this."
            />
            <Slider
              label="Top K"
              value={sampling.top_k}
              min={0}
              max={200}
              step={1}
              onChange={(top_k) => set({ top_k })}
              hint="Considers only this many candidates. 0 turns it off."
            />
            <Slider
              label="Min P"
              value={sampling.min_p}
              min={0}
              max={1}
              step={0.01}
              onChange={(min_p) => set({ min_p })}
              hint="Drops candidates far less likely than the best one."
            />
          </Card>

          <Card title="Penalties">
            <Slider
              label="Repeat penalty"
              value={sampling.repeat_penalty}
              min={0.5}
              max={2}
              step={0.01}
              onChange={(repeat_penalty) => set({ repeat_penalty })}
              hint="Above 1 discourages the model from repeating itself."
            />
            <Slider
              label="Presence penalty"
              value={sampling.presence_penalty}
              min={-2}
              max={2}
              step={0.01}
              onChange={(presence_penalty) => set({ presence_penalty })}
              hint="Pushes the model towards subjects it has not raised yet."
            />
            <Slider
              label="Frequency penalty"
              value={sampling.frequency_penalty}
              min={-2}
              max={2}
              step={0.01}
              onChange={(frequency_penalty) => set({ frequency_penalty })}
              hint="Pushes down words it has already used often."
            />
          </Card>

          <Card title="Length and determinism">
            <Slider
              label="Max tokens"
              value={sampling.max_tokens}
              min={16}
              max={4096}
              step={16}
              onChange={(max_tokens) => set({ max_tokens })}
              hint="The gateway clamps this to what the loaded context can actually hold."
            />
            <div className="field" style={{ marginTop: 14 }}>
              <label className="field__label" htmlFor="seed">
                Seed
              </label>
              <input
                id="seed"
                className="input tnum"
                type="number"
                value={sampling.seed}
                onChange={(event) => set({ seed: Number(event.target.value) })}
              />
              <span style={{ fontSize: 11.5, color: "var(--text-muted)" }}>
                −1 asks the engine to choose one, so each answer differs. Any other
                value is sent as given, which makes a run repeatable.
              </span>
            </div>
          </Card>
        </div>

        <div className="notice notice--info">
          These are the panel's defaults and are stored with your settings, so
          they follow the gateway rather than this browser. Each one is sent with
          the request and acted on by the engine.
        </div>
      </div>
    </>
  );
}

function Slider({
  label,
  value,
  min,
  max,
  step,
  onChange,
  hint,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  onChange: (next: number) => void;
  hint: string;
}) {
  const id = `slider-${label.replace(/\W+/g, "-").toLowerCase()}`;
  return (
    <div className="field" style={{ marginBottom: 16 }}>
      <div style={{ display: "flex", justifyContent: "space-between", gap: 12 }}>
        <label className="field__label" htmlFor={id}>
          {label}
        </label>
        <input
          className="input tnum"
          style={{ width: 84, padding: "3px 8px", textAlign: "right" }}
          type="number"
          value={value}
          min={min}
          max={max}
          step={step}
          onChange={(event) => onChange(Number(event.target.value))}
          aria-label={`${label} value`}
        />
      </div>
      <input
        id={id}
        className="range"
        type="range"
        value={value}
        min={min}
        max={max}
        step={step}
        onChange={(event) => onChange(Number(event.target.value))}
      />
      <div className="range__ends">
        <span className="tnum">{min}</span>
        <span className="tnum">{max}</span>
      </div>
      <span style={{ fontSize: 11.5, color: "var(--text-muted)" }}>{hint}</span>
    </div>
  );
}
