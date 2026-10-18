import { useEffect, useState } from "react";
import { Circle, type LucideIcon, type LucideProps } from "lucide-react";

type IconMap = Record<string, LucideIcon | undefined>;

// The full lucide set (~1700 icons) is a single lazy chunk, fetched on first use of an icon-by-name.
let iconMap: IconMap | null = null;
let loading: Promise<IconMap> | null = null;

function loadMap(): Promise<IconMap> {
  loading ??= import("lucide-react").then((m) => (iconMap = m.icons as unknown as IconMap));
  return loading;
}

function toPascal(name: string): string {
  return name
    .trim()
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((p) => p.charAt(0).toUpperCase() + p.slice(1))
    .join("");
}

/** Sync lookup; `null` while the icon map is still loading, `undefined` for an unknown name. */
function lookup(name: string | undefined): LucideIcon | null | undefined {
  if (!name) return undefined;
  return iconMap ? iconMap[toPascal(name)] : null;
}

/** Resolve a lucide icon by kebab-case name (falls back to `Circle` for unknown names). */
export async function loadIcon(name: string | undefined): Promise<LucideIcon> {
  const map = await loadMap();
  return map[toPascal(name ?? "")] ?? Circle;
}

/** Lucide icon by kebab-case name (fallback: a plain circle; an empty box of the same size while loading). */
export function Icon({ name, size = 16, ...rest }: { name?: string } & LucideProps) {
  const [, bump] = useState(0);
  const hit = lookup(name);
  useEffect(() => {
    if (hit !== null) return;
    let live = true;
    void loadMap().then(() => live && bump((n) => n + 1));
    return () => {
      live = false;
    };
  }, [hit]);
  if (hit === null) return <span aria-hidden="true" style={{ display: "inline-block", width: size, height: size, flex: "none" }} />;
  if (hit === undefined) {
    // no name or unknown name: plain circle (Circle is cheap and statically available)
    return <Circle size={size} strokeWidth={1.75} aria-hidden="true" {...rest} />;
  }
  const C = hit;
  return <C size={size} strokeWidth={1.75} aria-hidden="true" {...rest} />;
}
