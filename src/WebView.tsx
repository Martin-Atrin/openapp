// §13 web surface: a placeholder element whose viewport rect is reported to the kernel, which draws a native
// child webview on top. Nothing in the shell may overlap this element.
import { useEffect, useRef, useState } from "react";
import { Check, Globe, Loader2 } from "lucide-react";
import type { Guide, Placement, WebRect } from "./contract";
import { kernel } from "./kernel";
import { useShell } from "./store";

/** Intersect the element rect with every clipping ancestor and the viewport (so a scrolled-away view hides). */
function visibleRect(el: HTMLElement): WebRect | null {
  const r = el.getBoundingClientRect();
  let l = r.left, t = r.top, rt = r.right, b = r.bottom;
  for (let p = el.parentElement; p; p = p.parentElement) {
    const o = getComputedStyle(p);
    if (o.overflowX !== "visible" || o.overflowY !== "visible") {
      const c = p.getBoundingClientRect();
      l = Math.max(l, c.left); t = Math.max(t, c.top); rt = Math.min(rt, c.right); b = Math.min(b, c.bottom);
    }
  }
  l = Math.max(l, 0); t = Math.max(t, 0); rt = Math.min(rt, window.innerWidth); b = Math.min(b, window.innerHeight);
  const width = rt - l, height = b - t;
  if (width < 2 || height < 2) return null;
  return { x: Math.round(l), y: Math.round(t), width: Math.round(width), height: Math.round(height) };
}

const same = (a: WebRect | null, b: WebRect | null) =>
  a === b || (!!a && !!b && a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height);

export function WebView({ placement, url }: { placement: Placement; url: string }) {
  const { overlayOpen, snapshot } = useShell();
  const ref = useRef<HTMLDivElement>(null);
  // Sheets/modals draw over the canvas: web surfaces outside the top presentation must hide (native views sit on top).
  const pres = (snapshot.presented ?? []).filter((p) => p.as === "sheet" || p.as === "modal");
  const inPresentation = placement.id.startsWith("present:");
  const coveredByPresentation = inPresentation ? placement.id !== `present:${pres[pres.length - 1]?.id}` : pres.length > 0;
  const blocked = useRef(overlayOpen || coveredByPresentation);
  blocked.current = overlayOpen || coveredByPresentation;
  const state = snapshot.web?.[placement.id];
  // §20.3 guide strip: for this placement, or (no placement given) for the web view in the top sheet/modal
  const guide = snapshot.guide;
  const topPid = pres.length ? `present:${pres[pres.length - 1]!.id}` : undefined;
  const showGuide = !!guide && guide.steps.length > 0 && (guide.placement ? guide.placement === placement.id : placement.id === topPid);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const id = placement.id;
    let last: WebRect | null | undefined;
    let raf = 0;
    const send = (r: WebRect | null) => {
      if (last !== undefined && same(last, r)) return;
      last = r;
      kernel.webBounds(id, r).catch(() => {});
    };
    // One rAF-throttled pass per frame: catches resizes, scrolls, window resizes AND pure position shifts
    // (region animation, transcript growth) which ResizeObserver alone would miss. Only sends on change.
    const tick = () => {
      send(blocked.current ? null : visibleRect(el));
      raf = requestAnimationFrame(tick);
    };
    tick();
    return () => {
      cancelAnimationFrame(raf);
      last = undefined;
      kernel.webBounds(id, null).catch(() => {}); // unmounted / inactive tab
    };
  }, [placement.id]);

  return (
    <div className={`oa-webbox${showGuide ? " has-guide" : ""}`}>
      {showGuide && guide ? <GuideStrip guide={guide} placementId={placement.id} /> : null}
      <div ref={ref} className="oa-web" data-placement-web={placement.id}>
        <div className="oa-web-ph" aria-hidden="true">
          {state?.loading ? <Loader2 size={18} className="oa-spin" /> : <Globe size={18} />}
          <span className="oa-web-title">{state?.title || "Web"}</span>
          <span className="oa-web-url">{state?.url || url}</span>
        </div>
      </div>
    </div>
  );
}

/** Kernel chrome above a web view: the step the user is on and a Done button (sends "Done: <step>" to the agent). */
function GuideStrip({ guide, placementId }: { guide: Guide; placementId: string }) {
  const { runAction } = useShell();
  const [busy, setBusy] = useState(false);
  const n = guide.steps.length;
  const cur = Math.min(Math.max(0, guide.current), n - 1);
  const step = guide.steps[cur]!;
  const done = guide.done?.includes(cur) ?? false;
  return (
    <div className="oa-guide" role="region" aria-label="Guide" aria-live="polite">
      <div className="oa-guide-count">Step {cur + 1} of {n}</div>
      <div className="oa-guide-body">
        <div className="oa-guide-title">{step.title}</div>
        {step.detail ? <div className="oa-guide-detail">{step.detail}</div> : null}
      </div>
      <button
        type="button"
        className="oa-btn oa-btn-primary"
        disabled={done || busy}
        onClick={async () => {
          setBusy(true);
          await runAction(placementId, { host: "guide.done", step: cur });
          setBusy(false);
        }}
      >
        {done ? <><Check size={14} aria-hidden="true" /> Sent</> : "Done"}
      </button>
    </div>
  );
}
