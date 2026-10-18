// Aceternity UI — Spotlight, new (registry: @aceternity/spotlight-new).
// Adapted: teal token gradients (--spot-1/--spot-2, light and dark), strict TS,
// no drift under prefers-reduced-motion, contained to its parent instead of the viewport.
import { motion, useReducedMotion } from "motion/react";

type SpotlightProps = {
  gradientFirst?: string;
  gradientSecond?: string;
  gradientThird?: string;
  translateY?: number;
  width?: number;
  height?: number;
  smallWidth?: number;
  duration?: number;
  xOffset?: number;
  /** Render only the left beam (the one behind the hero copy). */
  leftOnly?: boolean;
};

export const Spotlight = ({
  gradientFirst = "radial-gradient(68.54% 68.72% at 55.02% 31.46%, var(--spot-1) 0, var(--spot-2) 50%, transparent 80%)",
  gradientSecond = "radial-gradient(50% 50% at 50% 50%, var(--spot-1) 0, var(--spot-2) 80%, transparent 100%)",
  gradientThird = "radial-gradient(50% 50% at 50% 50%, var(--spot-2) 0, var(--spot-2) 80%, transparent 100%)",
  translateY = -350,
  width = 560,
  height = 1380,
  smallWidth = 240,
  duration = 7,
  xOffset = 100,
  leftOnly = false,
}: SpotlightProps = {}) => {
  const reduce = useReducedMotion();
  const drift = (dir: 1 | -1) =>
    reduce
      ? {}
      : {
          animate: { x: [0, dir * xOffset, 0] },
          transition: { duration, repeat: Infinity, repeatType: "reverse" as const, ease: "easeInOut" as const },
        };

  return (
    <motion.div
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: reduce ? 0 : 1.5 }}
      className="pointer-events-none absolute inset-0 h-full w-full"
      aria-hidden="true"
    >
      <motion.div {...drift(1)} className="pointer-events-none absolute top-0 left-0 h-full w-full">
        <div
          style={{ transform: `translateY(${translateY}px) rotate(-45deg)`, background: gradientFirst, width, height }}
          className="absolute top-0 left-0"
        />
        <div
          style={{ transform: "rotate(-45deg) translate(5%, -50%)", background: gradientSecond, width: smallWidth, height }}
          className="absolute top-0 left-0 origin-top-left"
        />
        <div
          style={{ transform: "rotate(-45deg) translate(-180%, -70%)", background: gradientThird, width: smallWidth, height }}
          className="absolute top-0 left-0 origin-top-left"
        />
      </motion.div>

      {!leftOnly && (
        <motion.div {...drift(-1)} className="pointer-events-none absolute top-0 right-0 h-full w-full">
          <div
            style={{ transform: `translateY(${translateY}px) rotate(45deg)`, background: gradientFirst, width, height }}
            className="absolute top-0 right-0"
          />
          <div
            style={{ transform: "rotate(45deg) translate(-5%, -50%)", background: gradientSecond, width: smallWidth, height }}
            className="absolute top-0 right-0 origin-top-right"
          />
          <div
            style={{ transform: "rotate(45deg) translate(180%, -70%)", background: gradientThird, width: smallWidth, height }}
            className="absolute top-0 right-0 origin-top-right"
          />
        </motion.div>
      )}
    </motion.div>
  );
};
