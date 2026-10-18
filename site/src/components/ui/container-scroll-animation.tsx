// Aceternity UI — Container Scroll Animation (registry: @aceternity/container-scroll-animation).
// Adapted: strict TS, OpenApp tokens for the frame, content-sized card (no fixed
// 30/40rem heights or 60/80rem scroll runway), offsets tuned for a hero that sits
// near the top of the page, a gentler tilt on narrow screens, and no tilt/scale/shift
// under prefers-reduced-motion.
import { useEffect, useRef, useState, type ReactNode } from "react";
import { useScroll, useTransform, motion, useReducedMotion, type MotionValue } from "motion/react";
import { cn } from "@/lib/utils";

export const ContainerScroll = ({
  titleComponent,
  children,
  className,
}: {
  titleComponent?: ReactNode;
  children: ReactNode;
  className?: string;
}) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const reduce = useReducedMotion();
  const { scrollYProgress } = useScroll({ target: containerRef, offset: ["start 95%", "start 15%"] });
  const [isMobile, setIsMobile] = useState(false);

  useEffect(() => {
    const mq = window.matchMedia("(max-width: 768px)");
    const update = () => setIsMobile(mq.matches);
    update();
    mq.addEventListener("change", update);
    return () => mq.removeEventListener("change", update);
  }, []);

  const rotate = useTransform(scrollYProgress, [0, 1], isMobile ? [12, 0] : [20, 0]);
  const scale = useTransform(scrollYProgress, [0, 1], isMobile ? [0.94, 1] : [1.05, 1]);
  const translate = useTransform(scrollYProgress, [0, 1], [0, isMobile ? -24 : -60]);

  return (
    <div ref={containerRef} className={cn("relative flex items-center justify-center overflow-x-clip", className)}>
      <div className="relative w-full" style={{ perspective: "1000px" }}>
        {titleComponent && <Header translate={translate} reduce={!!reduce}>{titleComponent}</Header>}
        <Card rotate={rotate} scale={scale} reduce={!!reduce}>
          {children}
        </Card>
      </div>
    </div>
  );
};

export const Header = ({
  translate,
  reduce,
  children,
}: {
  translate: MotionValue<number>;
  reduce: boolean;
  children: ReactNode;
}) => (
  <motion.div style={reduce ? undefined : { translateY: translate }} className="mx-auto max-w-5xl text-center">
    {children}
  </motion.div>
);

export const Card = ({
  rotate,
  scale,
  reduce,
  children,
}: {
  rotate: MotionValue<number>;
  scale: MotionValue<number>;
  reduce: boolean;
  children: ReactNode;
}) => (
  <motion.div
    style={reduce ? undefined : { rotateX: rotate, scale }}
    className="mx-auto w-full max-w-[960px] rounded-[22px] border border-line bg-surface/70 p-1.5 shadow-[0_0_#0000,0_9px_20px_rgb(var(--shadow)/0.10),0_37px_37px_rgb(var(--shadow)/0.08),0_84px_50px_rgb(var(--shadow)/0.05)] will-change-transform sm:p-2.5 md:rounded-[28px] md:p-3"
  >
    <div className="h-full w-full rounded-[var(--radius-win)]">{children}</div>
  </motion.div>
);
