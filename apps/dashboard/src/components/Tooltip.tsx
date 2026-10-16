import type { ReactNode } from "react";

/** A pointer-following tooltip; flips left/up near the viewport edges. */
export function Tooltip({
  x,
  y,
  children,
}: {
  x: number;
  y: number;
  children: ReactNode;
}) {
  const left = x + 16 + 300 > window.innerWidth ? x - 16 - 300 : x + 16;
  const top = y + 16 + 220 > window.innerHeight ? y - 16 - 200 : y + 16;
  return (
    <div
      className="tooltip"
      style={{ left: Math.max(4, left), top: Math.max(4, top) }}
      role="tooltip"
    >
      {children}
    </div>
  );
}

/** Label/value pairs, value emphasised. */
export function KeyValues({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <div className="kv">
      {rows.map(([key, value]) => (
        <FragmentRow key={key} name={key} value={value} />
      ))}
    </div>
  );
}

function FragmentRow({ name, value }: { name: string; value: ReactNode }) {
  return (
    <>
      <span>{name}</span>
      <span>{value}</span>
    </>
  );
}

/** A status label: coloured dot + text, never colour alone. */
export function StatusLabel({
  color,
  label,
  hollow = false,
}: {
  color: string;
  label: string;
  hollow?: boolean;
}) {
  return (
    <span className="status">
      <span
        className="dot"
        style={
          hollow
            ? { border: `2px solid ${color}`, width: 8, height: 8 }
            : { background: color }
        }
      />
      {label}
    </span>
  );
}
