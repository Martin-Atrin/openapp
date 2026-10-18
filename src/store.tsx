import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { Action, ChatEvent, ChatItem, Placement, Snapshot } from "./contract";
import { kernel } from "./kernel";
import { applyTheme, themeVars } from "./theme";
import { recState, toggleRecording } from "./audio";

export type ToastTone = "info" | "error";
export interface Toast {
  id: number;
  text: string;
  tone: ToastTone;
}

export interface Shell {
  snapshot: Snapshot;
  chat: ChatItem[];
  toasts: Toast[];
  vars: Record<string, string>;
  toast(text: string, tone?: ToastTone): void;
  dismissToast(id: number): void;
  runAction(placementId: string, action: unknown, input?: Record<string, unknown>): Promise<boolean>;
  reportError(placementId: string, message: string): void;
  placementsAt(anchor: string): Placement[];
  /** a permission or question card is open: web surfaces must hide (§13) */
  overlayOpen: boolean;
}

const ShellContext = createContext<Shell | null>(null);

export function useShell(): Shell {
  const s = useContext(ShellContext);
  if (!s) throw new Error("useShell outside provider");
  return s;
}

const EMPTY: Snapshot = {
  layout: { regions: [], placements: [] },
  theme: {},
  modules: [],
  views: {},
  agent: { name: "Agent", status: "starting" },
  chat: [],
};

function applyChat(chat: ChatItem[], e: ChatEvent): ChatItem[] {
  if (e.op === "append") {
    const i = chat.findIndex((c) => c.id === e.item.id);
    if (i >= 0) return chat.map((c, j) => (j === i ? e.item : c));
    return [...chat, e.item];
  }
  return chat.map((c) => (c.id === e.id ? { ...c, ...e.patch } : c));
}

export function ShellProvider({ children }: { children: ReactNode }) {
  const [snapshot, setSnapshot] = useState<Snapshot>(EMPTY);
  const [chat, setChat] = useState<ChatItem[]>([]);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [booted, setBooted] = useState(false);
  const toastId = useRef(0);

  const dismissToast = useCallback((id: number) => setToasts((t) => t.filter((x) => x.id !== id)), []);
  const toast = useCallback((text: string, tone: ToastTone = "info") => {
    const id = ++toastId.current;
    setToasts((t) => [...t.slice(-3), { id, text, tone }]);
    setTimeout(() => dismissToast(id), tone === "error" ? 7000 : 4000);
  }, [dismissToast]);

  // Subscribe first, then fetch the snapshot; buffer chat events that race the initial fetch.
  useEffect(() => {
    let alive = true;
    const unsubs: (() => void)[] = [];
    const buffer: ChatEvent[] = [];
    let initialised = false;
    const keep = (p: Promise<() => void>) => p.then((u) => (alive ? unsubs.push(u) : u()));

    keep(kernel.onSnapshot((s) => {
      setSnapshot(s);
      setChat(s.chat ?? []); // kernel snapshot is authoritative (e.g. after /reset)
    }));
    keep(kernel.onChat((e) => {
      if (!initialised) buffer.push(e);
      else setChat((c) => applyChat(c, e));
    }));
    keep(kernel.onNotify((n) => toast(n.text)));

    kernel.getSnapshot().then((s) => {
      if (!alive) return;
      setSnapshot(s);
      setChat(buffer.reduce(applyChat, s.chat ?? []));
      initialised = true;
      setBooted(true);
    }).catch((err) => {
      if (!alive) return;
      initialised = true;
      setBooted(true);
      toast(`Could not reach the kernel: ${String(err)}`, "error");
    });
    return () => {
      alive = false;
      unsubs.forEach((u) => u());
    };
  }, [toast]);

  const vars = useMemo(() => themeVars(snapshot.theme), [snapshot.theme]);
  useEffect(() => applyTheme(vars), [vars]);

  const runAction = useCallback(async (placementId: string, action: unknown, input?: Record<string, unknown>) => {
    if (!action || typeof action !== "object") {
      toast("This control has no valid action.", "error");
      return false;
    }
    let act = action as Action;
    if ("host" in act && act.host === "audio.record") {
      // Host microphone: handled in the shell (getUserMedia + MediaRecorder), bytes go out via save_audio.
      // Before a new recording starts, the kernel checks the module's microphone grant (it may show a consent card).
      try {
        if (recState().recording !== placementId) {
          const res = await kernel.uiAction(placementId, act);
          if (!res.ok) {
            toast(res.error || "Microphone access was not allowed.", "error");
            return false;
          }
        }
        return await toggleRecording(placementId, act);
      } catch (err) {
        toast((err as Error).message || String(err), "error");
        return false;
      }
    }
    if (input && !("host" in act)) act = { ...act, args: { ...(act.args ?? {}), ...input } };
    try {
      const res = await kernel.uiAction(placementId, act, input);
      if (!res.ok) {
        toast(res.error || "Action failed.", "error");
        return false;
      }
      return true;
    } catch (err) {
      toast(String(err), "error");
      return false;
    }
  }, [toast]);

  const reportError = useCallback((placementId: string, message: string) => {
    kernel.reportUiError(placementId, message).catch(() => {});
  }, []);

  const placementsAt = useCallback((anchor: string) => {
    return snapshot.layout.placements
      .map((p, i) => ({ p, i }))
      .filter(({ p }) => p.anchor === anchor)
      .sort((a, b) => (a.p.order ?? 0) - (b.p.order ?? 0) || a.i - b.i)
      .map(({ p }) => p);
  }, [snapshot.layout.placements]);

  const overlayOpen = useMemo(
    () => chat.some((c) => (c.kind === "permission" || c.kind === "question") && c.resolved === undefined),
    [chat],
  );

  const value = useMemo<Shell>(
    () => ({ snapshot, chat, toasts, vars, toast, dismissToast, runAction, reportError, placementsAt, overlayOpen }),
    [snapshot, chat, toasts, vars, toast, dismissToast, runAction, reportError, placementsAt, overlayOpen],
  );

  // Avoid flashing the empty-state before the first snapshot arrives.
  return <ShellContext.Provider value={value}>{booted ? children : null}</ShellContext.Provider>;
}
