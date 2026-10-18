// Aceternity UI — Flip Words (registry: @aceternity/flip-words).
// Adapted: strict TS, timer cleanup, no trailing space after the last word,
// token colour; under prefers-reduced-motion it does not cycle and shows `staticText`.
import { useCallback, useEffect, useState } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { cn } from "@/lib/utils";

export const FlipWords = ({
  words,
  duration = 3000,
  className,
  staticText,
}: {
  words: string[];
  duration?: number;
  className?: string;
  /** Shown instead of the cycling word when the reader prefers reduced motion. */
  staticText?: string;
}) => {
  const [currentWord, setCurrentWord] = useState(words[0] ?? "");
  const [isAnimating, setIsAnimating] = useState(false);
  const reduce = useReducedMotion();

  const startAnimation = useCallback(() => {
    const word = words[words.indexOf(currentWord) + 1] ?? words[0] ?? "";
    setCurrentWord(word);
    setIsAnimating(true);
  }, [currentWord, words]);

  useEffect(() => {
    if (reduce || isAnimating) return;
    const id = window.setTimeout(startAnimation, duration);
    return () => window.clearTimeout(id);
  }, [isAnimating, duration, startAnimation, reduce]);

  if (reduce) {
    return <span className={cn("relative inline", className)}>{staticText ?? currentWord}</span>;
  }

  const parts = currentWord.split(" ");
  return (
    <AnimatePresence onExitComplete={() => setIsAnimating(false)}>
      <motion.span
        initial={{ opacity: 0, y: 10 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ type: "spring", stiffness: 100, damping: 10 }}
        exit={{ opacity: 0, y: -40, x: 40, filter: "blur(8px)", scale: 2, position: "absolute" }}
        className={cn("relative z-10 inline-block text-left", className)}
        key={currentWord}
      >
        {parts.map((word, wordIndex) => (
          <motion.span
            key={word + wordIndex}
            initial={{ opacity: 0, y: 10, filter: "blur(8px)" }}
            animate={{ opacity: 1, y: 0, filter: "blur(0px)" }}
            transition={{ delay: wordIndex * 0.3, duration: 0.3 }}
            className="inline-block whitespace-nowrap"
          >
            {word.split("").map((letter, letterIndex) => (
              <motion.span
                key={word + letterIndex}
                initial={{ opacity: 0, y: 10, filter: "blur(8px)" }}
                animate={{ opacity: 1, y: 0, filter: "blur(0px)" }}
                transition={{ delay: wordIndex * 0.3 + letterIndex * 0.05, duration: 0.2 }}
                className="inline-block"
              >
                {letter}
              </motion.span>
            ))}
            {wordIndex < parts.length - 1 && <span className="inline-block">&nbsp;</span>}
          </motion.span>
        ))}
      </motion.span>
    </AnimatePresence>
  );
};
