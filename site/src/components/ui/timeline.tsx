// Aceternity UI — Timeline (registry: @aceternity/timeline).
// Adapted: strict TS, the demo heading replaced by a `header` slot, a teal beam on a
// `line` track, token dots and titles, height re-measured with ResizeObserver
// (fonts and minis change it after first paint), full static beam under reduced motion.
import { useMotionValueEvent, useScroll, useTransform, motion, useReducedMotion } from "motion/react";
import { useEffect, useRef, useState, type ReactNode } from "react";

export interface TimelineEntry {
  title: ReactNode;
  content: ReactNode;
}

export const Timeline = ({ data, header }: { data: TimelineEntry[]; header?: ReactNode }) => {
  const ref = useRef<HTMLDivElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState(0);
  const [reached, setReached] = useState(-1);
  const reduce = useReducedMotion();

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setHeight(el.getBoundingClientRect().height));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const { scrollYProgress } = useScroll({ target: containerRef, offset: ["start 10%", "end 50%"] });
  const heightTransform = useTransform(scrollYProgress, [0, 1], [0, height]);
  const opacityTransform = useTransform(scrollYProgress, [0, 0.1], [0, 1]);

  // Light each step's dot once the beam passes it.
  useMotionValueEvent(heightTransform, "change", (h) => {
    const wrap = ref.current;
    if (!wrap) return;
    const top = wrap.getBoundingClientRect().top;
    let last = -1;
    wrap.querySelectorAll<HTMLElement>("[data-dot]").forEach((dot, i) => {
      if (dot.getBoundingClientRect().top - top + 8 <= h) last = i;
    });
    setReached(last);
  });

  return (
    <div className="w-full" ref={containerRef}>
      {header}
      <div ref={ref} className="relative pb-4">
      <ol className="m-0 list-none p-0">
        {data.map((item, index) => {
          const lit = reduce || index <= reached;
          return (
            <li key={index} className="flex justify-start pt-10 md:gap-10 md:pt-24 first:md:pt-12">
              <div className="sticky top-32 z-40 flex flex-col items-center self-start md:w-full md:max-w-xs md:flex-row lg:max-w-sm">
                <div className="absolute left-3 flex h-10 w-10 items-center justify-center rounded-full bg-paper">
                  <div
                    data-dot
                    className={
                      "h-4 w-4 rounded-full border p-2 transition-colors duration-300 " +
                      (lit ? "border-vm bg-vm" : "border-line bg-vm-soft")
                    }
                  />
                </div>
                <h3 className="hidden text-[1.45rem] leading-tight md:block md:pl-20">{item.title}</h3>
              </div>

              <div className="relative w-full min-w-0 pr-0 pl-16 md:pl-4">
                <h3 className="mb-4 block text-left text-[1.25rem] md:hidden">{item.title}</h3>
                {item.content}
              </div>
            </li>
          );
        })}
      </ol>
        <div
          aria-hidden="true"
          style={{ height: height + "px" }}
          className="absolute top-0 left-8 w-[2px] overflow-hidden bg-[linear-gradient(to_bottom,var(--tw-gradient-stops))] from-transparent from-[0%] via-line to-transparent to-[99%] [mask-image:linear-gradient(to_bottom,transparent_0%,black_10%,black_90%,transparent_100%)]"
        >
          <motion.div
            style={reduce ? { height: "100%" } : { height: heightTransform, opacity: opacityTransform }}
            className="absolute inset-x-0 top-0 w-[2px] rounded-full bg-gradient-to-t from-vm from-[0%] via-vm/60 via-[10%] to-transparent"
          />
        </div>
      </div>
    </div>
  );
};
