// Aceternity UI — Hover Border Gradient (registry: @aceternity/hover-border-gradient).
// Adapted: strict TS, teal highlight from the current colour scheme instead of
// white/blue, OpenApp control radius and tokens, no orbiting under reduced motion.
import { useEffect, useState, type ElementType, type HTMLAttributes, type PropsWithChildren } from "react";
import { motion, useReducedMotion } from "motion/react";
import { cn } from "@/lib/utils";
import { useDarkScheme } from "@/lib/use-dark-scheme";

type Direction = "TOP" | "LEFT" | "BOTTOM" | "RIGHT";
const directions: Direction[] = ["TOP", "LEFT", "BOTTOM", "RIGHT"];

export function HoverBorderGradient({
  children,
  containerClassName,
  className,
  as: Tag = "button",
  duration = 1,
  clockwise = true,
  ...props
}: PropsWithChildren<
  {
    as?: ElementType;
    containerClassName?: string;
    className?: string;
    duration?: number;
    clockwise?: boolean;
    href?: string;
  } & HTMLAttributes<HTMLElement>
>) {
  const [hovered, setHovered] = useState(false);
  const [direction, setDirection] = useState<Direction>("TOP");
  const reduce = useReducedMotion();
  const dark = useDarkScheme();
  // Plain colours (not var()) so motion can interpolate the gradients.
  const c = dark ? "243, 238, 227" : "20, 18, 16";

  const movingMap: Record<Direction, string> = {
    TOP: `radial-gradient(20.7% 50% at 50% 0%, rgba(${c}, 1) 0%, rgba(${c}, 0) 100%)`,
    LEFT: `radial-gradient(16.6% 43.1% at 0% 50%, rgba(${c}, 1) 0%, rgba(${c}, 0) 100%)`,
    BOTTOM: `radial-gradient(20.7% 50% at 50% 100%, rgba(${c}, 1) 0%, rgba(${c}, 0) 100%)`,
    RIGHT: `radial-gradient(16.2% 41.2% at 100% 50%, rgba(${c}, 1) 0%, rgba(${c}, 0) 100%)`,
  };
  const highlight = `radial-gradient(75% 181.16% at 50% 50%, rgba(${c}, 1) 0%, rgba(${c}, 0) 100%)`;

  useEffect(() => {
    if (hovered || reduce) return;
    const interval = window.setInterval(() => {
      setDirection((prev) => {
        const i = directions.indexOf(prev);
        const next = clockwise ? (i - 1 + directions.length) % directions.length : (i + 1) % directions.length;
        return directions[next] ?? "TOP";
      });
    }, duration * 1000);
    return () => window.clearInterval(interval);
  }, [hovered, reduce, clockwise, duration]);

  return (
    <Tag
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      className={cn(
        "relative flex h-min w-fit flex-col flex-nowrap content-center items-center justify-center gap-10 overflow-visible rounded-[11px] border border-line bg-line/60 p-px no-underline decoration-clone transition duration-500",
        containerClassName,
      )}
      {...props}
    >
      <div className={cn("z-10 w-auto rounded-[inherit] bg-ink px-5 py-3 font-[560] text-paper", className)}>
        {children}
      </div>
      <motion.div
        className="absolute inset-0 z-0 flex-none overflow-hidden rounded-[inherit]"
        style={{ filter: "blur(2px)", position: "absolute", width: "100%", height: "100%" }}
        initial={{ background: movingMap[direction] }}
        animate={{ background: hovered ? [movingMap[direction], highlight] : movingMap[direction] }}
        transition={{ ease: "linear", duration: reduce ? 0 : duration }}
      />
      <div className="absolute inset-[2px] z-[1] flex-none rounded-[9px] bg-ink" />
    </Tag>
  );
}
