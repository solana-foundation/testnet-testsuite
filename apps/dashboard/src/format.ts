// Number and time formatting shared by tiles, tooltips and tables.

export function compact(value: number | null | undefined, digits = 1): string {
  if (value == null || !Number.isFinite(value)) return "–";
  const abs = Math.abs(value);
  if (abs >= 1e9) return `${(value / 1e9).toFixed(digits)}B`;
  if (abs >= 1e6) return `${(value / 1e6).toFixed(digits)}M`;
  if (abs >= 1e3) return `${(value / 1e3).toFixed(digits)}K`;
  if (abs >= 100 || Number.isInteger(value)) return value.toFixed(0);
  if (abs >= 1) return value.toFixed(digits);
  if (abs === 0) return "0";
  return value.toPrecision(2);
}

export function percent(value: number | null | undefined, digits = 1): string {
  if (value == null || !Number.isFinite(value)) return "–";
  return `${(value * 100).toFixed(digits)}%`;
}

export function seconds(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "–";
  if (value < 1) return `${Math.round(value * 1000)} ms`;
  if (value < 120) return `${value.toFixed(1)} s`;
  return `${(value / 60).toFixed(1)} min`;
}

export function clock(us: number): string {
  return new Date(us / 1000).toLocaleTimeString([], { hour12: false });
}

export function dateTime(us: number): string {
  return new Date(us / 1000).toLocaleString([], { hour12: false });
}

export function short(key: string, keep = 4): string {
  return key.length > keep * 2 + 1
    ? `${key.slice(0, keep)}…${key.slice(-keep)}`
    : key;
}

export type Unit = "count" | "per_s" | "percent" | "seconds" | "plain";

export function formatUnit(
  value: number | null | undefined,
  unit: Unit,
): string {
  switch (unit) {
    case "percent":
      return percent(value);
    case "seconds":
      return seconds(value);
    case "per_s":
      return `${compact(value)}/s`;
    default:
      return compact(value);
  }
}
