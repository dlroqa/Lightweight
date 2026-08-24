interface MeterProps {
  label: string;
  value: string;
  of?: string;
  fraction: number | null;
  color: string;
  foot?: string;
}

/** A labelled bar, as the Dashboard's System Resources card uses. */
export function Meter({ label, value, of, fraction, color, foot }: MeterProps) {
  const filled = fraction === null ? 0 : Math.min(1, Math.max(0, fraction));
  return (
    <div className="meter">
      <div className="meter__head">
        <span style={{ fontWeight: 500 }}>{label}</span>
        <span className="meter__value tnum">
          <strong>{value}</strong>
          {of && ` / ${of}`}
        </span>
      </div>
      <div
        className="meter__track"
        role="progressbar"
        aria-label={label}
        aria-valuenow={fraction === null ? undefined : Math.round(filled * 100)}
        aria-valuemin={0}
        aria-valuemax={100}
      >
        <div
          className="meter__fill"
          style={{ width: `${filled * 100}%`, background: color }}
        />
      </div>
      {(foot || fraction !== null) && (
        <div className="meter__foot">
          <span>{foot ?? ""}</span>
          <span className="tnum">
            {fraction === null ? "not measured" : `${Math.round(filled * 100)}%`}
          </span>
        </div>
      )}
    </div>
  );
}
