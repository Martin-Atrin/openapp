// §21 G4 native shell, shell side: window mode, app-wide shortcuts, lucide → PNG icons for the tray,
// native file drops on views. Talks to the kernel through `primitive_command` (shell-only Tauri command).
import { createElement, useEffect, useRef } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { loadIcon } from "./Icon";
import type { Snapshot } from "./contract";

const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Window mode: the kernel opens module windows with `window.__OA_WINDOW__ = "<id>"`. */
export function windowId(): string | null {
  const w = (window as unknown as { __OA_WINDOW__?: unknown }).__OA_WINDOW__;
  if (typeof w === "string" && w) return w;
  const q = new URLSearchParams(window.location.search).get("window");
  return q || null;
}

export interface ShellWindow {
  id: string;
  view: string;
  module: string;
  title?: string;
  fullscreen?: boolean;
  alwaysOnTop?: boolean;
}

interface Shortcut {
  id: string;
  global: boolean;
  code: string;
  mods: { meta: boolean; ctrl: boolean; alt: boolean; shift: boolean };
  inInputs?: boolean;
}

export async function primitiveCommand<T = unknown>(name: string, args: Record<string, unknown>, placementId?: string): Promise<T> {
  if (!inTauri) throw new Error("not available in the browser preview");
  return invoke<T>("primitive_command", { name, args, placementId: placementId ?? null });
}

export function hostList<T>(snap: Snapshot, name: string): T[] {
  const v = snap.hostData?.[name];
  return Array.isArray(v) ? (v as T[]) : [];
}

// ---------- shortcuts ----------

export interface KeyInfo {
  code: string;
  meta: boolean;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  typing: boolean;
  repeat?: boolean;
}

let shortcuts: Shortcut[] = [];

/** Match a key press (from this document or forwarded by an html view) against app-wide shortcuts; fire it. */
export function handleKey(k: KeyInfo): boolean {
  if (k.repeat) return false;
  const sc = shortcuts.find(
    (s) => !s.global && s.code === k.code && s.mods.meta === k.meta && s.mods.ctrl === k.ctrl && s.mods.alt === k.alt && s.mods.shift === k.shift,
  );
  if (!sc || (k.typing && !sc.inInputs)) return false;
  primitiveCommand("shortcut.fire", { id: sc.id }).catch((e) => console.warn("shortcut", e));
  return true;
}

function isTyping(t: EventTarget | null): boolean {
  const el = t as HTMLElement | null;
  if (!el || !el.tagName) return false;
  return el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT" || el.isContentEditable;
}

export function useShortcuts(snapshot: Snapshot) {
  shortcuts = hostList<Shortcut>(snapshot, "shortcuts");
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      const hit = handleKey({ code: e.code, meta: e.metaKey, ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, typing: isTyping(e.target), repeat: e.repeat });
      if (hit) e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
}

// ---------- tray icons ----------

const rendering = new Set<string>();

async function renderIconPng(name: string, size = 44): Promise<string> {
  const host = document.createElement("div");
  host.style.cssText = "position:fixed;left:-9999px;top:-9999px";
  document.body.appendChild(host);
  const root = createRoot(host);
  // (flushSync does not flush inside an effect: render, then wait until the SVG is in the DOM)
  root.render(createElement(await loadIcon(name), { size, strokeWidth: 2, color: "#000", absoluteStrokeWidth: false }));
  for (let i = 0; i < 50 && !host.querySelector("svg"); i++) await new Promise((r) => setTimeout(r, 10));
  const svg = host.innerHTML;
  root.unmount();
  host.remove();
  const img = new Image();
  img.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(svg);
  await img.decode();
  const c = document.createElement("canvas");
  c.width = size;
  c.height = size;
  c.getContext("2d")!.drawImage(img, 0, 0, size, size);
  return c.toDataURL("image/png").split(",")[1] ?? "";
}

/** The kernel lists lucide icons it wants (tray item); render each once and hand back a PNG. */
export function useIconRenderer(snapshot: Snapshot) {
  const wanted = hostList<string>(snapshot, "shell.icons");
  useEffect(() => {
    for (const name of wanted) {
      if (rendering.has(name)) continue;
      rendering.add(name);
      renderIconPng(name)
        .then((png) => primitiveCommand("shell.icon", { name, png }))
        .catch((e) => console.warn("icon", name, e))
        .finally(() => setTimeout(() => rendering.delete(name), 5000));
    }
  }, [wanted.join(",")]);
}

// ---------- kernel → shell events ----------

export function useShellEvents(cb: (e: { op: string }) => void) {
  const ref = useRef(cb);
  ref.current = cb;
  useEffect(() => {
    if (!inTauri) return;
    let un: (() => void) | undefined;
    let alive = true;
    listen<{ op: string }>("oa://shell", (e) => ref.current(e.payload)).then((u) => (alive ? (un = u) : u()));
    return () => {
      alive = false;
      un?.();
    };
  }, []);
}

// ---------- native file drops ----------

export interface FileRef {
  path: string;
  name?: string;
  size?: number;
  mime?: string;
  error?: string;
}

/** The element a drop at (x, y) lands on: a spec dropzone, or an html view that called openapp.onDrop. */
function dropTarget(x: number, y: number): HTMLElement | null {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  if (!el) return null;
  const zone = el.closest<HTMLElement>("[data-dropzone]");
  if (zone) return zone;
  if (el.tagName === "IFRAME" && (el as HTMLElement).dataset.acceptsDrop === "1") return el;
  return null;
}

export function useFileDrops(toast: (text: string, tone?: "info" | "error") => void) {
  useEffect(() => {
    if (!inTauri) return;
    let un: (() => void) | undefined;
    let alive = true;
    let over: HTMLElement | null = null;
    const setOver = (el: HTMLElement | null) => {
      if (over === el) return;
      over?.classList.remove("oa-drop-over");
      over = el;
      over?.classList.add("oa-drop-over");
    };
    import("@tauri-apps/api/webview").then(({ getCurrentWebview }) =>
      getCurrentWebview()
        .onDragDropEvent(async (ev) => {
          const p = ev.payload;
          const dpr = window.devicePixelRatio || 1;
          if (p.type === "leave") return setOver(null);
          const x = p.position.x / dpr;
          const y = p.position.y / dpr;
          const target = dropTarget(x, y);
          if (p.type !== "drop") return setOver(target);
          setOver(null);
          if (!target) {
            if (p.paths.length) toast("Drop files on a view that accepts them.");
            return;
          }
          const placementId = target.closest<HTMLElement>("[data-placement]")?.dataset.placement;
          if (!placementId) return;
          try {
            const r = await primitiveCommand<{ files: FileRef[] }>("files.drop", { paths: p.paths }, placementId);
            target.dispatchEvent(new CustomEvent("oa-drop", { detail: r.files }));
          } catch (e) {
            toast(String(e), "error");
          }
        })
        .then((u) => (alive ? (un = u) : u())),
    );
    return () => {
      alive = false;
      un?.();
      setOver(null);
    };
  }, [toast]);
}
