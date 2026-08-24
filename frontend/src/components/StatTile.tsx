import type { ReactNode } from "react";

import { Sparkline } from "./Sparkline";

interface StatTileProps {
  icon: ReactNode;
  tint: string;
  label: string;
  value: string;
  unit?: string;
  sub?: string;
  series?: number[];
  seriesColor?: string;
}

/**
 * One headline number, as the reference draws it: a tinted icon chip, the
 * label, a large figure, a quiet sub-line, and the recent shape of it.
 */
export function StatTile({
  icon,
  tint,
  label,
  value,
  unit,
  sub,
  series,
  seriesColor,
}: StatTileProps) {
  return (
    <div className="card tile">
      <div className="tile__head">
        <span
          className="tile__icon"
          style={{ background: `color-mix(in srgb, ${tint} 14%, transparent)`, color: tint }}
        >
          {icon}
        </span>
        <span className="tile__label">{label}</span>
      </div>
      <div className="tile__body">
        <div>
          <div className="tile__value tnum">
            {value}
            {unit && <span className="tile__unit">{unit}</span>}
          </div>
          {sub && <div className="tile__sub">{sub}</div>}
        </div>
        {series && (
          <Sparkline
            values={series}
            color={seriesColor ?? tint}
            label={`${label} over the last few samples`}
          />
        )}
      </div>
    </div>
  );
}
