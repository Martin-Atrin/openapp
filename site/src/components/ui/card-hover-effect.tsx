// Aceternity UI — Card Hover Effect (registry: @aceternity/card-hover-effect).
// Adapted: strict TS, list items instead of links (these cards have no destination),
// a status line per card, OpenApp tokens (vm-soft hover, surface cards, line borders).
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useState, type ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface HoverEffectItem {
  title: ReactNode;
  description: ReactNode;
  footer?: ReactNode;
  key: string;
}

export const HoverEffect = ({ items, className }: { items: HoverEffectItem[]; className?: string }) => {
  const [hoveredIndex, setHoveredIndex] = useState<number | null>(null);
  const reduce = useReducedMotion();

  return (
    <ul className={cn("m-0 grid list-none grid-cols-1 p-0 md:grid-cols-2 lg:grid-cols-3", className)}>
      {items.map((item, idx) => (
        <li
          key={item.key}
          className="group relative block h-full w-full p-1.5"
          onMouseEnter={() => setHoveredIndex(idx)}
          onMouseLeave={() => setHoveredIndex(null)}
        >
          <AnimatePresence>
            {hoveredIndex === idx && (
              <motion.span
                className="absolute inset-0 block h-full w-full rounded-[20px] bg-vm-soft"
                layoutId={reduce ? undefined : "hoverBackground"}
                initial={{ opacity: 0 }}
                animate={{ opacity: 1, transition: { duration: 0.15 } }}
                exit={{ opacity: 0, transition: { duration: 0.15, delay: 0.2 } }}
              />
            )}
          </AnimatePresence>
          <Card>
            <CardTitle>{item.title}</CardTitle>
            <CardDescription>{item.description}</CardDescription>
            {item.footer}
          </Card>
        </li>
      ))}
    </ul>
  );
};

export const Card = ({ className, children }: { className?: string; children: ReactNode }) => (
  <div
    className={cn(
      "relative z-20 flex h-full w-full flex-col overflow-hidden rounded-2xl border border-line bg-surface p-5 transition-colors group-hover:border-vm/40",
      className,
    )}
  >
    {children}
  </div>
);

export const CardTitle = ({ className, children }: { className?: string; children: ReactNode }) => (
  <h3 className={cn("m-0 text-[1.04rem] font-semibold text-ink", className)}>{children}</h3>
);

export const CardDescription = ({ className, children }: { className?: string; children: ReactNode }) => (
  <p className={cn("mt-2 mb-4 flex-1 text-[0.95rem] leading-relaxed text-ink-2", className)}>{children}</p>
);
