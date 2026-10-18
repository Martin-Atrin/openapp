import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Composer } from "./Composer";
import { Picker } from "./Picker";
import { Transcript } from "./Chat";
import { Anchor, PlacementView } from "./PlacementView";
import { hostList, useFileDrops, useIconRenderer, useShellEvents, useShortcuts, windowId, type ShellWindow } from "./shell";
import { Presentations } from "./Presentations";
import { Region } from "./Region";
import { ShellProvider, useShell } from "./store";
import { Toasts } from "./Toasts";
import { ChevronDown } from "lucide-react";
import type { Edge } from "./contract";

function Edges({ edge }: { edge: Edge }) {
  const { snapshot } = useShell();
  const regions = snapshot.layout.regions.filter((r) => r.edge === edge);
  if (!regions.length) return null;
  return (
    <div className={`oa-edge oa-edge-${edge}`}>
      {regions.map((r) => (
        <Region key={r.id} region={r} />
      ))}
    </div>
  );
}

/** §19.1: `main` is a pinned, collapsible strip at the top of the chat column (outside the scroller). */
function Pinned() {
  const { placementsAt } = useShell();
  const [open, setOpen] = useState(true);
  if (!placementsAt("main").length) return null;
  return (
    <div className="oa-pinned">
      <div className="oa-pinned-inner">
        <div className="oa-pinned-bar">
          <button type="button" className="oa-pinned-toggle" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
            <ChevronDown size={13} style={{ transform: open ? undefined : "rotate(-90deg)" }} aria-hidden="true" />
            Pinned
          </button>
        </div>
        {open ? <div className="oa-pinned-body"><Anchor name="main" className="oa-main" /></div> : null}
      </div>
    </div>
  );
}

function Center() {
  const { chat, placementsAt, snapshot } = useShell();
  const activeSession = snapshot.activeSession;
  const { agent } = snapshot;
  // §17: a VM provider that is not signed in yet shows its sign-in card instead of the composer.
  const current = snapshot.harnesses?.find((h) => h.id === agent.harness);
  const needsSignIn = !!current?.vm && current.signedIn === false && agent.status === "ready";
  // "Pick another provider" (after an error or from the sign-in card) reopens the picker locally;
  // cleared once a new choice starts.
  const [forced, setForced] = useState(false);
  // §21 G4: OpenApp menu › Provider… reopens the picker
  useShellEvents((e) => {
    if (e.op === "provider") setForced(true);
  });
  useEffect(() => {
    if (forced && (agent.status === "starting" || agent.status === "working" || (agent.status === "ready" && !needsSignIn))) setForced(false);
  }, [forced, agent.status, needsSignIn]);
  const pickerMode: "pick" | "starting" | "error" | "signin" | null =
    forced || agent.status === "unselected" ? "pick"
    : agent.status === "error" ? "error"
    : needsSignIn ? "signin"
    : agent.status === "starting" && chat.length === 0 ? "starting"
    : null;
  const hasMain = placementsAt("main").length > 0;
  const empty = chat.length === 0 && !hasMain;
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const onScroll = () => {
    const el = scroller.current;
    if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
  };
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [chat]);
  // Session switch: the transcript swaps; jump to the bottom of the new one (composer text is local state and stays).
  useLayoutEffect(() => {
    stick.current = true;
    const el = scroller.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [activeSession]);
  // Keep pinned when the content resizes (html views, images).
  useEffect(() => {
    const el = scroller.current?.firstElementChild;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      const s = scroller.current;
      if (s && stick.current) s.scrollTop = s.scrollHeight;
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  return (
    <main className="oa-center" data-empty={empty}>
      <div className="oa-spacer oa-spacer-top" />
      <Pinned />
      <div className="oa-stream" ref={scroller} onScroll={onScroll}>
        <div className="oa-stream-inner">
          <Transcript chat={chat} />
        </div>
      </div>
      {pickerMode ? (
        <Picker mode={pickerMode} empty={empty} onReopen={() => setForced(true)} />
      ) : (
        <Composer empty={empty} />
      )}
      <div className="oa-spacer oa-spacer-bottom" />
    </main>
  );
}

/** §21 G4: app-wide shortcuts and native file drops in every window; tray icons rendered by the main window. */
function NativeShell({ main }: { main: boolean }) {
  const { snapshot, toast } = useShell();
  useShortcuts(snapshot);
  useFileDrops(toast);
  return main ? <IconRenderer /> : null;
}

function IconRenderer() {
  const { snapshot } = useShell();
  useIconRenderer(snapshot);
  return null;
}

/** §21 G4 window mode: a module view alone in its own native window (placement `window:<id>`). */
function WindowShell({ id }: { id: string }) {
  const { snapshot } = useShell();
  const w = hostList<ShellWindow>(snapshot, "windows").find((x) => x.id === id);
  useEffect(() => {
    if (w?.title) document.title = w.title;
  }, [w?.title]);
  const slash = w ? w.view.indexOf("/") : -1;
  return (
    <div className="oa-window">
      <NativeShell main={false} />
      {w && slash > 0 ? (
        <PlacementView placement={{ id: `window:${id}`, module: w.view.slice(0, slash), view: w.view.slice(slash + 1), anchor: "window", label: w.title }} />
      ) : (
        <div className="oa-window-empty">This window is no longer open.</div>
      )}
      <Toasts />
    </div>
  );
}

function Shell() {
  return (
    <div className="oa-shell">
      <NativeShell main />
      <Edges edge="top" />
      <div className="oa-middle">
        <Edges edge="left" />
        <Center />
        <Edges edge="right" />
      </div>
      <Edges edge="bottom" />
      <Presentations />
      <Toasts />
    </div>
  );
}

export default function App() {
  const win = windowId();
  return <ShellProvider>{win ? <WindowShell id={win} /> : <Shell />}</ShellProvider>;
}
