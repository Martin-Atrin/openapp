import { useCallback, useEffect, useRef, useState } from "react";
import type { Placement } from "./contract";
import { kernel } from "./kernel";
import { useShell } from "./store";
import { publish as channelPublish, subscribe as channelSubscribe } from "./channels";
import { handleKey, type FileRef, type KeyInfo } from "./shell";

type Msg = { oa: string; id?: number; tool?: string; action?: string; args?: Record<string, unknown>; height?: number; message?: string; topic?: string; data?: unknown; b64?: string };

export function HtmlView({ placement, url, rev }: { placement: Placement; url: string; rev: number }) {
  const { vars, runAction, reportError, toast } = useShell();
  const ref = useRef<HTMLIFrameElement>(null);
  const [height, setHeight] = useState(120);
  // §21 G4: the page called openapp.onDrop (native drops land here)
  const [acceptsDrop, setAcceptsDrop] = useState(false);
  const fill = placement.id.startsWith("window:");
  const varsRef = useRef(vars);
  varsRef.current = vars;
  // §21 G1: channel subscriptions of the page (topic → unsubscribe); dropped when the page reloads or unmounts
  const subs = useRef(new Map<string, () => void>());
  useEffect(() => {
    const m = subs.current;
    return () => {
      m.forEach((u) => u());
      m.clear();
    };
  }, [url, rev]);

  const post = useCallback((msg: unknown) => {
    ref.current?.contentWindow?.postMessage(msg, "*");
  }, []);

  // Theme: on every change.
  useEffect(() => {
    post({ oa: "theme", vars });
  }, [vars, post]);

  useEffect(() => {
    const onMessage = (event: MessageEvent) => {
      const frame = ref.current;
      if (!frame || event.source !== frame.contentWindow) return;
      const m = event.data as Msg | null;
      if (!m || typeof m !== "object" || typeof m.oa !== "string") return;
      switch (m.oa) {
        case "ready":
          post({ oa: "theme", vars: varsRef.current });
          break;
        case "resize": {
          const h = Number(m.height);
          if (Number.isFinite(h)) setHeight(Math.max(40, Math.ceil(h)));
          break;
        }
        case "error":
          reportError(placement.id, `[html:${placement.module}/${placement.view}] ${String(m.message ?? "error")}`);
          break;
        case "call": {
          const id = m.id;
          kernel
            .iframeCall(placement.module, String(m.tool ?? ""), m.args ?? {})
            .then((result) => post({ oa: "result", id, ok: true, result }))
            .catch((err) => post({ oa: "result", id, ok: false, error: String(err) }));
          break;
        }
        case "subscribe": {
          const id = m.id;
          const topic = String(m.topic ?? "");
          if (subs.current.has(topic)) {
            post({ oa: "result", id, ok: true, result: null });
            break;
          }
          const un = channelSubscribe(
            placement.id,
            topic,
            (msg) => post({ oa: "channel", topic: msg.topic, seq: msg.seq, ts: msg.ts, data: msg.data, b64: msg.b64 }),
            (err) => {
              subs.current.delete(topic);
              post({ oa: "result", id, ok: false, error: err });
            },
            () => post({ oa: "result", id, ok: true, result: null }),
          );
          subs.current.set(topic, un);
          break;
        }
        case "unsubscribe": {
          const topic = String(m.topic ?? "");
          subs.current.get(topic)?.();
          subs.current.delete(topic);
          break;
        }
        case "publish": {
          const id = m.id;
          channelPublish(placement.id, String(m.topic ?? ""), m.data, m.b64)
            .then((result) => post({ oa: "result", id, ok: true, result }))
            .catch((err) => post({ oa: "result", id, ok: false, error: String(err) }));
          break;
        }
        case "host": {
          const id = m.id;
          const action = { host: String(m.action ?? ""), ...(m.args ?? {}) };
          if (action.host === "audio.record") {
            runAction(placement.id, action)
              .then((ok) => post({ oa: "result", id, ok, result: null, error: ok ? undefined : "host action failed" }))
              .catch((err) => post({ oa: "result", id, ok: false, error: String(err) }));
            break;
          }
          // the result goes back to the page (files.open refs, clipboard.read text, window.open id, …)
          kernel
            .uiAction(placement.id, action as never)
            .then((res) => {
              if (!res.ok) toast(res.error || "Action failed.", "error");
              post({ oa: "result", id, ok: res.ok, result: res.result ?? null, error: res.ok ? undefined : res.error || "host action failed" });
            })
            .catch((err) => post({ oa: "result", id, ok: false, error: String(err) }));
          break;
        }
        case "key":
          handleKey(m as unknown as KeyInfo);
          break;
        case "dropzone":
          setAcceptsDrop(!!(m as { on?: boolean }).on);
          break;
      }
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [placement.id, placement.module, placement.view, post, reportError, runAction, toast]);

  // §21 G4: files dropped on the iframe (the shell copied/granted them) → openapp.onDrop callbacks
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onDrop = (e: Event) => post({ oa: "drop", files: (e as CustomEvent<FileRef[]>).detail ?? [] });
    el.addEventListener("oa-drop", onDrop);
    return () => el.removeEventListener("oa-drop", onDrop);
  }, [post, url, rev]);

  // Microphone / camera are delegated to every module view; the kernel's capture gate (primitives/media.rs) allows
  // a view's oamod://<module> origin only with that module's grant (the bridge asks for it first: media.request).
  return (
    <iframe
      ref={ref}
      key={`${url}#${rev}`}
      className={fill ? "oa-iframe oa-iframe-fill" : "oa-iframe"}
      title={`${placement.module}/${placement.view}`}
      src={url}
      sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-modals allow-pointer-lock"
      allow="camera; microphone; fullscreen; gamepad"
      data-accepts-drop={acceptsDrop ? "1" : undefined}
      style={fill ? undefined : { height }}
      onLoad={() => post({ oa: "theme", vars: varsRef.current })}
    />
  );
}
