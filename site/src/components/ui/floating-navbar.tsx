// Aceternity UI — Floating Navbar (registry: @aceternity/floating-navbar).
// Adapted: OpenApp tokens, a link CTA instead of the Login button, strict TS,
// inert while hidden so off-screen links never take keyboard focus.
import { useState, type ReactNode } from "react";
import { motion, AnimatePresence, useScroll, useMotionValueEvent, useReducedMotion } from "motion/react";
import { cn } from "@/lib/utils";

export interface FloatingNavItem {
  name: string;
  link: string;
  icon?: ReactNode;
}

export const FloatingNav = ({
  navItems,
  cta,
  className,
}: {
  navItems: FloatingNavItem[];
  cta?: { name: string; link: string; icon?: ReactNode };
  className?: string;
}) => {
  const { scrollYProgress } = useScroll();
  const [visible, setVisible] = useState(false);
  const reduce = useReducedMotion();

  useMotionValueEvent(scrollYProgress, "change", (current) => {
    const direction = current - (scrollYProgress.getPrevious() ?? 0);
    if (current < 0.05) setVisible(false);
    else setVisible(direction < 0);
  });

  return (
    <AnimatePresence mode="wait">
      <motion.div
        initial={{ opacity: 1, y: -100 }}
        animate={{ y: visible ? 0 : -100, opacity: visible ? 1 : 0 }}
        transition={{ duration: reduce ? 0 : 0.2 }}
        inert={!visible}
        aria-hidden={!visible}
        className={cn("fixed inset-x-0 top-4 z-[5000] mx-auto flex max-w-fit items-center justify-center sm:top-6", className)}
      >
        <nav
          aria-label="Site, floating"
          className="flex items-center justify-center gap-1 rounded-full border border-line bg-surface/85 px-1.5 py-1.5 shadow-[0_12px_32px_-18px_rgb(var(--shadow)/0.45)] backdrop-blur-md sm:gap-2 sm:px-2"
        >
          <div className="flex items-center gap-0.5 sm:gap-1">
            {navItems.map((navItem) => (
              <a
                key={navItem.link}
                href={navItem.link}
                aria-label={navItem.name}
                className="relative flex items-center gap-1 rounded-full px-3 py-2 text-sm font-medium text-ink-2 no-underline transition-colors hover:bg-mac-soft hover:text-ink sm:px-4"
              >
                <span className="block sm:hidden" aria-hidden="true">{navItem.icon}</span>
                <span className="hidden sm:block">{navItem.name}</span>
              </a>
            ))}
          </div>
          {cta && (
            <>
              <div className="h-5 w-px bg-line" />
              <a
                href={cta.link}
                aria-label={cta.name}
                className="relative flex items-center gap-1.5 rounded-full bg-ink px-3 py-2 text-sm font-medium text-paper no-underline transition-colors hover:bg-vm hover:text-surface sm:px-4"
              >
                <span aria-hidden="true">{cta.icon}</span>
                <span className="hidden sm:block">{cta.name}</span>
              </a>
            </>
          )}
        </nav>
      </motion.div>
    </AnimatePresence>
  );
};
