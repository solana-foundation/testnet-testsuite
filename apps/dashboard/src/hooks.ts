import { type RefObject, useEffect, useState } from "react";

/** Bumps when the theme changes (OS setting or the toggle), so canvas
 *  drawings re-read their CSS-variable colours. */
export function useThemeVersion(): number {
  const [version, setVersion] = useState(0);
  useEffect(() => {
    const bump = () => setVersion((v) => v + 1);
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    media.addEventListener("change", bump);
    const attrs = new MutationObserver(bump);
    attrs.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    return () => {
      media.removeEventListener("change", bump);
      attrs.disconnect();
    };
  }, []);
  return version;
}

/** The element's content width, tracked. */
export function useWidth(
  ref: RefObject<HTMLElement | null>,
  initial = 800,
): number {
  const [width, setWidth] = useState(initial);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const observer = new ResizeObserver(([entry]) => {
      if (entry) setWidth(Math.floor(entry.contentRect.width));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

/** Re-run `load` every `ms` (and when deps change); returns its last result. */
export function usePoll<T>(
  load: () => Promise<T>,
  ms: number,
  deps: unknown[],
): { data: T | null; error: string | null; loading: boolean } {
  const [state, setState] = useState<{
    data: T | null;
    error: string | null;
    loading: boolean;
  }>({
    data: null,
    error: null,
    loading: true,
  });
  useEffect(() => {
    let alive = true;
    const run = () => {
      setState((s) => ({ ...s, loading: true }));
      load().then(
        (data) => alive && setState({ data, error: null, loading: false }),
        (error: unknown) =>
          alive &&
          setState((s) => ({ ...s, error: String(error), loading: false })),
      );
    };
    run();
    const timer = window.setInterval(run, ms);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, deps);
  return state;
}
