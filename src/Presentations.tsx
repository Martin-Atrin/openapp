// §19.1 presentations: kernel chrome that shows a module view as a sheet, modal or inline card.
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type PointerEvent as RPointerEvent } from "react";
import { X } from "lucide-react";
import type { Placement, Presented } from "./contract";
import { PlacementView } from "./PlacementView";
import { useShell } from "./store";

export const presentPlacementId = (p: Presented) => `present:${p.id}`;

function asPlacement(p: Presented): Placement {
  const i = p.view.indexOf("/");
  return { id: presentPlacementId(p), module: p.view.slice(0, i), view: p.view.slice(i + 1), anchor: "present", label: p.title };
}

/** Close the presentation through the kernel (view.close closes the top one; pass the view for clarity). */
function useClose() {
  const { runAction } = useShell();
  return useCallback((p: Presented) => void runAction(presentPlacementId(p), { host: "view.close", view: p.view }), [runAction]);
}

export function Presentations() {
  const { snapshot } = useShell();
  const close = useClose();
  const list = (snapshot.presented ?? []).filter((p) => p.as === "sheet" || p.as === "modal");
  const top = list[list.length - 1];

  // Esc closes the top presentation.
  useEffect(() => {
    if (!top) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) {
        e.preventDefault();
        close(top);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [top, close]);

  return (
    <>
      {list.map((p, i) => (p.as === "sheet" ? <Sheet key={p.id} p={p} z={40 + i} onClose={() => close(p)} /> : <Modal key={p.id} p={p} z={40 + i} onClose={() => close(p)} />))}
    </>
  );
}

function Header({ p, onClose, closeRef }: { p: Presented; onClose: () => void; closeRef?: React.Ref<HTMLButtonElement> }) {
  return (
    <div className="oa-pres-head">
      <h2 className="oa-pres-title">{p.title || p.view.split("/")[1]}</h2>
      <button type="button" ref={closeRef} className="oa-iconbtn" aria-label="Close" title="Close (Esc)" onClick={onClose}><X size={16} aria-hidden="true" /></button>
    </div>
  );
}

function Sheet({ p, z, onClose }: { p: Presented; z: number; onClose: () => void }) {
  const [width, setWidth] = useState(Math.max(280, p.width ?? 440));
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => closeRef.current?.focus(), []);

  const onDown = (e: RPointerEvent<HTMLDivElement>) => {
    e.preventDefault();
    const el = e.currentTarget;
    el.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => setWidth(Math.min(window.innerWidth - 80, Math.max(280, window.innerWidth - ev.clientX)));
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
  };
  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowLeft") setWidth((w) => Math.min(window.innerWidth - 80, w + 24));
    else if (e.key === "ArrowRight") setWidth((w) => Math.max(280, w - 24));
  };

  return (
    <aside className="oa-sheet" style={{ width, zIndex: z }} role="dialog" aria-label={p.title || p.view} data-present={p.id}>
      <div className="oa-sheet-grip" role="separator" aria-orientation="vertical" aria-label="Resize panel" aria-valuenow={width} tabIndex={0} onPointerDown={onDown} onKeyDown={onKey} />
      <Header p={p} onClose={onClose} closeRef={closeRef} />
      <div className="oa-pres-body"><PlacementView placement={asPlacement(p)} /></div>
    </aside>
  );
}

function Modal({ p, z, onClose }: { p: Presented; z: number; onClose: () => void }) {
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => closeRef.current?.focus(), []);
  return (
    <div className="oa-backdrop" style={{ zIndex: z }} onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="oa-modal" role="dialog" aria-modal="true" aria-label={p.title || p.view} style={{ width: Math.min(p.width ?? 480, window.innerWidth - 32) }} data-present={p.id}>
        <Header p={p} onClose={onClose} closeRef={closeRef} />
        <div className="oa-pres-body"><PlacementView placement={asPlacement(p)} /></div>
      </div>
    </div>
  );
}

/** Inline presentations: inserted after the chat item `after` (fallback: end of thread), scrolled into view. */
export function useInline(): Presented[] {
  const { snapshot } = useShell();
  return (snapshot.presented ?? []).filter((p) => p.as === "inline");
}

export function InlineList({ list, onClose }: { list: Presented[]; onClose: (p: Presented) => void }) {
  return <>{list.map((p) => <InlineCard key={p.id} p={p} onClose={() => onClose(p)} />)}</>;
}

export function useCloseInline() {
  return useClose();
}

function InlineCard({ p, onClose }: { p: Presented; onClose: () => void }) {
  const ref = useRef<HTMLElement>(null);
  useLayoutEffect(() => {
    ref.current?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }, []);
  return (
    <section ref={ref} className="oa-inline" data-present={p.id} aria-label={p.title || p.view}>
      <Header p={p} onClose={onClose} />
      <div className="oa-pres-body"><PlacementView placement={asPlacement(p)} /></div>
    </section>
  );
}
