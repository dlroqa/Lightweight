interface SparklineProps {
  values: number[];
  color: string;
  width?: number;
  height?: number;
  /** A short sentence for a screen reader, since the shape carries the meaning. */
  label: string;
}

/**
 * A small area chart.
 *
 * Hand-drawn SVG rather than a charting library: this and the four line charts
 * on Performance are every chart the panel has, and a library for them would be
 * several times the size of the rest of the bundle.
 *
 * The area fill and the emphasised endpoint are deliberate — a bare polyline
 * reads as decoration, and the endpoint is the value the number beside it is
 * showing.
 */
export function Sparkline({
  values,
  color,
  width = 92,
  height = 40,
  label,
}: SparklineProps) {
  if (values.length < 2) {
    // Space is reserved even with nothing to draw, so the tile does not resize
    // the moment a second sample arrives.
    return <div style={{ width, height }} aria-hidden="true" />;
  }

  const low = Math.min(...values);
  const high = Math.max(...values);
  // A flat series would divide by zero; drawn along the middle instead.
  const span = high - low || 1;
  const step = width / (values.length - 1);
  const pad = 3;
  const usable = height - pad * 2;

  const points = values.map((value, index) => {
    const x = index * step;
    const y = pad + usable - ((value - low) / span) * usable;
    return [x, y] as const;
  });

  const line = points
    .map(([x, y], index) => `${index === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`)
    .join(" ");
  const area = `${line} L${width},${height} L0,${height} Z`;
  const last = points[points.length - 1];
  const gradientId = `spark-${label.replace(/\W+/g, "-")}`;

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      role="img"
      aria-label={label}
      style={{ overflow: "visible", flex: "none" }}
    >
      <defs>
        <linearGradient id={gradientId} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={color} stopOpacity="0.28" />
          <stop offset="100%" stopColor={color} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={area} fill={`url(#${gradientId})`} />
      <path
        d={line}
        fill="none"
        stroke={color}
        strokeWidth="1.75"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      {last && <circle cx={last[0]} cy={last[1]} r="2.5" fill={color} />}
    </svg>
  );
}
