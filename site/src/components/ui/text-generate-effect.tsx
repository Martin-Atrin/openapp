// Aceternity UI — Text Generate Effect (registry: @aceternity/text-generate-effect).
// Adapted: strict TS, runs once when scrolled into view, inherits the heading's own
// type and colour (no forced font/size/black-white), plain text under reduced motion.
import { useEffect } from "react";
import { motion, stagger, useAnimate, useInView, useReducedMotion } from "motion/react";
import { cn } from "@/lib/utils";

export const TextGenerateEffect = ({
  words,
  className,
  filter = true,
  duration = 0.5,
}: {
  words: string;
  className?: string;
  filter?: boolean;
  duration?: number;
}) => {
  const [scope, animate] = useAnimate<HTMLSpanElement>();
  const inView = useInView(scope, { once: true, amount: 0.6 });
  const reduce = useReducedMotion();
  const wordsArray = words.split(" ");

  useEffect(() => {
    if (!inView || reduce) return;
    void animate(
      "span",
      { opacity: 1, filter: filter ? "blur(0px)" : "none" },
      { duration: duration || 1, delay: stagger(0.2) },
    );
  }, [inView, reduce, animate, filter, duration]);

  if (reduce) return <span className={className}>{words}</span>;

  return (
    <span ref={scope} className={cn("inline", className)}>
      {wordsArray.map((word, idx) => (
        <motion.span
          key={word + idx}
          className="opacity-0"
          style={{ filter: filter ? "blur(10px)" : "none" }}
        >
          {word}
          {idx < wordsArray.length - 1 ? " " : ""}
        </motion.span>
      ))}
    </span>
  );
};
