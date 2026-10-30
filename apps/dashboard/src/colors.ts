// Colour roles, read from the CSS custom properties in theme.css so light and
// dark swap in one place. Categorical slots follow the validated reference
// palette (dataviz skill, references/palette.md); hues are assigned to
// entities in a fixed order per chart and never by rank, and a ninth entity
// folds into the neutral "other" colour.

export const SLOTS = 8;

export function cssVar(name: string): string {
  return getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
}

/** CSS variable for `name` from a fixed entity order; unknown names (and a
 *  ninth entity) are "other". Resolve with `cssVar` on canvases, or use as
 *  `var(...)` in styles, so a theme switch recolours everything. */
export function entityVar(name: string, order: readonly string[]): string {
  const index = order.indexOf(name);
  return index >= 0 && index < SLOTS ? `--series-${index + 1}` : "--other";
}

/** Producing clients on the leader lane, by testnet stake (2026-10-16: Agave
 *  49%, AgaveBam 22%, JitoLabs 17%, Firedancer 9%; the rest < 2% each are
 *  "other"). The lane is a sequence of adjacent cells, so four hues pass the
 *  adjacent-pair checks; the legend names each one. Order is fixed, so a
 *  client keeps its colour. */
export const CLIENTS = ["Agave", "Firedancer", "JitoLabs", "AgaveBam"] as const;

export function clientVar(client: string | null): string {
  return entityVar(client ?? "", CLIENTS);
}

export const OUTCOMES = [
  "landed",
  "failed",
  "simulation_failed",
  "expired",
  "ambiguous",
  "rejected",
  "client",
] as const;

export const PROGRAMS = [
  "raydium-clmm",
  "orca-whirlpool",
  "manifest",
  "jupiter",
  "spl-token",
  "system",
  "ata",
  "other",
] as const;
