import { useSyncExternalStore } from "react";

const query = "(prefers-color-scheme: dark)";

function subscribe(onChange: () => void): () => void {
  const mq = window.matchMedia(query);
  mq.addEventListener("change", onChange);
  return () => mq.removeEventListener("change", onChange);
}

/** True when the reader's system is in dark mode (the site follows prefers-color-scheme). */
export function useDarkScheme(): boolean {
  return useSyncExternalStore(subscribe, () => window.matchMedia(query).matches, () => false);
}
