import { useCallback, useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { ArrowUp, Hammer, MessageCircle, Square } from "lucide-react";
import { kernel } from "./kernel";
import { Anchor } from "./PlacementView";
import { useShell } from "./store";
import { ModelChip } from "./ModelChip";

const STATUS_LABEL: Record<string, string> = { unselected: "no provider", starting: "starting", ready: "ready", working: "working", error: "error" };

export function Composer({ empty }: { empty: boolean }) {
  const { snapshot, toast, runAction } = useShell();
  const { agent } = snapshot;
  const [text, setText] = useState("");
  const ta = useRef<HTMLTextAreaElement>(null);
  const working = agent.status === "working";
  const mode = snapshot.activeMode ?? snapshot.sessions?.find((x) => x.id === snapshot.activeSession)?.mode ?? "chat";
  const toggleMode = () => void runAction("chat", { host: "mode.set", mode: mode === "build" ? "chat" : "build" });

  // Auto-grow; refit when the width changes (stylesheet load, window/region resize).
  const fit = useCallback(() => {
    const el = ta.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 220)}px`;
  }, []);
  useLayoutEffect(fit, [text, fit]);
  useEffect(() => {
    const el = ta.current;
    if (!el) return;
    let w = el.clientWidth;
    const ro = new ResizeObserver(() => {
      if (el.clientWidth !== w) {
        w = el.clientWidth;
        fit();
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [fit]);

  // oa://composer {insert}: insert at cursor.
  useEffect(() => {
    let un: (() => void) | undefined;
    let alive = true;
    kernel.onComposer(({ insert }) => {
      const el = ta.current;
      setText((cur) => {
        const s = el?.selectionStart ?? cur.length;
        const e = el?.selectionEnd ?? cur.length;
        const next = cur.slice(0, s) + insert + cur.slice(e);
        const pos = s + insert.length;
        requestAnimationFrame(() => {
          el?.focus();
          el?.setSelectionRange(pos, pos);
        });
        return next;
      });
    }).then((u) => (alive ? (un = u) : u()));
    return () => {
      alive = false;
      un?.();
    };
  }, []);

  const send = useCallback(async () => {
    const t = text.trim();
    if (!t || working) return;
    setText("");
    try {
      await kernel.sendPrompt(t);
    } catch (err) {
      setText(t);
      toast(`Could not send: ${String(err)}`, "error");
    }
  }, [text, working, toast]);

  const stop = useCallback(() => {
    kernel.cancelTurn().catch((err) => toast(String(err), "error"));
  }, [toast]);

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void send();
    }
  };

  return (
    <div className="oa-composer-block" data-empty={empty}>
      <p className="oa-hint" aria-hidden={!empty}>Tell the agent what to build.</p>
      <div className="oa-composer">
        <Anchor name="composer.above" className="oa-composer-above" />
        <textarea
          ref={ta}
          className="oa-composer-input"
          rows={1}
          value={text}
          autoFocus
          placeholder={empty ? "Describe a feature, a layout, a look…" : "Message the agent"}
          aria-label="Message the agent"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className="oa-composer-bar">
          <div className="oa-composer-left">
            <button
              type="button"
              className={`oa-mode${mode === "build" ? " is-build" : ""}`}
              onClick={toggleMode}
              aria-pressed={mode === "build"}
              title={mode === "build" ? "Build mode: the agent may change the app. Click for Chat." : "Chat mode. Click for Build (the agent may change the app)."}
              aria-label={`Mode: ${mode}. Click to switch.`}
            >
              {mode === "build" ? <Hammer size={13} aria-hidden="true" /> : <MessageCircle size={13} aria-hidden="true" />}
              {mode === "build" ? "Build" : "Chat"}
            </button>
            <ModelChip mode={mode} />
            <div className={`oa-agent oa-agent-${agent.status}`} title={agent.error || undefined}>
              <span className="oa-agent-dot" aria-hidden="true" />
              <span className="oa-agent-name">{agent.name}</span>
              <span className="oa-agent-state">{STATUS_LABEL[agent.status] ?? agent.status}</span>
            </div>
          </div>
          <div className="oa-composer-right">
            <Anchor name="composer.actions" className="oa-composer-actions" />
            {working ? (
              <button type="button" className="oa-send oa-stop" onClick={stop} aria-label="Stop" title="Stop">
                <Square size={13} fill="currentColor" aria-hidden="true" />
              </button>
            ) : (
              <button type="button" className="oa-send" onClick={send} disabled={!text.trim()} aria-label="Send" title="Send (Enter)">
                <ArrowUp size={17} aria-hidden="true" />
              </button>
            )}
          </div>
        </div>
      </div>
      {agent.status === "error" && agent.error ? <p className="oa-agent-error" role="alert">{agent.error}</p> : null}
    </div>
  );
}
