import { useEffect, useState } from "react";

/**
 * Track a CSS media query from React, so layout that a stylesheet cannot reach
 * on its own — a rail rendered as a drawer, labels dropped from the DOM at
 * tablet width — follows the same breakpoints the stylesheet uses.
 *
 * The first value is read synchronously so the first paint is already correct,
 * and the listener is torn down with the component. Server-safe: when
 * `matchMedia` is missing it simply reports `false`.
 */
export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() =>
    typeof window !== "undefined" && "matchMedia" in window
      ? window.matchMedia(query).matches
      : false,
  );

  useEffect(() => {
    if (typeof window === "undefined" || !("matchMedia" in window)) return;
    const list = window.matchMedia(query);
    const update = () => setMatches(list.matches);
    update();
    list.addEventListener("change", update);
    return () => list.removeEventListener("change", update);
  }, [query]);

  return matches;
}
