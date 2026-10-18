import type { ThemeValue } from "./contract";

const DENSITY: Record<string, number> = { compact: 0.8, normal: 1, comfy: 1.25 };

/** Map a resolved theme to CSS custom properties. radius -> px, density -> --oa-space scale. */
export function themeVars(theme: Record<string, ThemeValue>): Record<string, string> {
  const vars: Record<string, string> = {};
  for (const [key, value] of Object.entries(theme)) {
    if (key === "radius") {
      const n = Number(value);
      if (Number.isFinite(n)) vars["--oa-radius"] = `${n}px`;
    } else if (key === "density") {
      vars["--oa-density"] = String(value);
      vars["--oa-space"] = String(DENSITY[String(value)] ?? 1);
    } else {
      vars[`--oa-${key}`] = String(value);
    }
  }
  return vars;
}

let applied = new Set<string>();

export function applyTheme(vars: Record<string, string>): void {
  const root = document.documentElement;
  for (const k of applied) if (!(k in vars)) root.style.removeProperty(k);
  for (const [k, v] of Object.entries(vars)) root.style.setProperty(k, v);
  applied = new Set(Object.keys(vars));
}
