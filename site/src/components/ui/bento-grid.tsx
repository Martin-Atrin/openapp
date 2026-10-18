// Aceternity UI — Bento Grid (registry: @aceternity/bento-grid).
// Adapted: OpenApp tokens (surface, line, ink), semantic list markup, title as h4,
// hover nudge disabled under prefers-reduced-motion.
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export const BentoGrid = ({ className, children }: { className?: string; children?: ReactNode }) => (
  <ul className={cn("mx-auto m-0 grid list-none grid-cols-1 gap-4 p-0 md:auto-rows-[17rem] md:grid-cols-3", className)}>
    {children}
  </ul>
);

export const BentoGridItem = ({
  className,
  title,
  description,
  header,
  icon,
}: {
  className?: string;
  title?: ReactNode;
  description?: ReactNode;
  header?: ReactNode;
  icon?: ReactNode;
}) => (
  <li
    className={cn(
      "group/bento row-span-1 flex min-w-0 flex-col justify-between gap-4 rounded-xl border border-line bg-surface p-4 shadow-[0_1px_2px_rgb(var(--shadow)/0.06)] transition duration-200 hover:shadow-[0_18px_36px_-22px_rgb(var(--shadow)/0.45)]",
      className,
    )}
  >
    {header}
    <div className="transition duration-200 group-hover/bento:translate-x-1.5 motion-reduce:transform-none motion-reduce:transition-none">
      {icon}
      <h4 className="mt-1 mb-1 text-[1rem] font-[620] text-ink">{title}</h4>
      <p className="m-0 text-[0.9rem] leading-snug text-ink-2">{description}</p>
    </div>
  </li>
);
